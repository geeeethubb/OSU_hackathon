//! Pass C, one person partition at a time: sessions, attribution, assists
//! and the daily rows.

use std::cmp::Ordering;

use super::arena::PartList;
use super::codes::{id_cmp, unmix, CODE_MASK};
use super::{Ctx, LOCAL_BITS};
use crate::util::{DAY_MS, SESSION_GAP_MS, WEEK_MS};

const TS_MASK: u64 = 0x7FFF_FFFF;

pub const NO_CATEGORY: u32 = u32::MAX;

/// What one worker produces in pass C.
pub struct PassCOut {
    /// user_daily rows, column by column.
    pub names: Vec<u64>,
    pub days: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub top_category: Vec<u32>,
    /// `[sessions, purchasing_sessions, revenue_cents, assisted_sessions]`
    /// per `(campaign, day)`, indexed `campaign * day_count + day`.
    pub campaign_days: Vec<[i64; 4]>,
}

impl PassCOut {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            names: Vec::new(),
            days: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top_category: Vec::new(),
            campaign_days: vec![[0; 4]; ctx.campaign_ids.len() * ctx.day_count],
        }
    }
}

pub struct ScratchC {
    words: Vec<u64>,
    events: Vec<u64>,
    purchases: Vec<(u64, u64)>,
    clicks: Vec<(u64, u64)>,
    assist_stamp: Vec<u32>,
    serial: u32,
    category_revenue: Vec<i64>,
}

impl ScratchC {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            words: Vec::new(),
            events: Vec::new(),
            purchases: Vec::new(),
            clicks: Vec::new(),
            assist_stamp: vec![0; ctx.campaign_ids.len()],
            serial: 0,
            category_revenue: vec![0; ctx.category_names.len()],
        }
    }
}

#[inline]
fn event_ts(key: u64) -> i64 {
    ((key >> 1) & TS_MASK) as i64
}

/// A click's time relative to `window_start` (so it can be negative).
#[inline]
fn click_ts(key: u64) -> i64 {
    ((key >> 1) & TS_MASK) as i64 - WEEK_MS
}

/// Appends one finished user_daily row. `totals` is
/// `[sessions, events, revenue_cents, purchases]`.
fn flush_row(ctx: &Ctx, out: &mut PassCOut, name: u64, day: i64, totals: [i64; 4], category_revenue: &mut [i64]) {
    out.names.push(name);
    out.days.push(day as i32);
    out.sessions.push(totals[0]);
    out.events.push(totals[1]);
    out.revenue.push(totals[2]);
    if totals[3] == 0 {
        out.top_category.push(NO_CATEGORY);
        return;
    }
    // Highest summed purchase cents; a tie goes to the byte-wise smallest name.
    let mut best = 0;
    for candidate in 1..category_revenue.len() {
        let better = category_revenue[candidate] > category_revenue[best]
            || (category_revenue[candidate] == category_revenue[best]
                && ctx.category_names[candidate] < ctx.category_names[best]);
        if better {
            best = candidate;
        }
    }
    out.top_category.push(best as u32);
    category_revenue.iter_mut().for_each(|sum| *sum = 0);
}

/// Sessions, attribution and daily rows for one person partition.
pub fn sessions_partition(
    ctx: &Ctx,
    part: usize,
    event_lists: &[PartList],
    click_lists: &[PartList],
    click_exotic: &[Box<[u8]>],
    out: &mut PassCOut,
    scratch: &mut ScratchC,
) {
    scratch.words.clear();
    for list in event_lists {
        list.copy_into(&mut scratch.words);
    }
    scratch.events.clear();
    scratch.purchases.clear();
    let mut at = 0;
    while at < scratch.words.len() {
        let key = scratch.words[at];
        at += 1;
        scratch.events.push(key);
        if key & 1 == 1 {
            scratch.purchases.push((key, scratch.words[at]));
            at += 1;
        }
    }
    if scratch.events.is_empty() {
        return;
    }
    scratch.words.clear();
    for list in click_lists {
        list.copy_into(&mut scratch.words);
    }
    scratch.clicks.clear();
    for pair in scratch.words.chunks_exact(2) {
        scratch.clicks.push((pair[0], pair[1]));
    }

    // (person, ts) order for events and purchases. Clicks: latest ts last and,
    // within one ts, the byte-wise smallest click ID last, so the click that
    // credits a session is always the last one at or before its start.
    scratch.events.sort_unstable();
    scratch.purchases.sort_unstable_by_key(|purchase| purchase.0);
    scratch.clicks.sort_unstable_by(|a, b| match a.0.cmp(&b.0) {
        Ordering::Equal => id_cmp(b.1 & CODE_MASK, a.1 & CODE_MASK, b'k', click_exotic),
        other => other,
    });

    let events = &scratch.events;
    let purchases = &scratch.purchases;
    let clicks = &scratch.clicks;
    let category_revenue = &mut scratch.category_revenue;
    let assist_stamp = &mut scratch.assist_stamp;
    let day_count = ctx.day_count;
    let organic = ctx.organic as usize;

    let mut event_at = 0;
    let mut click_at = 0;
    let mut purchase_at = 0;
    while event_at < events.len() {
        let local = events[event_at] >> 32;
        let mut person_end = event_at;
        while person_end < events.len() && events[person_end] >> 32 == local {
            person_end += 1;
        }
        while click_at < clicks.len() && clicks[click_at].0 >> 32 < local {
            click_at += 1;
        }
        let clicks_start = click_at;
        while click_at < clicks.len() && clicks[click_at].0 >> 32 == local {
            click_at += 1;
        }
        while purchase_at < purchases.len() && purchases[purchase_at].0 >> 32 < local {
            purchase_at += 1;
        }
        let purchases_start = purchase_at;
        while purchase_at < purchases.len() && purchases[purchase_at].0 >> 32 == local {
            purchase_at += 1;
        }
        let person_events = &events[event_at..person_end];
        let person_clicks = &clicks[clicks_start..click_at];
        let person_purchases = &purchases[purchases_start..purchase_at];
        let name = unmix(((part as u64) << LOCAL_BITS) | local);

        // The open user_daily row.
        let mut row_day = i64::MIN;
        let (mut row_sessions, mut row_events, mut row_revenue, mut row_purchases) = (0i64, 0i64, 0i64, 0i64);

        let mut at = 0;
        let mut purchase_cursor = 0;
        while at < person_events.len() {
            let start = event_ts(person_events[at]);
            let day = (ctx.window_start + start).div_euclid(DAY_MS);
            if day != row_day {
                if row_day != i64::MIN {
                    flush_row(ctx, out, name, row_day, [row_sessions, row_events, row_revenue, row_purchases], category_revenue);
                }
                row_day = day;
                (row_sessions, row_events, row_revenue, row_purchases) = (0, 0, 0, 0);
            }
            let mut last = start;
            let mut session_events = 0i64;
            while at < person_events.len() {
                let ts = event_ts(person_events[at]);
                if ts - last > SESSION_GAP_MS {
                    break;
                }
                last = ts;
                session_events += 1;
                at += 1;
            }
            let (mut session_revenue, mut session_purchases) = (0i64, 0i64);
            while purchase_cursor < person_purchases.len() && event_ts(person_purchases[purchase_cursor].0) <= last {
                let detail = person_purchases[purchase_cursor].1;
                let price = (detail & 0xFFFF_FFFF) as i64;
                session_revenue += price;
                session_purchases += 1;
                category_revenue[(detail >> 32) as usize] += price;
                purchase_cursor += 1;
            }
            row_sessions += 1;
            row_events += session_events;
            row_revenue += session_revenue;
            row_purchases += session_purchases;

            // Clicks with start - 7 days <= ts <= start.
            let first = person_clicks.partition_point(|click| click_ts(click.0) < start - WEEK_MS);
            let end = person_clicks.partition_point(|click| click_ts(click.0) <= start);
            let day_index = (day - ctx.first_day) as usize;
            let credited = if end > first { (person_clicks[end - 1].1 >> 42) as usize } else { organic };
            let cell = &mut out.campaign_days[credited * day_count + day_index];
            cell[0] += 1;
            cell[1] += i64::from(session_purchases > 0);
            cell[2] += session_revenue;
            scratch.serial = scratch.serial.wrapping_add(1);
            if scratch.serial == 0 {
                assist_stamp.iter_mut().for_each(|stamp| *stamp = 0);
                scratch.serial = 1;
            }
            for click in &person_clicks[first..end] {
                let campaign = (click.1 >> 42) as usize;
                if assist_stamp[campaign] != scratch.serial {
                    assist_stamp[campaign] = scratch.serial;
                    out.campaign_days[campaign * day_count + day_index][3] += 1;
                }
            }
        }
        flush_row(ctx, out, name, row_day, [row_sessions, row_events, row_revenue, row_purchases], category_revenue);
        event_at = person_end;
    }
}

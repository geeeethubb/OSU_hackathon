//! Pass C, one person partition at a time: sessions, attribution, assists
//! and the daily rows.
//!
//! A partition is never sorted as a whole. Its events, purchases and clicks
//! are grouped by person with a counting sort (a small hash table gives every
//! person of the partition a dense index). Then, per person:
//!
//! * Sessions. A few timestamps are sorted and scanned. Many timestamps are
//!   summarized per half-hour bucket (min, max, count): two events of one
//!   bucket are less than 30 minutes apart, so a bucket never holds a session
//!   break, and walking the occupied buckets in order a new session starts
//!   exactly where `bucket.min - previous_bucket.max` is over 30 minutes.
//! * Purchases go to the session that holds their timestamp: the last one
//!   that starts at or before it.
//! * Clicks. A click at `t` can credit or assist exactly the sessions with
//!   `t <= start <= t + 7 days`, a contiguous run of the person's sessions.
//!   The click that credits a session is the latest one at or before its
//!   start, so it is enough to keep, per session, the best click among those
//!   whose run begins there and to carry it forward while it is in reach.
//!   A campaign assists the union of its clicks' runs, kept as one bit per
//!   session and counted per day with popcounts. No click is ever paired
//!   with every session it reaches.
//!
//! Rows of one person are emitted together, in day order; persons come in the
//! order they first appear in the partition's lists.

use std::cmp::Ordering;

use super::arena::PartList;
use super::codes::{id_cmp, unmix, CODE_MASK};
use super::{Ctx, LOCAL_BITS};
use crate::util::{DAY_MS, SESSION_GAP_MS, WEEK_MS};

const TS_MASK: u64 = 0x7FFF_FFFF;
const GAP: u32 = SESSION_GAP_MS as u32;
const WEEK: u32 = WEEK_MS as u32;
const DAY: u32 = DAY_MS as u32;

/// Half-hour buckets a 31-bit timestamp can fall into.
const BUCKETS: usize = (TS_MASK / SESSION_GAP_MS as u64) as usize + 1;
const BUCKET_WORDS: usize = BUCKETS.div_ceil(64);
/// Days a 31-bit timestamp can fall on, for a window that starts late in a day.
const MAX_ROWS: usize = ((TS_MASK + DAY_MS as u64) / DAY_MS as u64) as usize + 1;
/// A bucket summary `[min, max, count, unused]` that no event has touched.
const EMPTY_CELL: [u32; 4] = [u32::MAX, 0, 0, 0];

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

/// What pass C needs from `Ctx`, in the form it uses.
struct Params {
    /// Epoch day of the window's first day.
    first_day: i64,
    /// Milliseconds from that day's midnight to `window_start`.
    day_offset: u32,
    day_count: usize,
    campaigns: usize,
    organic: usize,
    categories: usize,
    /// Per category, how many category names are byte-wise smaller: the
    /// order that decides a tie between two categories' sums.
    category_rank: Vec<u32>,
}

impl Params {
    fn new(
        window_start: i64,
        first_day: i64,
        day_count: usize,
        campaigns: usize,
        organic: usize,
        category_names: &[Box<[u8]>],
    ) -> Self {
        let day_offset = window_start - first_day * DAY_MS;
        assert!((0..DAY_MS).contains(&day_offset));
        let category_rank =
            category_names.iter().map(|name| category_names.iter().filter(|other| *other < name).count() as u32).collect();
        Self {
            first_day,
            day_offset: day_offset as u32,
            day_count,
            campaigns,
            organic,
            categories: category_names.len(),
            category_rank,
        }
    }
}

/// Where the per-person strategies switch. Every setting gives the same
/// output; the tests run them all.
#[derive(Clone, Copy)]
struct Tuning {
    /// A person with at most this many events is sorted; a bigger one goes
    /// through the bucket summaries.
    sort_max: usize,
    /// Session starts are counted one by one for a click while
    /// `sessions * clicks` is below this; above, a per-bucket table answers.
    lookup_min: usize,
}

impl Tuning {
    const DEFAULT: Tuning = Tuning { sort_max: 32, lookup_min: 512 };
}

/// The persons of one partition and their rows, grouped.
#[derive(Default)]
struct Group {
    /// Open addressing: `(local key + 1) << 31 | person index`, 0 for empty.
    table: Vec<u64>,
    /// `32 - log2(table.len())`.
    shift: u32,
    /// Local key per person index, in order of first appearance.
    locals: Vec<u32>,
    /// Per person `[events, purchases, clicks]`: counts while the lists are
    /// read, then the end of the person's slice in the grouped arrays.
    ends: Vec<[u32; 3]>,
    /// Events in list order: `person index << 32 | ts`.
    staged: Vec<u64>,
    /// Purchases in list order: `[person index << 32 | ts, price | category << 32]`.
    staged_purchases: Vec<[u64; 2]>,
    /// Clicks in list order: `[person index << 32 | ts, click code | campaign << 42]`.
    staged_clicks: Vec<[u64; 2]>,
    /// Event timestamps, person after person.
    events: Vec<u32>,
    /// `[ts, price | category << 32]`, person after person.
    purchases: Vec<[u64; 2]>,
    /// `[ts, click code | campaign << 42]`, person after person.
    clicks: Vec<[u64; 2]>,
}

const INDEX_MASK: u64 = 0x7FFF_FFFF;
const HASH: u32 = 0x9E37_79B1;

impl Group {
    /// Empties the table, sized for about as many persons as the previous partition had.
    fn begin(&mut self) {
        let capacity = (self.locals.len() * 4).clamp(256, 1 << 24).next_power_of_two();
        self.table.clear();
        self.table.resize(capacity, 0);
        self.shift = 32 - capacity.trailing_zeros();
        self.locals.clear();
        self.ends.clear();
    }

    /// The index of the person with this local key, new or known.
    #[inline(always)]
    fn person(&mut self, local: u32) -> usize {
        let tag = u64::from(local) + 1;
        let mask = self.table.len() - 1;
        let mut slot = (local.wrapping_mul(HASH) >> self.shift) as usize;
        loop {
            // SAFETY: `slot` is below the table's power-of-two length.
            let entry = unsafe { *self.table.get_unchecked(slot) };
            if entry >> 31 == tag {
                return (entry & INDEX_MASK) as usize;
            }
            if entry == 0 {
                return self.insert(local, slot);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// The index of the person with this local key, if it has events.
    #[inline(always)]
    fn find(&self, local: u32) -> Option<usize> {
        let tag = u64::from(local) + 1;
        let mask = self.table.len() - 1;
        let mut slot = (local.wrapping_mul(HASH) >> self.shift) as usize;
        loop {
            // SAFETY: `slot` is below the table's power-of-two length.
            let entry = unsafe { *self.table.get_unchecked(slot) };
            if entry >> 31 == tag {
                return Some((entry & INDEX_MASK) as usize);
            }
            if entry == 0 {
                return None;
            }
            slot = (slot + 1) & mask;
        }
    }

    #[inline(never)]
    fn insert(&mut self, local: u32, slot: usize) -> usize {
        let index = self.locals.len();
        self.table[slot] = ((u64::from(local) + 1) << 31) | index as u64;
        self.locals.push(local);
        self.ends.push([0; 3]);
        if (index + 1) * 2 > self.table.len() {
            self.grow();
        }
        index
    }

    #[cold]
    fn grow(&mut self) {
        let capacity = self.table.len() * 2;
        self.table.clear();
        self.table.resize(capacity, 0);
        self.shift = 32 - capacity.trailing_zeros();
        let mask = capacity - 1;
        for (index, &local) in self.locals.iter().enumerate() {
            let mut slot = (local.wrapping_mul(HASH) >> self.shift) as usize;
            while self.table[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            self.table[slot] = ((u64::from(local) + 1) << 31) | index as u64;
        }
    }

    /// Reads the partition's event lists: finds every person, counts its
    /// events and purchases, and stages both with the person's index.
    fn read_events(&mut self, lists: &[PartList]) {
        self.staged.clear();
        self.staged_purchases.clear();
        for list in lists {
            for block in list.blocks() {
                // At most one event per word.
                self.staged.reserve(block.len());
                let staged = self.staged.as_mut_ptr();
                let mut count = self.staged.len();
                let mut at = 0;
                while at < block.len() {
                    let key = block[at];
                    at += 1;
                    let index = self.person((key >> 32) as u32);
                    let head = ((index as u64) << 32) | ((key >> 1) & TS_MASK);
                    // SAFETY: `index` was just returned for an entry of `ends`;
                    // one word is staged per event and `block.len()` are reserved.
                    unsafe {
                        self.ends.get_unchecked_mut(index)[0] += 1;
                        *staged.add(count) = head;
                    }
                    count += 1;
                    if key & 1 == 1 {
                        // A block holds whole records: the purchase's second word follows.
                        self.ends[index][1] += 1;
                        self.staged_purchases.push([head, block[at]]);
                        at += 1;
                    }
                }
                // SAFETY: every word below `count` was written above.
                unsafe { self.staged.set_len(count) };
            }
        }
    }

    /// Reads the partition's click lists, keeping the clicks of persons that have events.
    fn read_clicks(&mut self, lists: &[PartList]) {
        self.staged_clicks.clear();
        for list in lists {
            for block in list.blocks() {
                // A block holds whole records: two words per click.
                for click in block.chunks_exact(2) {
                    if let Some(index) = self.find((click[0] >> 32) as u32) {
                        self.ends[index][2] += 1;
                        self.staged_clicks.push([((index as u64) << 32) | ((click[0] >> 1) & TS_MASK), click[1]]);
                    }
                }
            }
        }
    }

    /// Moves everything staged into per-person slices. Afterwards person
    /// `index` owns `ends[index - 1][kind]..ends[index][kind]` of each array.
    fn scatter(&mut self) {
        let mut totals = [0u32; 3];
        for end in &mut self.ends {
            for kind in 0..3 {
                let count = end[kind];
                end[kind] = totals[kind];
                totals[kind] += count;
            }
        }
        // Every slot below the new lengths is written by the loops that
        // follow: the counts add up to the staged rows.
        self.events.clear();
        self.events.resize(self.staged.len(), 0);
        self.purchases.clear();
        self.purchases.resize(self.staged_purchases.len(), [0; 2]);
        self.clicks.clear();
        self.clicks.resize(self.staged_clicks.len(), [0; 2]);
        for &word in &self.staged {
            let cursor = &mut self.ends[(word >> 32) as usize][0];
            self.events[*cursor as usize] = word as u32;
            *cursor += 1;
        }
        for row in &self.staged_purchases {
            let cursor = &mut self.ends[(row[0] >> 32) as usize][1];
            self.purchases[*cursor as usize] = [row[0] & TS_MASK, row[1]];
            *cursor += 1;
        }
        for row in &self.staged_clicks {
            let cursor = &mut self.ends[(row[0] >> 32) as usize][2];
            self.clicks[*cursor as usize] = [row[0] & TS_MASK, row[1]];
            *cursor += 1;
        }
    }
}

/// Scratch for one person at a time.
struct Work {
    /// Bucket summaries `[min, max, count, unused]`; all `EMPTY_CELL` between persons.
    cells: Vec<[u32; 4]>,
    /// One bit per bucket with events; all zero between persons.
    occupied: [u64; BUCKET_WORDS],
    /// Sort path: the index of each session's first event, and one past the last.
    first: Vec<u32>,
    /// Sessions of the person in start order: start and event count.
    start: Vec<u32>,
    count: Vec<u32>,
    /// Purchase cents and purchase count per session.
    revenue: Vec<i64>,
    purchases: Vec<u32>,
    /// Row (day) number per session.
    row: Vec<u8>,
    /// Per row: its first session (one extra entry closes the last row) and its day index.
    row_first: [u32; MAX_ROWS + 1],
    row_day: [u32; MAX_ROWS],
    /// Purchase cents per `(row, category)`; all zero between persons.
    category_sums: Vec<i64>,
    /// Per session: `(click ts + 1) << 32 | click index` of the best click
    /// among those whose reach begins at this session; 0 for none.
    best: Vec<u64>,
    /// Credited campaign per session.
    credited: Vec<u32>,
    /// Per bucket: `starts before the bucket << 32 | the start inside it`
    /// (`u32::MAX` when it has none).
    lookup: Vec<u64>,
    /// Campaign -> 1 + its slot for the current person; all zero between persons.
    slot_of: Vec<u32>,
    slot_campaign: Vec<u32>,
    /// Per slot, one bit per session the campaign assists.
    masks: Vec<u64>,
}

impl Work {
    fn new(params: &Params, tuning: Tuning) -> Self {
        let sessions = BUCKETS.max(tuning.sort_max) + 1;
        Self {
            cells: vec![EMPTY_CELL; BUCKETS],
            occupied: [0; BUCKET_WORDS],
            first: vec![0; tuning.sort_max + 2],
            start: vec![0; sessions],
            count: vec![0; sessions],
            revenue: vec![0; sessions],
            purchases: vec![0; sessions],
            row: vec![0; sessions],
            row_first: [0; MAX_ROWS + 1],
            row_day: [0; MAX_ROWS],
            category_sums: vec![0; MAX_ROWS * params.categories],
            best: vec![0; sessions],
            credited: vec![0; sessions],
            lookup: vec![0; BUCKETS + 1],
            slot_of: vec![0; params.campaigns],
            slot_campaign: Vec::new(),
            masks: Vec::new(),
        }
    }
}

pub struct ScratchC {
    params: Params,
    tuning: Tuning,
    group: Group,
    work: Work,
}

impl ScratchC {
    pub fn new(ctx: &Ctx) -> Self {
        let params = Params::new(
            ctx.window_start,
            ctx.first_day,
            ctx.day_count,
            ctx.campaign_ids.len(),
            ctx.organic as usize,
            &ctx.category_names,
        );
        Self::with(params, Tuning::DEFAULT)
    }

    fn with(params: Params, tuning: Tuning) -> Self {
        let work = Work::new(&params, tuning);
        Self { params, tuning, group: Group::default(), work }
    }
}

/// How many of the ascending `starts` are `<= limit`.
#[inline(always)]
fn starts_up_to(starts: &[u32], limit: u32) -> usize {
    if starts.len() <= 16 {
        starts.iter().filter(|&&start| start <= limit).count()
    } else {
        starts.partition_point(|&start| start <= limit)
    }
}

/// How many of the ascending `starts` are `< limit`.
#[inline(always)]
fn starts_before(starts: &[u32], limit: u32) -> usize {
    if starts.len() <= 16 {
        starts.iter().filter(|&&start| start < limit).count()
    } else {
        starts.partition_point(|&start| start < limit)
    }
}

/// Sessions of one person by sorting its timestamps. Returns how many.
fn sessions_by_sort(events: &mut [u32], work: &mut Work) -> usize {
    events.sort_unstable();
    let first = &mut work.first[..events.len() + 1];
    let mut sessions = 0;
    first[0] = 0;
    for at in 1..events.len() {
        // Written before the gap is looked at; it stays only if a session starts here.
        first[sessions + 1] = at as u32;
        sessions += usize::from(events[at] - events[at - 1] > GAP);
    }
    sessions += 1;
    first[sessions] = events.len() as u32;
    for session in 0..sessions {
        work.start[session] = events[first[session] as usize];
        work.count[session] = first[session + 1] - first[session];
    }
    sessions
}

/// Sessions of one person from half-hour bucket summaries. Returns how many.
fn sessions_by_buckets(events: &[u32], work: &mut Work) -> usize {
    let (mut low, mut high) = (u32::MAX, 0u32);
    for &ts in events {
        let bucket = (ts / GAP) as usize;
        // SAFETY: `ts` is a 31-bit value, so `bucket < BUCKETS`.
        unsafe {
            let cell = work.cells.get_unchecked_mut(bucket);
            cell[0] = cell[0].min(ts);
            cell[1] = cell[1].max(ts);
            cell[2] += 1;
            *work.occupied.get_unchecked_mut(bucket >> 6) |= 1 << (bucket & 63);
        }
        low = low.min(ts);
        high = high.max(ts);
    }
    let mut sessions = 0;
    let mut previous_max = 0;
    let mut count = 0;
    for word in (low / GAP) as usize >> 6..=(high / GAP) as usize >> 6 {
        let mut bits = std::mem::take(&mut work.occupied[word]);
        while bits != 0 {
            let bucket = word * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let [min, max, cell_count, _] = std::mem::replace(&mut work.cells[bucket], EMPTY_CELL);
            // Buckets come in time order, so `min >= previous_max`.
            if sessions == 0 || min - previous_max > GAP {
                if sessions != 0 {
                    work.count[sessions - 1] = count;
                }
                work.start[sessions] = min;
                sessions += 1;
                count = 0;
            }
            count += cell_count;
            previous_max = max;
        }
    }
    work.count[sessions - 1] = count;
    sessions
}

/// Sets bits `from..to` (`from < to`) of a bit set of 64-bit words.
#[inline(always)]
fn set_bits(words: &mut [u64], from: usize, to: usize) {
    let (first, last) = (from >> 6, (to - 1) >> 6);
    let head = u64::MAX << (from & 63);
    let tail = u64::MAX >> (63 - ((to - 1) & 63));
    if first == last {
        words[first] |= head & tail;
    } else {
        words[first] |= head;
        for word in &mut words[first + 1..last] {
            *word = u64::MAX;
        }
        words[last] |= tail;
    }
}

/// The category with the highest sum; a tie goes to the byte-wise smallest
/// name. Clears the sums.
fn top_category(rank: &[u32], sums: &mut [i64]) -> u32 {
    let mut best = 0;
    for candidate in 1..sums.len() {
        let better =
            sums[candidate] > sums[best] || (sums[candidate] == sums[best] && rank[candidate] < rank[best]);
        if better {
            best = candidate;
        }
    }
    sums.fill(0);
    best as u32
}

/// Credit and assists of one person: fills `work.credited` and adds the
/// assists to `out`. `clicks` is not empty.
fn attribute(
    params: &Params,
    tuning: Tuning,
    work: &mut Work,
    out: &mut PassCOut,
    click_exotic: &[Box<[u8]>],
    sessions: usize,
    clicks: &[[u64; 2]],
) {
    let words = sessions.div_ceil(64);
    let use_lookup = sessions > 64 || sessions * clicks.len() >= tuning.lookup_min;
    let mut limit = 0;
    if use_lookup {
        // Starts are more than a bucket apart, so a bucket holds at most one.
        for session in 0..sessions {
            let start = work.start[session];
            let own = (start / GAP) as usize;
            while limit < own {
                work.lookup[limit] = (session as u64) << 32 | u64::from(u32::MAX);
                limit += 1;
            }
            work.lookup[own] = (session as u64) << 32 | u64::from(start);
            limit = own + 1;
        }
        // One entry for every later time.
        work.lookup[limit] = (sessions as u64) << 32 | u64::from(u32::MAX);
    }
    work.best[..sessions].fill(0);
    let starts = &work.start[..sessions];
    let mut slots = 0;
    for (at, click) in clicks.iter().enumerate() {
        // Both in ms: `ts` since `window_start - 7 days`, starts since
        // `window_start`. The click reaches the sessions with
        // `ts - 7 days <= start <= ts`: sessions `from..to`.
        let ts = click[0] as u32;
        let floor = ts.saturating_sub(WEEK);
        let (from, to) = if use_lookup {
            let low = work.lookup[((floor / GAP) as usize).min(limit)];
            let high = work.lookup[((ts / GAP) as usize).min(limit)];
            (
                (low >> 32) as usize + usize::from((low as u32) < floor),
                (high >> 32) as usize + usize::from(high as u32 <= ts),
            )
        } else {
            (starts_before(starts, floor), starts_up_to(starts, ts))
        };
        if from >= to {
            continue;
        }
        // Credit: session `from` is the first one this click can credit, and
        // of the clicks that begin there the latest wins; in one millisecond,
        // the byte-wise smallest click ID.
        let candidate = (u64::from(ts) + 1) << 32 | at as u64;
        let current = work.best[from];
        if current >> 32 == candidate >> 32 {
            let other = clicks[(current & 0xFFFF_FFFF) as usize][1];
            if id_cmp(click[1] & CODE_MASK, other & CODE_MASK, b'k', click_exotic) == Ordering::Less {
                work.best[from] = candidate;
            }
        } else if candidate > current {
            work.best[from] = candidate;
        }
        // Assists: the campaign covers sessions `from..to`.
        let campaign = (click[1] >> 42) as usize;
        let mut slot = work.slot_of[campaign] as usize;
        if slot == 0 {
            slots += 1;
            slot = slots;
            work.slot_of[campaign] = slot as u32;
            work.slot_campaign.push(campaign as u32);
            work.masks.resize(slots * words, 0);
        }
        set_bits(&mut work.masks[(slot - 1) * words..slot * words], from, to);
    }

    // The click that credits a session is the latest one at or before its
    // start: the best of this session's own, else the one carried over. It
    // counts while `start - 7 days <= ts`, which in these units is `start <= ts`.
    let mut carried = 0u64;
    for session in 0..sessions {
        if work.best[session] != 0 {
            carried = work.best[session];
        }
        work.credited[session] = if (carried >> 32) as u32 > starts[session] {
            (clicks[(carried & 0xFFFF_FFFF) as usize][1] >> 42) as u32
        } else {
            params.organic as u32
        };
    }

    // Assists per campaign and day: the covered sessions of each row.
    for slot in 0..slots {
        let campaign = work.slot_campaign[slot] as usize;
        work.slot_of[campaign] = 0;
        let days = &mut out.campaign_days[campaign * params.day_count..(campaign + 1) * params.day_count];
        for word in 0..words {
            let mut bits = work.masks[slot * words + word];
            let base = word * 64;
            while bits != 0 {
                let row = work.row[base + bits.trailing_zeros() as usize] as usize;
                // The part of this row's sessions that lies in this word.
                let from = (work.row_first[row] as usize).max(base) - base;
                let to = (work.row_first[row + 1] as usize).min(base + 64) - base;
                let piece = (u64::MAX >> (64 - (to - from))) << from;
                days[work.row_day[row] as usize][3] += i64::from((bits & piece).count_ones());
                bits &= !piece;
            }
        }
    }
    work.slot_campaign.clear();
    work.masks.clear();
}

/// Everything for one person: its rows and its share of the campaign cells.
#[allow(clippy::too_many_arguments)]
fn person(
    params: &Params,
    tuning: Tuning,
    work: &mut Work,
    out: &mut PassCOut,
    click_exotic: &[Box<[u8]>],
    name: u64,
    events: &mut [u32],
    purchases: &[[u64; 2]],
    clicks: &[[u64; 2]],
) {
    let sessions = if events.len() <= tuning.sort_max {
        sessions_by_sort(events, work)
    } else {
        sessions_by_buckets(events, work)
    };

    // One row per day with a session start.
    let mut rows = 0;
    let mut previous_day = u32::MAX;
    for session in 0..sessions {
        let day = (work.start[session] + params.day_offset) / DAY;
        if day != previous_day {
            work.row_first[rows] = session as u32;
            work.row_day[rows] = day;
            rows += 1;
            previous_day = day;
        }
        work.row[session] = (rows - 1) as u8;
    }
    work.row_first[rows] = sessions as u32;

    let has_purchases = !purchases.is_empty();
    if has_purchases {
        work.revenue[..sessions].fill(0);
        work.purchases[..sessions].fill(0);
        for purchase in purchases {
            // The session that holds the purchase: the last one that starts at or before it.
            let session = starts_up_to(&work.start[..sessions], purchase[0] as u32).saturating_sub(1);
            let price = (purchase[1] & 0xFFFF_FFFF) as i64;
            work.revenue[session] += price;
            work.purchases[session] += 1;
            work.category_sums[work.row[session] as usize * params.categories + (purchase[1] >> 32) as usize] += price;
        }
    }

    let has_clicks = !clicks.is_empty();
    if has_clicks {
        attribute(params, tuning, work, out, click_exotic, sessions, clicks);
    }

    for row in 0..rows {
        let (from, to) = (work.row_first[row] as usize, work.row_first[row + 1] as usize);
        let day = work.row_day[row] as usize;
        let mut event_count = 0i64;
        for session in from..to {
            event_count += i64::from(work.count[session]);
        }
        let (mut revenue, mut bought, mut buying_sessions) = (0i64, 0u32, 0i64);
        if has_purchases {
            for session in from..to {
                revenue += work.revenue[session];
                bought += work.purchases[session];
                buying_sessions += i64::from(work.purchases[session] > 0);
            }
        }
        let top = if bought == 0 {
            NO_CATEGORY
        } else {
            top_category(
                &params.category_rank,
                &mut work.category_sums[row * params.categories..(row + 1) * params.categories],
            )
        };
        out.names.push(name);
        out.days.push((params.first_day + day as i64) as i32);
        out.sessions.push((to - from) as i64);
        out.events.push(event_count);
        out.revenue.push(revenue);
        out.top_category.push(top);
        if has_clicks {
            for session in from..to {
                let cell = &mut out.campaign_days[work.credited[session] as usize * params.day_count + day];
                cell[0] += 1;
                if has_purchases {
                    cell[1] += i64::from(work.purchases[session] > 0);
                    cell[2] += work.revenue[session];
                }
            }
        } else {
            let cell = &mut out.campaign_days[params.organic * params.day_count + day];
            cell[0] += (to - from) as i64;
            cell[1] += buying_sessions;
            cell[2] += revenue;
        }
    }
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
    let _ = ctx;
    partition_core(part, event_lists, click_lists, click_exotic, out, scratch);
}

/// `sessions_partition` without the `Ctx`: everything it needs is in the scratch.
fn partition_core(
    part: usize,
    event_lists: &[PartList],
    click_lists: &[PartList],
    click_exotic: &[Box<[u8]>],
    out: &mut PassCOut,
    scratch: &mut ScratchC,
) {
    let ScratchC { params, tuning, group, work } = scratch;
    group.begin();
    group.read_events(event_lists);
    if group.locals.is_empty() {
        return;
    }
    group.read_clicks(click_lists);
    // Person indexes and slice ends are kept in 31 and 32 bits.
    assert!(group.staged.len() <= INDEX_MASK as usize && group.staged_clicks.len() <= u32::MAX as usize);
    group.scatter();

    let mut starts = [0usize; 3];
    for index in 0..group.locals.len() {
        let ends = group.ends[index].map(|end| end as usize);
        let name = unmix(((part as u64) << LOCAL_BITS) | u64::from(group.locals[index]));
        person(
            params,
            *tuning,
            work,
            out,
            click_exotic,
            name,
            &mut group.events[starts[0]..ends[0]],
            &group.purchases[starts[1]..ends[1]],
            &group.clicks[starts[2]..ends[2]],
        );
        starts = ends;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::fast::arena::{Arena, PartWriter};
    use crate::fast::codes::{id_bytes, pattern_code, EXOTIC_BASE};

    /// SplitMix64: small, seedable, good enough to shuffle test data.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        /// Both ends included.
        fn range(&mut self, low: u64, high: u64) -> u64 {
            low + self.below(high - low + 1)
        }

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[self.below(items.len() as u64) as usize]
        }

        fn one_in(&mut self, count: u64) -> bool {
            self.below(count) == 0
        }

        fn shuffle<T>(&mut self, items: &mut [T]) {
            for at in (1..items.len()).rev() {
                items.swap(at, self.below(at as u64 + 1) as usize);
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct Event {
        local: u32,
        /// Milliseconds since `window_start`.
        ts: u32,
        /// `(price, category)`.
        purchase: Option<(u32, u32)>,
    }

    #[derive(Clone, Copy, Debug)]
    struct Click {
        local: u32,
        /// Milliseconds since `window_start - 7 days`.
        ts: u32,
        code: u64,
        campaign: u32,
    }

    /// What `Ctx` holds for a run.
    struct World {
        window_start: i64,
        /// `window_end - window_start`.
        length: u32,
        first_day: i64,
        day_count: usize,
        category_names: Vec<Box<[u8]>>,
        /// Campaigns including `organic`, the last one.
        campaigns: usize,
        exotic: Vec<Box<[u8]>>,
    }

    impl World {
        fn new(window_start: i64, length: u32, categories: &[&str], campaigns: usize, exotic: &[&str]) -> Self {
            let first_day = window_start.div_euclid(DAY_MS);
            let boxed = |names: &[&str]| names.iter().map(|name| name.as_bytes().into()).collect();
            World {
                window_start,
                length,
                first_day,
                day_count: ((window_start + i64::from(length) - 1).div_euclid(DAY_MS) - first_day + 1) as usize,
                category_names: boxed(categories),
                campaigns,
                exotic: boxed(exotic),
            }
        }
    }

    struct Partition {
        part: usize,
        events: Vec<Event>,
        clicks: Vec<Click>,
    }

    /// Both output tables in a form that does not depend on row order.
    #[derive(Debug, PartialEq)]
    struct Tables {
        /// `(name, day) -> (sessions, events, revenue, top category)`.
        rows: BTreeMap<(u64, i32), (i64, i64, i64, u32)>,
        campaign_days: Vec<[i64; 4]>,
    }

    /// rules.md read literally for one partition: sort each person's events,
    /// cut sessions at gaps over 30 minutes, and try every click of the
    /// person against every session.
    fn reference(world: &World, partition: &Partition, tables: &mut Tables) {
        let organic = world.campaigns - 1;
        let click_id = |code: u64| {
            let mut bytes = Vec::new();
            id_bytes(code, b'k', &world.exotic, &mut bytes);
            bytes
        };
        let mut people: BTreeMap<u32, Vec<Event>> = BTreeMap::new();
        for event in &partition.events {
            people.entry(event.local).or_default().push(*event);
        }
        for (&local, events) in &mut people {
            events.sort_by_key(|event| event.ts);
            let name = unmix(((partition.part as u64) << LOCAL_BITS) | u64::from(local));
            let clicks: Vec<&Click> = partition.clicks.iter().filter(|click| click.local == local).collect();
            let mut category_sums: BTreeMap<(i32, u32), i64> = BTreeMap::new();
            let mut at = 0;
            while at < events.len() {
                let start = i64::from(events[at].ts);
                let day = (world.window_start + start).div_euclid(DAY_MS);
                let day_index = (day - world.first_day) as usize;
                let (mut last, mut count, mut revenue, mut bought) = (start, 0i64, 0i64, 0);
                while at < events.len() && i64::from(events[at].ts) - last <= SESSION_GAP_MS {
                    last = i64::from(events[at].ts);
                    count += 1;
                    if let Some((price, category)) = events[at].purchase {
                        revenue += i64::from(price);
                        bought += 1;
                        *category_sums.entry((day as i32, category)).or_default() += i64::from(price);
                    }
                    at += 1;
                }
                let row = tables.rows.entry((name, day as i32)).or_insert((0, 0, 0, NO_CATEGORY));
                row.0 += 1;
                row.1 += count;
                row.2 += revenue;

                let mut credited: Option<&Click> = None;
                let mut assisting = BTreeSet::new();
                for click in &clicks {
                    let click_time = i64::from(click.ts) - WEEK_MS;
                    if click_time <= start && start - click_time <= WEEK_MS {
                        assisting.insert(click.campaign);
                        let better = match credited {
                            None => true,
                            Some(best) => {
                                click.ts > best.ts || (click.ts == best.ts && click_id(click.code) < click_id(best.code))
                            }
                        };
                        if better {
                            credited = Some(click);
                        }
                    }
                }
                let campaign = credited.map_or(organic, |click| click.campaign as usize);
                let cell = &mut tables.campaign_days[campaign * world.day_count + day_index];
                cell[0] += 1;
                cell[1] += i64::from(bought > 0);
                cell[2] += revenue;
                for campaign in assisting {
                    tables.campaign_days[campaign as usize * world.day_count + day_index][3] += 1;
                }
            }
            // Highest sum of the day; a tie goes to the byte-wise smallest name.
            let mut tops: BTreeMap<i32, (i64, u32)> = BTreeMap::new();
            for (&(day, category), &sum) in &category_sums {
                let top = tops.entry(day).or_insert((sum, category));
                let names = &world.category_names;
                if sum > top.0 || (sum == top.0 && names[category as usize] < names[top.1 as usize]) {
                    *top = (sum, category);
                }
            }
            for (day, (_, category)) in tops {
                tables.rows.get_mut(&(name, day)).unwrap().3 = category;
            }
        }
    }

    /// Runs pass C over the partitions with one scratch and one output, as a
    /// worker does, with the rows cut into lists and blocks at random.
    fn run(world: &World, tuning: Tuning, partitions: &[Partition], rng: &mut Rng) -> Tables {
        let params = Params::new(
            world.window_start,
            world.first_day,
            world.day_count,
            world.campaigns,
            world.campaigns - 1,
            &world.category_names,
        );
        let mut scratch = ScratchC::with(params, tuning);
        let mut out = PassCOut {
            names: Vec::new(),
            days: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top_category: Vec::new(),
            campaign_days: vec![[0; 4]; world.campaigns * world.day_count],
        };
        for partition in partitions {
            let workers = rng.range(1, 4) as usize;
            // Two header words per block, and room for at least one record.
            let event_block = rng.pick(&[4, 5, 7, 512]);
            let click_block = rng.pick(&[4, 5, 6, 256]);
            let mut arenas: Vec<Arena> = (0..workers).map(|_| Arena::new()).collect();
            let mut event_writers = vec![PartWriter::EMPTY; workers];
            let mut click_writers = vec![PartWriter::EMPTY; workers];
            for event in &partition.events {
                let worker = rng.below(workers as u64) as usize;
                let key =
                    (u64::from(event.local) << 32) | (u64::from(event.ts) << 1) | u64::from(event.purchase.is_some());
                let words = 1 + usize::from(event.purchase.is_some());
                let at = event_writers[worker].reserve(&mut arenas[worker], event_block, words);
                // SAFETY: `reserve` returned room for `words` words.
                unsafe {
                    *at = key;
                    if let Some((price, category)) = event.purchase {
                        *at.add(1) = u64::from(price) | (u64::from(category) << 32);
                    }
                }
            }
            for click in &partition.clicks {
                let worker = rng.below(workers as u64) as usize;
                let key = (u64::from(click.local) << 32) | (u64::from(click.ts) << 1);
                let at = click_writers[worker].reserve(&mut arenas[worker], click_block, 2);
                // SAFETY: `reserve` returned room for two words.
                unsafe {
                    *at = key;
                    *at.add(1) = click.code | (u64::from(click.campaign) << 42);
                }
            }
            let event_lists: Vec<PartList> = event_writers.iter_mut().map(PartWriter::finish).collect();
            let click_lists: Vec<PartList> = click_writers.iter_mut().map(PartWriter::finish).collect();
            partition_core(partition.part, &event_lists, &click_lists, &world.exotic, &mut out, &mut scratch);
        }
        let count = out.names.len();
        for length in [out.days.len(), out.sessions.len(), out.events.len(), out.revenue.len(), out.top_category.len()]
        {
            assert_eq!(length, count);
        }
        let mut tables = Tables { rows: BTreeMap::new(), campaign_days: out.campaign_days.clone() };
        for at in 0..count {
            let key = (out.names[at], out.days[at]);
            let row = (out.sessions[at], out.events[at], out.revenue[at], out.top_category[at]);
            assert!(tables.rows.insert(key, row).is_none(), "two rows for one person and day");
        }
        tables
    }

    fn expected(world: &World, partitions: &[Partition]) -> Tables {
        let mut tables =
            Tables { rows: BTreeMap::new(), campaign_days: vec![[0; 4]; world.campaigns * world.day_count] };
        for partition in partitions {
            reference(world, partition, &mut tables);
        }
        tables
    }

    /// A distinct generator-style click ID per `serial`, as its code.
    fn click_code(serial: u64) -> u64 {
        const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        // A bijection on 0..36^5, so short and long IDs mix.
        let mut value = serial.wrapping_mul(2_654_435_761) % 60_466_176;
        let mut id = vec![b'k'];
        loop {
            id.push(DIGITS[(value % 36) as usize]);
            value /= 36;
            if value == 0 {
                break;
            }
        }
        pattern_code(&id, b'k').unwrap()
    }

    /// Session starts of a person's timestamps, the plain way.
    fn starts_of(mut ts: Vec<u32>) -> Vec<u32> {
        ts.sort_unstable();
        let mut starts = vec![ts[0]];
        for pair in ts.windows(2) {
            if pair[1] - pair[0] > GAP {
                starts.push(pair[1]);
            }
        }
        starts
    }

    /// One person's timestamps: walks forward in steps chosen to land on
    /// every kind of edge, then shuffles.
    fn person_times(world: &World, rng: &mut Rng) -> Vec<u32> {
        let length = u64::from(world.length);
        let day_offset = (world.window_start - world.first_day * DAY_MS) as u64;
        let (gap, day) = (u64::from(GAP), u64::from(DAY));
        let style = rng.below(12);
        let budget = match style {
            0..=4 => rng.range(1, 6),
            5..=7 => rng.range(5, 60),
            8 | 9 => rng.range(40, 400),
            10 => rng.range(300, 3000),
            _ => rng.range(600, 1500),
        } as usize;
        let mut ts = match rng.below(4) {
            0 => 0,
            1 => rng.below(length),
            2 => (gap - 1).min(length - 1),
            _ => rng.below(gap.min(length)),
        };
        let mut times = Vec::new();
        while times.len() < budget {
            times.push(ts as u32);
            let step = if style == 11 {
                // As many sessions as a window can hold.
                rng.pick(&[gap + 1, gap + 1, gap + 1, gap + 2, 0])
            } else {
                match rng.below(16) {
                    0 | 1 => 0,
                    2 => 1,
                    3 => gap - 1,
                    4 => gap,
                    5 => gap + 1,
                    6 => gap + 2,
                    7 => 2 * gap,
                    8 => 2 * gap + 1,
                    // The next bucket edge and the millisecond before it.
                    9 => (ts / gap + 1) * gap - ts,
                    10 => (ts / gap + 1) * gap - 1 - ts,
                    // The next UTC midnight and the millisecond before it.
                    11 => day - (ts + day_offset) % day,
                    12 => day - (ts + day_offset) % day - 1,
                    13 => rng.below(120_000),
                    14 => rng.below(3 * gap),
                    _ => rng.below(3 * day),
                }
            };
            if ts + step >= length {
                if rng.one_in(3) {
                    times.push((length - 1) as u32);
                }
                break;
            }
            ts += step;
        }
        rng.shuffle(&mut times);
        times
    }

    fn random_partition(world: &World, part: usize, serial: &mut u64, rng: &mut Rng) -> Partition {
        let mut partition = Partition { part, events: Vec::new(), clicks: Vec::new() };
        let persons = match rng.below(10) {
            0 => rng.range(150, 700),
            1 => 1,
            _ => rng.range(2, 40),
        };
        let categories = world.category_names.len() as u64;
        let real_campaigns = world.campaigns as u64 - 1;
        let reach = i64::from(world.length) + WEEK_MS;
        let mut locals = BTreeSet::new();
        let mut exotic_left: Vec<u64> = (0..world.exotic.len() as u64).collect();
        for _ in 0..persons {
            let local = match rng.below(40) {
                0 => 0,
                1 => u32::MAX,
                _ => rng.next() as u32,
            };
            if !locals.insert(local) {
                continue;
            }
            // In a partition of many persons most are small.
            let times = if persons > 100 && !rng.one_in(50) {
                let mut times = person_times(world, rng);
                times.truncate(rng.range(1, 12) as usize);
                times
            } else {
                person_times(world, rng)
            };
            let buys = rng.pick(&[0, 0, 0, 6, 6, 2, 1]);
            for &ts in &times {
                let purchase = (buys != 0 && rng.one_in(buys))
                    .then(|| (rng.pick(&[1, 1, 2, 3, 5, 100, 250, 10_000_000]), rng.below(categories) as u32));
                partition.events.push(Event { local, ts, purchase });
            }
            let starts = starts_of(times);
            let count = match rng.below(8) {
                0 | 1 => 0,
                2..=4 => rng.range(1, 8),
                5 | 6 => rng.range(8, 80),
                _ => rng.range(80, 1200),
            };
            let pool = rng.pick(&[1, 3, real_campaigns]).min(real_campaigns);
            let mut previous = 0i64;
            for _ in 0..count {
                // The session start on the click clock, and a click around it.
                let start = i64::from(rng.pick(&starts)) + WEEK_MS;
                let ts = match rng.below(13) {
                    0 | 1 => start,
                    2 => start + 1,
                    3 => start - 1,
                    4 => start - WEEK_MS,
                    5 => start - WEEK_MS - 1,
                    6 => start - WEEK_MS + 1,
                    7 => start - rng.below(DAY_MS as u64) as i64,
                    8 => start - rng.below((WEEK_MS + SESSION_GAP_MS) as u64) as i64,
                    9 => previous,
                    10 => start / SESSION_GAP_MS * SESSION_GAP_MS,
                    _ => rng.below(reach as u64) as i64,
                }
                .clamp(0, reach - 1);
                previous = ts;
                let code = if !exotic_left.is_empty() && rng.one_in(6) {
                    EXOTIC_BASE + exotic_left.swap_remove(rng.below(exotic_left.len() as u64) as usize)
                } else {
                    *serial += 1;
                    click_code(*serial)
                };
                let click = Click { local, ts: ts as u32, code, campaign: rng.below(pool) as u32 };
                // Copies of a click have identical values.
                for _ in 0..rng.pick(&[1, 1, 1, 1, 1, 1, 2, 3]) {
                    partition.clicks.push(click);
                }
            }
        }
        // Clicks of persons without events are nobody's.
        for _ in 0..rng.below(4) {
            let local = rng.next() as u32;
            if !locals.contains(&local) {
                *serial += 1;
                let ts = rng.below(reach as u64) as u32;
                partition.clicks.push(Click { local, ts, code: click_code(*serial), campaign: 0 });
            }
        }
        rng.shuffle(&mut partition.events);
        rng.shuffle(&mut partition.clicks);
        partition
    }

    fn random_world(rng: &mut Rng) -> World {
        let day = DAY_MS;
        let length = rng.pick(&[14 * day, 14 * day, 17 * day, 3 * day + 1_025_000, 2 * day, 3_600_000, 1_860_000]);
        let offset = match rng.below(5) {
            0 | 1 => 0,
            2 => 26_000_000,
            3 => day - 1,
            _ => rng.below(day as u64) as i64,
        };
        let mut names = vec!["shoes", "books", "Toys", "zebra", "a", "B", "unknown"];
        rng.shuffle(&mut names);
        names.truncate(rng.range(1, 7) as usize);
        let exotic: &[&str] = if rng.one_in(2) { &["k", "K7", "k7Z", " k7", "k_", "kzzzzzzzzz"] } else { &[] };
        let window_start = rng.range(19_000, 21_500) as i64 * day + offset;
        World::new(window_start, length as u32, &names, rng.pick(&[2, 3, 5, 40, 400]), exotic)
    }

    #[test]
    fn matches_the_literal_rules_on_random_partitions() {
        let worlds = if cfg!(debug_assertions) { 40 } else { 400 };
        for seed in 0..worlds {
            let mut rng = Rng(seed);
            let world = random_world(&mut rng);
            let mut serial = 0;
            // A worker gets each partition once, in rising order.
            let first_part = rng.below(1000) as usize;
            let partitions: Vec<Partition> = (0..rng.range(1, 5) as usize)
                .map(|at| random_partition(&world, first_part + at, &mut serial, &mut rng))
                .collect();
            let want = expected(&world, &partitions);
            let tuning = Tuning {
                sort_max: rng.pick(&[0, 1, 2, 5, 16, 32, 100, 100_000]),
                lookup_min: rng.pick(&[0, 1, 64, 512, usize::MAX]),
            };
            let got = run(&world, tuning, &partitions, &mut rng);
            assert!(got == want, "seed {seed}: tables differ");
            // The default settings on the same data.
            let got = run(&world, Tuning::DEFAULT, &partitions, &mut rng);
            assert!(got == want, "seed {seed}: tables differ with the default tuning");
        }
    }

    /// One user, one partition, the official fixtures' window and three campaigns.
    fn tables_for(times: &[u32], clicks: &[(u32, u64, u32)]) -> (World, Tables) {
        let world = World::new(1_790_812_800_000, 14 * DAY, &["shoes", "books"], 3, &[]);
        let events = times.iter().map(|&ts| Event { local: 7, ts, purchase: None }).collect();
        let clicks = clicks.iter().map(|&(ts, code, campaign)| Click { local: 7, ts, code, campaign }).collect();
        let partitions = [Partition { part: 3, events, clicks }];
        let got = run(&world, Tuning::DEFAULT, &partitions, &mut Rng(1));
        assert!(got == expected(&world, &partitions));
        (world, got)
    }

    #[test]
    fn gaps_at_bucket_edges() {
        let sessions = |times: &[u32]| tables_for(times, &[]).1.rows.values().map(|row| row.0).sum::<i64>();
        // Exactly 30 minutes joins, one millisecond more splits, wherever the bucket edges fall.
        for base in [0, 1, GAP - 1, GAP, GAP + 1, 5 * GAP - 1, 5 * GAP] {
            assert_eq!(sessions(&[base, base + GAP - 1]), 1);
            assert_eq!(sessions(&[base, base + GAP]), 1);
            assert_eq!(sessions(&[base, base + GAP + 1]), 2);
            assert_eq!(sessions(&[base + GAP, base, base + 2 * GAP, base + 3 * GAP + 1, base + 3 * GAP + 1]), 2);
        }
        // A session over midnight is one row, on its start day.
        let (_, tables) = tables_for(&[DAY - 1, DAY, DAY + GAP, DAY + 2 * GAP + 1], &[]);
        let rows: Vec<_> = tables.rows.values().copied().collect();
        assert_eq!(rows, [(1, 3, 0, NO_CATEGORY), (1, 1, 0, NO_CATEGORY)]);
    }

    #[test]
    fn click_reach_and_ties() {
        let start = 3 * DAY + 5 * GAP;
        // On the click clock the session starts at `start + WEEK`. Returns the
        // credited campaign and every campaign's assists.
        let credited = |clicks: &[(u32, u64, u32)]| {
            let (world, tables) = tables_for(&[start, start + 1], clicks);
            let cell = |campaign: usize| tables.campaign_days[campaign * world.day_count + 3];
            let campaign = (0..world.campaigns).find(|&campaign| cell(campaign)[0] == 1).unwrap();
            let assists: Vec<i64> = (0..world.campaigns).map(|campaign| cell(campaign)[3]).collect();
            (campaign, assists)
        };
        let (a, b) = (click_code(1), click_code(2));
        assert_eq!(credited(&[]), (2, vec![0, 0, 0]));
        assert_eq!(credited(&[(start + WEEK, a, 0)]), (0, vec![1, 0, 0]));
        assert_eq!(credited(&[(start + WEEK + 1, a, 0)]), (2, vec![0, 0, 0]));
        assert_eq!(credited(&[(start, a, 1)]), (1, vec![0, 1, 0]));
        assert_eq!(credited(&[(start - 1, a, 1)]), (2, vec![0, 0, 0]));
        // The later click wins; both campaigns assist, each once.
        let clicks = [(start + 5, a, 0), (start + 6, b, 1), (start + 5, a, 0), (start + 7, click_code(3), 1)];
        assert_eq!(credited(&clicks), (1, vec![1, 1, 0]));
        // In one millisecond the smaller click ID wins, whichever comes first.
        let (small, large) = if a < b { (a, b) } else { (b, a) };
        assert_eq!(credited(&[(start + 5, small, 0), (start + 5, large, 1)]), (0, vec![1, 1, 0]));
        assert_eq!(credited(&[(start + 5, large, 1), (start + 5, small, 0)]), (0, vec![1, 1, 0]));
    }
}

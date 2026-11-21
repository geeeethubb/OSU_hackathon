//! Parsing of event, click and link lines into compact records.
//!
//! This is the exact, general parser (it follows rules.md field by field).
//! Faster paths must produce the same records for the lines they accept.

use super::arena::{Arena, PartWriter};
use super::codes::{id_code, mix};
use super::{Ctx, EVENT_BLOCK, LOCAL_BITS, LOCAL_MASK, PARTS};
use crate::util::{parse_price, parse_ts, split_fields, LATE_MS, WEEK_MS};

/// A valid click before its user is mapped to a person.
#[derive(Clone, Copy)]
pub struct ClickRecord {
    pub user: u64,
    /// Click ID code in the low 42 bits, campaign index above.
    pub code_and_campaign: u64,
    /// Milliseconds since `window_start - 7 days`.
    pub ts: u32,
}

/// Everything one worker produces while parsing.
pub struct ParseOut {
    pub arena: Arena,
    /// One list per event-ID partition. Record layout:
    /// word 0 = local event key (32) | ts since window start (31) | valid (1);
    /// word 1 = user code (42) | ingest seconds (21) | is purchase (1);
    /// then, for purchases only, word 2 = price (32 low) | category (32 high).
    pub events: Vec<PartWriter>,
    pub clicks: Vec<ClickRecord>,
    pub links: Vec<(u64, u64)>,
}

impl ParseOut {
    pub fn new() -> Self {
        Self { arena: Arena::new(), events: vec![PartWriter::EMPTY; PARTS], clicks: Vec::new(), links: Vec::new() }
    }
}

impl Default for ParseOut {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
pub fn find_newline(haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&byte| byte == b'\n')
}

/// Calls `visit` for every data line that starts inside `[start, end)`.
/// The header (the file's first line) is never a data line; a line that
/// straddles `end` belongs to this chunk, and one straddling `start` to the
/// previous chunk.
#[inline]
pub fn lines_in_chunk<'a>(buffer: &'a [u8], start: usize, end: usize, mut visit: impl FnMut(&'a [u8])) {
    let from = if start == 0 { 0 } else { start - 1 };
    let mut position = match find_newline(&buffer[from..]) {
        Some(offset) => from + offset + 1,
        None => return,
    };
    let limit = end.min(buffer.len());
    while position < limit {
        let line_end = find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset);
        visit(&buffer[position..line_end]);
        position = line_end + 1;
    }
}

#[inline]
fn emit_event(
    out: &mut ParseOut,
    event_code: u64,
    ts: u64,
    valid: bool,
    user_code: u64,
    ingest: u64,
    purchase: Option<(u32, u32)>,
) {
    let mixed = mix(event_code);
    let part = (mixed >> LOCAL_BITS) as usize;
    let local = mixed & LOCAL_MASK;
    let word0 = (local << 32) | (ts << 1) | u64::from(valid);
    let word1 = (user_code << 22) | (ingest << 1) | u64::from(purchase.is_some());
    let writer = &mut out.events[part];
    writer.push(&mut out.arena, EVENT_BLOCK, word0);
    writer.push(&mut out.arena, EVENT_BLOCK, word1);
    if let Some((price, category)) = purchase {
        writer.push(&mut out.arena, EVENT_BLOCK, u64::from(price) | (u64::from(category) << 32));
    }
}

/// One event line, exactly as rules.md reads it.
pub fn parse_event_line(ctx: &Ctx, line: &[u8], out: &mut ParseOut) {
    let Some([event_id, user_id, ts, ingest_ts, event_type, product_id, price_cents]) = split_fields::<7>(line)
    else {
        return;
    };
    if event_id.is_empty() {
        return;
    }
    let event_code = id_code(event_id, b'e', &ctx.event_ids);
    // ingest_ts is always valid F1 per the rules. Anything this engine cannot
    // store exactly (not whole seconds, far from the window) sends the whole
    // run to the reference pipeline instead of guessing.
    let Some(ingest_ms) = parse_ts(ingest_ts) else {
        ctx.request_fallback();
        return;
    };
    let ingest_seconds = ingest_ms.div_euclid(1_000) - ctx.ingest_base_seconds;
    if ingest_ms.rem_euclid(1_000) != 0 || !(0..(1 << 21)).contains(&ingest_seconds) {
        ctx.request_fallback();
        return;
    }
    let ts = parse_ts(ts).filter(|&ts| ctx.window_start <= ts && ts < ctx.window_end);
    let is_purchase = event_type == b"purchase";
    let known_type = is_purchase || event_type == b"view" || event_type == b"cart";
    let price = if is_purchase { parse_price(price_cents) } else { Some(0) };
    let mut valid = !user_id.is_empty() && ts.is_some() && known_type && !product_id.is_empty() && price.is_some();
    if let (true, Some(ts)) = (valid, ts) {
        if ingest_ms - ts > LATE_MS {
            valid = false;
        }
    }
    if !valid {
        emit_event(out, event_code, 0, false, 0, ingest_seconds as u64, None);
        return;
    }
    let user_code = id_code(user_id, b'u', &ctx.user_ids);
    let purchase = if is_purchase {
        let category = ctx.product_category.get(product_id).copied().unwrap_or(ctx.unknown_category);
        Some((price.unwrap_or(0), category))
    } else {
        None
    };
    let ts = (ts.unwrap_or(0) - ctx.window_start) as u64;
    emit_event(out, event_code, ts, true, user_code, ingest_seconds as u64, purchase);
}

/// One click line. Invalid clicks are dropped here: copies of a click are
/// identical, so validation gives every copy the same answer.
pub fn parse_click_line(ctx: &Ctx, line: &[u8], out: &mut ParseOut) {
    let Some([click_id, user_id, campaign_id, ts]) = split_fields::<4>(line) else {
        return;
    };
    if click_id.is_empty() || user_id.is_empty() {
        return;
    }
    let click_window_start = ctx.window_start - WEEK_MS;
    let Some(ts) = parse_ts(ts).filter(|&ts| click_window_start <= ts && ts < ctx.window_end) else {
        return;
    };
    let Some(&campaign) = ctx.campaign_index.get(campaign_id) else {
        return;
    };
    let click_code = id_code(click_id, b'k', &ctx.click_ids);
    let user = id_code(user_id, b'u', &ctx.user_ids);
    out.clicks.push(ClickRecord {
        user,
        code_and_campaign: click_code | (u64::from(campaign) << 42),
        ts: (ts - click_window_start) as u32,
    });
}

pub fn parse_link_line(ctx: &Ctx, line: &[u8], out: &mut ParseOut) {
    let Some([a, b]) = split_fields::<2>(line) else {
        return;
    };
    if a.is_empty() || b.is_empty() {
        return;
    }
    out.links.push((id_code(a, b'u', &ctx.user_ids), id_code(b, b'u', &ctx.user_ids)));
}

pub fn parse_events_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    lines_in_chunk(buffer, start, end, |line| parse_event_line(ctx, line, out));
}

pub fn parse_clicks_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    lines_in_chunk(buffer, start, end, |line| parse_click_line(ctx, line, out));
}

pub fn parse_links_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    lines_in_chunk(buffer, start, end, |line| parse_link_line(ctx, line, out));
}

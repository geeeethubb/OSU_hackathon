//! Parsing of event, click and link lines into compact records.
//!
//! The `*_slow` functions are the exact, general parsers: they follow
//! rules.md field by field. The AVX2 fast paths in `simd.rs` must produce the
//! same rows for the lines they accept.

use super::arena::{Arena, PartWriter};
use super::codes::{id_code, mix};
use super::{Ctx, EVENT_BLOCK, LOCAL_BITS, LOCAL_MASK, PARTS};
use crate::util::{parse_price, parse_ts, split_fields, LATE_MS, WEEK_MS};

/// One event line that takes part in dedup (well-formed, non-empty event ID).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventRow {
    pub event: u64,
    /// User code; 0 when the row is invalid.
    pub user: u64,
    /// Milliseconds since `window_start`; 0 when the row is invalid.
    pub ts: u32,
    /// Seconds since `Ctx::ingest_base_seconds`.
    pub ingest: u32,
    /// Passes validation and is not late.
    pub valid: bool,
    /// `(price, category)` for a valid purchase.
    pub purchase: Option<(u32, u32)>,
}

/// A valid click before its user is mapped to a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Where the first data line that starts at or after `start` begins. The
/// header (the file's first line) is never a data line, and a line that
/// straddles `start` belongs to the previous chunk.
#[inline]
pub fn first_line_start(buffer: &[u8], start: usize) -> usize {
    let from = if start == 0 { 0 } else { start - 1 };
    match find_newline(&buffer[from..]) {
        Some(offset) => from + offset + 1,
        None => buffer.len(),
    }
}

/// Calls `visit` for every data line that starts inside `[position, end)`.
/// A line that straddles `end` belongs to this chunk.
#[inline]
pub fn lines_from<'a>(buffer: &'a [u8], mut position: usize, end: usize, mut visit: impl FnMut(&'a [u8])) {
    let limit = end.min(buffer.len());
    while position < limit {
        let line_end = find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset);
        visit(&buffer[position..line_end]);
        position = line_end + 1;
    }
}

#[inline(always)]
pub fn emit_event(out: &mut ParseOut, row: &EventRow) {
    let mixed = mix(row.event);
    let part = (mixed >> LOCAL_BITS) as usize;
    let word0 = ((mixed & LOCAL_MASK) << 32) | (u64::from(row.ts) << 1) | u64::from(row.valid);
    let word1 = (row.user << 22) | (u64::from(row.ingest) << 1) | u64::from(row.purchase.is_some());
    let writer = &mut out.events[part];
    writer.push(&mut out.arena, EVENT_BLOCK, word0);
    writer.push(&mut out.arena, EVENT_BLOCK, word1);
    if let Some((price, category)) = row.purchase {
        writer.push(&mut out.arena, EVENT_BLOCK, u64::from(price) | (u64::from(category) << 32));
    }
}

/// One event line, exactly as rules.md reads it. `None` drops the line.
pub fn event_row_slow(ctx: &Ctx, line: &[u8]) -> Option<EventRow> {
    let [event_id, user_id, ts, ingest_ts, event_type, product_id, price_cents] = split_fields::<7>(line)?;
    if event_id.is_empty() {
        return None;
    }
    let event = id_code(event_id, b'e', &ctx.event_ids);
    // ingest_ts is always valid F1 per the rules. Anything this engine cannot
    // store exactly (not whole seconds, far from the window) sends the whole
    // run to the reference pipeline instead of guessing.
    let Some(ingest_ms) = parse_ts(ingest_ts) else {
        ctx.request_fallback();
        return None;
    };
    let ingest_seconds = ingest_ms.div_euclid(1_000) - ctx.ingest_base_seconds;
    if ingest_ms.rem_euclid(1_000) != 0 || !(0..(1 << 21)).contains(&ingest_seconds) {
        ctx.request_fallback();
        return None;
    }
    let ingest = ingest_seconds as u32;
    let invalid = EventRow { event, user: 0, ts: 0, ingest, valid: false, purchase: None };
    let Some(ts) = parse_ts(ts).filter(|&ts| ctx.window_start <= ts && ts < ctx.window_end) else {
        return Some(invalid);
    };
    let is_purchase = event_type == b"purchase";
    let known_type = is_purchase || event_type == b"view" || event_type == b"cart";
    if user_id.is_empty() || !known_type || product_id.is_empty() || ingest_ms - ts > LATE_MS {
        return Some(invalid);
    }
    let purchase = if is_purchase {
        let Some(price) = parse_price(price_cents) else {
            return Some(invalid);
        };
        Some((price, ctx.product_category.get(product_id).copied().unwrap_or(ctx.unknown_category)))
    } else {
        None
    };
    let user = id_code(user_id, b'u', &ctx.user_ids);
    Some(EventRow { event, user, ts: (ts - ctx.window_start) as u32, ingest, valid: true, purchase })
}

/// One click line. Invalid clicks are dropped here: copies of a click are
/// identical, so validation gives every copy the same answer.
pub fn click_row_slow(ctx: &Ctx, line: &[u8]) -> Option<ClickRecord> {
    let [click_id, user_id, campaign_id, ts] = split_fields::<4>(line)?;
    if click_id.is_empty() || user_id.is_empty() {
        return None;
    }
    let click_window_start = ctx.window_start - WEEK_MS;
    let ts = parse_ts(ts).filter(|&ts| click_window_start <= ts && ts < ctx.window_end)?;
    let &campaign = ctx.campaign_index.get(campaign_id)?;
    let click_code = id_code(click_id, b'k', &ctx.click_ids);
    let user = id_code(user_id, b'u', &ctx.user_ids);
    Some(ClickRecord {
        user,
        code_and_campaign: click_code | (u64::from(campaign) << 42),
        ts: (ts - click_window_start) as u32,
    })
}

pub fn link_row_slow(ctx: &Ctx, line: &[u8]) -> Option<(u64, u64)> {
    let [a, b] = split_fields::<2>(line)?;
    if a.is_empty() || b.is_empty() {
        return None;
    }
    Some((id_code(a, b'u', &ctx.user_ids), id_code(b, b'u', &ctx.user_ids)))
}

pub fn parse_events_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    #[allow(unused_mut)]
    let mut position = first_line_start(buffer, start);
    #[cfg(target_arch = "x86_64")]
    if ctx.fast_rows {
        use super::simd::{event_row_fast, ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD};
        let limit = end.min(buffer.len());
        let safe = buffer.len().saturating_sub(ROW_PAD);
        let mut row = EventRow::default();
        while position < limit && position < safe {
            if ctx.diag == 2 {
                // SAFETY: at least ROW_PAD readable bytes follow `position`.
                let length = unsafe { super::simd::row_length(buffer.as_ptr().add(position)) };
                position += length + 1;
                continue;
            }
            // SAFETY: at least ROW_PAD readable bytes follow `position`.
            let (length, status) = unsafe { event_row_fast(ctx, buffer.as_ptr().add(position), &mut row) };
            if status == ROW_LONG {
                // No newline within 128 bytes: the general parser takes this line.
                let line_end = find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset);
                if let Some(row) = event_row_slow(ctx, &buffer[position..line_end]) {
                    emit_event(out, &row);
                }
                position = line_end + 1;
                continue;
            }
            let line = &buffer[position..position + length];
            let fast = match status {
                ROW_EMIT => Some(row),
                ROW_DROP => None,
                _ => event_row_slow(ctx, line),
            };
            if ctx.verify && (status == ROW_EMIT || status == ROW_DROP) {
                let slow = event_row_slow(ctx, line);
                assert!(
                    slow == fast,
                    "event row mismatch on {:?}: fast {:?}, slow {:?}",
                    String::from_utf8_lossy(line),
                    fast,
                    slow
                );
            }
            if let Some(row) = &fast {
                if ctx.diag == 1 {
                    std::hint::black_box(row);
                } else {
                    emit_event(out, row);
                }
            }
            position += length + 1;
        }
        // The last few lines of the file fall through to the general parser.
    }
    lines_from(buffer, position, end, |line| {
        if let Some(row) = event_row_slow(ctx, line) {
            emit_event(out, &row);
        }
    });
}

pub fn parse_clicks_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    let position = first_line_start(buffer, start);
    lines_from(buffer, position, end, |line| {
        if let Some(click) = click_row_slow(ctx, line) {
            out.clicks.push(click);
        }
    });
}

pub fn parse_links_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    let position = first_line_start(buffer, start);
    lines_from(buffer, position, end, |line| {
        if let Some(pair) = link_row_slow(ctx, line) {
            out.links.push(pair);
        }
    });
}

//! Parsing of event, click and link lines into compact records.
//!
//! The `*_slow` functions are the exact, general parsers: they follow
//! rules.md field by field. The AVX2 fast paths in `simd.rs` must produce the
//! same rows for the lines they accept.

use super::arena::{Arena, PartWriter};
use super::codes::{id_code, mix};
use super::{Ctx, EVENT_BLOCK};
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
    /// `mixed code >> event_shift` is an event's partition.
    pub event_shift: u32,
    /// Valid clicks in parse order, three words each: user code, click code
    /// and campaign, ts. They live in the arena like the events do, so the
    /// list never reallocates and sits in huge pages where those exist.
    pub clicks: PartWriter,
    pub links: Vec<(u64, u64)>,
}

/// Words per block of a worker's click list, and words per click in it.
pub const RAW_CLICK_BLOCK: usize = 512;
pub const RAW_CLICK_WORDS: usize = 3;

impl ParseOut {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            arena: Arena::new(),
            events: vec![PartWriter::EMPTY; ctx.event_parts()],
            event_shift: ctx.event_shift(),
            clicks: PartWriter::EMPTY,
            links: Vec::new(),
        }
    }
}

#[inline(always)]
pub fn emit_click(out: &mut ParseOut, click: &ClickRecord) {
    let at = out.clicks.reserve(&mut out.arena, RAW_CLICK_BLOCK, RAW_CLICK_WORDS);
    // SAFETY: `reserve` returned room for three words.
    unsafe {
        *at = click.user;
        *at.add(1) = click.code_and_campaign;
        *at.add(2) = u64::from(click.ts);
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

/// Stores one event record in the partition its event ID belongs to.
#[inline(always)]
pub fn emit_raw(
    out: &mut ParseOut,
    event: u64,
    ts: u64,
    valid: u64,
    user: u64,
    ingest: u64,
    purchase: Option<(u32, u32)>,
) {
    let mixed = mix(event);
    let part = (mixed >> out.event_shift) as usize;
    // The local key: the low 32 bits hold every bit below the partition's.
    let word0 = (mixed << 32) | (ts << 1) | valid;
    let word1 = (user << 22) | (ingest << 1) | u64::from(purchase.is_some());
    // SAFETY: a mixed code has `CODE_BITS` bits, so `part` is below the
    // number of writers; `reserve` returns room for the words written.
    unsafe {
        let writer = out.events.get_unchecked_mut(part);
        let at = writer.reserve(&mut out.arena, EVENT_BLOCK, 2 + usize::from(purchase.is_some()));
        *at = word0;
        *at.add(1) = word1;
        if let Some((price, category)) = purchase {
            *at.add(2) = u64::from(price) | (u64::from(category) << 32);
        }
    }
}

#[inline(always)]
pub fn emit_event(out: &mut ParseOut, row: &EventRow) {
    emit_raw(out, row.event, u64::from(row.ts), u64::from(row.valid), row.user, u64::from(row.ingest), row.purchase);
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

/// The fast loop over event rows, specialized for what happens to a parsed
/// row (`simd::MODE_*`). Returns where the general parser must continue.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn events_fast_loop<const MODE: u32>(
    ctx: &Ctx,
    buffer: &[u8],
    mut position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{event_row_fast, MODE_ROW, ROW_DONE, ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD};
    let limit = end.min(buffer.len());
    let safe = buffer.len().saturating_sub(ROW_PAD);
    let mut row = EventRow::default();
    while position < limit && position < safe {
        // SAFETY: at least ROW_PAD readable bytes follow `position`.
        let (length, status) = unsafe { event_row_fast::<MODE>(ctx, buffer.as_ptr().add(position), out, &mut row) };
        if status == ROW_DONE {
            position += length + 1;
            continue;
        }
        if status == ROW_LONG {
            // No newline within the fast path's reach: the general parser takes this line.
            let line_end = find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset);
            if let Some(row) = event_row_slow(ctx, &buffer[position..line_end]) {
                emit_event(out, &row);
            }
            position = line_end + 1;
            continue;
        }
        let line = &buffer[position..position + length];
        let parsed = match status {
            ROW_EMIT => Some(row),
            ROW_DROP => None,
            _ => event_row_slow(ctx, line),
        };
        if MODE == MODE_ROW && (status == ROW_EMIT || status == ROW_DROP) {
            let slow = event_row_slow(ctx, line);
            assert!(
                slow == parsed,
                "event row mismatch on {:?}: fast {:?}, slow {:?}",
                String::from_utf8_lossy(line),
                parsed,
                slow
            );
        }
        if let Some(row) = &parsed {
            emit_event(out, row);
        }
        position += length + 1;
    }
    // Diagnostic modes fold their work into `row.event`; keep it alive.
    std::hint::black_box(row.event);
    position
}

pub fn parse_events_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    #[allow(unused_mut)]
    let mut position = first_line_start(buffer, start);
    #[cfg(target_arch = "x86_64")]
    if ctx.fast_rows {
        use super::simd::{MODE_ROW, MODE_STORE};
        position = match (ctx.verify, ctx.diag) {
            (true, _) => events_fast_loop::<MODE_ROW>(ctx, buffer, position, end, out),
            (false, 0) => events_fast_loop::<MODE_STORE>(ctx, buffer, position, end, out),
            (false, 2) => events_fast_loop::<2>(ctx, buffer, position, end, out),
            (false, 3) => events_fast_loop::<3>(ctx, buffer, position, end, out),
            (false, 4) => events_fast_loop::<4>(ctx, buffer, position, end, out),
            (false, 5) => events_fast_loop::<5>(ctx, buffer, position, end, out),
            (false, 6) => events_fast_loop::<6>(ctx, buffer, position, end, out),
            (false, _) => events_fast_loop::<7>(ctx, buffer, position, end, out),
        };
        // The last few lines of the file fall through to the general parser.
    }
    lines_from(buffer, position, end, |line| {
        if let Some(row) = event_row_slow(ctx, line) {
            emit_event(out, &row);
        }
    });
}

pub fn parse_clicks_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    #[allow(unused_mut)]
    let mut position = first_line_start(buffer, start);
    #[cfg(target_arch = "x86_64")]
    if ctx.fast_rows {
        use super::simd::{click_row_fast, ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD};
        let limit = end.min(buffer.len());
        let safe = buffer.len().saturating_sub(ROW_PAD);
        let verify = ctx.verify;
        let mut click = ClickRecord { user: 0, code_and_campaign: 0, ts: 0 };
        while position < limit && position < safe {
            // SAFETY: at least ROW_PAD readable bytes follow `position`.
            let (length, status) = unsafe { click_row_fast(ctx, buffer.as_ptr().add(position), &mut click) };
            if status == ROW_EMIT && !verify {
                emit_click(out, &click);
                position += length + 1;
                continue;
            }
            if status == ROW_LONG {
                let line_end = find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset);
                if let Some(click) = click_row_slow(ctx, &buffer[position..line_end]) {
                    emit_click(out, &click);
                }
                position = line_end + 1;
                continue;
            }
            let line = &buffer[position..position + length];
            let fast = match status {
                ROW_EMIT => Some(click),
                ROW_DROP => None,
                _ => click_row_slow(ctx, line),
            };
            if verify && (status == ROW_EMIT || status == ROW_DROP) {
                let slow = click_row_slow(ctx, line);
                assert!(
                    slow == fast,
                    "click row mismatch on {:?}: fast {:?}, slow {:?}",
                    String::from_utf8_lossy(line),
                    fast,
                    slow
                );
            }
            if let Some(click) = fast {
                emit_click(out, &click);
            }
            position += length + 1;
        }
    }
    lines_from(buffer, position, end, |line| {
        if let Some(click) = click_row_slow(ctx, line) {
            emit_click(out, &click);
        }
    });
}

pub fn parse_links_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    #[allow(unused_mut)]
    let mut position = first_line_start(buffer, start);
    #[cfg(target_arch = "x86_64")]
    if ctx.fast_rows {
        use super::simd::{link_row_fast, ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD};
        let limit = end.min(buffer.len());
        let safe = buffer.len().saturating_sub(ROW_PAD);
        let verify = ctx.verify;
        let mut pair = (0u64, 0u64);
        while position < limit && position < safe {
            // SAFETY: at least ROW_PAD readable bytes follow `position`.
            let (length, status) = unsafe { link_row_fast(ctx, buffer.as_ptr().add(position), &mut pair) };
            if status == ROW_EMIT && !verify {
                out.links.push(pair);
                position += length + 1;
                continue;
            }
            if status == ROW_LONG {
                let line_end = find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset);
                if let Some(pair) = link_row_slow(ctx, &buffer[position..line_end]) {
                    out.links.push(pair);
                }
                position = line_end + 1;
                continue;
            }
            let line = &buffer[position..position + length];
            let fast = match status {
                ROW_EMIT => Some(pair),
                ROW_DROP => None,
                _ => link_row_slow(ctx, line),
            };
            if verify && (status == ROW_EMIT || status == ROW_DROP) {
                let slow = link_row_slow(ctx, line);
                assert!(
                    slow == fast,
                    "link row mismatch on {:?}: fast {:?}, slow {:?}",
                    String::from_utf8_lossy(line),
                    fast,
                    slow
                );
            }
            if let Some(pair) = fast {
                out.links.push(pair);
            }
            position += length + 1;
        }
    }
    lines_from(buffer, position, end, |line| {
        if let Some(pair) = link_row_slow(ctx, line) {
            out.links.push(pair);
        }
    });
}

//! Parsing of event, click and link lines into compact records.
//!
//! The `*_slow` functions are the exact, general parsers: they follow
//! rules.md field by field. The AVX2 fast paths in `simd.rs` must produce the
//! same rows for the lines they accept.

use super::arena::{Arena, PartWriter};
use super::codes::{id_code, mix};
use super::keys::{base36_value, Keys};
use super::{Ctx, EVENT_BLOCK};
use crate::util::{parse_price, parse_ts, split_fields, LATE_MS, WEEK_MS};

/// One event line that takes part in dedup (well-formed, non-empty event ID).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventRow {
    /// The event's code; in index mode, its dense index.
    pub event: u64,
    /// User code; 0 when the row is invalid. In index mode a generated user
    /// ID is its user number instead, and `user_is_number` says so.
    pub user: u64,
    pub user_is_number: bool,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClickRecord {
    /// User code. In index mode a fast kernel hands a generated user ID back
    /// as its user number instead, and `user_is_number` says so.
    pub user: u64,
    pub user_is_number: bool,
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
    /// Index mode: one list per run of `1 << index_shift` event indexes. Same
    /// record as `events`, except word 0 = index within the run (16) |
    /// unused (15) | user is a user number (1) | ts (31) | valid (1).
    pub indexed: Vec<PartWriter>,
    /// Index mode: users of valid rows whose event ID is not a generated one.
    /// Their persons must be handled by the general path as a whole.
    pub odd_event_users: Vec<u64>,
    /// Valid clicks in parse order, three words each: user code, click code
    /// and campaign, ts. They live in the arena like the events do, so the
    /// list never reallocates and sits in huge pages where those exist.
    /// Three words per click: user code, click code and campaign, ts. In
    /// index mode only the clicks of IDs that are not generated ones.
    pub clicks: PartWriter,
    /// Index mode: the clicks of generated user IDs, one list per run of
    /// `CLICK_USERS` user indexes, two words per click:
    /// ts (32) | alias (9) | user index within the run (9), then click code
    /// and campaign.
    pub user_clicks: Vec<PartWriter>,
    /// The keys of index mode, for `emit_click`.
    pub keys: Option<Keys>,
    /// `Keys::index_shift`, for `emit_indexed`.
    pub index_shift: u32,
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
            indexed: vec![PartWriter::EMPTY; ctx.keys.map_or(0, |keys| index_parts(&keys))],
            odd_event_users: Vec::new(),
            clicks: PartWriter::EMPTY,
            user_clicks: vec![PartWriter::EMPTY; ctx.keys.map_or(0, |keys| click_parts(&keys))],
            keys: ctx.keys,
            index_shift: ctx.keys.map_or(0, |keys| keys.index_shift),
            links: Vec::new(),
        }
    }
}

/// User indexes per click list of index mode, as a shift.
pub const CLICK_SHIFT: u32 = 9;
pub const CLICK_USERS: usize = 1 << CLICK_SHIFT;
/// Words per block of those lists: a list holds a few hundred clicks.
const USER_CLICK_BLOCK: usize = 128;

/// How many click lists cover every user index below the keys' bound.
pub fn click_parts(keys: &Keys) -> usize {
    (keys.user_bound >> CLICK_SHIFT) as usize + 1
}

/// The click as index mode stores it: the user of a generated user ID is its
/// user number. A click that already carries a number is returned as it is.
#[inline]
pub fn click_indexed_form(keys: &Keys, click: ClickRecord) -> ClickRecord {
    if click.user_is_number {
        return click;
    }
    match keys.user_number_of_code(click.user) {
        Some(number) => ClickRecord { user: number, user_is_number: true, ..click },
        None => click,
    }
}

#[inline(always)]
pub fn emit_click(out: &mut ParseOut, click: &ClickRecord) {
    // A kernel that read the user number from the text says so; any other
    // click of index mode has its code turned into the number here.
    let number = if click.user_is_number {
        Some(click.user)
    } else {
        out.keys.as_ref().and_then(|keys| keys.user_number_of_code(click.user))
    };
    if let Some(number) = number {
        // SAFETY: the number's user index is below the keys' bound, which
        // sized `user_clicks`; `reserve` returned room for two words.
        unsafe {
            let writer = out.user_clicks.get_unchecked_mut((number >> (9 + CLICK_SHIFT)) as usize);
            let at = writer.reserve(&mut out.arena, USER_CLICK_BLOCK, 2);
            *at = u64::from(click.ts) | ((number & ((1 << (9 + CLICK_SHIFT)) - 1)) << 32);
            *at.add(1) = click.code_and_campaign;
        }
        return;
    }
    let user = click.user;
    let at = out.clicks.reserve(&mut out.arena, RAW_CLICK_BLOCK, RAW_CLICK_WORDS);
    // SAFETY: `reserve` returned room for three words.
    unsafe {
        *at = user;
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

/// How many index partitions cover every index below the keys' bound.
pub fn index_parts(keys: &Keys) -> usize {
    (keys.index_bound >> keys.index_shift) as usize + 1
}

/// Index mode: stores one event record in the partition its index belongs to.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn emit_indexed(
    out: &mut ParseOut,
    index: u64,
    ts: u64,
    valid: u64,
    user: u64,
    user_is_number: u64,
    ingest: u64,
    purchase: Option<(u32, u32)>,
) {
    let part = (index >> out.index_shift) as usize;
    let word0 = ((index & ((1u64 << out.index_shift) - 1)) << 48) | (user_is_number << 32) | (ts << 1) | valid;
    let word1 = (user << 22) | (ingest << 1) | u64::from(purchase.is_some());
    // SAFETY: indexes are below the keys' bound, which sized `indexed`;
    // `reserve` returns room for the words written.
    unsafe {
        let writer = out.indexed.get_unchecked_mut(part);
        let at = writer.reserve(&mut out.arena, EVENT_BLOCK, 2 + usize::from(purchase.is_some()));
        *at = word0;
        *at.add(1) = word1;
        if let Some((price, category)) = purchase {
            *at.add(2) = u64::from(price) | (u64::from(category) << 32);
        }
    }
}

/// What the general parser makes of a line in index mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlowIndexed {
    Drop,
    /// The event ID is not a generated one: the row keeps its codes and goes
    /// to the general path.
    Generic(EventRow),
    /// `event` is the index; `user` a user number when `user_is_number`.
    Indexed(EventRow),
}

/// One event line in index mode, exactly as rules.md reads it.
pub fn event_row_slow_indexed(ctx: &Ctx, keys: &Keys, line: &[u8]) -> SlowIndexed {
    let Some(row) = event_row_slow(ctx, line) else {
        return SlowIndexed::Drop;
    };
    let Some([event_id, user_id, ..]) = split_fields::<7>(line) else {
        return SlowIndexed::Drop;
    };
    let number = |id: &[u8], prefix: u8| if id.first() == Some(&prefix) { base36_value(&id[1..]) } else { None };
    let Some(index) = number(event_id, b'e').and_then(|value| keys.event_index(value)) else {
        return SlowIndexed::Generic(row);
    };
    let (user, user_is_number) = if !row.valid {
        (0, false)
    } else if let Some(user_number) = number(user_id, b'u').and_then(|value| keys.user_number(value)) {
        (user_number, true)
    } else {
        (row.user, false)
    };
    SlowIndexed::Indexed(EventRow { event: index, user, user_is_number, ..row })
}

/// Stores what `event_row_slow_indexed` returned.
pub fn emit_slow_indexed(out: &mut ParseOut, parsed: &SlowIndexed) {
    match parsed {
        SlowIndexed::Drop => {}
        SlowIndexed::Generic(row) => {
            emit_event(out, row);
            if row.valid {
                out.odd_event_users.push(row.user);
            }
        }
        SlowIndexed::Indexed(row) => emit_indexed(
            out,
            row.event,
            u64::from(row.ts),
            u64::from(row.valid),
            row.user,
            u64::from(row.user_is_number),
            u64::from(row.ingest),
            row.purchase,
        ),
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
    let invalid = EventRow { event, user: 0, user_is_number: false, ts: 0, ingest, valid: false, purchase: None };
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
    Some(EventRow {
        event,
        user,
        user_is_number: false,
        ts: (ts - ctx.window_start) as u32,
        ingest,
        valid: true,
        purchase,
    })
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
        user_is_number: false,
        code_and_campaign: click_code | (u64::from(campaign) << 42),
        ts: (ts - click_window_start) as u32,
    })
}

/// One click line in index mode: the click of `click_row_slow`, with the
/// user of a generated user ID as its user number.
pub fn click_row_slow_indexed(ctx: &Ctx, keys: &Keys, line: &[u8]) -> Option<ClickRecord> {
    click_row_slow(ctx, line).map(|click| click_indexed_form(keys, click))
}

pub fn link_row_slow(ctx: &Ctx, line: &[u8]) -> Option<(u64, u64)> {
    let [a, b] = split_fields::<2>(line)?;
    if a.is_empty() || b.is_empty() {
        return None;
    }
    Some((id_code(a, b'u', &ctx.user_ids), id_code(b, b'u', &ctx.user_ids)))
}

/// The fast loop over event rows, specialized for what happens to a parsed
/// row (`simd::MODE_*`) and for index mode. Returns where the general parser
/// must continue.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn events_fast_loop<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    buffer: &[u8],
    position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{event_kernel, KERNEL_GENERAL, KERNEL_LEAN};
    match event_kernel() {
        KERNEL_GENERAL => events_rows_loop::<MODE, INDEXED, KERNEL_GENERAL>(ctx, buffer, position, end, out),
        _ => events_rows_loop::<MODE, INDEXED, KERNEL_LEAN>(ctx, buffer, position, end, out),
    }
}

/// `events_fast_loop` with one of the event kernels (`simd::KERNEL_*`).
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn events_rows_loop<const MODE: u32, const INDEXED: bool, const KERNEL: u32>(
    ctx: &Ctx,
    buffer: &[u8],
    mut position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{
        event_row_fast, event_row_lean, KERNEL_GENERAL, MODE_ROW, ROW_DONE, ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD,
    };
    let limit = end.min(buffer.len());
    let safe = buffer.len().saturating_sub(ROW_PAD);
    let mut row = EventRow::default();
    while position < limit && position < safe {
        // SAFETY: at least ROW_PAD readable bytes follow `position`.
        let (length, status) = unsafe {
            let p = buffer.as_ptr().add(position);
            if KERNEL == KERNEL_GENERAL {
                event_row_fast::<MODE, INDEXED>(ctx, p, out, &mut row)
            } else {
                event_row_lean::<MODE, INDEXED>(ctx, p, out, &mut row)
            }
        };
        if status == ROW_DONE {
            position += length + 1;
            continue;
        }
        // No newline within the fast path's reach: find the end of the line.
        let line_end = if status == ROW_LONG {
            find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset)
        } else {
            position + length
        };
        let line = &buffer[position..line_end];
        position = line_end + 1;
        let checked = MODE == MODE_ROW && (status == ROW_EMIT || status == ROW_DROP);
        if INDEXED {
            let keys = ctx.keys.as_ref().expect("index mode needs keys");
            // Everything but a row the fast path parsed goes through the general parser.
            let parsed = match status {
                ROW_EMIT => SlowIndexed::Indexed(row),
                ROW_DROP => SlowIndexed::Drop,
                _ => event_row_slow_indexed(ctx, keys, line),
            };
            if checked {
                let slow = event_row_slow_indexed(ctx, keys, line);
                assert!(
                    slow == parsed,
                    "indexed event row mismatch on {:?}: fast {:?}, slow {:?}",
                    String::from_utf8_lossy(line),
                    parsed,
                    slow
                );
            }
            emit_slow_indexed(out, &parsed);
            continue;
        }
        let parsed = match status {
            ROW_EMIT => Some(row),
            ROW_DROP => None,
            _ => event_row_slow(ctx, line),
        };
        if checked {
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
        position = match (ctx.keys.is_some(), ctx.verify, ctx.diag) {
            (true, true, _) => events_fast_loop::<MODE_ROW, true>(ctx, buffer, position, end, out),
            (true, false, _) => events_fast_loop::<MODE_STORE, true>(ctx, buffer, position, end, out),
            (false, true, _) => events_fast_loop::<MODE_ROW, false>(ctx, buffer, position, end, out),
            (false, false, 0) => events_fast_loop::<MODE_STORE, false>(ctx, buffer, position, end, out),
            (false, false, 2) => events_fast_loop::<2, false>(ctx, buffer, position, end, out),
            (false, false, 3) => events_fast_loop::<3, false>(ctx, buffer, position, end, out),
            (false, false, 4) => events_fast_loop::<4, false>(ctx, buffer, position, end, out),
            (false, false, 5) => events_fast_loop::<5, false>(ctx, buffer, position, end, out),
            (false, false, 6) => events_fast_loop::<6, false>(ctx, buffer, position, end, out),
            (false, false, _) => events_fast_loop::<7, false>(ctx, buffer, position, end, out),
        };
        // The last few lines of the file fall through to the general parser.
    }
    lines_from(buffer, position, end, |line| match &ctx.keys {
        Some(keys) => emit_slow_indexed(out, &event_row_slow_indexed(ctx, keys, line)),
        None => {
            if let Some(row) = event_row_slow(ctx, line) {
                emit_event(out, &row);
            }
        }
    });
}

/// The fast loop over click rows, specialized for index mode (`INDEXED`: the
/// kernel reads a generated user ID as its user number, and `ctx.keys` must
/// be set). Returns where the general parser must continue.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn clicks_rows_loop<const INDEXED: bool>(
    ctx: &Ctx,
    buffer: &[u8],
    mut position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{click_row_fast, ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD};
    let limit = end.min(buffer.len());
    let safe = buffer.len().saturating_sub(ROW_PAD);
    let verify = ctx.verify;
    let mut click = ClickRecord::default();
    while position < limit && position < safe {
        // SAFETY: at least ROW_PAD readable bytes follow `position`.
        let (length, status) =
            unsafe { click_row_fast::<INDEXED>(ctx, buffer.as_ptr().add(position), &mut click) };
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
            // In index mode the kernel must hand back exactly the indexed
            // form of what the general parser reads.
            let slow = match &ctx.keys {
                Some(keys) if INDEXED => click_row_slow_indexed(ctx, keys, line),
                _ => click_row_slow(ctx, line),
            };
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
    position
}

pub fn parse_clicks_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    #[allow(unused_mut)]
    let mut position = first_line_start(buffer, start);
    #[cfg(target_arch = "x86_64")]
    if ctx.fast_rows {
        position = if ctx.keys.is_some() {
            clicks_rows_loop::<true>(ctx, buffer, position, end, out)
        } else {
            clicks_rows_loop::<false>(ctx, buffer, position, end, out)
        };
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

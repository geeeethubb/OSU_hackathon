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

/// Stores a click whose user is known by its code: three words in the
/// worker's click list.
#[inline(always)]
pub fn emit_code_click(out: &mut ParseOut, user: u64, code_and_campaign: u64, ts: u64) {
    let at = out.clicks.reserve(&mut out.arena, RAW_CLICK_BLOCK, RAW_CLICK_WORDS);
    // SAFETY: `reserve` returned room for three words.
    unsafe {
        *at = user;
        *at.add(1) = code_and_campaign;
        *at.add(2) = ts;
    }
}

/// Index mode: stores a click of a generated user ID, known by its user
/// number: two words in the list of its run of user indexes.
///
/// # Safety
/// The number's user index (`number >> 9`) must be below the keys' bound.
#[inline(always)]
pub unsafe fn emit_number_click(out: &mut ParseOut, number: u64, code_and_campaign: u64, ts: u64) {
    // SAFETY: the keys' bound sized `user_clicks`; `reserve` returned room
    // for two words.
    let writer = out.user_clicks.get_unchecked_mut((number >> (9 + CLICK_SHIFT)) as usize);
    let at = writer.reserve(&mut out.arena, USER_CLICK_BLOCK, 2);
    *at = ts | ((number & ((1 << (9 + CLICK_SHIFT)) - 1)) << 32);
    *at.add(1) = code_and_campaign;
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
    match number {
        // SAFETY: the number's user index is below the keys' bound: the
        // kernels and `user_number_of_code` hand out no other numbers.
        Some(number) => unsafe { emit_number_click(out, number, click.code_and_campaign, u64::from(click.ts)) },
        None => emit_code_click(out, click.user, click.code_and_campaign, u64::from(click.ts)),
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
    use super::simd::{event_kernel, LeanFrame, KERNEL_GENERAL, KERNEL_LEAN, KERNEL_LEAN_PLAIN, MODE_ROW, MODE_STORE};
    let kernel = event_kernel();
    // The kernel in assembly is for index mode.
    let assembly = INDEXED && kernel == KERNEL_LEAN;
    // Its loop with the store inside, where the writers are as that takes them.
    let run = assembly && MODE == MODE_STORE && LeanFrame::stores(ctx);
    match kernel {
        KERNEL_GENERAL => events_rows_loop::<MODE, INDEXED, KERNEL_GENERAL>(ctx, buffer, position, end, out),
        // Checked rows go one by one through the loop that compares them.
        _ if MODE == MODE_ROW && assembly => {
            events_rows_loop::<MODE, INDEXED, KERNEL_LEAN>(ctx, buffer, position, end, out)
        }
        _ if MODE == MODE_ROW => events_rows_loop::<MODE, INDEXED, KERNEL_LEAN_PLAIN>(ctx, buffer, position, end, out),
        // Index mode has no diagnostic modes of its own: SS_DIAG=2 and 7 pick them here.
        _ if INDEXED && MODE == MODE_STORE && ctx.diag == 2 => {
            events_lean_loop::<2, INDEXED>(ctx, buffer, position, end, out)
        }
        // Timing only: the loop in assembly without its store.
        _ if run && ctx.diag == 7 => events_assembly_run::<7>(ctx, buffer, position, end, out),
        _ if INDEXED && MODE == MODE_STORE && ctx.diag == 7 && assembly => {
            events_assembly_loop::<7>(ctx, buffer, position, end, out)
        }
        _ if INDEXED && MODE == MODE_STORE && ctx.diag == 7 => {
            events_lean_loop::<7, INDEXED>(ctx, buffer, position, end, out)
        }
        // Timing only: the store into one block (elsewhere SS_DIAG=8 stores as usual).
        _ if run && ctx.diag == 8 => events_assembly_run::<8>(ctx, buffer, position, end, out),
        _ if run => events_assembly_run::<0>(ctx, buffer, position, end, out),
        _ if assembly => events_assembly_loop::<MODE>(ctx, buffer, position, end, out),
        _ => events_lean_loop::<MODE, INDEXED>(ctx, buffer, position, end, out),
    }
}

/// The lean kernel in assembly (index mode) when rows are stored: the loop
/// over the rows and the store are in the assembly too, which comes back
/// here only for a row that needs something else. That is `KIND` 0.
///
/// The other two kinds are for timing only and keep nothing. `KIND` 8
/// (`SS_DIAG=8`): the same loop and the same store, but every record goes
/// into one block that is written over and over. `KIND` 7 (`SS_DIAG=7`):
/// the same loop without the store's instructions. With `SS_DIAG=1` (the
/// real lists, then stop) the three tell what the kernel costs, what the
/// store's instructions add, and what the lists add.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn events_assembly_run<const KIND: u32>(
    ctx: &Ctx,
    buffer: &[u8],
    mut position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{event_lean_common, event_lean_run, lean_setup, LeanFrame, MODE_STORE, ROW_PAD};
    let stop = end.min(buffer.len()).min(buffer.len().saturating_sub(ROW_PAD));
    let (lean, _) = lean_setup(ctx);
    let mut row = EventRow::default();
    // The assembly reads up to 16 bytes before a row.
    while position < stop.min(16) {
        position = if KIND == 0 {
            event_row_plain::<MODE_STORE, true>(ctx, buffer, position, out, &mut row)
        } else {
            event_row_plain::<7, true>(ctx, buffer, position, out, &mut row)
        };
    }
    if position < stop {
        let mut frame = LeanFrame::new(ctx);
        // The one block of the timing variant, and a writer's worth of
        // words for it: the cursor, the end of the block, and two more.
        let mut block = [0u64; 512];
        let block_start = block.as_mut_ptr();
        let mut one_writer = [block_start, block_start.wrapping_add(block.len()), block_start, block_start];
        if KIND == 8 {
            frame.one_list();
        }
        let frame = &mut frame as *mut LeanFrame;
        let base = buffer.as_ptr();
        // SAFETY (all of this): a row that starts before `stop` has ROW_PAD
        // readable bytes after it and, here, 16 before it; the tables and
        // the frame are those of `ctx`, whose writers `LeanFrame::stores`
        // found fit (the caller asked).
        let mut p = unsafe { base.add(position) };
        let last = unsafe { base.add(stop) };
        while p < last {
            let (status, at) = unsafe {
                match KIND {
                    0 => event_lean_run::<true>(lean, p, last, frame, out.indexed.as_mut_ptr()),
                    8 => event_lean_run::<true>(lean, p, last, frame, one_writer.as_mut_ptr().cast()),
                    _ => event_lean_run::<false>(lean, p, last, frame, std::ptr::null_mut()),
                }
            };
            p = match status {
                0 => at,
                3 if KIND == 0 => unsafe {
                    // The row's list needs a new block: `emit_indexed` gets it one.
                    event_lean_common::<MODE_STORE>((*frame).read(status), out, &mut row);
                    (*frame).next
                },
                3 => {
                    // The one block is full: from its start again; the row is dropped.
                    one_writer[0] = block_start;
                    unsafe { (*frame).next }
                }
                _ => unsafe {
                    // A purchase (what was read of it is in the frame) or a declined row.
                    let position = at.offset_from(base) as usize;
                    let read = (*frame).read(status);
                    let (ts, ingest, index, user, keep) =
                        (read.ts, read.ingest_seconds, read.index, read.user_number, read.keep);
                    let frame = frame as *const LeanFrame;
                    let next = if KIND == 0 {
                        event_row_rare::<MODE_STORE>(
                            ctx, frame, buffer, position, out, &mut row, status, ts, ingest, index, user, keep,
                        )
                    } else {
                        event_row_rare::<7>(ctx, frame, buffer, position, out, &mut row, status, ts, ingest, index, user, keep)
                    };
                    base.add(next)
                },
            };
        }
        position = unsafe { p.offset_from(base) } as usize;
        std::hint::black_box(&block);
    }
    // The timing variants fold what they read into `row.event`; keep it alive.
    std::hint::black_box(row.event);
    position
}

/// The lean kernel in assembly (index mode) row by row, for the diagnostic
/// mode that stores nothing (`MODE` 7) and for a run whose writers the loop
/// in assembly cannot store through.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn events_assembly_loop<const MODE: u32>(
    ctx: &Ctx,
    buffer: &[u8],
    mut position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{event_lean_asm, event_lean_common, lean_setup, LeanFrame, ROW_PAD};
    let stop = end.min(buffer.len()).min(buffer.len().saturating_sub(ROW_PAD));
    let (lean, _) = lean_setup(ctx);
    let mut row = EventRow::default();
    // The assembly reads up to 16 bytes before a row.
    while position < stop.min(16) {
        position = event_row_plain::<MODE, true>(ctx, buffer, position, out, &mut row);
    }
    if position < stop {
        let mut frame = LeanFrame::new(ctx);
        let frame = &mut frame as *mut LeanFrame;
        let base = buffer.as_ptr();
        // SAFETY (all of this): a row that starts before `stop` has ROW_PAD
        // readable bytes after it and, here, 16 before it; the tables and
        // the frame are those of `ctx`.
        let mut p = unsafe { base.add(position) };
        let last = unsafe { base.add(stop) };
        while p < last {
            let read = unsafe { event_lean_asm(lean, p, frame) };
            if read.status == 0 {
                unsafe {
                    event_lean_common::<MODE>(read, out, &mut row);
                    p = (*frame).next;
                }
                continue;
            }
            let position = unsafe { p.offset_from(base) } as usize;
            let next = event_row_rare::<MODE>(
                ctx,
                frame,
                buffer,
                position,
                out,
                &mut row,
                read.status,
                read.ts,
                read.ingest_seconds,
                read.index,
                read.user_number,
                read.keep,
            );
            p = unsafe { base.add(next) };
        }
        position = unsafe { p.offset_from(base) } as usize;
    }
    // Diagnostic modes fold their work into `row.event`; keep it alive.
    std::hint::black_box(row.event);
    position
}

/// A row the kernel in assembly did not finish by itself: a purchase it read
/// up to the product and the price (`status` 2; the arguments after it are
/// then what the assembly read), or a row it declined, which goes to the
/// lean kernel as the compiler makes it. Returns where the next row starts.
/// (What was read comes as separate arguments so that none of it is stored
/// anywhere on the common path.)
#[cfg(target_arch = "x86_64")]
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn event_row_rare<const MODE: u32>(
    ctx: &Ctx,
    frame: *const super::simd::LeanFrame,
    buffer: &[u8],
    position: usize,
    out: &mut ParseOut,
    row: &mut EventRow,
    status: u64,
    ts: u64,
    ingest_seconds: u64,
    index: u64,
    user_number: u64,
    keep: u64,
) -> usize {
    if status == 2 {
        let read = super::simd::LeanRow { status, ts, ingest_seconds, index, user_number, keep };
        // SAFETY: the frame is the one the assembly just filled for this
        // row, which ends right before `next`, inside the buffer.
        unsafe {
            super::simd::event_lean_purchase::<MODE>(ctx, &*frame, read, out, row);
            return (*frame).next.offset_from(buffer.as_ptr()) as usize;
        }
    }
    event_row_plain::<MODE, true>(ctx, buffer, position, out, row)
}

/// One row through the lean kernel as the compiler makes it and, if that
/// declines it, through the general kernel and the general parser. Returns
/// where the next row starts. `position` has ROW_PAD bytes after it.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn event_row_plain<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    buffer: &[u8],
    position: usize,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> usize {
    use super::simd::{event_row_lean_step, lean_setup, ROW_DECLINE};
    let (lean, window_length) = lean_setup(ctx);
    // SAFETY: at least ROW_PAD readable bytes follow `position`, and the
    // tables are those of `ctx`.
    let (length, _, status) = unsafe {
        event_row_lean_step::<MODE, INDEXED>(ctx, lean, window_length, buffer.as_ptr().add(position), out, row)
    };
    if status == ROW_DECLINE {
        return event_row_declined::<MODE, INDEXED>(ctx, buffer, position, out);
    }
    position + length + 1
}

/// The lean kernel's loop when rows are stored (or, in a diagnostic mode,
/// dropped). Nothing is called on its common path.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn events_lean_loop<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    buffer: &[u8],
    mut position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{event_row_lean_step, lean_setup, ROW_DECLINE, ROW_PAD};
    let stop = end.min(buffer.len()).min(buffer.len().saturating_sub(ROW_PAD));
    let (lean, window_length) = lean_setup(ctx);
    // Opaque to the optimizer: see `LeanTables`.
    let lean = std::hint::black_box(lean);
    let mut row = EventRow::default();
    while position < stop {
        // SAFETY: at least ROW_PAD readable bytes follow `position`, and the
        // tables are those of `ctx`.
        let (length, guess, status) = unsafe {
            event_row_lean_step::<MODE, INDEXED>(ctx, lean, window_length, buffer.as_ptr().add(position), out, &mut row)
        };
        if status == ROW_DECLINE {
            position = event_row_declined::<MODE, INDEXED>(ctx, buffer, position, out);
        } else if length != guess {
            position = row_end_elsewhere(position, length);
        } else {
            position += guess + 1;
        }
    }
    // Diagnostic modes fold their work into `row.event`; keep it alive.
    std::hint::black_box(row.event);
    position
}

/// One row the lean kernel declined: through the general kernel and, if
/// that declines it too, through the general parser. Returns where the next
/// row starts.
#[cfg(target_arch = "x86_64")]
#[cold]
#[inline(never)]
fn event_row_declined<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    buffer: &[u8],
    position: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{event_row_fast, MODE_STORE, ROW_DONE, ROW_DROP, ROW_LONG};
    let mut row = EventRow::default();
    // SAFETY: at least ROW_PAD readable bytes follow `position`.
    let (length, status) =
        unsafe { event_row_fast::<MODE, INDEXED>(ctx, buffer.as_ptr().add(position), out, &mut row) };
    std::hint::black_box(row.event);
    let line_end = if status == ROW_LONG {
        find_newline(&buffer[position..]).map_or(buffer.len(), |offset| position + offset)
    } else {
        position + length
    };
    // Stored (ROW_DONE) or malformed (ROW_DROP) is settled; anything else is
    // the general parser's. The diagnostic modes store nothing.
    if MODE == MODE_STORE && status != ROW_DONE && status != ROW_DROP {
        let line = &buffer[position..line_end];
        if INDEXED {
            let keys = ctx.keys.as_ref().expect("index mode needs keys");
            emit_slow_indexed(out, &event_row_slow_indexed(ctx, keys, line));
        } else if let Some(row) = event_row_slow(ctx, line) {
            emit_event(out, &row);
        }
    }
    line_end + 1
}

/// The lean kernel read a row whose end is not where its early look for the
/// newline said (a row under 64 or over 96 bytes). A call on purpose: it
/// keeps this a branch the processor predicts, so that in the common case
/// the next row's start does not wait for this row's last field.
#[cfg(target_arch = "x86_64")]
#[cold]
#[inline(never)]
fn row_end_elsewhere(position: usize, length: usize) -> usize {
    position + length + 1
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
        event_row_fast, event_row_lean, event_row_lean_asm, LeanFrame, KERNEL_GENERAL, KERNEL_LEAN, MODE_ROW, ROW_DONE,
        ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD,
    };
    let limit = end.min(buffer.len());
    let safe = buffer.len().saturating_sub(ROW_PAD);
    let mut row = EventRow::default();
    // The kernel in assembly, where it is asked for: index mode only.
    let mut frame = (INDEXED && KERNEL == KERNEL_LEAN).then(|| LeanFrame::new(ctx));
    while position < limit && position < safe {
        // SAFETY: at least ROW_PAD readable bytes follow `position`, and
        // where the assembly runs at least 16 come before it.
        let (length, status) = unsafe {
            let p = buffer.as_ptr().add(position);
            if KERNEL == KERNEL_GENERAL {
                event_row_fast::<MODE, INDEXED>(ctx, p, out, &mut row)
            } else {
                match frame.as_mut() {
                    Some(frame) if position >= 16 => event_row_lean_asm::<MODE>(ctx, frame, p, out, &mut row),
                    _ => event_row_lean::<MODE, INDEXED>(ctx, p, out, &mut row),
                }
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

/// The fast loop over click rows, specialized for what happens to a parsed
/// row (`simd::MODE_STORE`: the kernel stores it; `simd::MODE_ROW`: it comes
/// back and is checked against the general parser first), for index mode
/// (`INDEXED`: a generated user ID is read as its user number, and
/// `ctx.keys` must be set) and for one of the click kernels
/// (`simd::KERNEL_*`). Returns where the general parser must continue.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn clicks_rows_loop<const MODE: u32, const INDEXED: bool, const KERNEL: u32>(
    ctx: &Ctx,
    buffer: &[u8],
    mut position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{
        click_row_fast, click_row_lean, ClickFrame, KERNEL_GENERAL, MODE_ROW, ROW_DONE, ROW_DROP, ROW_EMIT, ROW_LONG,
        ROW_PAD,
    };
    let limit = end.min(buffer.len());
    let safe = buffer.len().saturating_sub(ROW_PAD);
    // The lean kernel's view of the window: a local, so that the kernel's
    // loads from it are known not to change under the loop's stores.
    let frame = ClickFrame::new(ctx);
    let mut click = ClickRecord::default();
    while position < limit && position < safe {
        // SAFETY: at least ROW_PAD readable bytes follow `position`.
        let (length, status) = unsafe {
            let p = buffer.as_ptr().add(position);
            if KERNEL == KERNEL_GENERAL {
                click_row_fast::<INDEXED>(ctx, p, &mut click)
            } else {
                click_row_lean::<MODE, INDEXED>(ctx, &frame, p, out, &mut click)
            }
        };
        if status == ROW_DONE {
            position += length + 1;
            continue;
        }
        if status == ROW_EMIT && MODE != MODE_ROW {
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
        if MODE == MODE_ROW && (status == ROW_EMIT || status == ROW_DROP) {
            // In index mode a kernel must hand back exactly the indexed
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

/// `clicks_rows_loop` with the click kernel of this run.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn clicks_fast_loop<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    buffer: &[u8],
    position: usize,
    end: usize,
    out: &mut ParseOut,
) -> usize {
    use super::simd::{click_kernel, KERNEL_GENERAL, KERNEL_LEAN};
    match click_kernel() {
        KERNEL_GENERAL => clicks_rows_loop::<MODE, INDEXED, KERNEL_GENERAL>(ctx, buffer, position, end, out),
        _ => clicks_rows_loop::<MODE, INDEXED, KERNEL_LEAN>(ctx, buffer, position, end, out),
    }
}

pub fn parse_clicks_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    #[allow(unused_mut)]
    let mut position = first_line_start(buffer, start);
    #[cfg(target_arch = "x86_64")]
    if ctx.fast_rows {
        use super::simd::{MODE_ROW, MODE_STORE};
        position = match (ctx.keys.is_some(), ctx.verify) {
            (true, true) => clicks_fast_loop::<MODE_ROW, true>(ctx, buffer, position, end, out),
            (true, false) => clicks_fast_loop::<MODE_STORE, true>(ctx, buffer, position, end, out),
            (false, true) => clicks_fast_loop::<MODE_ROW, false>(ctx, buffer, position, end, out),
            (false, false) => clicks_fast_loop::<MODE_STORE, false>(ctx, buffer, position, end, out),
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

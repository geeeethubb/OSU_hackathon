//! Parsing of event, click and link lines into compact records.
//!
//! The `*_slow` functions are the exact, general parsers: they follow
//! rules.md field by field. The AVX2 fast paths in `simd.rs` must produce the
//! same rows for the lines they accept.

use super::arena::{Arena, PartList, PartWriter};
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
    /// Per event partition, records of events whose ID is long and
    /// hexadecimal (`codes::LONG_EVENT`): the ID's first byte and value, then
    /// the record's two words (and the purchase word).
    pub long_events: Vec<PartWriter>,
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
    /// The long click IDs this worker kept (`Interner::per_thread`).
    pub click_ids: Vec<(u8, u128)>,
    /// The worker's event, long-event and click lists, sealed by the worker
    /// itself once it has parsed its last chunk (`seal`).
    pub sealed: Option<Sealed>,
    /// Stored records of valid rows with a long user ID whose user code
    /// waits for its user-table slot (`defer_user`); `pending_count` of them,
    /// the oldest at `pending_head` once the ring is full (`pending_depth`
    /// entries, a power of two). Empty at the end of every chunk.
    pub pending: [Pending; PENDING_MOST],
    pub pending_head: usize,
    pub pending_count: usize,
    pub pending_depth: usize,
}

/// The most entries the ring of pending user codes can have.
pub const PENDING_MOST: usize = 32;

/// How many stored records a worker's lookups of long user IDs trail its
/// parse by: `SS_PENDING` (1, 2, 4, 8, 16 or 32; default 16).
pub fn pending_depth() -> usize {
    static DEPTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DEPTH.get_or_init(|| {
        let depth = std::env::var("SS_PENDING").ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(16);
        depth.clamp(1, PENDING_MOST).next_power_of_two()
    })
}

/// A record already stored whose user code is not in it yet: the address of
/// the word that takes the code (as 0 until then), the code's shift in it,
/// and the user ID, one byte (`first`) and 32 hexadecimal digits (`value`).
#[derive(Clone, Copy, Default)]
pub struct Pending {
    pub value: u128,
    pub word: usize,
    pub first: u8,
    pub shift: u8,
}

/// Puts a stored record's pending user code into the worker's ring; the
/// oldest entry, once the ring is full, gets its user's code, by when the
/// user-table slot asked for (`Interner::prefetch_hex`) has arrived. The
/// record's words are only read after the parse, and every chunk settles
/// its ring before it ends (`settle_pending`).
#[inline(always)]
pub fn defer_user(ctx: &Ctx, out: &mut ParseOut, word: *mut u64, shift: u32, first: u8, value: u128) {
    let at = out.pending_head;
    if out.pending_count == out.pending_depth {
        let oldest = out.pending[at];
        settle(ctx, &oldest);
    } else {
        out.pending_count += 1;
    }
    out.pending[at] = Pending { value, word: word as usize, first, shift: shift as u8 };
    out.pending_head = (at + 1) & (out.pending_depth - 1);
}

/// Puts the user's code into the record's word.
#[inline(always)]
fn settle(ctx: &Ctx, pending: &Pending) {
    let code = ctx.user_ids.hex_code(pending.first, pending.value);
    // SAFETY: the word is in a record this worker stored in its arena,
    // which nothing reads before the parse ends.
    unsafe {
        *(pending.word as *mut u64) |= code << pending.shift;
    }
}

/// Settles every entry still in the worker's ring.
pub fn settle_pending(ctx: &Ctx, out: &mut ParseOut) {
    let depth = out.pending_depth;
    let count = out.pending_count;
    let first = (out.pending_head + depth - count) & (depth - 1);
    for offset in 0..count {
        let pending = out.pending[(first + offset) & (depth - 1)];
        settle(ctx, &pending);
    }
    out.pending_count = 0;
    out.pending_head = 0;
}

/// A parse worker's lists of events by event partition, of long events by
/// the same partitions, and of clicks, sealed.
pub struct Sealed {
    pub events: Vec<PartList>,
    pub long_events: Vec<PartList>,
    pub clicks: PartList,
}

impl ParseOut {
    /// Seals the event, long-event and click lists: on the worker's own
    /// thread, beside the other workers, rather than all workers' lists one
    /// after another on one thread once the parse has ended (three per
    /// partition and worker: some 12,000 at 4 threads, each a block header
    /// that is no longer in any cache). The writers are not used after it.
    pub fn seal(&mut self) {
        self.sealed = Some(Sealed {
            events: self.events.iter_mut().map(|writer| writer.finish(super::EVENT_BLOCK)).collect(),
            long_events: self.long_events.iter_mut().map(|writer| writer.finish(super::EVENT_BLOCK)).collect(),
            clicks: self.clicks.finish(RAW_CLICK_BLOCK),
        });
    }
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
            long_events: vec![PartWriter::EMPTY; ctx.event_parts()],
            indexed: vec![PartWriter::EMPTY; ctx.keys.map_or(0, |keys| index_parts(&keys))],
            odd_event_users: Vec::new(),
            clicks: PartWriter::EMPTY,
            user_clicks: vec![PartWriter::EMPTY; ctx.keys.map_or(0, |keys| click_parts(&keys))],
            keys: ctx.keys,
            index_shift: ctx.keys.map_or(0, |keys| keys.index_shift),
            links: Vec::new(),
            click_ids: Vec::new(),
            sealed: None,
            pending: [Pending::default(); PENDING_MOST],
            pending_head: 0,
            pending_count: 0,
            pending_depth: pending_depth(),
        }
    }
}

/// User indexes per click list of index mode, as a shift.
pub const CLICK_SHIFT: u32 = 9;
pub const CLICK_USERS: usize = 1 << CLICK_SHIFT;
/// Words per block of those lists: a list holds a few hundred clicks.
pub const USER_CLICK_BLOCK: usize = 128;

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
pub fn emit_code_click(out: &mut ParseOut, user: u64, code_and_campaign: u64, ts: u64) -> *mut u64 {
    let at = out.clicks.reserve(&mut out.arena, RAW_CLICK_BLOCK, RAW_CLICK_WORDS);
    // SAFETY: `reserve` returned room for three words.
    unsafe {
        *at = user;
        *at.add(1) = code_and_campaign;
        *at.add(2) = ts;
    }
    // The user word.
    at
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
        None => {
            emit_code_click(out, click.user, click.code_and_campaign, u64::from(click.ts));
        }
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

/// `emit_raw` of a short event ID's record whose user code comes later
/// (`defer_user`): stored with user 0; returns the word that takes the
/// code, at bit 22.
#[inline(always)]
pub fn emit_raw_pending(out: &mut ParseOut, event: u64, ts: u64, ingest: u64, purchase: Option<(u32, u32)>) -> *mut u64 {
    debug_assert!(event != super::codes::LONG_EVENT);
    let mixed = mix(event);
    let part = (mixed >> out.event_shift) as usize;
    let word0 = (mixed << 32) | (ts << 1) | 1;
    let word1 = (ingest << 1) | u64::from(purchase.is_some());
    // SAFETY: as in `emit_raw`.
    unsafe {
        let writer = out.events.get_unchecked_mut(part);
        let at = writer.reserve(&mut out.arena, EVENT_BLOCK, 2 + usize::from(purchase.is_some()));
        *at = word0;
        *at.add(1) = word1;
        if let Some((price, category)) = purchase {
            *at.add(2) = u64::from(price) | (u64::from(category) << 32);
        }
        at.add(1)
    }
}

/// `emit_long_value` of a record whose user code comes later (`defer_user`):
/// stored with user 0; returns the word that takes the code, at bit 22.
#[inline(always)]
pub fn emit_long_value_pending(
    out: &mut ParseOut,
    first: u8,
    value: u128,
    ts: u64,
    ingest: u64,
    purchase: Option<(u32, u32)>,
) -> *mut u64 {
    let part = long_part(first, value, out.event_shift);
    let word0 = (u64::from(first) << 56) | (ts << 1) | 1;
    let word1 = (ingest << 1) | u64::from(purchase.is_some());
    // SAFETY: as in `emit_long_value`.
    unsafe {
        let writer = out.long_events.get_unchecked_mut(part);
        let at = writer.reserve(&mut out.arena, EVENT_BLOCK, 4 + usize::from(purchase.is_some()));
        *at = value as u64;
        *at.add(1) = (value >> 64) as u64;
        *at.add(2) = word0;
        *at.add(3) = word1;
        if let Some((price, category)) = purchase {
            *at.add(4) = u64::from(price) | (u64::from(category) << 32);
        }
        at.add(3)
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
    if event == super::codes::LONG_EVENT {
        emit_long(out, ts, valid, user, ingest, purchase);
        return;
    }
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

/// The event partition of a long hexadecimal event ID: the same number of
/// partitions as the coded IDs', picked by the value, so that every copy of
/// one event lands in one partition.
#[inline]
pub fn long_part(first: u8, value: u128, event_shift: u32) -> usize {
    let folded = (value as u64) ^ ((value >> 64) as u64).rotate_left(29) ^ u64::from(first);
    let hashed = folded.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (hashed >> (64 - (super::codes::CODE_BITS - event_shift))) as usize
}

/// Stores the record of an event whose ID is long and hexadecimal: its first
/// byte and value (from `codes::take_long_value`), then the usual words with
/// no key in them.
#[cold]
#[inline(never)]
fn emit_long(out: &mut ParseOut, ts: u64, valid: u64, user: u64, ingest: u64, purchase: Option<(u32, u32)>) {
    let (first, value) = super::codes::take_long_value();
    emit_long_value(out, first, value, ts, valid, user, ingest, purchase);
}

/// Stores the record of an event whose ID is one byte (`first`) and 32
/// hexadecimal digits (`value`): the value in two words, then the usual
/// words with the first byte on top of the first one.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn emit_long_value(
    out: &mut ParseOut,
    first: u8,
    value: u128,
    ts: u64,
    valid: u64,
    user: u64,
    ingest: u64,
    purchase: Option<(u32, u32)>,
) {
    let part = long_part(first, value, out.event_shift);
    let word0 = (u64::from(first) << 56) | (ts << 1) | valid;
    let word1 = (user << 22) | (ingest << 1) | u64::from(purchase.is_some());
    // SAFETY: `long_part` is below the number of event partitions, which
    // sized `long_events`; `reserve` returns room for the words written.
    unsafe {
        let writer = out.long_events.get_unchecked_mut(part);
        let at = writer.reserve(&mut out.arena, EVENT_BLOCK, 4 + usize::from(purchase.is_some()));
        *at = value as u64;
        *at.add(1) = (value >> 64) as u64;
        *at.add(2) = word0;
        *at.add(3) = word1;
        if let Some((price, category)) = purchase {
            *at.add(4) = u64::from(price) | (u64::from(category) << 32);
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
        Some((price, ctx.category_of(product_id)))
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
    // The kernel in assembly: in index mode, and on the general path with
    // IDs coded by their text (`SS_KERNEL=plain` keeps the compiled one).
    let assembly = kernel == KERNEL_LEAN;
    // Its loop with the store inside, where the writers are as that takes them.
    let run = INDEXED && assembly && MODE == MODE_STORE && LeanFrame::stores(ctx);
    let run_codes = !INDEXED && assembly && MODE == MODE_STORE && LeanFrame::stores_codes(ctx);
    match kernel {
        KERNEL_GENERAL => events_rows_loop::<MODE, INDEXED, KERNEL_GENERAL>(ctx, buffer, position, end, out),
        // Checked rows go one by one through the loop that compares them.
        _ if MODE == MODE_ROW && assembly => {
            events_rows_loop::<MODE, INDEXED, KERNEL_LEAN>(ctx, buffer, position, end, out)
        }
        _ if run_codes && ctx.diag == 0 => events_assembly_run_codes(ctx, buffer, position, end, out),
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
        _ if INDEXED && assembly => events_assembly_loop::<MODE>(ctx, buffer, position, end, out),
        _ => events_lean_loop::<MODE, INDEXED>(ctx, buffer, position, end, out),
    }
}

/// The kernel in assembly on the general path, rows stored: the loop over the
/// rows and the store (into the event partitions, as `emit_raw` stores) are
/// in the assembly, which comes back here only for a row that needs
/// something else: a list that needs a new block, a purchase whose product
/// or price it leaves to `event_lean_purchase`, or a row it declines (one
/// with a long ID, among others), which goes through the compiled kernels.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn events_assembly_run_codes(ctx: &Ctx, buffer: &[u8], mut position: usize, end: usize, out: &mut ParseOut) -> usize {
    use super::simd::{
        event_lean_common, event_lean_purchase, event_lean_run_codes, lean_setup, LeanFrame, MODE_STORE, ROW_PAD,
    };
    let stop = end.min(buffer.len()).min(buffer.len().saturating_sub(ROW_PAD));
    let (lean, _) = lean_setup(ctx);
    let mut row = EventRow::default();
    // The assembly reads up to 16 bytes before a row.
    while position < stop.min(16) {
        position = event_row_plain::<MODE_STORE, false>(ctx, buffer, position, out, &mut row);
    }
    if position < stop {
        let mut frame = LeanFrame::codes(ctx);
        let frame = &mut frame as *mut LeanFrame;
        let base = buffer.as_ptr();
        // SAFETY (all of this): a row that starts before `stop` has ROW_PAD
        // readable bytes after it and, here, 16 before it; the tables and the
        // frame are those of `ctx`, whose writers `LeanFrame::stores_codes`
        // found fit (the caller asked), taken anew for every call.
        let mut p = unsafe { base.add(position) };
        let last = unsafe { base.add(stop) };
        while p < last {
            let (status, at) = unsafe { event_lean_run_codes(lean, p, last, frame, out.events.as_mut_ptr()) };
            p = match status {
                0 => at,
                3 => unsafe {
                    // The row's list needs a new block: `emit_raw` gets it one.
                    event_lean_common::<MODE_STORE, false>((*frame).read(status), out, &mut row);
                    (*frame).next
                },
                2 => unsafe {
                    // A purchase, read up to its product and price.
                    event_lean_purchase::<MODE_STORE, false>(ctx, &*frame, (*frame).read(status), out, &mut row);
                    (*frame).next
                },
                _ => {
                    // Declined: the compiled kernels, from the long-ID one on.
                    let position = unsafe { at.offset_from(base) } as usize;
                    unsafe { base.add(event_row_declined::<MODE_STORE, false>(ctx, buffer, position, out)) }
                }
            };
        }
        position = unsafe { p.offset_from(base) } as usize;
    }
    std::hint::black_box(row.event);
    position
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
                    event_lean_common::<MODE_STORE, true>((*frame).read(status), out, &mut row);
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
                    event_lean_common::<MODE, true>(read, out, &mut row);
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
    if status == 2 || status == 4 {
        let read = super::simd::LeanRow { status, ts, ingest_seconds, index, user_number, keep };
        // SAFETY: the frame is the one the assembly just filled for this
        // row, which ends right before `next`, inside the buffer.
        unsafe {
            if status == 2 {
                super::simd::event_lean_purchase::<MODE, true>(ctx, &*frame, read, out, row);
            } else {
                super::simd::event_lean_bought::<MODE, true>(&*frame, read, out, row);
            }
            return (*frame).next.offset_from(buffer.as_ptr()) as usize;
        }
    }
    // A declined row. The compiled lean kernel takes the same rows as the
    // assembly except where the row's end was not where the assembly looked
    // first (730 of 319,915 declined rows on 20M seed 2), so it would read
    // nearly every one of them again only to decline it too: such rows go
    // straight to the general kernel, which takes every row the lean kernel
    // takes, with the same values. `SS_RARE=0` or `=a` keeps the old route.
    if super::simd::rare_rows() & 2 != 0 {
        return event_row_declined::<MODE, true>(ctx, buffer, position, out);
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
    use super::simd::{
        event_lean_long, event_lean_wide, event_row_fast, wide_rows, MODE_ROW, MODE_STORE, ROW_DECLINE, ROW_DONE,
        ROW_DROP, ROW_LONG,
    };
    let mut row = EventRow::default();
    super::simd::prefetch_input(buffer.as_ptr().wrapping_add(position), super::simd::input_lead());
    if !INDEXED && MODE != MODE_ROW {
        // SAFETY: at least ROW_PAD readable bytes follow `position`.
        let (length, status) = unsafe { event_lean_long::<MODE>(ctx, buffer.as_ptr().add(position), out, &mut row) };
        if status != ROW_DECLINE {
            std::hint::black_box(row.event);
            return position + length + 1;
        }
        if wide_rows() {
            // SAFETY: as above.
            let (length, status) = unsafe { event_lean_wide::<MODE>(ctx, buffer.as_ptr().add(position), out, &mut row) };
            if status != ROW_DECLINE {
                std::hint::black_box(row.event);
                return position + length + 1;
            }
        }
    }
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
    // The kernel in assembly, where it is asked for: in index mode, and on
    // the general path with IDs coded by their text.
    let mut frame = (KERNEL == KERNEL_LEAN).then(|| if INDEXED { LeanFrame::new(ctx) } else { LeanFrame::codes(ctx) });
    while position < limit && position < safe {
        // SAFETY: at least ROW_PAD readable bytes follow `position`, and
        // where the assembly runs at least 16 come before it.
        let (length, status) = unsafe {
            let p = buffer.as_ptr().add(position);
            if KERNEL == KERNEL_GENERAL {
                event_row_fast::<MODE, INDEXED>(ctx, p, out, &mut row)
            } else {
                match frame.as_mut() {
                    Some(frame) if position >= 16 => event_row_lean_asm::<MODE, INDEXED>(ctx, frame, p, out, &mut row),
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
            // A long event ID is LONG_EVENT in both rows: its value is left
            // aside by each parser in turn, and must be the same too.
            let fast_long = super::codes::take_long_value();
            let slow = event_row_slow(ctx, line);
            assert!(
                slow == parsed,
                "event row mismatch on {:?}: fast {:?}, slow {:?}",
                String::from_utf8_lossy(line),
                parsed,
                slow
            );
            if parsed.as_ref().is_some_and(|row| row.event == super::codes::LONG_EVENT) {
                assert_eq!(
                    super::codes::take_long_value(),
                    fast_long,
                    "long event ID mismatch on {:?}",
                    String::from_utf8_lossy(line)
                );
            }
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
    settle_pending(ctx, out);
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
        click_row_fast, click_row_lean, click_row_shapes, ClickFrame, KERNEL_GENERAL, KERNEL_LEAN, MODE_ROW, ROW_DONE,
        ROW_DROP, ROW_EMIT, ROW_LONG, ROW_PAD,
    };
    let limit = end.min(buffer.len());
    let safe = buffer.len().saturating_sub(ROW_PAD);
    // The lean kernels' view of the window: a local, so that a kernel's
    // loads from it are known not to change under the loop's stores.
    let frame = ClickFrame::new(ctx);
    let mut click = ClickRecord::default();
    let lead = super::simd::input_lead();
    while position < limit && position < safe {
        super::simd::prefetch_input(buffer.as_ptr().wrapping_add(position), lead);
        // SAFETY: at least ROW_PAD readable bytes follow `position`.
        let (length, status) = unsafe {
            let p = buffer.as_ptr().add(position);
            match KERNEL {
                KERNEL_GENERAL => click_row_fast::<INDEXED>(ctx, p, &mut click),
                KERNEL_LEAN => click_row_lean::<MODE, INDEXED>(ctx, &frame, p, out, &mut click),
                _ => click_row_shapes::<MODE, INDEXED>(ctx, &frame, p, out, &mut click),
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

/// `clicks_rows_loop` with one of the click kernels (`simd::KERNEL_*`).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn clicks_fast_loop<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    buffer: &[u8],
    position: usize,
    end: usize,
    out: &mut ParseOut,
    kernel: u32,
) -> usize {
    use super::simd::{KERNEL_GENERAL, KERNEL_LEAN, KERNEL_SHAPES};
    match kernel {
        KERNEL_GENERAL => clicks_rows_loop::<MODE, INDEXED, KERNEL_GENERAL>(ctx, buffer, position, end, out),
        KERNEL_LEAN => clicks_rows_loop::<MODE, INDEXED, KERNEL_LEAN>(ctx, buffer, position, end, out),
        _ => clicks_rows_loop::<MODE, INDEXED, KERNEL_SHAPES>(ctx, buffer, position, end, out),
    }
}

pub fn parse_clicks_chunk(ctx: &Ctx, buffer: &[u8], start: usize, end: usize, out: &mut ParseOut) {
    #[cfg(target_arch = "x86_64")]
    let kernel = super::simd::click_kernel();
    #[cfg(not(target_arch = "x86_64"))]
    let kernel = 0;
    parse_clicks_chunk_with(ctx, buffer, start, end, out, kernel);
}

/// `parse_clicks_chunk` with the click kernel named (`simd::KERNEL_*`), so
/// that one process can run both.
#[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables))]
pub fn parse_clicks_chunk_with(
    ctx: &Ctx,
    buffer: &[u8],
    start: usize,
    end: usize,
    out: &mut ParseOut,
    kernel: u32,
) {
    #[allow(unused_mut)]
    let mut position = first_line_start(buffer, start);
    #[cfg(target_arch = "x86_64")]
    if ctx.fast_rows {
        use super::simd::{MODE_ROW, MODE_STORE};
        position = match (ctx.keys.is_some(), ctx.verify) {
            (true, true) => clicks_fast_loop::<MODE_ROW, true>(ctx, buffer, position, end, out, kernel),
            (true, false) => clicks_fast_loop::<MODE_STORE, true>(ctx, buffer, position, end, out, kernel),
            (false, true) => clicks_fast_loop::<MODE_ROW, false>(ctx, buffer, position, end, out, kernel),
            (false, false) => clicks_fast_loop::<MODE_STORE, false>(ctx, buffer, position, end, out, kernel),
        };
    }
    lines_from(buffer, position, end, |line| {
        if let Some(click) = click_row_slow(ctx, line) {
            emit_click(out, &click);
        }
    });
    settle_pending(ctx, out);
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
            if status == ROW_LONG && super::simd::link_long_path() {
                // SAFETY: at least ROW_PAD readable bytes follow `position`.
                let (length, status) =
                    unsafe { super::simd::link_lean_long(ctx, buffer.as_ptr().add(position), &mut pair) };
                if status == ROW_EMIT {
                    if verify {
                        let line = &buffer[position..position + length];
                        let slow = link_row_slow(ctx, line);
                        assert!(
                            slow == Some(pair),
                            "long link row mismatch on {:?}: fast {:?}, slow {:?}",
                            String::from_utf8_lossy(line),
                            pair,
                            slow
                        );
                    }
                    out.links.push(pair);
                    position += length + 1;
                    continue;
                }
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

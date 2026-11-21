//! Index mode: events in index order, most persons sessionized where they lie.
//!
//! The generator hands out event indexes user by user, in time order, and
//! all IDs of a user share a user index (see `keys`). When the data shows
//! both, a partition of consecutive indexes holds whole users one after the
//! other: putting each record in the slot of its index picks the winning copy
//! of every event without hashing, and reading the slots in order gives each
//! user's events already grouped (a "run").
//!
//! A person whose linked IDs all have one user index (a simple link
//! component, or an ID in no link) has all its events in that user's run and
//! all its clicks under that user index in the click index, so it goes
//! straight to the session kernel. Everything else takes the general path
//! through the person partitions: IDs of complex components, IDs that are not
//! generated ones, and events whose own ID is not a generated one.
//!
//! What this relies on is checked, not assumed:
//! - "each user's events are one run" holds when user indexes never decrease
//!   along the index order. `Runner` checks that inside a partition and
//!   `chains` across partitions; a violation makes the engine start over on
//!   the general path.
//! - Time order inside a run is not relied on: the kernel orders the events.
//! - A run that reaches a partition's edge may continue in the next one, so
//!   first and last runs are set aside and joined afterwards (`chains`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use super::arena::PartList;
use super::codes::{unmix, CODE_MASK};
use super::dedup::{forward, forward_click, forward_named, PassBOut};
use super::keys::Keys;
use super::links::{LinkTable, SIMPLE, SLOW};
use super::parse::CLICK_USERS;
use super::sessions::{PassCOut, Person, ScratchC, Sessions};

const INGEST_MASK: u64 = 0x1F_FFFF;
const TS_MASK: u64 = 0x7FFF_FFFF;
/// IDs per user index.
const ALIASES: u64 = 512;
/// Set in a resolved name when the ID belongs to a complex component.
const COMPLEX: u64 = 1 << 63;

/// One winning event: the two slot words (see `parse::emit_indexed`) and the
/// purchase word.
pub type Record = [u64; 3];

/// Slot word 0, from the bottom: the record's valid bit, its timestamp, the
/// bit that says its user is a user number, and the tag of the partition
/// that placed it.
const VALID: u64 = 1;
const NUMBERED: u64 = 1 << 32;
const TAG_SHIFT: u32 = 33;
const TAG_MAX: u64 = (1 << (64 - TAG_SHIFT)) - 1;
/// The record's own bits of that word.
const RECORD_BITS: u64 = (1 << TAG_SHIFT) - 1;
/// What decides whether a slot holds a valid event of a generated user ID
/// that the partition at hand placed: its tag and the two flags.
const EVENT_BITS: u64 = !RECORD_BITS | NUMBERED | VALID;

/// Where a partition's records are placed by index.
pub struct ScratchI {
    /// The winning record per index: its two words, with the partition's
    /// tag where word 0 had the index.
    slots: Vec<[u64; 2]>,
    /// The purchase word of the winning record, for purchases.
    purchases: Vec<u64>,
    /// The tag of the partition being read. A slot holds a record of this
    /// partition exactly when it carries this tag: tags count up from 1, so
    /// whatever an earlier partition left in a slot is never taken for a
    /// record, and nothing is cleared between partitions.
    tag: u64,
    /// How the lists are read (`PLACE_PLAIN` and the others).
    mode: u8,
}

impl ScratchI {
    /// For partitions of `1 << index_shift` indexes.
    pub fn new(index_shift: u32) -> Self {
        let slots = 1usize << index_shift;
        let mode = std::env::var("SS_PLACE").ok().and_then(|value| value.parse().ok()).unwrap_or(PLACE_PLAIN);
        Self { slots: vec![[0; 2]; slots], purchases: vec![0; slots], tag: 0, mode }
    }

    /// Word 0 of a slot that holds a valid event of a generated user ID of
    /// the partition being read, apart from its timestamp.
    #[inline(always)]
    fn event(&self) -> u64 {
        self.tag << TAG_SHIFT | NUMBERED | VALID
    }
}

/// One record into the slot of its index, if it is the first copy the
/// partition has there or one with a smaller ingest_ts. Returns whether it
/// was written.
#[inline(always)]
fn put(slots: &mut [[u64; 2]], purchases: &mut [u64], tag: u64, word0: u64, word1: u64, purchase: u64) -> bool {
    let slot = (word0 >> 48) as usize & (slots.len() - 1);
    let held = slots[slot];
    // Step 2 of the rules: the copy with the smallest ingest_ts wins (the
    // first seen among equal ones, which are identical).
    let first = held[0] >> TAG_SHIFT != tag;
    let wins = first || ((word1 >> 1) & INGEST_MASK) < ((held[1] >> 1) & INGEST_MASK);
    if wins {
        slots[slot] = [(word0 & RECORD_BITS) | tag << TAG_SHIFT, word1];
        // Only a purchase writes its word: every reader looks at
        // `purchases[slot]` only when the slot's winner has the purchase
        // bit, and that winner wrote it here. What an earlier winner or
        // partition left is never read.
        if word1 & 1 == 1 {
            purchases[slot] = purchase;
        }
    }
    wins
}

/// The records of one block into their slots. Returns whether any was written.
#[inline(always)]
fn place_block(block: &[u64], slots: &mut [[u64; 2]], purchases: &mut [u64], tag: u64) -> bool {
    let mut any = false;
    let mut at = 0;
    while at < block.len() {
        // Blocks hold whole records: two words, plus one for a purchase.
        let (word0, word1) = (block[at], block[at + 1]);
        let has_purchase = (word1 & 1) as usize;
        let purchase = if has_purchase == 1 { block[at + 2] } else { 0 };
        at += 2 + has_purchase;
        any |= put(slots, purchases, tag, word0, word1, purchase);
    }
    any
}

/// `place_block` with the slots asked for a window of records ahead of the
/// writes: the slot table does not stay in the nearest cache while two
/// threads share a core.
#[inline(always)]
fn place_block_ahead(block: &[u64], slots: &mut [[u64; 2]], purchases: &mut [u64], tag: u64) -> bool {
    const WINDOW: usize = 32;
    let mut starts = [0usize; WINDOW];
    let mut any = false;
    let mut at = 0;
    while at < block.len() {
        let mut count = 0;
        while count < WINDOW && at < block.len() {
            let slot = (block[at] >> 48) as usize & (slots.len() - 1);
            #[cfg(target_arch = "x86_64")]
            {
                use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
                // SAFETY: a prefetch reads nothing; the address is inside the table.
                unsafe { _mm_prefetch::<_MM_HINT_T0>(slots.as_ptr().add(slot) as *const i8) };
            }
            let _ = slot;
            starts[count] = at;
            count += 1;
            at += 2 + (block[at + 1] & 1) as usize;
        }
        for &start in &starts[..count] {
            let (word0, word1) = (block[start], block[start + 1]);
            let purchase = if word1 & 1 == 1 { block[start + 2] } else { 0 };
            any |= put(slots, purchases, tag, word0, word1, purchase);
        }
    }
    any
}

/// Asks for every cache line of a block: the lists were written long ago by
/// other threads, and a block is too short for the hardware prefetcher to
/// get ahead of the reader.
#[inline(always)]
fn prefetch(block: &[u64]) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        let mut at = 0;
        while at < block.len() {
            // SAFETY: a prefetch reads nothing; the address is inside the block.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(block.as_ptr().add(at) as *const i8) };
            at += 8;
        }
        if let Some(last) = block.last() {
            // SAFETY: as above.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(last as *const u64 as *const i8) };
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = block;
}

/// How `place` reads the lists (`SS_PLACE`, for A/B runs; the slot table
/// is the same whichever it is).
pub const PLACE_PLAIN: u8 = 0;
/// The next block of a list is asked for while the current one is read.
pub const PLACE_NEXT_BLOCK: u8 = 1;
/// The lists are read a block at a time in turn, each list's next block
/// asked for a whole round before it is read.
pub const PLACE_IN_TURN: u8 = 2;
/// `PLACE_NEXT_BLOCK`, and the slots are asked for a window of records
/// before they are written.
pub const PLACE_SLOTS_AHEAD: u8 = 3;

/// Places a partition's records by index. Returns whether there were any.
fn place(lists: &[PartList], scratch: &mut ScratchI) -> bool {
    match scratch.mode {
        PLACE_NEXT_BLOCK => place_as::<PLACE_NEXT_BLOCK>(lists, scratch),
        PLACE_IN_TURN => place_as::<PLACE_IN_TURN>(lists, scratch),
        PLACE_SLOTS_AHEAD => place_as::<PLACE_SLOTS_AHEAD>(lists, scratch),
        _ => place_as::<PLACE_PLAIN>(lists, scratch),
    }
}

/// `place`, reading the lists the way `MODE` says. The order in which the
/// records are read does not matter: copies of one event with equal
/// ingest_ts are identical.
fn place_as<const MODE: u8>(lists: &[PartList], scratch: &mut ScratchI) -> bool {
    if scratch.tag == TAG_MAX {
        // Out of tags (2^31 partitions on one thread): start over.
        scratch.slots.fill([0; 2]);
        scratch.tag = 0;
    }
    scratch.tag += 1;
    let tag = scratch.tag;
    let (slots, purchases) = (&mut scratch.slots[..], &mut scratch.purchases[..]);
    let mut any = false;
    if MODE == PLACE_IN_TURN {
        let mut readers: Vec<_> = lists.iter().map(PartList::blocks).collect();
        let mut ahead: Vec<Option<&[u64]>> = readers.iter_mut().map(Iterator::next).collect();
        for block in ahead.iter().flatten() {
            prefetch(block);
        }
        let mut left = true;
        while left {
            left = false;
            for (reader, next) in readers.iter_mut().zip(ahead.iter_mut()) {
                let Some(block) = next.take() else {
                    continue;
                };
                *next = reader.next();
                if let Some(following) = *next {
                    prefetch(following);
                    left = true;
                }
                any |= place_block(block, slots, purchases, tag);
            }
        }
        return any;
    }
    for list in lists {
        if MODE == PLACE_PLAIN {
            for block in list.blocks() {
                any |= place_block(block, slots, purchases, tag);
            }
            continue;
        }
        let mut blocks = list.blocks();
        let mut following = blocks.next();
        while let Some(block) = following {
            // The block after this one arrives while this one is read.
            following = blocks.next();
            if let Some(next) = following {
                prefetch(next);
            }
            any |= if MODE == PLACE_SLOTS_AHEAD {
                place_block_ahead(block, slots, purchases, tag)
            } else {
                place_block(block, slots, purchases, tag)
            };
        }
    }
    any
}

/// The valid clicks of generated user IDs, grouped by user index.
pub struct ClickIndex {
    /// `ends[user]` is one past the user's last click in `clicks`; its first
    /// is at `ends[user - 1]` (0 for user 0).
    ends: Vec<u32>,
    /// `[ts since seven days before the window (32 low) | alias (9 bits
    /// above), click code (42 low) | campaign]`.
    clicks: Vec<[u64; 2]>,
}

impl ClickIndex {
    #[inline]
    pub fn of(&self, user: u64) -> &[[u64; 2]] {
        let user = user as usize;
        if user >= self.ends.len() {
            return &[];
        }
        let start = if user == 0 { 0 } else { self.ends[user - 1] as usize };
        &self.clicks[start..self.ends[user] as usize]
    }

    pub fn len(&self) -> usize {
        self.clicks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clicks.is_empty()
    }
}

/// Builds a `ClickIndex` from the per-user-range click lists of index mode
/// (`parse::emit_click`): every range is counted and laid out on its own, in
/// parallel, so the writes of one range stay within a few kilobytes.
pub struct ClickIndexBuilder {
    /// Where each range's clicks begin; one more entry closes the last range.
    bases: Vec<u32>,
    ends: Vec<u32>,
    clicks: Vec<[u64; 2]>,
}

/// The two arrays while several threads fill disjoint parts of them.
#[derive(Clone, Copy)]
pub struct ClickTarget {
    ends: *mut u32,
    clicks: *mut [u64; 2],
}

// SAFETY: a range's entries are written by the one thread that fills that
// range, and nothing reads the arrays until all have finished.
unsafe impl Send for ClickTarget {}
unsafe impl Sync for ClickTarget {}

impl ClickIndexBuilder {
    /// How many clicks a range holds: `lists` are its click lists, one per thread.
    pub fn count(lists: impl Iterator<Item = PartList>) -> u32 {
        lists.map(|list| list.words() / 2).sum::<usize>() as u32
    }

    /// `counts[range]` is the number of clicks of each range (`count`).
    /// Ranges after the last one that has clicks are left out.
    pub fn new(counts: &[u32]) -> (Self, ClickTarget) {
        let ranges = counts.len();
        let mut bases = Vec::with_capacity(ranges + 1);
        let (mut total, mut used) = (0usize, 0usize);
        for (range, &clicks) in counts.iter().enumerate() {
            bases.push(total as u32);
            if clicks > 0 {
                used = range + 1;
            }
            total += clicks as usize;
        }
        assert!(total <= u32::MAX as usize);
        bases.truncate(used);
        bases.push(total as u32);
        let mut ends: Vec<u32> = Vec::with_capacity(used * CLICK_USERS);
        let mut clicks: Vec<[u64; 2]> = Vec::with_capacity(total);
        // SAFETY: `fill` writes every entry of every used range exactly once
        // before `finish` hands the vectors out; neither type has invalid values.
        #[allow(clippy::uninit_vec)]
        unsafe {
            ends.set_len(used * CLICK_USERS);
            clicks.set_len(total);
        }
        let target = ClickTarget { ends: ends.as_mut_ptr(), clicks: clicks.as_mut_ptr() };
        (Self { bases, ends, clicks }, target)
    }

    /// The ranges `fill` must be called for, each once.
    pub fn ranges(&self) -> usize {
        self.bases.len() - 1
    }

    /// Lays out one range: `lists` are its click lists, one per thread.
    pub fn fill(&self, range: usize, lists: &[PartList], target: ClickTarget) {
        let mut counts = [0u32; CLICK_USERS];
        for list in lists {
            for block in list.blocks() {
                for click in block.chunks_exact(2) {
                    counts[((click[0] >> 41) as usize) & (CLICK_USERS - 1)] += 1;
                }
            }
        }
        let mut cursors = [0u32; CLICK_USERS];
        let mut running = self.bases[range];
        for user in 0..CLICK_USERS {
            cursors[user] = running;
            running += counts[user];
            // SAFETY: `range` is below `ranges()`, so the entry exists; this
            // call is the only writer of this range's entries.
            unsafe { *target.ends.add(range * CLICK_USERS + user) = running };
        }
        debug_assert_eq!(running, self.bases[range + 1]);
        for list in lists {
            for block in list.blocks() {
                for click in block.chunks_exact(2) {
                    let user = ((click[0] >> 41) as usize) & (CLICK_USERS - 1);
                    // SAFETY: the same clicks were counted just above, so the
                    // cursor stays inside this range's part of `clicks`.
                    unsafe { *target.clicks.add(cursors[user] as usize) = [click[0] & ((1 << 41) - 1), click[1]] };
                    cursors[user] += 1;
                }
            }
        }
    }

    pub fn finish(self) -> ClickIndex {
        ClickIndex { ends: self.ends, clicks: self.clicks }
    }
}

/// Winning events whose own ID is not a generated one (they went through the
/// general dedup pass) but whose person is simple: they join their user's run.
pub struct Extras {
    by_user: HashMap<u64, (AtomicBool, Vec<Record>)>,
    /// One bit per `user % FILTER_BITS` that has extras: almost every run
    /// asks about a user without any, and this answers without hashing.
    filter: Vec<u64>,
}

const FILTER_BITS: u64 = 1 << 14;

impl Default for Extras {
    fn default() -> Self {
        Self { by_user: HashMap::new(), filter: vec![0; (FILTER_BITS / 64) as usize] }
    }
}

impl Extras {
    /// Sorts the general dedup pass's survivors: `lists[thread][partition]`
    /// are its person lists. Events of simple persons with a generated name
    /// go to their user's extras; everything else is written to `out` again.
    pub fn collect(keys: &Keys, links: &LinkTable, lists: &[Vec<PartList>], out: &mut PassBOut) -> Self {
        let mut extras = Self::default();
        for per_thread in lists {
            for (part, list) in per_thread.iter().enumerate() {
                for block in list.blocks() {
                    let mut at = 0;
                    while at < block.len() {
                        // One word per event, plus one for a purchase.
                        let key = block[at];
                        let is_purchase = key & 1;
                        let purchase = if is_purchase == 1 { block[at + 1] } else { 0 };
                        at += 1 + is_purchase as usize;
                        let name = unmix(((part as u64) << out.person_shift) | (key >> 32));
                        let word0 = (1 << 32) | (key & (TS_MASK << 1)) | 1;
                        match (links.lookup(name), keys.user_number_of_code(name)) {
                            ((_, false), Some(number)) => {
                                let user = number >> 9;
                                extras.filter[((user % FILTER_BITS) / 64) as usize] |= 1 << (user % 64);
                                let entry = extras.by_user.entry(user).or_default();
                                entry.1.push([word0, (number << 22) | is_purchase, purchase]);
                            }
                            _ => forward_named(out, name, word0, is_purchase, purchase),
                        }
                    }
                }
            }
        }
        // In time order: merged into its user's run that way, an extra does
        // not put the run's events out of order.
        for (_, records) in extras.by_user.values_mut() {
            records.sort_by_key(|record| (record[0] >> 1) & TS_MASK);
        }
        extras
    }

    pub fn len(&self) -> usize {
        self.by_user.len()
    }

    /// The extras of `user`, if it has any.
    #[inline(always)]
    fn of(&self, user: u64) -> Option<&(AtomicBool, Vec<Record>)> {
        if self.filter[((user % FILTER_BITS) / 64) as usize] & (1 << (user % 64)) == 0 {
            return None;
        }
        self.by_user.get(&user)
    }

    pub fn is_empty(&self) -> bool {
        self.by_user.is_empty()
    }
}

/// What every thread reads while runs are processed.
pub struct Shared<'a> {
    pub keys: &'a Keys,
    pub links: &'a LinkTable,
    pub clicks: &'a ClickIndex,
    pub extras: &'a Extras,
    pub click_exotic: &'a [Box<[u8]>],
    /// Index partitions and worker threads, to size the output columns.
    pub parts: usize,
    pub threads: usize,
}

/// The clicks of complex components' generated IDs, sent to their persons'
/// partitions: the general path needs them, and no run visits a user
/// because of its clicks.
pub fn forward_complex_clicks(keys: &Keys, links: &LinkTable, clicks: &ClickIndex, out: &mut PassBOut) {
    for &code in links.complex_members() {
        let Some(number) = keys.user_number_of_code(code) else {
            continue;
        };
        let name = links.person(code);
        let alias = number & (ALIASES - 1);
        for click in clicks.of(number >> 9) {
            if (click[0] >> 32) == alias {
                forward_click(out, name, click[0] & 0xFFFF_FFFF, click[1]);
            }
        }
    }
}

/// The first and last run of one partition, which may continue next door.
pub struct Edge {
    part: usize,
    first_user: u64,
    first: Vec<Record>,
    /// `u64::MAX` when the partition has a single run (it is `first`).
    last_user: u64,
    last: Vec<Record>,
}

/// Joins the edge runs of neighbouring partitions into whole runs, in index
/// order, without copying a record. `None` when user indexes decrease
/// somewhere: the data is not laid out the way index mode needs.
pub fn chains(mut edges: Vec<Edge>) -> Option<Vec<Chain>> {
    edges.sort_unstable_by_key(|edge| edge.part);
    let mut whole: Vec<Chain> = Vec::new();
    let mut open: Option<Chain> = None;
    for edge in edges {
        match &mut open {
            Some((user, pieces)) if *user == edge.first_user => pieces.push(edge.first),
            _ => {
                if let Some(previous) = open.take() {
                    if edge.first_user < previous.0 {
                        return None;
                    }
                    whole.push(previous);
                }
                open = Some((edge.first_user, vec![edge.first]));
            }
        }
        if edge.last_user != u64::MAX {
            // The runner checked the order of the runs inside a partition:
            // this is a later user.
            whole.extend(open.take());
            open = Some((edge.last_user, vec![edge.last]));
        }
    }
    whole.extend(open);
    Some(whole)
}

/// One user's run that touched partition edges: its records, in the pieces
/// the partitions set aside (index order).
pub type Chain = (u64, Vec<Vec<Record>>);

/// The main person of a run read straight from the slots, as the session
/// kernel has it: in its batch, or on its own.
#[derive(Clone, Copy)]
enum Open {
    Batch(Person),
    Alone(Sessions),
}

/// One thread's state for the index pass.
pub struct Runner {
    place: ScratchI,
    kernel: ScratchC,
    /// Whether the main persons of direct runs go to the session kernel in
    /// batches. `SS_BATCH=0` does them one at a time instead and
    /// `SS_BATCH=<n>` sets how many persons a batch holds, for A/B runs: the
    /// same rows either way.
    batched: bool,
    /// Per alias: the run that resolved it last, and what it resolved to
    /// (the person's name code, with `COMPLEX` for a complex component).
    aliases: Vec<[u64; 2]>,
    run: u64,
    records: Vec<Record>,
    /// The person being collected: event timestamps and purchases.
    ts: Vec<u32>,
    purchases: Vec<[u64; 2]>,
    clicks: Vec<[u64; 2]>,
    /// A run read straight from the slots: the aliases of its main person,
    /// and that person's name code (0: the name is `main_number`'s ID).
    main_mask: u64,
    main_name: u64,
    main_number: u64,
    /// Events of a run's further persons. On the direct path
    /// `[user number, ts << 1 | is purchase, purchase word]`, in
    /// `process_run` the same with the name code first.
    others: Vec<Record>,
    pub edges: Vec<Edge>,
    /// User indexes decreased inside a partition.
    pub violated: bool,
    /// Counters for `SS_TIMING`: inline persons, their events, events sent to the general path.
    pub inline_persons: u64,
    pub inline_events: u64,
    pub general_events: u64,
    /// With `measure`: nanoseconds spent placing records and inside the session kernel.
    pub measure: bool,
    pub place_nanos: u64,
    pub kernel_nanos: u64,
    pub total_nanos: u64,
}

impl Runner {
    pub fn new(mut kernel: ScratchC, index_shift: u32) -> Self {
        let batch = std::env::var("SS_BATCH").ok().and_then(|value| value.parse::<usize>().ok());
        if let Some(persons) = batch.filter(|&persons| persons > 0) {
            kernel.set_batch_persons(persons);
        }
        Self {
            place: ScratchI::new(index_shift),
            kernel,
            batched: batch != Some(0),
            aliases: vec![[0; 2]; ALIASES as usize],
            run: 0,
            records: Vec::new(),
            ts: Vec::new(),
            purchases: Vec::new(),
            clicks: Vec::new(),
            main_mask: 0,
            main_name: 0,
            main_number: 0,
            others: Vec::new(),
            edges: Vec::new(),
            violated: false,
            inline_persons: 0,
            inline_events: 0,
            general_events: 0,
            measure: false,
            place_nanos: 0,
            kernel_nanos: 0,
            total_nanos: 0,
        }
    }

    /// The winners of one index partition. Runs in the middle are processed:
    /// straight from the slots when the user's summary allows it, through
    /// `process_run` otherwise. The first and the last run are set aside as
    /// the partition's `Edge`, since they may continue next door.
    pub fn partition(
        &mut self,
        shared: &Shared,
        part: usize,
        lists: &[PartList],
        b_out: &mut PassBOut,
        c_out: &mut PassCOut,
    ) {
        let started = self.measure.then(std::time::Instant::now);
        let any = place(lists, &mut self.place);
        if let Some(started) = started {
            self.place_nanos += started.elapsed().as_nanos() as u64;
        }
        if !any {
            return;
        }
        if self.batched {
            self.read::<true>(shared, part, b_out, c_out);
        } else {
            self.read::<false>(shared, part, b_out, c_out);
        }
        if let Some(started) = started {
            self.total_nanos += started.elapsed().as_nanos() as u64;
        }
    }

    /// The runs of the partition whose records `place` has laid out: the
    /// main persons of direct runs join the session kernel's batch
    /// (`BATCHED`) or are done one at a time.
    fn read<const BATCHED: bool>(
        &mut self,
        shared: &Shared,
        part: usize,
        b_out: &mut PassBOut,
        c_out: &mut PassCOut,
    ) {
        let mut current = u64::MAX;
        let mut edge = Edge { part, first_user: u64::MAX, first: Vec::new(), last_user: u64::MAX, last: Vec::new() };
        let mut records = std::mem::take(&mut self.records);
        records.clear();
        // Whether the current run is collected as records (the first run, and
        // users the summary cannot describe) or read straight from the slots.
        let mut gathering = true;
        let mut run_start = 0usize;
        // A run read straight from the slots, once its main person has an
        // event: that person's aliases (0 before that, and in every other
        // kind of run) and the person as the session kernel has it, in its
        // batch (`person`) or on its own (`alone`).
        let mut main = 0u64;
        let mut person = Person::default();
        let mut alone = Sessions::default();
        let event = self.place.event();
        let slot_count = self.place.slots.len();
        let mut slot = 0usize;
        while slot < slot_count {
            if main != 0 {
                // The main person's next events, for as long as nothing else
                // comes: its sessions grow as the slots are read, and its
                // timestamps are never collected.
                let (slots, bought) = (&self.place.slots[..], &self.place.purchases[..]);
                // `number >> 6` of this user's IDs with an alias below 64.
                let user_bits = current << 3;
                // Timestamp and purchase bit of a slot that holds a valid
                // event of a generated user ID that this partition placed,
                // under one of the main person's aliases.
                let event_at = |slot: usize| {
                    let [word0, word1] = slots[slot];
                    // The timestamp if the slot holds such an event; a value
                    // above every timestamp if it holds anything else.
                    let ts = (word0 ^ event).rotate_right(1);
                    let number = word1 >> 22;
                    let ours = ts <= TS_MASK && number >> 6 == user_bits && (main >> (number & 63)) & 1 != 0;
                    ours.then_some((ts, word1 & 1))
                };
                if BATCHED {
                    let sessions = self.kernel.batch_scratch().0;
                    while slot < slots.len() {
                        // A purchase is taken below, one slot at a time: the
                        // loop then has nothing to call.
                        let Some((ts, 0)) = event_at(slot) else {
                            break;
                        };
                        if !person.add(sessions, ts as u32) {
                            break;
                        }
                        slot += 1;
                    }
                } else {
                    let (start, count) = self.kernel.session_scratch();
                    while slot < slots.len() {
                        let Some((ts, is_purchase)) = event_at(slot) else {
                            break;
                        };
                        if !alone.add(start, count, ts as u32) {
                            break;
                        }
                        if is_purchase == 1 {
                            self.purchases.push([ts, bought[slot]]);
                        }
                        slot += 1;
                    }
                }
                if slot == slot_count {
                    break;
                }
            }
            // Everything else, one slot at a time.
            let this = slot;
            slot += 1;
            let [word0, word1] = self.place.slots[this];
            // Nothing placed here by this partition, or a winner that is not
            // valid: the winner's own valid bit decides, and an invalid
            // winner drops the event.
            if word0 >> TAG_SHIFT != self.place.tag || word0 & VALID == 0 {
                continue;
            }
            // The record's own word 0 again.
            let word0 = word0 & RECORD_BITS;
            let is_purchase = word1 & 1;
            let purchase = if is_purchase == 1 { self.place.purchases[this] } else { 0 };
            if (word0 >> 32) & 1 == 0 {
                // Not a generated user ID: `word1` holds its code.
                forward(shared.links, b_out, word0, word1, purchase);
                self.general_events += 1;
                continue;
            }
            let user = word1 >> 31;
            if user != current {
                if current != u64::MAX {
                    self.violated |= user < current;
                    if edge.first_user == u64::MAX {
                        edge.first_user = current;
                        edge.first = std::mem::take(&mut records);
                    } else if gathering {
                        self.process_run(shared, b_out, c_out, current, &[&records]);
                        records.clear();
                    } else {
                        let open = if BATCHED { Open::Batch(person) } else { Open::Alone(alone) };
                        self.finish_direct(shared, c_out, current, main, open);
                    }
                    gathering = !self.begin_direct(shared, user);
                    main = 0;
                }
                current = user;
                run_start = this;
            }
            if gathering {
                records.push([word0, word1, purchase]);
                continue;
            }
            let number = word1 >> 22;
            let alias = number & (ALIASES - 1);
            let ts = (word0 >> 1) & TS_MASK;
            if alias < 64 && self.main_mask == 0 {
                // Without a simple component the first ID met becomes the main person.
                self.main_mask = 1 << alias;
                self.main_name = 0;
                self.main_number = number;
            }
            if alias >= 64 || (self.main_mask >> alias) & 1 == 0 {
                // An ID in no link component: a person of its own.
                self.others.push([number, (ts << 1) | is_purchase, purchase]);
                continue;
            }
            let in_order = if main == 0 {
                if BATCHED {
                    if self.kernel.batch_is_full() {
                        self.finish_batch(shared, c_out);
                    }
                    person = self.kernel.begin_person(c_out, shared.click_exotic, ts as u32);
                } else {
                    alone = Sessions::begin(self.kernel.session_scratch().0, ts as u32);
                }
                main = self.main_mask;
                true
            } else if BATCHED {
                person.add(self.kernel.batch_scratch().0, ts as u32)
            } else {
                let (start, count) = self.kernel.session_scratch();
                alone.add(start, count, ts as u32)
            };
            if !in_order {
                // The run is not in time order, so its main person cannot be
                // done this way. Nothing of the run has left this runner or
                // the batch: it is read again as records, and goes through
                // `process_run` like the runs the summary cannot describe.
                if BATCHED {
                    self.kernel.abort_person(person);
                }
                self.regather(run_start, this, &mut records);
                records.push([word0, word1, purchase]);
                gathering = true;
                main = 0;
                continue;
            }
            if is_purchase == 1 {
                if BATCHED {
                    person.purchase(self.kernel.batch_scratch().1, purchase);
                } else {
                    self.purchases.push([ts, purchase]);
                }
            }
        }
        if current != u64::MAX {
            if edge.first_user == u64::MAX {
                edge.first_user = current;
                edge.first = std::mem::take(&mut records);
            } else {
                if !gathering {
                    // The last run was read straight from the slots, and
                    // nothing of it has left this runner or the batch: read
                    // it again as records.
                    if BATCHED && main != 0 {
                        self.kernel.abort_person(person);
                    }
                    self.regather(run_start, slot_count, &mut records);
                }
                edge.last_user = current;
                edge.last = std::mem::take(&mut records);
            }
            self.edges.push(edge);
        }
        self.records = records;
        self.finish_batch(shared, c_out);
        self.kernel.flush(c_out);
        if !c_out.names.is_empty() {
            // A thread handles about `parts / threads` partitions like this one.
            self.kernel.reserve_rows(c_out, c_out.names.len() * shared.parts / shared.threads * 3 / 2);
        }
    }

    /// Starts a run that is read straight from the slots, if the user's
    /// summary describes its IDs and nothing else joins the run.
    #[inline(always)]
    fn begin_direct(&mut self, shared: &Shared, user: u64) -> bool {
        // What the links say about this user's IDs, in one read.
        let [head, mask] = shared.links.summary(user);
        if head & SLOW != 0 || shared.extras.of(user).is_some() {
            return false;
        }
        self.ts.clear();
        self.purchases.clear();
        self.others.clear();
        // Without a simple component the first ID met becomes the main person.
        self.main_mask = if head & SIMPLE != 0 { mask } else { 0 };
        self.main_name = head & CODE_MASK;
        true
    }

    /// The events of the run that began at slot `from`, up to slot `to`,
    /// as records: a run that was being read straight from the slots and
    /// has to go through `process_run` after all. Every valid event of a
    /// generated user ID in that range belongs to the run. What the run had
    /// collected so far is dropped.
    #[cold]
    #[inline(never)]
    fn regather(&mut self, from: usize, to: usize, records: &mut Vec<Record>) {
        self.ts.clear();
        self.purchases.clear();
        self.others.clear();
        let event = self.place.event();
        for slot in from..to {
            let [word0, word1] = self.place.slots[slot];
            if word0 & EVENT_BITS == event {
                let purchase = if word1 & 1 == 1 { self.place.purchases[slot] } else { 0 };
                records.push([word0 & RECORD_BITS, word1, purchase]);
            }
        }
    }

    /// Rows and cells of the persons in the session kernel's batch.
    fn finish_batch(&mut self, shared: &Shared, c_out: &mut PassCOut) {
        let started = self.measure.then(std::time::Instant::now);
        self.kernel.finish_batch(c_out, shared.click_exotic);
        if let Some(started) = started {
            self.kernel_nanos += started.elapsed().as_nanos() as u64;
        }
    }

    /// Ends a run read straight from the slots: its main person (`main` are
    /// its aliases, 0 if it has no event, and `open` is what the session
    /// kernel has of it), then the persons of its other IDs.
    fn finish_direct(&mut self, shared: &Shared, c_out: &mut PassCOut, user: u64, main: u64, open: Open) {
        if main != 0 {
            let name =
                if self.main_name != 0 { self.main_name } else { shared.keys.code_of_user_number(self.main_number) };
            self.inline_persons += 1;
            let clicks = shared.clicks.of(user).iter().filter_map(|click| {
                let alias = click[0] >> 32;
                (alias < 64 && (main >> alias) & 1 != 0).then_some([click[0] & 0xFFFF_FFFF, click[1]])
            });
            match open {
                Open::Batch(person) => {
                    self.inline_events += person.events();
                    self.kernel.end_person(person, name, clicks);
                }
                Open::Alone(sessions) => {
                    self.inline_events += u64::from(sessions.events());
                    self.clicks.clear();
                    self.clicks.extend(clicks);
                    let started = self.measure.then(std::time::Instant::now);
                    let (purchases, clicks) = (&self.purchases, &self.clicks);
                    self.kernel.finish_sessions(sessions, c_out, shared.click_exotic, name, purchases, clicks);
                    if let Some(started) = started {
                        self.kernel_nanos += started.elapsed().as_nanos() as u64;
                    }
                }
            }
        }
        if !self.others.is_empty() {
            // Rare: IDs of one user that no link joins are separate persons.
            // The sort keeps each one's events in the order they came.
            let mut others = std::mem::take(&mut self.others);
            others.sort_by_key(|other| other[0]);
            let mut from = 0;
            while from < others.len() {
                let number = others[from][0];
                let to = from + others[from..].iter().take_while(|other| other[0] == number).count();
                self.ts.clear();
                self.purchases.clear();
                for &[_, stamp, purchase] in &others[from..to] {
                    self.ts.push((stamp >> 1) as u32);
                    if stamp & 1 == 1 {
                        self.purchases.push([stamp >> 1, purchase]);
                    }
                }
                self.clicks.clear();
                for click in shared.clicks.of(user) {
                    if (click[0] >> 32) == number & (ALIASES - 1) {
                        self.clicks.push([click[0] & 0xFFFF_FFFF, click[1]]);
                    }
                }
                self.run_kernel(shared, c_out, shared.keys.code_of_user_number(number));
                from = to;
            }
            self.others = others;
        }
    }

    /// Adds the assists still pending after runs processed outside `partition`.
    pub fn flush(&mut self, c_out: &mut PassCOut) {
        self.kernel.flush(c_out);
    }

    /// Hands the session kernel back, for the general pass that follows.
    pub fn into_kernel(self) -> ScratchC {
        self.kernel
    }

    /// One user's events (`word1 >> 31` is the same user index in all of
    /// them), given in pieces and in any order, each ID resolved through the
    /// link table: each simple person in it goes to the session kernel,
    /// events of complex components go to their persons' partitions.
    ///
    /// The user's extras are merged in by time. In generated data the
    /// pieces are in time order, so every person's events then reach the
    /// kernel in order and it does not have to order them; a whale with a
    /// few odd-ID events used to cost a second pass over all its events.
    pub fn process_run(
        &mut self,
        shared: &Shared,
        b_out: &mut PassBOut,
        c_out: &mut PassCOut,
        user: u64,
        pieces: &[&[Record]],
    ) {
        // The user's events whose own ID is not a generated one join the run.
        let extra: &[Record] = match shared.extras.of(user) {
            Some((used, more)) => {
                used.store(true, Ordering::Relaxed);
                more
            }
            None => &[],
        };
        self.run += 1;
        self.ts.clear();
        self.purchases.clear();
        self.others.clear();
        // The first simple person met; codes are never 0.
        let mut first = 0u64;
        // The extras not merged in yet; they are in time order.
        let mut waiting = extra;
        for piece in pieces {
            for &record in *piece {
                let ts = (record[0] >> 1) & TS_MASK;
                while let Some((&early, rest)) = waiting.split_first() {
                    if (early[0] >> 1) & TS_MASK >= ts {
                        break;
                    }
                    self.sort_out(shared, b_out, &mut first, early);
                    waiting = rest;
                }
                self.sort_out(shared, b_out, &mut first, record);
            }
        }
        for &late in waiting {
            self.sort_out(shared, b_out, &mut first, late);
        }
        if first != 0 {
            self.person(shared, c_out, user, first);
            if !self.others.is_empty() {
                // Rare: IDs of one user that no link joins are separate
                // persons. The sort keeps each one's events in the order
                // they came, which is time order when the run's was.
                let mut others = std::mem::take(&mut self.others);
                others.sort_by_key(|other| other[0]);
                let mut from = 0;
                while from < others.len() {
                    let name = others[from][0];
                    let to = from + others[from..].iter().take_while(|other| other[0] == name).count();
                    self.ts.clear();
                    self.purchases.clear();
                    for &[_, stamp, purchase] in &others[from..to] {
                        self.ts.push((stamp >> 1) as u32);
                        if stamp & 1 == 1 {
                            self.purchases.push([stamp >> 1, purchase]);
                        }
                    }
                    self.person(shared, c_out, user, name);
                    from = to;
                }
                self.others = others;
            }
        }
    }

    /// One event of the run `process_run` is reading: on to its person's
    /// partition if its ID belongs to a complex component, to the events of
    /// the first simple person met (`first`), or to the others'.
    #[inline(always)]
    fn sort_out(&mut self, shared: &Shared, b_out: &mut PassBOut, first: &mut u64, record: Record) {
        let [word0, word1, purchase] = record;
        let name = resolve(&mut self.aliases, self.run, shared, word1 >> 22);
        let ts = (word0 >> 1) & TS_MASK;
        let is_purchase = word1 & 1;
        if name & COMPLEX != 0 {
            forward_named(b_out, name & CODE_MASK, word0, is_purchase, purchase);
            self.general_events += 1;
            return;
        }
        if *first == 0 {
            *first = name;
        }
        if name == *first {
            self.ts.push(ts as u32);
            if is_purchase == 1 {
                self.purchases.push([ts, purchase]);
            }
        } else {
            self.others.push([name, (ts << 1) | is_purchase, purchase]);
        }
    }

    /// The simple person `name` of `user`, whose events are in `self.ts` and
    /// `self.purchases`: adds the clicks whose own ID resolves to the same
    /// name and runs the session kernel.
    fn person(&mut self, shared: &Shared, c_out: &mut PassCOut, user: u64, name: u64) {
        self.clicks.clear();
        for click in shared.clicks.of(user) {
            let number = (user << 9) | (click[0] >> 32);
            if resolve(&mut self.aliases, self.run, shared, number) == name {
                self.clicks.push([click[0] & 0xFFFF_FFFF, click[1]]);
            }
        }
        self.run_kernel(shared, c_out, name);
    }

    /// The session kernel on the person collected in `ts`, `purchases` and
    /// `clicks`. Events read in index order are in time order in generated
    /// data, which the kernel uses when it holds and does not need: it
    /// orders the events itself otherwise.
    #[inline(always)]
    fn run_kernel(&mut self, shared: &Shared, c_out: &mut PassCOut, name: u64) {
        let count = self.ts.len();
        if count == 0 {
            return;
        }
        self.inline_persons += 1;
        self.inline_events += count as u64;
        let started = self.measure.then(std::time::Instant::now);
        self.kernel.one_person_in_order(c_out, shared.click_exotic, name, &self.ts, &self.purchases, &self.clicks);
        if let Some(started) = started {
            self.kernel_nanos += started.elapsed().as_nanos() as u64;
        }
    }
}

/// What the generated user ID `number` resolves to in run `run`: its
/// person's name code, with `COMPLEX` when the link component is complex.
/// One table probe per ID and run.
#[inline(always)]
fn resolve(aliases: &mut [[u64; 2]], run: u64, shared: &Shared, number: u64) -> u64 {
    let entry = &mut aliases[(number & (ALIASES - 1)) as usize];
    if entry[0] != run {
        let (name, complex) = shared.links.lookup(shared.keys.code_of_user_number(number));
        *entry = [run, name | if complex { COMPLEX } else { 0 }];
    }
    entry[1]
}

/// The users that have extras but no run of their own.
pub fn leftover_extras(shared: &Shared, runner: &mut Runner, b_out: &mut PassBOut, c_out: &mut PassCOut) {
    for (&user, (used, _)) in &shared.extras.by_user {
        if !used.load(Ordering::Relaxed) {
            runner.process_run(shared, b_out, c_out, user, &[]);
        }
    }
    runner.flush(c_out);
}

/// Diagnostic (`SS_STATS=1`): how well the data fits index mode's assumptions.
pub fn stats(keys: &Keys, links: &LinkTable, indexed_lists: &[Vec<PartList>], parts: usize) {
    let mut scratch = ScratchI::new(keys.index_shift);
    let mut lists: Vec<PartList> = Vec::new();
    let (mut occupied, mut valid, mut numbered, mut other_user) = (0u64, 0u64, 0u64, 0u64);
    let (mut user_back, mut runs, mut ts_back_runs) = (0u64, 0u64, 0u64);
    let (mut complex_events, mut multi_name_runs) = (0u64, 0u64);
    let mut previous_user = u64::MAX;
    let mut previous_ts = 0u64;
    let mut run_ts_back = false;
    let mut names: Vec<u64> = Vec::new();
    for part in 0..parts {
        lists.clear();
        lists.extend(indexed_lists.iter().map(|per_thread| per_thread[part]));
        if !place(&lists, &mut scratch) {
            continue;
        }
        for &[word0, word1] in &scratch.slots {
            if word0 >> TAG_SHIFT != scratch.tag {
                continue;
            }
            occupied += 1;
            if word0 & 1 == 0 {
                continue;
            }
            valid += 1;
            if (word0 >> 32) & 1 == 0 {
                other_user += 1;
                continue;
            }
            numbered += 1;
            let user = word1 >> 31;
            let ts = (word0 >> 1) & TS_MASK;
            if user != previous_user {
                runs += 1;
                ts_back_runs += u64::from(run_ts_back);
                multi_name_runs += u64::from(names.len() > 1);
                run_ts_back = false;
                names.clear();
                user_back += u64::from(previous_user != u64::MAX && user < previous_user);
                previous_user = user;
                previous_ts = 0;
            }
            run_ts_back |= ts < previous_ts;
            previous_ts = ts;
            let (name, complex) = links.lookup(keys.code_of_user_number(word1 >> 22));
            complex_events += u64::from(complex);
            if !complex && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    eprintln!(
        "SS_STATS occupied={occupied} valid={valid} numbered={numbered} other_user={other_user} runs={runs} \
         user_back={user_back} ts_back_runs={ts_back_runs} complex_events={complex_events} multi_name_runs={multi_name_runs}"
    );
}

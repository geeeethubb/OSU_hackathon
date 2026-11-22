//! Pass B, one event-ID partition at a time: pick each event's winning copy,
//! drop invalid winners, map the user to a person and write the survivor to
//! that person's partition. Clicks are mapped to their persons here too.
//!
//! The partition's records are read in place, twice: once to find the
//! losing copies, once to forward the survivors.

use super::arena::{Arena, PartList, PartWriter};
use super::codes::mix;
use super::links::LinkTable;
use super::parse::RAW_CLICK_WORDS;
use super::{Ctx, CLICK_BLOCK, PERSON_BLOCK};

const TS_MASK: u64 = 0x7FFF_FFFF;
const INGEST_MASK: u64 = 0x1F_FFFF;
/// How many records ahead of its lookup a person-table slot is prefetched.
/// The table is far bigger than the cache, so an unprefetched lookup stalls.
const AHEAD: usize = 16;

/// What one worker writes in pass B.
pub struct PassBOut {
    pub arena: Arena,
    /// Per person partition: word = local person key (32) | ts (31) | is purchase (1),
    /// followed for purchases by price (32 low) | category (32 high).
    pub events: Vec<PartWriter>,
    /// Per person partition, two words per click:
    /// local person key (32) | click ts since window start - 7 days (31) | 0,
    /// then click code (42 low) | campaign (22 high).
    pub clicks: Vec<PartWriter>,
    /// `mixed code >> person_shift` is a person's partition.
    pub person_shift: u32,
    /// Words per block of the event and click lists. Index mode sends few
    /// records this way, spread over all partitions: small blocks then.
    pub event_block: usize,
    pub click_block: usize,
    /// The person lists of events and of clicks, sealed by the worker itself
    /// at the end of pass B (`seal`); left to the caller in index mode.
    pub sealed: Option<(Vec<PartList>, Vec<PartList>)>,
}

impl PassBOut {
    pub fn new(ctx: &Ctx) -> Self {
        let sparse = ctx.keys.is_some();
        Self {
            arena: Arena::new(),
            events: vec![PartWriter::EMPTY; ctx.person_parts()],
            clicks: vec![PartWriter::EMPTY; ctx.person_parts()],
            person_shift: ctx.person_shift(),
            event_block: if sparse { 32 } else { PERSON_BLOCK },
            click_block: if sparse { 32 } else { CLICK_BLOCK },
            sealed: None,
        }
    }

    /// Seals the person lists on the worker's own thread, beside the other
    /// workers (as `ParseOut::seal`). The writers are not used after it.
    pub fn seal(&mut self) {
        let (event_block, click_block) = (self.event_block, self.click_block);
        self.sealed = Some((
            self.events.iter_mut().map(|writer| writer.finish(event_block)).collect(),
            self.clicks.iter_mut().map(|writer| writer.finish(click_block)).collect(),
        ));
    }
}

/// One slot of the per-partition winner table.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Slot {
    /// Sequence number of the winning copy plus one; 0 marks an empty slot.
    tag: u32,
    /// The event's local key.
    key: u32,
    /// The winning copy's ingest time.
    ingest: u32,
}

#[derive(Default)]
pub struct ScratchB {
    table: Vec<Slot>,
    /// One bit per record, in iteration order: set for losing copies.
    lost: Vec<u64>,
    /// The same two for the long event IDs of a partition.
    long_table: Vec<LongSlot>,
    long_lost: Vec<u64>,
    /// A partition's survivors (or a batch's clicks) with their user's hash,
    /// as read and then by bucket of that hash (`sorted_lookups`), and the
    /// buckets' counts and starts.
    unsorted: Vec<[u64; 4]>,
    sorted: Vec<[u64; 4]>,
    buckets: Vec<u32>,
    /// Diagnostics (`SS_TIMING`): nanoseconds this worker spent choosing
    /// winners and forwarding survivors.
    pub measure: bool,
    pub winners_nanos: u64,
    pub forward_nanos: u64,
}

/// Calls `visit(word0, word1, purchase_word)` for every record of the lists.
#[inline(always)]
fn for_each_record(lists: &[PartList], mut visit: impl FnMut(u64, u64, u64)) {
    for list in lists {
        for block in list.blocks() {
            let mut at = 0;
            while at < block.len() {
                // SAFETY: blocks hold whole records: two words, plus one when
                // the purchase bit of the second word is set.
                let (word0, word1) = unsafe { (*block.get_unchecked(at), *block.get_unchecked(at + 1)) };
                let has_purchase = (word1 & 1) as usize;
                let purchase = if has_purchase == 1 { unsafe { *block.get_unchecked(at + 2) } } else { 0 };
                at += 2 + has_purchase;
                visit(word0, word1, purchase);
            }
        }
    }
}

/// Dedup one event-ID partition and forward the survivors: the records of
/// coded IDs (`lists`) and those of long hexadecimal IDs (`long_lists`),
/// which are told apart by their value. No ID is in both.
pub fn dedup_partition(
    links: &LinkTable,
    lists: &[PartList],
    long_lists: &[PartList],
    out: &mut PassBOut,
    scratch: &mut ScratchB,
) {
    dedup_coded(links, lists, out, scratch);
    if long_lists.iter().any(|list| list.words() != 0) {
        dedup_long(links, long_lists, out, scratch);
    }
}

/// Calls `visit(first, value, word0, word1, purchase_word)` for every record
/// of long event IDs: value low and high words, the two record words, and
/// the purchase word when the purchase bit of the second is set. `first` is
/// the ID's first byte, kept in the top byte of word0.
#[inline(always)]
fn for_each_long_record(lists: &[PartList], mut visit: impl FnMut(u8, u128, u64, u64, u64)) {
    for list in lists {
        for block in list.blocks() {
            let mut at = 0;
            while at < block.len() {
                // SAFETY: blocks hold whole records: four words, plus one
                // when the purchase bit of the fourth is set.
                let (low, high, word0, word1) = unsafe {
                    (*block.get_unchecked(at), *block.get_unchecked(at + 1), *block.get_unchecked(at + 2), *block.get_unchecked(at + 3))
                };
                let has_purchase = (word1 & 1) as usize;
                let purchase = if has_purchase == 1 { unsafe { *block.get_unchecked(at + 4) } } else { 0 };
                at += 4 + has_purchase;
                visit((word0 >> 56) as u8, u128::from(high) << 64 | u128::from(low), word0 & !(0xFF << 56), word1, purchase);
            }
        }
    }
}

/// One slot of the per-partition table of long event IDs: the ID's value in
/// two words, then its first byte (bits 56..64), the winning copy's ingest
/// time (bits 32..53) and that copy's sequence number plus one (bits 0..32;
/// a whole word of 0 marks an empty slot).
#[derive(Clone, Copy, Default)]
struct LongSlot {
    low: u64,
    high: u64,
    meta: u64,
}

/// Step 2 of the rules for the long event IDs of one partition: per ID the
/// copy with the smallest ingest_ts (the first seen among equal ones, which
/// are identical), then the survivors forwarded like the others.
fn dedup_long(links: &LinkTable, lists: &[PartList], out: &mut PassBOut, scratch: &mut ScratchB) {
    let words: usize = lists.iter().map(PartList::words).sum();
    let most_records = words / 4;
    let bits = (most_records.max(8) * 3 / 2).next_power_of_two().trailing_zeros();
    let capacity = 1usize << bits;
    let table = &mut scratch.long_table;
    table.clear();
    table.resize(capacity, LongSlot::default());
    let lost = &mut scratch.long_lost;
    lost.clear();
    lost.resize(most_records / 64 + 1, 0);
    let started = scratch.measure.then(std::time::Instant::now);
    let mut sequence = 0u32;
    for_each_long_record(lists, |first, value, _, word1, _| {
        let ingest = (word1 >> 1) & INGEST_MASK;
        let (low, high) = (value as u64, (value >> 64) as u64);
        // `parse::long_part` picked this partition with the top bits of one
        // multiply of these words: every ID here has the same ones. The slot
        // comes from a different multiply (the same one would crowd the
        // partition's IDs into a few slots, and probing would go quadratic).
        let folded = low ^ high.rotate_left(29) ^ u64::from(first);
        let mut at = ((folded ^ (folded >> 29)).wrapping_mul(0xD6E8_FEB8_6659_FD93) >> (64 - bits)) as usize;
        let named = u64::from(first) << 56;
        let meta = named | (ingest << 32) | u64::from(sequence + 1);
        loop {
            // SAFETY: `at` is masked to the table's capacity.
            let slot = unsafe { table.get_unchecked_mut(at) };
            if slot.meta == 0 {
                *slot = LongSlot { low, high, meta };
                break;
            }
            if slot.low == low && slot.high == high && slot.meta >> 56 == u64::from(first) {
                let loser = if ingest < (slot.meta >> 32) & INGEST_MASK {
                    let previous = (slot.meta as u32) - 1;
                    slot.meta = meta;
                    previous
                } else {
                    sequence
                };
                lost[(loser >> 6) as usize] |= 1u64 << (loser & 63);
                break;
            }
            at = (at + 1) & (capacity - 1);
        }
        sequence += 1;
    });
    let between = scratch.measure.then(std::time::Instant::now);
    let lost = &scratch.long_lost;
    // As in `dedup_coded`: survivors wait in a small ring while their
    // person-table slot loads.
    let mut ring = [[0u64; 5]; AHEAD];
    let mut waiting = 0usize;
    let mut slot = 0usize;
    let mut sequence = 0u32;
    for_each_long_record(lists, |_, _, word0, word1, purchase| {
        let is_lost = (lost[(sequence >> 6) as usize] >> (sequence & 63)) & 1;
        sequence += 1;
        if (word0 & 1) & !is_lost == 0 {
            return;
        }
        let [hash, at] = links.probe(word1 >> 22);
        if waiting == AHEAD {
            forward_ring(links, out, ring[slot]);
        } else {
            waiting += 1;
        }
        ring[slot] = [word0, word1, purchase, hash, at];
        slot = (slot + 1) % AHEAD;
    });
    // Oldest first: a full ring starts at `slot`, a partly filled one at 0.
    let first = if waiting == AHEAD { slot } else { 0 };
    for offset in 0..waiting {
        forward_ring(links, out, ring[(first + offset) % AHEAD]);
    }
    if let (Some(started), Some(between)) = (started, between) {
        scratch.winners_nanos += (between - started).as_nanos() as u64;
        scratch.forward_nanos += between.elapsed().as_nanos() as u64;
    }
}

/// Dedup the coded event IDs of one partition and forward the survivors.
fn dedup_coded(links: &LinkTable, lists: &[PartList], out: &mut PassBOut, scratch: &mut ScratchB) {
    let words: usize = lists.iter().map(PartList::words).sum();
    if words == 0 {
        return;
    }
    // Every record has at least two words.
    let most_records = words / 2;
    let bits = (most_records.max(8) * 3 / 2).next_power_of_two().trailing_zeros();
    let capacity = 1usize << bits;
    scratch.table.clear();
    scratch.table.resize(capacity, Slot::default());
    scratch.lost.clear();
    scratch.lost.resize(most_records / 64 + 1, 0);
    let table = &mut scratch.table[..];
    let lost = &mut scratch.lost[..];

    // Step 2 of the rules: per event ID keep the copy with the smallest
    // ingest_ts (the first seen among equal ones, which are identical).
    let started = scratch.measure.then(std::time::Instant::now);
    let mut sequence = 0u32;
    for_each_record(lists, |word0, word1, _| {
        let key = (word0 >> 32) as u32;
        let ingest = ((word1 >> 1) & INGEST_MASK) as u32;
        let mut at = (key.wrapping_mul(0x9E37_79B1) >> (32 - bits)) as usize;
        loop {
            // SAFETY: `at` is always masked into the table.
            let slot = unsafe { table.get_unchecked_mut(at) };
            if slot.tag == 0 {
                *slot = Slot { tag: sequence + 1, key, ingest };
                break;
            }
            if slot.key == key {
                let loser = if ingest < slot.ingest {
                    let previous = slot.tag - 1;
                    slot.tag = sequence + 1;
                    slot.ingest = ingest;
                    previous
                } else {
                    sequence
                };
                lost[(loser >> 6) as usize] |= 1u64 << (loser & 63);
                break;
            }
            at = (at + 1) & (capacity - 1);
        }
        sequence += 1;
    });

    let between = scratch.measure.then(std::time::Instant::now);

    if sorted_lookups() {
        // The winner's own valid bit decides: an invalid winner drops the event.
        let mut sequence = 0u32;
        let unsorted = &mut scratch.unsorted;
        unsorted.clear();
        for_each_record(lists, |word0, word1, purchase| {
            let is_lost = (lost[(sequence >> 6) as usize] >> (sequence & 63)) & 1;
            sequence += 1;
            if (word0 & 1) & !is_lost == 0 {
                return;
            }
            unsorted.push([word0, word1, purchase, mix(word1 >> 22)]);
        });
        bucket_by_hash(&scratch.unsorted, &mut scratch.sorted, &mut scratch.buckets);
        forward_sorted(links, out, &scratch.sorted, |out, mixed, [word0, word1, purchase, _]| {
            forward_mixed(out, mixed, word0, word1 & 1, purchase)
        });
        if let (Some(started), Some(between)) = (started, between) {
            scratch.winners_nanos += (between - started).as_nanos() as u64;
            scratch.forward_nanos += between.elapsed().as_nanos() as u64;
        }
        return;
    }

    // The winner's own valid bit decides: an invalid winner drops the event.
    // Survivors wait in a small ring while their person-table slot loads.
    let mut ring = [[0u64; 5]; AHEAD];
    let mut waiting = 0usize;
    let mut slot = 0usize;
    let mut sequence = 0u32;
    for_each_record(lists, |word0, word1, purchase| {
        let is_lost = (lost[(sequence >> 6) as usize] >> (sequence & 63)) & 1;
        sequence += 1;
        if (word0 & 1) & !is_lost == 0 {
            return;
        }
        let [hash, at] = links.probe(word1 >> 22);
        if waiting == AHEAD {
            forward_ring(links, out, ring[slot]);
        } else {
            waiting += 1;
        }
        ring[slot] = [word0, word1, purchase, hash, at];
        slot = (slot + 1) % AHEAD;
    });
    // Oldest first: a full ring starts at `slot`, a partly filled one at 0.
    let first = if waiting == AHEAD { slot } else { 0 };
    for offset in 0..waiting {
        forward_ring(links, out, ring[(first + offset) % AHEAD]);
    }
    if let (Some(started), Some(between)) = (started, between) {
        scratch.winners_nanos += (between - started).as_nanos() as u64;
        scratch.forward_nanos += between.elapsed().as_nanos() as u64;
    }
}

/// Whether pass B sorts a partition's survivors (and a batch's clicks) by
/// the hash of their user before it looks them up: yes unless `SS_SORTED=0`.
fn sorted_lookups() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SS_SORTED").as_deref() != Ok("0"))
}

/// Buckets by the top bits of a user's hash (`codes::mix`, `CODE_BITS` bits).
const HASH_BUCKET_BITS: u32 = 11;

/// `sorted` = `unsorted` (records whose fourth word is their user's hash)
/// by bucket of that hash, each bucket in the order read. A lookup's link
/// table position is the top bits of the same hash, so the lookups of the
/// sorted records go through the link table's bitmap and rank words in the
/// order of their addresses, which the processor's own prefetching follows,
/// where in the order read each one is a jump to a random line of them.
fn bucket_by_hash(unsorted: &[[u64; 4]], sorted: &mut Vec<[u64; 4]>, buckets: &mut Vec<u32>) {
    let shift = super::codes::CODE_BITS - HASH_BUCKET_BITS;
    buckets.clear();
    buckets.resize((1 << HASH_BUCKET_BITS) + 1, 0);
    for record in unsorted {
        buckets[(record[3] >> shift) as usize + 1] += 1;
    }
    for bucket in 1..buckets.len() {
        buckets[bucket] += buckets[bucket - 1];
    }
    sorted.clear();
    sorted.resize(unsorted.len(), [0; 4]);
    for record in unsorted {
        let bucket = (record[3] >> shift) as usize;
        // SAFETY: the counts above put every record below `unsorted.len()`.
        unsafe {
            let at = buckets.get_unchecked_mut(bucket);
            *sorted.get_unchecked_mut(*at as usize) = *record;
            *at += 1;
        }
    }
}

/// The lookups and writes of records sorted by `bucket_by_hash`: each waits
/// in the ring of `AHEAD` while its link-table entry loads, as in the order
/// read, and `emit` writes it with its person's mixed name.
#[inline(always)]
fn forward_sorted(
    links: &LinkTable,
    out: &mut PassBOut,
    sorted: &[[u64; 4]],
    emit: impl Fn(&mut PassBOut, u64, [u64; 4]),
) {
    let mut ring = [([0u64; 4], [0u64; 2], 0u64); AHEAD];
    let mut waiting = 0usize;
    let mut slot = 0usize;
    for record in sorted {
        let user = record[1] >> 22;
        let probe = links.probe_hashed(user, record[3]);
        if waiting == AHEAD {
            let (waited, waited_probe, waited_user) = ring[slot];
            emit(out, links.person_mixed_probed(waited_user, waited_probe), waited);
        } else {
            waiting += 1;
        }
        ring[slot] = (*record, probe, user);
        slot = (slot + 1) % AHEAD;
    }
    // Oldest first: a full ring starts at `slot`, a partly filled one at 0.
    let first = if waiting == AHEAD { slot } else { 0 };
    for offset in 0..waiting {
        let (waited, waited_probe, waited_user) = ring[(first + offset) % AHEAD];
        emit(out, links.person_mixed_probed(waited_user, waited_probe), waited);
    }
}

/// Writes one surviving event waiting in a ring (record words, then what
/// `LinkTable::probe` returned for its user) to its person's partition.
#[inline(always)]
fn forward_ring(links: &LinkTable, out: &mut PassBOut, waiting: [u64; 5]) {
    let [word0, word1, purchase, hash, at] = waiting;
    forward_mixed(out, links.person_mixed_probed(word1 >> 22, [hash, at]), word0, word1 & 1, purchase);
}

/// Writes one surviving event to its person's partition.
#[inline(always)]
pub fn forward(links: &LinkTable, out: &mut PassBOut, word0: u64, word1: u64, purchase: u64) {
    forward_named(out, links.person(word1 >> 22), word0, word1 & 1, purchase);
}

/// Writes one surviving event of the person named `name` (a name code) to
/// that person's partition. `word0` holds the timestamp as `parse` stores it.
#[inline(always)]
pub fn forward_named(out: &mut PassBOut, name: u64, word0: u64, is_purchase: u64, purchase: u64) {
    forward_mixed(out, mix(name), word0, is_purchase, purchase);
}

/// `forward_named` with the name already mixed (`codes::mix`).
#[inline(always)]
pub fn forward_mixed(out: &mut PassBOut, mixed: u64, word0: u64, is_purchase: u64, purchase: u64) {
    let part = (mixed >> out.person_shift) as usize;
    // The local key: the low 32 bits hold every bit below the partition's.
    let key = (mixed << 32) | (word0 & (TS_MASK << 1)) | is_purchase;
    // SAFETY: a mixed code has `CODE_BITS` bits, so `part` is below the
    // number of writers; `reserve` returns room for the words written.
    unsafe {
        let at = out.events.get_unchecked_mut(part).reserve(&mut out.arena, out.event_block, 1 + is_purchase as usize);
        *at = key;
        if is_purchase == 1 {
            *at.add(1) = purchase;
        }
    }
}

/// Writes one click of the person named `name` to that person's partition.
/// `ts` is in milliseconds since seven days before the window.
#[inline(always)]
pub fn forward_click(out: &mut PassBOut, name: u64, ts: u64, code_and_campaign: u64) {
    forward_click_mixed(out, mix(name), ts, code_and_campaign);
}

/// `forward_click` with the name already mixed (`codes::mix`).
#[inline(always)]
pub fn forward_click_mixed(out: &mut PassBOut, mixed: u64, ts: u64, code_and_campaign: u64) {
    let part = (mixed >> out.person_shift) as usize;
    // SAFETY: a mixed code has `CODE_BITS` bits, so `part` is below the
    // number of writers; `reserve` returns room for two words.
    unsafe {
        let at = out.clicks.get_unchecked_mut(part).reserve(&mut out.arena, out.click_block, 2);
        *at = (mixed << 32) | (ts << 1);
        *at.add(1) = code_and_campaign;
    }
}

/// Map a batch of parsed clicks (blocks of `[user, click code and campaign,
/// ts]` triples) to their persons' partitions.
pub fn map_clicks(links: &LinkTable, blocks: &[&[u64]], out: &mut PassBOut) {
    // As the forward of events: each click waits in a ring of 16 while its
    // link-table slot loads, and the probe made for that prefetch is where
    // its lookup starts (no second hash and rank); a person named by the
    // click's own user takes the probe's hash as its mixed name.
    let mut ring = [[0u64; 5]; AHEAD];
    let mut waiting = 0usize;
    let mut slot = 0usize;
    for block in blocks {
        for click in block.chunks_exact(RAW_CLICK_WORDS) {
            let [hash, at] = links.probe(click[0]);
            if waiting == AHEAD {
                forward_click_ring(links, out, ring[slot]);
            } else {
                waiting += 1;
            }
            ring[slot] = [click[0], click[1], click[2], hash, at];
            slot = (slot + 1) % AHEAD;
        }
    }
    // Oldest first: a full ring starts at `slot`, a partly filled one at 0.
    let first = if waiting == AHEAD { slot } else { 0 };
    for offset in 0..waiting {
        forward_click_ring(links, out, ring[(first + offset) % AHEAD]);
    }
}

/// Writes one click waiting in a ring (user, click code and campaign, ts,
/// then what `LinkTable::probe` returned for the user) to its person's
/// partition.
#[inline(always)]
fn forward_click_ring(links: &LinkTable, out: &mut PassBOut, waiting: [u64; 5]) {
    let [user, code_and_campaign, ts, hash, at] = waiting;
    forward_click_mixed(out, links.person_mixed_probed(user, [hash, at]), ts, code_and_campaign);
}

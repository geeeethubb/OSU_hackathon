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
use super::{CLICK_BLOCK, LOCAL_BITS, LOCAL_MASK, PARTS, PERSON_BLOCK};

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
}

impl PassBOut {
    pub fn new() -> Self {
        Self { arena: Arena::new(), events: vec![PartWriter::EMPTY; PARTS], clicks: vec![PartWriter::EMPTY; PARTS] }
    }
}

impl Default for PassBOut {
    fn default() -> Self {
        Self::new()
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

/// Dedup one event-ID partition and forward the survivors.
pub fn dedup_partition(links: &LinkTable, lists: &[PartList], out: &mut PassBOut, scratch: &mut ScratchB) {
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

    // The winner's own valid bit decides: an invalid winner drops the event.
    // Survivors wait in a small ring while their person-table slot loads.
    let mut ring = [[0u64; 3]; AHEAD];
    let mut waiting = 0usize;
    let mut slot = 0usize;
    let mut sequence = 0u32;
    for_each_record(lists, |word0, word1, purchase| {
        let is_lost = (lost[(sequence >> 6) as usize] >> (sequence & 63)) & 1;
        sequence += 1;
        if (word0 & 1) & !is_lost == 0 {
            return;
        }
        links.prefetch(word1 >> 22);
        if waiting == AHEAD {
            let [old0, old1, old_purchase] = ring[slot];
            forward(links, out, old0, old1, old_purchase);
        } else {
            waiting += 1;
        }
        ring[slot] = [word0, word1, purchase];
        slot = (slot + 1) % AHEAD;
    });
    // Oldest first: a full ring starts at `slot`, a partly filled one at 0.
    let first = if waiting == AHEAD { slot } else { 0 };
    for offset in 0..waiting {
        let [word0, word1, purchase] = ring[(first + offset) % AHEAD];
        forward(links, out, word0, word1, purchase);
    }
}

/// Writes one surviving event to its person's partition.
#[inline(always)]
fn forward(links: &LinkTable, out: &mut PassBOut, word0: u64, word1: u64, purchase: u64) {
    let mixed = mix(links.person(word1 >> 22));
    let part = (mixed >> LOCAL_BITS) as usize;
    let is_purchase = word1 & 1;
    let key = ((mixed & LOCAL_MASK) << 32) | (word0 & (TS_MASK << 1)) | is_purchase;
    // SAFETY: a mixed code has `CODE_BITS` bits, so `part < PARTS`, the
    // number of writers; `reserve` returns room for the words written.
    unsafe {
        let at = out.events.get_unchecked_mut(part).reserve(&mut out.arena, PERSON_BLOCK, 1 + is_purchase as usize);
        *at = key;
        if is_purchase == 1 {
            *at.add(1) = purchase;
        }
    }
}

/// Map a batch of parsed clicks (blocks of `[user, click code and campaign,
/// ts]` triples) to their persons' partitions.
pub fn map_clicks(links: &LinkTable, blocks: &[&[u64]], out: &mut PassBOut) {
    for block in blocks {
        for (index, click) in block.chunks_exact(RAW_CLICK_WORDS).enumerate() {
            if let Some(&later) = block.get((index + AHEAD) * RAW_CLICK_WORDS) {
                links.prefetch(later);
            }
            let mixed = mix(links.person(click[0]));
            let part = (mixed >> LOCAL_BITS) as usize;
            // SAFETY: a mixed code has `CODE_BITS` bits, so `part < PARTS`, the
            // number of writers; `reserve` returns room for two words.
            unsafe {
                let at = out.clicks.get_unchecked_mut(part).reserve(&mut out.arena, CLICK_BLOCK, 2);
                *at = ((mixed & LOCAL_MASK) << 32) | (click[2] << 1);
                *at.add(1) = click[1];
            }
        }
    }
}

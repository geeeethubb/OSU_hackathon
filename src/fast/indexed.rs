//! Index mode's pass over one index partition.
//!
//! A partition holds the records of `INDEX_SLOTS` consecutive event indexes.
//! Putting each record in the slot of its index picks the winning copy of
//! every event without hashing (copies of an event share an index), and
//! reading the slots in order visits the events in index order.

use super::arena::PartList;
use super::dedup::{forward, PassBOut};
use super::keys::Keys;
use super::links::LinkTable;
use super::parse::INDEX_SLOTS;

const INGEST_MASK: u64 = 0x1F_FFFF;

pub struct ScratchI {
    /// The winning record per index.
    slots: Vec<[u64; 2]>,
    /// The purchase word of the winning record, for purchases.
    purchases: Vec<u64>,
    /// One bit per slot: a record was placed there.
    occupied: Vec<u64>,
}

impl ScratchI {
    pub fn new() -> Self {
        Self { slots: vec![[0; 2]; INDEX_SLOTS], purchases: vec![0; INDEX_SLOTS], occupied: vec![0; INDEX_SLOTS / 64] }
    }
}

impl Default for ScratchI {
    fn default() -> Self {
        Self::new()
    }
}

/// Places a partition's records by index. Returns whether there were any.
fn place(lists: &[PartList], scratch: &mut ScratchI) -> bool {
    scratch.occupied.fill(0);
    let mut any = false;
    for list in lists {
        for block in list.blocks() {
            let mut at = 0;
            while at < block.len() {
                // Blocks hold whole records: two words, plus one for a purchase.
                let (word0, word1) = (block[at], block[at + 1]);
                let has_purchase = (word1 & 1) as usize;
                let purchase = if has_purchase == 1 { block[at + 2] } else { 0 };
                at += 2 + has_purchase;
                let slot = (word0 >> 48) as usize & (INDEX_SLOTS - 1);
                let bit = 1u64 << (slot & 63);
                // Step 2 of the rules: the copy with the smallest ingest_ts
                // wins (the first seen among equal ones, which are identical).
                let first = scratch.occupied[slot >> 6] & bit == 0;
                if first || ((word1 >> 1) & INGEST_MASK) < ((scratch.slots[slot][1] >> 1) & INGEST_MASK) {
                    scratch.occupied[slot >> 6] |= bit;
                    scratch.slots[slot] = [word0, word1];
                    scratch.purchases[slot] = purchase;
                    any = true;
                }
            }
        }
    }
    any
}

/// Picks the partition's winning copies and sends every valid one to its
/// person's partition, for the general session pass.
pub fn forward_partition(
    keys: &Keys,
    links: &LinkTable,
    lists: &[PartList],
    out: &mut PassBOut,
    scratch: &mut ScratchI,
) {
    if !place(lists, scratch) {
        return;
    }
    for word_index in 0..scratch.occupied.len() {
        let mut bits = scratch.occupied[word_index];
        while bits != 0 {
            let slot = word_index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let [word0, word1] = scratch.slots[slot];
            // The winner's own valid bit decides: an invalid winner drops the event.
            if word0 & 1 == 0 {
                continue;
            }
            let user = word1 >> 22;
            let code = if (word0 >> 32) & 1 == 1 { keys.code_of_user_number(user) } else { user };
            forward(links, out, word0, (code << 22) | (word1 & 0x3F_FFFF), scratch.purchases[slot]);
        }
    }
}

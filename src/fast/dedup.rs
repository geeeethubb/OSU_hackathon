//! Pass B, one event-ID partition at a time: pick each event's winning copy,
//! drop invalid winners, map the user to a person and write the survivor to
//! that person's partition. Clicks are mapped to their persons here too.

use super::arena::{Arena, PartList, PartWriter};
use super::codes::mix;
use super::links::LinkTable;
use super::parse::ClickRecord;
use super::{CLICK_BLOCK, LOCAL_BITS, LOCAL_MASK, PARTS, PERSON_BLOCK};

const TS_MASK: u64 = 0x7FFF_FFFF;
const INGEST_MASK: u64 = 0x1F_FFFF;

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

#[derive(Default)]
pub struct ScratchB {
    words: Vec<u64>,
    /// (word 0, word 1, purchase word) per record.
    records: Vec<[u64; 3]>,
    table: Vec<u32>,
}

/// Dedup one event-ID partition and forward the survivors.
pub fn dedup_partition(links: &LinkTable, lists: &[PartList], out: &mut PassBOut, scratch: &mut ScratchB) {
    scratch.words.clear();
    for list in lists {
        list.copy_into(&mut scratch.words);
    }
    let words = &scratch.words;
    let records = &mut scratch.records;
    records.clear();
    let mut at = 0;
    while at < words.len() {
        let (word0, word1) = (words[at], words[at + 1]);
        at += 2;
        let purchase = if word1 & 1 == 1 {
            at += 1;
            words[at - 1]
        } else {
            0
        };
        records.push([word0, word1, purchase]);
    }

    // Step 2 of the rules: per event ID keep the copy with the smallest
    // ingest_ts. A losing copy is dropped by clearing its valid bit; the
    // winner keeps its own, so an invalid winner drops the whole event.
    let count = records.len();
    let bits = (count.max(8) * 2).next_power_of_two().trailing_zeros();
    let capacity = 1usize << bits;
    scratch.table.clear();
    scratch.table.resize(capacity, 0);
    let table = &mut scratch.table;
    for index in 0..count {
        let key = (records[index][0] >> 32) as u32;
        let mut slot = (key.wrapping_mul(0x9E37_79B1) >> (32 - bits)) as usize;
        loop {
            let entry = table[slot];
            if entry == 0 {
                table[slot] = index as u32 + 1;
                break;
            }
            let other = (entry - 1) as usize;
            if (records[other][0] >> 32) as u32 == key {
                let ingest_new = (records[index][1] >> 1) & INGEST_MASK;
                let ingest_old = (records[other][1] >> 1) & INGEST_MASK;
                if ingest_new < ingest_old {
                    records[other][0] &= !1;
                    table[slot] = index as u32 + 1;
                } else {
                    records[index][0] &= !1;
                }
                break;
            }
            slot = (slot + 1) & (capacity - 1);
        }
    }

    for record in records.iter() {
        if record[0] & 1 == 0 {
            continue;
        }
        let person = links.person(record[1] >> 22);
        let mixed = mix(person);
        let part = (mixed >> LOCAL_BITS) as usize;
        let is_purchase = record[1] & 1;
        let key = ((mixed & LOCAL_MASK) << 32) | (record[0] & (TS_MASK << 1)) | is_purchase;
        let writer = &mut out.events[part];
        writer.push(&mut out.arena, PERSON_BLOCK, key);
        if is_purchase == 1 {
            writer.push(&mut out.arena, PERSON_BLOCK, record[2]);
        }
    }
}

/// Map a batch of parsed clicks to their persons' partitions.
pub fn map_clicks(links: &LinkTable, clicks: &[ClickRecord], out: &mut PassBOut) {
    for click in clicks {
        let mixed = mix(links.person(click.user));
        let part = (mixed >> LOCAL_BITS) as usize;
        let writer = &mut out.clicks[part];
        writer.push(&mut out.arena, CLICK_BLOCK, ((mixed & LOCAL_MASK) << 32) | (u64::from(click.ts) << 1));
        writer.push(&mut out.arena, CLICK_BLOCK, click.code_and_campaign);
    }
}

//! Compact, exact integer codes for IDs, which are opaque strings.
//!
//! An ID of a prefix and 1 to 8 characters of `[0-9a-z]` maps to a number
//! below 37^8 by reading it as a left-aligned base-37 string, so numeric
//! order equals byte-wise order. Every other ID is "exotic" and gets
//! `37^8 + index` from an interner. Both together fit 42 bits and are exact:
//! equal codes mean equal IDs. The codes encode the text of an ID and nothing
//! else; no ID is read as a number of whoever made it.
//!
//! Exotic IDs of one shape are common (one in eight distinct IDs of current
//! generated data): one character and 32 lowercase hexadecimal digits. Those
//! are interned by their 128-bit value in a table split in 256 shards with a
//! lock each, so that the parse threads do not wait on one another and no ID
//! needs a heap block of its own; every other exotic ID goes to one map.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Mutex;

pub const P37: [u64; 9] =
    [1, 37, 1_369, 50_653, 1_874_161, 69_343_957, 2_565_726_409, 94_931_877_133, 3_512_479_453_921];
pub const EXOTIC_BASE: u64 = P37[8];
pub const CODE_BITS: u32 = 42;
pub const CODE_MASK: u64 = (1 << CODE_BITS) - 1;

const fn char_values() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut at = 0;
    while at < 10 {
        table[b'0' as usize + at] = 1 + at as u8;
        at += 1;
    }
    at = 0;
    while at < 26 {
        table[b'a' as usize + at] = 11 + at as u8;
        at += 1;
    }
    table
}
pub static CHAR_VALUE: [u8; 256] = char_values();

/// The code of a generator-style ID, or `None` if it is exotic.
#[inline]
pub fn pattern_code(id: &[u8], prefix: u8) -> Option<u64> {
    let length = id.len();
    if !(2..=9).contains(&length) || id[0] != prefix {
        return None;
    }
    let mut code = 0u64;
    for &byte in &id[1..] {
        let value = CHAR_VALUE[byte as usize];
        if value == 0 {
            return None;
        }
        code = code * 37 + u64::from(value);
    }
    Some(code * P37[9 - length])
}

/// Appends the ID a pattern code stands for.
pub fn decode_pattern(code: u64, prefix: u8, out: &mut Vec<u8>) {
    debug_assert!(code < EXOTIC_BASE);
    out.push(prefix);
    let mut rest = code;
    for power in (0..8).rev() {
        let digit = (rest / P37[power]) as u8;
        rest %= P37[power];
        if digit == 0 {
            break;
        }
        out.push(if digit <= 10 { b'0' + digit - 1 } else { b'a' + digit - 11 });
    }
}

/// Lowercase hexadecimal digits: their value, 0xFF for any other byte.
const fn hex_values() -> [u8; 256] {
    let mut table = [0xFFu8; 256];
    let mut at = 0;
    while at < 10 {
        table[b'0' as usize + at] = at as u8;
        at += 1;
    }
    at = 0;
    while at < 6 {
        table[b'a' as usize + at] = 10 + at as u8;
        at += 1;
    }
    table
}
static HEX_VALUE: [u8; 256] = hex_values();
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The value of an ID of one character and 32 lowercase hexadecimal digits.
#[inline]
fn hex_id(id: &[u8]) -> Option<u128> {
    if id.len() != 33 {
        return None;
    }
    let half = |digits: &[u8]| -> Option<u64> {
        let mut value = 0u64;
        let mut bad = 0u8;
        for &byte in digits {
            let digit = HEX_VALUE[byte as usize];
            bad |= digit;
            value = value << 4 | u64::from(digit & 15);
        }
        // A digit is at most 15, so any 0xFF shows in the high bits.
        (bad < 16).then_some(value)
    };
    let high = half(&id[1..17])?;
    let low = half(&id[17..33])?;
    Some(u128::from(high) << 64 | u128::from(low))
}

/// The keys are outputs of a keyed bijection or otherwise arbitrary: one
/// multiply spreads them over the buckets.
#[derive(Default)]
struct HexHasher(u64);

impl Hasher for HexHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.0 = (self.0 ^ u64::from(value)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    #[inline]
    fn write_u128(&mut self, value: u128) {
        let folded = (value as u64) ^ ((value >> 64) as u64).rotate_left(29);
        self.0 = (self.0 ^ folded).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type HexShard = HashMap<(u8, u128), u32, BuildHasherDefault<HexHasher>>;
const HEX_SHARDS: usize = 256;

/// The code event IDs of one character and 32 lowercase hexadecimal digits
/// get from an interner made by `Interner::partition_local`: never a real
/// code (no index comes near it). The ID's value waits in `take_long_value`
/// for the record that is stored next on this thread.
pub const LONG_EVENT: u64 = CODE_MASK;

thread_local! {
    static LONG_VALUE: std::cell::Cell<(u8, u128)> = const { std::cell::Cell::new((0, 0)) };
}

/// The first byte and value of the last long event ID coded on this thread.
#[inline]
pub fn take_long_value() -> (u8, u128) {
    LONG_VALUE.with(std::cell::Cell::get)
}

/// Exotic indexes from `THREAD_INDEX_SHIFT` up are those of the IDs a parse
/// worker kept for itself (`Interner::per_thread`): worker slot plus one in
/// the bits above the shift, the position in that worker's list below.
const THREAD_INDEX_SHIFT: u32 = 32;
/// Worker slots that fit: (slot + 1) << 32 stays inside the exotic part of
/// the 42-bit code space.
const THREAD_SLOTS: u32 = 127;

thread_local! {
    /// This thread's worker slot (`u32::MAX`: none) and the hexadecimal IDs
    /// it kept, in the order their codes number them.
    static THREAD_IDS: std::cell::RefCell<(u32, Vec<(u8, u128)>)> =
        const { std::cell::RefCell::new((u32::MAX, Vec::new())) };
}

/// Makes this thread worker `slot` for the IDs an `Interner::per_thread`
/// codes, with an empty list. A slot that does not fit leaves the thread
/// with none: its IDs then take the shared map.
pub fn begin_thread_ids(slot: usize) {
    THREAD_IDS.with(|cell| {
        let mut ids = cell.borrow_mut();
        ids.0 = if slot < THREAD_SLOTS as usize { slot as u32 } else { u32::MAX };
        ids.1.clear();
    });
}

/// The IDs this thread kept since `begin_thread_ids`, by position; the
/// thread has no slot afterwards.
pub fn take_thread_ids() -> Vec<(u8, u128)> {
    THREAD_IDS.with(|cell| {
        let mut ids = cell.borrow_mut();
        ids.0 = u32::MAX;
        std::mem::take(&mut ids.1)
    })
}

/// The code of a hexadecimal ID this thread keeps for itself, or `None`
/// when the thread is no worker (or its list is full).
#[inline]
fn thread_code(first: u8, value: u128) -> Option<u64> {
    THREAD_IDS.with(|cell| {
        let mut ids = cell.borrow_mut();
        let position = ids.1.len() as u64;
        if ids.0 == u32::MAX || position >> THREAD_INDEX_SHIFT != 0 {
            return None;
        }
        ids.1.push((first, value));
        Some(EXOTIC_BASE + ((u64::from(ids.0) + 1) << THREAD_INDEX_SHIFT) + position)
    })
}

/// Exotic IDs of one kind (events, users or clicks).
pub struct Interner {
    /// The next exotic index, for both maps.
    next: AtomicU32,
    generic: Mutex<HashMap<Box<[u8]>, u32>>,
    hex: Box<[Mutex<HexShard>]>,
    /// Event IDs: they only need to be equal or not, inside the partition
    /// their value picks, so long hexadecimal ones are not interned here at
    /// all (`LONG_EVENT`): the dedup step compares their values.
    partition_local: bool,
    /// Click IDs: only their byte-wise order matters (two clicks of one
    /// person in one millisecond), never whether two are equal (copies of a
    /// click are identical, so either may stand for it). A parse worker
    /// appends each long hexadecimal one to its own list and codes it by
    /// its position there: no lookup, no lock. Copies get different codes.
    per_thread: bool,
}

impl Default for Interner {
    fn default() -> Self {
        Interner {
            next: AtomicU32::new(0),
            generic: Mutex::new(HashMap::new()),
            hex: (0..HEX_SHARDS).map(|_| Mutex::new(HexShard::default())).collect(),
            partition_local: false,
            per_thread: false,
        }
    }
}

impl Interner {
    /// An interner whose long hexadecimal IDs get `LONG_EVENT` (event IDs).
    pub fn partition_local() -> Self {
        Interner { partition_local: true, ..Interner::default() }
    }

    /// An interner whose long hexadecimal IDs a parse worker keeps for
    /// itself (click IDs); a snapshot needs the workers' lists
    /// (`Exotic::with_threads`).
    pub fn per_thread() -> Self {
        Interner { per_thread: true, ..Interner::default() }
    }

    /// The code of an exotic ID.
    #[inline(never)]
    pub fn code(&self, id: &[u8]) -> u64 {
        match hex_id(id) {
            Some(value) if self.partition_local => {
                LONG_VALUE.with(|cell| cell.set((id[0], value)));
                LONG_EVENT
            }
            Some(value) => match self.per_thread.then(|| thread_code(id[0], value)).flatten() {
                Some(code) => code,
                None => EXOTIC_BASE + u64::from(self.hex_index(id[0], value)),
            },
            None => self.generic_code(id),
        }
    }

    #[inline]
    fn hex_index(&self, first: u8, value: u128) -> u32 {
        let mut shard = self.hex[(value >> 120) as usize].lock().unwrap();
        if let Some(&at) = shard.get(&(first, value)) {
            return at;
        }
        let at = self.next.fetch_add(1, AtomicOrdering::Relaxed);
        shard.insert((first, value), at);
        at
    }

    #[cold]
    #[inline(never)]
    fn generic_code(&self, id: &[u8]) -> u64 {
        let mut generic = self.generic.lock().unwrap();
        if let Some(&at) = generic.get(id) {
            return EXOTIC_BASE + u64::from(at);
        }
        let at = self.next.fetch_add(1, AtomicOrdering::Relaxed);
        generic.insert(id.into(), at);
        EXOTIC_BASE + u64::from(at)
    }

    /// The exotic IDs interned so far, by index. `materialize`: every ID as
    /// bytes (for output); otherwise hexadecimal IDs stay values, which is
    /// enough to compare and print them.
    pub fn snapshot_with(&self, materialize: bool) -> Exotic {
        let count = self.next.load(AtomicOrdering::Acquire) as usize;
        let mut entries = vec![Entry::Bytes { start: 0, end: 0 }; count];
        let mut arena: Vec<u8> = Vec::new();
        for (id, &at) in self.generic.lock().unwrap().iter() {
            if (at as usize) < count {
                let start = arena.len() as u32;
                arena.extend_from_slice(id);
                entries[at as usize] = Entry::Bytes { start, end: arena.len() as u32 };
            }
        }
        for shard in self.hex.iter() {
            for (&(first, value), &at) in shard.lock().unwrap().iter() {
                if (at as usize) >= count {
                    continue;
                }
                entries[at as usize] = if materialize {
                    let start = arena.len() as u32;
                    arena.push(first);
                    push_hex(value, &mut arena);
                    Entry::Bytes { start, end: arena.len() as u32 }
                } else {
                    Entry::Hex { first, value }
                };
            }
        }
        Exotic { entries, arena, threads: Vec::new() }
    }

    /// The exotic IDs interned so far, by index (hexadecimal ones as values).
    pub fn snapshot(&self) -> Exotic {
        self.snapshot_with(false)
    }
}

#[inline]
fn push_hex(value: u128, out: &mut Vec<u8>) {
    for shift in (0..32).rev() {
        out.push(HEX_DIGITS[((value >> (shift * 4)) & 15) as usize]);
    }
}

#[derive(Clone, Copy)]
enum Entry {
    Bytes { start: u32, end: u32 },
    Hex { first: u8, value: u128 },
}

/// The exotic IDs of one kind by index, as `Interner::snapshot` saw them.
#[derive(Default)]
pub struct Exotic {
    entries: Vec<Entry>,
    arena: Vec<u8>,
    /// The IDs parse workers kept for themselves, by worker slot.
    threads: Vec<Vec<(u8, u128)>>,
}

impl Exotic {
    /// From IDs given as bytes, index by index (tests and small tables).
    pub fn from_list<T: AsRef<[u8]>>(list: &[T]) -> Self {
        let mut exotic = Exotic::default();
        for id in list {
            let start = exotic.arena.len() as u32;
            exotic.arena.extend_from_slice(id.as_ref());
            exotic.entries.push(Entry::Bytes { start, end: exotic.arena.len() as u32 });
        }
        exotic
    }

    /// With the IDs the parse workers kept, by worker slot.
    pub fn with_threads(mut self, threads: Vec<Vec<(u8, u128)>>) -> Self {
        self.threads = threads;
        self
    }

    /// Exotic ID `index`.
    #[inline]
    fn entry(&self, index: usize) -> Entry {
        match index >> THREAD_INDEX_SHIFT {
            0 => self.entries[index],
            slot => {
                let (first, value) = self.threads[slot - 1][index & ((1 << THREAD_INDEX_SHIFT) - 1)];
                Entry::Hex { first, value }
            }
        }
    }

    /// How many IDs the shared maps hold (those a worker kept not counted).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Appends the bytes of exotic ID `index`.
    pub fn append(&self, index: usize, out: &mut Vec<u8>) {
        match self.entry(index) {
            Entry::Bytes { start, end } => out.extend_from_slice(&self.arena[start as usize..end as usize]),
            Entry::Hex { first, value } => {
                out.push(first);
                push_hex(value, out);
            }
        }
    }

    /// The bytes of exotic ID `index`, when the snapshot holds them as bytes
    /// (always, for a snapshot made with `materialize`).
    pub fn text(&self, index: usize) -> &[u8] {
        match self.entry(index) {
            Entry::Bytes { start, end } => &self.arena[start as usize..end as usize],
            Entry::Hex { .. } => panic!("exotic ID {index} was not materialized"),
        }
    }

    /// Byte-wise order of two exotic IDs, when both are hexadecimal values of
    /// the same first character (fixed width, digits below letters in ASCII:
    /// the text order is the numeric order).
    #[inline]
    fn hex_order(&self, a: usize, b: usize) -> Option<Ordering> {
        match (self.entry(a), self.entry(b)) {
            (Entry::Hex { first: fa, value: va }, Entry::Hex { first: fb, value: vb }) if fa == fb => Some(va.cmp(&vb)),
            _ => None,
        }
    }
}

/// The code of a non-empty ID.
#[inline]
pub fn id_code(id: &[u8], prefix: u8, interner: &Interner) -> u64 {
    match pattern_code(id, prefix) {
        Some(code) => code,
        None => interner.code(id),
    }
}

/// Appends the bytes of the ID behind `code`.
pub fn id_bytes(code: u64, prefix: u8, exotic: &Exotic, out: &mut Vec<u8>) {
    if code < EXOTIC_BASE {
        decode_pattern(code, prefix, out);
    } else {
        exotic.append((code - EXOTIC_BASE) as usize, out);
    }
}

/// Byte-wise order of the IDs behind two codes of the same kind.
#[inline]
pub fn id_cmp(a: u64, b: u64, prefix: u8, exotic: &Exotic) -> Ordering {
    if a < EXOTIC_BASE && b < EXOTIC_BASE {
        return a.cmp(&b);
    }
    id_cmp_slow(a, b, prefix, exotic)
}

#[cold]
#[inline(never)]
fn id_cmp_slow(a: u64, b: u64, prefix: u8, exotic: &Exotic) -> Ordering {
    if a >= EXOTIC_BASE && b >= EXOTIC_BASE {
        if let Some(order) = exotic.hex_order((a - EXOTIC_BASE) as usize, (b - EXOTIC_BASE) as usize) {
            return order;
        }
    }
    let mut left = Vec::with_capacity(40);
    let mut right = Vec::with_capacity(40);
    id_bytes(a, prefix, exotic, &mut left);
    id_bytes(b, prefix, exotic, &mut right);
    left.cmp(&right)
}

// A bijection on 42-bit values whose high bits depend on every input bit:
// multiply, fold the top half down, multiply. It cuts a code into a
// (partition, local key) pair that can be put back together exactly.
const MIX_A: u64 = (0x2D3_5A5D_9C4B & CODE_MASK) | 1;
const MIX_B: u64 = (0x1B7_3E2F_A6D5 & CODE_MASK) | 1;

const fn inverse_mod_2_42(value: u64) -> u64 {
    // Newton's iteration doubles the number of correct low bits each round.
    let mut inverse = value;
    let mut round = 0;
    while round < 6 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(value.wrapping_mul(inverse)));
        round += 1;
    }
    inverse & CODE_MASK
}
const MIX_A_INVERSE: u64 = inverse_mod_2_42(MIX_A);
const MIX_B_INVERSE: u64 = inverse_mod_2_42(MIX_B);

#[inline]
pub fn mix(code: u64) -> u64 {
    let mut mixed = code.wrapping_mul(MIX_A) & CODE_MASK;
    mixed ^= mixed >> 21;
    mixed.wrapping_mul(MIX_B) & CODE_MASK
}

#[inline]
pub fn unmix(mixed: u64) -> u64 {
    let mut code = mixed.wrapping_mul(MIX_B_INVERSE) & CODE_MASK;
    code ^= code >> 21;
    code.wrapping_mul(MIX_A_INVERSE) & CODE_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_codes_round_trip_and_order() {
        let ids: [&[u8]; 8] = [b"u0", b"u00", b"u1", b"u10", b"u1z", b"u2", b"uzzzzzzzz", b"ua"];
        let mut codes: Vec<(u64, &[u8])> = ids.iter().map(|id| (pattern_code(id, b'u').unwrap(), *id)).collect();
        for (code, id) in &codes {
            let mut out = Vec::new();
            decode_pattern(*code, b'u', &mut out);
            assert_eq!(&out, id);
            assert!(*code < EXOTIC_BASE);
        }
        codes.sort();
        let mut sorted_ids: Vec<&[u8]> = ids.to_vec();
        sorted_ids.sort();
        assert_eq!(codes.iter().map(|(_, id)| *id).collect::<Vec<_>>(), sorted_ids);
        for exotic in [&b"u"[..], b"U1", b"u1A", b" u1", b"u123456789", b"e1", b"u1 "] {
            assert_eq!(pattern_code(exotic, b'u'), None);
        }
    }

    #[test]
    fn hex_ids_intern_exactly_and_order_like_bytes() {
        let interner = Interner::default();
        let ids: Vec<Vec<u8>> = vec![
            b"ue1f1e03abf0a711a0019d40e7ca04632".to_vec(),
            b"u00000000000000000000000000000000".to_vec(),
            b"uffffffffffffffffffffffffffffffff".to_vec(),
            b"ue1f1e03abf0a711a0019d40e7ca04633".to_vec(),
            b"ve1f1e03abf0a711a0019d40e7ca04632".to_vec(),
            b"uE1F1E03ABF0A711A0019D40E7CA04632".to_vec(),
            b"ue1f1e03abf0a711a0019d40e7ca0463".to_vec(),
            b"ue1f1e03abf0a711a0019d40e7ca046321".to_vec(),
            b"ug1f1e03abf0a711a0019d40e7ca04632".to_vec(),
            b"u9efba8ez".to_vec(),
            b"u".to_vec(),
        ];
        let codes: Vec<u64> = ids.iter().map(|id| id_code(id, b'u', &interner)).collect();
        // Equal IDs, equal codes; different IDs, different codes.
        for (a, id_a) in ids.iter().enumerate() {
            assert_eq!(id_code(id_a, b'u', &interner), codes[a]);
            for (b, id_b) in ids.iter().enumerate() {
                assert_eq!(codes[a] == codes[b], id_a == id_b, "{a} {b}");
            }
        }
        for materialize in [false, true] {
            let exotic = interner.snapshot_with(materialize);
            for (id, &code) in ids.iter().zip(&codes) {
                let mut out = Vec::new();
                id_bytes(code, b'u', &exotic, &mut out);
                assert_eq!(&out, id);
                if materialize && code >= EXOTIC_BASE {
                    assert_eq!(exotic.text((code - EXOTIC_BASE) as usize), &id[..]);
                }
            }
            for (a, id_a) in ids.iter().enumerate() {
                for (b, id_b) in ids.iter().enumerate() {
                    assert_eq!(id_cmp(codes[a], codes[b], b'u', &exotic), id_a.cmp(id_b), "{a} {b}");
                }
            }
        }
    }

    #[test]
    fn ids_kept_per_thread_order_like_bytes() {
        // Two workers keep their own long IDs; an ID coded on a thread with no
        // slot goes to the shared map. Order must be byte-wise across all.
        let interner = Interner::per_thread();
        let ids: Vec<Vec<u8>> = vec![
            b"ke1f1e03abf0a711a0019d40e7ca04632".to_vec(),
            b"k00000000000000000000000000000000".to_vec(),
            b"kffffffffffffffffffffffffffffffff".to_vec(),
            b"ke1f1e03abf0a711a0019d40e7ca04633".to_vec(),
            b"kE1F1E03ABF0A711A0019D40E7CA04632".to_vec(),
            b"k9efba8ez".to_vec(),
            b"ke1f1e03abf0a711a0019d40e7ca04632".to_vec(),
        ];
        let mut codes = Vec::new();
        begin_thread_ids(0);
        codes.extend(ids[..3].iter().map(|id| id_code(id, b'k', &interner)));
        let first = take_thread_ids();
        begin_thread_ids(1);
        codes.extend(ids[3..6].iter().map(|id| id_code(id, b'k', &interner)));
        let second = take_thread_ids();
        // No slot: the shared map.
        codes.push(id_code(&ids[6], b'k', &interner));
        assert_eq!((first.len(), second.len()), (3, 1));
        assert!(codes[0] != codes[6], "copies kept by a worker get their own code");
        let exotic = interner.snapshot().with_threads(vec![first, second]);
        for (a, id_a) in ids.iter().enumerate() {
            let mut out = Vec::new();
            id_bytes(codes[a], b'k', &exotic, &mut out);
            assert_eq!(&out, id_a);
            for (b, id_b) in ids.iter().enumerate() {
                assert_eq!(id_cmp(codes[a], codes[b], b'k', &exotic), id_a.cmp(id_b), "{a} {b}");
            }
        }
        // Every code stays inside the 42 bits a click record has for it.
        assert!(codes.iter().all(|&code| code <= CODE_MASK));
        begin_thread_ids(THREAD_SLOTS as usize);
        let code = id_code(&ids[1], b'k', &interner);
        assert!(take_thread_ids().is_empty(), "a slot that does not fit keeps nothing");
        assert!(code < EXOTIC_BASE + (1 << THREAD_INDEX_SHIFT));
    }

    #[test]
    fn mix_is_a_bijection() {
        for code in [0u64, 1, 2, 37, EXOTIC_BASE - 1, EXOTIC_BASE, EXOTIC_BASE + 12345, CODE_MASK] {
            assert_eq!(unmix(mix(code)), code);
            assert!(mix(code) <= CODE_MASK);
        }
    }
}

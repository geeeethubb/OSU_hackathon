//! Linked IDs: union-find over the link pairs, then a flat lookup table from
//! a user code to the code of its person's name (the byte-wise smallest ID).
//!
//! The union-find lives inside one open-addressing table (`Forest`): a slot
//! holds an ID's code and the slot of its parent, so finding an ID and
//! reaching its tree node is one memory access, and the home slots of the
//! pairs coming up are prefetched while the current pair is joined.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use super::codes::{id_cmp, mix, Interner, CODE_BITS, EXOTIC_BASE};
use crate::reference::FastHasher;

const HASH_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

/// Open-addressing table of `(user code, name code)` for every linked ID whose
/// person is named by a different ID. Anything not in the table names itself.
pub struct LinkTable {
    slots: Vec<[u64; 2]>,
    mask: usize,
    shift: u32,
}

impl LinkTable {
    fn with_capacity(entries: usize) -> Self {
        let bits = (entries.max(4) * 2).next_power_of_two().trailing_zeros();
        let capacity = 1usize << bits;
        Self { slots: vec![[0, 0]; capacity], mask: capacity - 1, shift: 64 - bits }
    }

    fn insert(&mut self, user: u64, name: u64) {
        let mut at = (user.wrapping_mul(HASH_MULTIPLIER) >> self.shift) as usize;
        loop {
            if self.slots[at][0] == 0 {
                self.slots[at] = [user, name];
                return;
            }
            at = (at + 1) & self.mask;
        }
    }

    /// Starts loading the cache line `person(user)` reads first, so that a
    /// lookup a few records later finds it in the cache. Changes no result.
    #[inline(always)]
    pub fn prefetch(&self, user: u64) {
        let at = (user.wrapping_mul(HASH_MULTIPLIER) >> self.shift) as usize;
        // `at` is inside the table: the same index `person` starts from.
        prefetch_line(self.slots.as_ptr().wrapping_add(at));
    }

    /// The name code of the person `user` belongs to. Codes are never 0.
    #[inline]
    pub fn person(&self, user: u64) -> u64 {
        let mut at = (user.wrapping_mul(HASH_MULTIPLIER) >> self.shift) as usize;
        loop {
            // SAFETY: `at` is always masked into the table.
            let slot = unsafe { self.slots.get_unchecked(at) };
            if slot[0] == user {
                return slot[1];
            }
            if slot[0] == 0 {
                return user;
            }
            at = (at + 1) & self.mask;
        }
    }
}

/// Starts loading the cache line at `pointer`. Never faults, changes nothing.
#[inline(always)]
fn prefetch_line<T>(pointer: *const T) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        // SAFETY: a prefetch is a hint; it is valid for any address.
        unsafe { _mm_prefetch(pointer as *const i8, _MM_HINT_T0) };
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = pointer;
}

/// Low bits of a forest word: the slot of the ID's parent. The code is above.
const PARENT_BITS: u32 = 64 - CODE_BITS;
const PARENT_MASK: u64 = (1 << PARENT_BITS) - 1;
/// The most slots a forest can have: a parent must fit in `PARENT_BITS`.
const FOREST_MAX_SLOTS: usize = 1 << PARENT_BITS;
/// How many pairs ahead of the one being joined the home slots are prefetched.
const PAIR_LOOKAHEAD: usize = 16;
/// How many slots ahead of the one being named its parent is prefetched.
const SCAN_LOOKAHEAD: usize = 24;
/// Renamed IDs wait this many deep while their lookup-table slots load.
const INSERT_BATCH: usize = 32;

/// Union-find over the linked IDs, stored in one open-addressing table. A
/// slot is `code << PARENT_BITS | parent slot`; a root is its own parent, and
/// an empty slot is 0 (codes are never 0). Joining always puts the byte-wise
/// smaller root on top, so the root of a tree is the name of its person.
struct Forest<'a> {
    slots: Vec<u64>,
    /// Exotic IDs by index, to compare them by their bytes.
    exotic: &'a [Box<[u8]>],
}

impl Forest<'_> {
    /// Where the probe for `code` starts.
    #[inline(always)]
    fn home(&self, code: u64) -> usize {
        // `mix` is below 2^CODE_BITS and the slot count at most 2^PARENT_BITS,
        // so the product fits 64 bits and the result is below the slot count.
        ((mix(code) * self.slots.len() as u64) >> CODE_BITS) as usize
    }

    /// The slot of `code`, which becomes a new root if it was not there.
    #[inline(always)]
    fn locate(&mut self, code: u64) -> usize {
        let mut at = self.home(code);
        loop {
            // SAFETY: `at` is always below the slot count.
            let word = unsafe { *self.slots.get_unchecked(at) };
            if word >> PARENT_BITS == code {
                return at;
            }
            if word == 0 {
                // SAFETY: as above.
                unsafe { *self.slots.get_unchecked_mut(at) = (code << PARENT_BITS) | at as u64 };
                return at;
            }
            at += 1;
            if at == self.slots.len() {
                at = 0;
            }
        }
    }

    /// The root of the tree `at` is in. Halves the path on the way.
    #[inline(always)]
    fn find(&mut self, mut at: usize) -> usize {
        loop {
            // SAFETY: `at` is an occupied slot and every parent is a slot.
            unsafe {
                let word = *self.slots.get_unchecked(at);
                let parent = (word & PARENT_MASK) as usize;
                if parent == at {
                    return at;
                }
                let above = *self.slots.get_unchecked(parent);
                let grandparent = (above & PARENT_MASK) as usize;
                if grandparent == parent {
                    return parent;
                }
                *self.slots.get_unchecked_mut(at) = (word & !PARENT_MASK) | grandparent as u64;
                at = grandparent;
            }
        }
    }

    /// Makes the trees of slots `a` and `b` one, rooted at the smaller ID.
    #[inline(always)]
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        let (word_a, word_b) = (self.slots[a], self.slots[b]);
        let (code_a, code_b) = (word_a >> PARENT_BITS, word_b >> PARENT_BITS);
        // Pattern codes order like their IDs; exotic IDs are compared by bytes.
        let a_is_smaller = if code_a < EXOTIC_BASE && code_b < EXOTIC_BASE {
            code_a < code_b
        } else {
            id_cmp(code_a, code_b, b'u', self.exotic) == Ordering::Less
        };
        if a_is_smaller {
            self.slots[b] = (word_b & !PARENT_MASK) | a as u64;
        } else {
            self.slots[a] = (word_a & !PARENT_MASK) | b as u64;
        }
    }
}

/// Joins every pair, names each group by its byte-wise smallest ID.
pub fn build(pairs: &[Vec<(u64, u64)>], users: &Interner) -> LinkTable {
    build_within(pairs, users, FOREST_MAX_SLOTS)
}

/// `build`, with the forest limited to `max_slots` slots (a parameter so the
/// tests can reach the path for inputs too large for a forest).
fn build_within(pairs: &[Vec<(u64, u64)>], users: &Interner, max_slots: usize) -> LinkTable {
    let pair_count: usize = pairs.iter().map(Vec::len).sum();
    // A pair brings at most two new IDs, so the forest stays under 8/9 full
    // and always has an empty slot to end a probe.
    let slot_count = pair_count * 2 + pair_count / 4 + 16;
    if slot_count > max_slots {
        return build_general(pairs, users);
    }
    // Exotic IDs (upper case and the like) sort by their bytes, not their code.
    let exotic = users.snapshot();
    let mut forest = Forest { slots: vec![0u64; slot_count], exotic: &exotic };
    for list in pairs {
        for (at, &(a, b)) in list.iter().enumerate() {
            if let Some(&(later_a, later_b)) = list.get(at + PAIR_LOOKAHEAD) {
                prefetch_line(forest.slots.as_ptr().wrapping_add(forest.home(later_a)));
                prefetch_line(forest.slots.as_ptr().wrapping_add(forest.home(later_b)));
            }
            let (a, b) = (forest.locate(a), forest.locate(b));
            forest.join(a, b);
        }
    }

    // Every ID that is not the root of its tree is renamed to the root.
    let renamed = forest
        .slots
        .iter()
        .enumerate()
        .filter(|&(at, &word)| word != 0 && (word & PARENT_MASK) as usize != at)
        .count();
    let mut table = LinkTable::with_capacity(renamed);
    let mut batch = [(0u64, 0u64); INSERT_BATCH];
    let mut waiting = 0;
    for at in 0..slot_count {
        if let Some(&later) = forest.slots.get(at + SCAN_LOOKAHEAD) {
            prefetch_line(forest.slots.as_ptr().wrapping_add((later & PARENT_MASK) as usize));
        }
        let word = forest.slots[at];
        if word == 0 || (word & PARENT_MASK) as usize == at {
            continue;
        }
        let root = forest.find(at);
        let (user, name) = (word >> PARENT_BITS, forest.slots[root] >> PARENT_BITS);
        table.prefetch(user);
        batch[waiting] = (user, name);
        waiting += 1;
        if waiting == INSERT_BATCH {
            for &(user, name) in &batch {
                table.insert(user, name);
            }
            waiting = 0;
        }
    }
    for &(user, name) in &batch[..waiting] {
        table.insert(user, name);
    }
    table
}

fn find(parent: &mut [u32], mut at: u32) -> u32 {
    while parent[at as usize] != at {
        let grandparent = parent[parent[at as usize] as usize];
        parent[at as usize] = grandparent;
        at = grandparent;
    }
    at
}

/// The same result with a hash map and dense indices: slower, but with no
/// limit on the number of IDs.
fn build_general(pairs: &[Vec<(u64, u64)>], users: &Interner) -> LinkTable {
    let pair_count: usize = pairs.iter().map(Vec::len).sum();
    let mut index: HashMap<u64, u32, BuildHasherDefault<FastHasher>> =
        HashMap::with_capacity_and_hasher(pair_count * 2, BuildHasherDefault::default());
    let mut codes: Vec<u64> = Vec::with_capacity(pair_count * 2);
    let mut parent: Vec<u32> = Vec::with_capacity(pair_count * 2);
    for &(a, b) in pairs.iter().flatten() {
        let mut intern = |code: u64| -> u32 {
            *index.entry(code).or_insert_with(|| {
                let at = codes.len() as u32;
                codes.push(code);
                parent.push(at);
                at
            })
        };
        let (a, b) = (intern(a), intern(b));
        let (a, b) = (find(&mut parent, a), find(&mut parent, b));
        if a != b {
            parent[a.max(b) as usize] = a.min(b);
        }
    }
    drop(index);

    // Exotic IDs (upper case and the like) sort by their bytes, not their code.
    let exotic = users.snapshot();
    let mut name: Vec<u64> = codes.clone();
    let mut root_of: Vec<u32> = Vec::with_capacity(codes.len());
    for at in 0..codes.len() {
        let root = find(&mut parent, at as u32);
        root_of.push(root);
        if id_cmp(codes[at], name[root as usize], b'u', &exotic) == Ordering::Less {
            name[root as usize] = codes[at];
        }
    }
    let renamed = (0..codes.len()).filter(|&at| name[root_of[at] as usize] != codes[at]).count();
    let mut table = LinkTable::with_capacity(renamed);
    for at in 0..codes.len() {
        let person = name[root_of[at] as usize];
        if person != codes[at] {
            table.insert(codes[at], person);
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fast::codes::{id_code, pattern_code};
    use std::collections::HashMap;

    /// A small reproducible generator (xorshift64*), so the tests need no crate.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }

        fn chance(&mut self, one_in: usize) -> bool {
            self.below(one_in) == 0
        }

        fn shuffle<T>(&mut self, items: &mut [T]) {
            for at in (1..items.len()).rev() {
                items.swap(at, self.below(at + 1));
            }
        }
    }

    type Id = Vec<u8>;

    /// The rules, straight from the ID bytes: give every ID the smallest ID
    /// any of its link lines can see, until nothing changes.
    fn naive_names(pairs: &[(Id, Id)]) -> HashMap<Id, Id> {
        // Number the distinct IDs so a pass over the lines is cheap; every
        // comparison is still between the IDs' bytes.
        let mut number: HashMap<&Id, usize> = HashMap::new();
        let mut ids: Vec<&Id> = Vec::new();
        let mut lines: Vec<(usize, usize)> = Vec::new();
        for (a, b) in pairs {
            let mut number_of = |id| {
                *number.entry(id).or_insert_with(|| {
                    ids.push(id);
                    ids.len() - 1
                })
            };
            lines.push((number_of(a), number_of(b)));
        }
        let mut name: Vec<usize> = (0..ids.len()).collect();
        loop {
            let mut changed = false;
            for &(a, b) in &lines {
                let smallest = if ids[name[a]] <= ids[name[b]] { name[a] } else { name[b] };
                for id in [a, b] {
                    if name[id] != smallest {
                        name[id] = smallest;
                        changed = true;
                    }
                }
            }
            if !changed {
                return ids.iter().zip(&name).map(|(&id, &name)| (id.clone(), ids[name].clone())).collect();
            }
        }
    }

    const PATTERN_CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    /// A random ID: mostly generator-style, sometimes exotic (upper case,
    /// spaces, too long, another prefix, non-ASCII).
    fn random_id(random: &mut Random, exotic_one_in: usize) -> Id {
        let mut id = vec![b'u'];
        // Short IDs make collisions between "different" draws likely, which is wanted.
        let longest = if random.chance(4) { 2 } else { 8 };
        let length = 1 + random.below(longest);
        for _ in 0..length {
            id.push(PATTERN_CHARS[random.below(PATTERN_CHARS.len())]);
        }
        if random.chance(exotic_one_in) {
            match random.below(6) {
                0 => id.make_ascii_uppercase(),
                1 => id.push(b' '),
                2 => id.insert(0, b' '),
                3 => id.extend_from_slice(b"0123456789"),
                4 => id[0] = b'U',
                _ => id.extend_from_slice("é".as_bytes()),
            }
        }
        id
    }

    /// Link lines shaped like the generator's: small trees, chains linked one
    /// after the next, shared devices that merge many people, repeats either
    /// way round and self-links, all shuffled.
    fn random_pairs(random: &mut Random, people: usize, exotic_one_in: usize) -> Vec<(Id, Id)> {
        let mut pairs: Vec<(Id, Id)> = Vec::new();
        let devices: Vec<Id> = (0..3).map(|_| random_id(random, exotic_one_in)).collect();
        for _ in 0..people {
            let chain = random.chance(40);
            let size = if chain { 20 + random.below(200) } else { 1 + random.below(8) };
            let ids: Vec<Id> = (0..size).map(|_| random_id(random, exotic_one_in)).collect();
            for at in 1..size {
                let earlier = if chain { at - 1 } else { random.below(at) };
                pairs.push((ids[earlier].clone(), ids[at].clone()));
            }
            if random.chance(12) {
                pairs.push((ids[random.below(size)].clone(), devices[random.below(devices.len())].clone()));
            }
            if random.chance(20) {
                let id = ids[random.below(size)].clone();
                pairs.push((id.clone(), id));
            }
        }
        for at in 0..pairs.len() {
            if random.chance(12) {
                let (a, b) = pairs[at].clone();
                pairs.push(if random.chance(2) { (a, b) } else { (b, a) });
            }
        }
        random.shuffle(&mut pairs);
        pairs
    }

    /// Builds with both paths and checks every linked ID, and some unlinked
    /// ones, against the naive names.
    fn check(random: &mut Random, pairs: &[(Id, Id)]) {
        let users = Interner::default();
        let code = |id: &[u8]| id_code(id, b'u', &users);
        // Spread the lines over per-thread lists, some of them empty.
        let mut lists: Vec<Vec<(u64, u64)>> = vec![Vec::new(); 1 + random.below(9)];
        for (a, b) in pairs {
            let list = random.below(lists.len());
            lists[list].push((code(a), code(b)));
        }
        let expected = naive_names(pairs);
        let unlinked: Vec<Id> =
            (0..200).map(|_| random_id(random, 4)).filter(|id| !expected.contains_key(id)).collect();
        for (path, table) in [
            ("forest", build_within(&lists, &users, FOREST_MAX_SLOTS)),
            ("general", build_within(&lists, &users, 0)),
        ] {
            for (id, name) in &expected {
                assert_eq!(
                    table.person(code(id)),
                    code(name),
                    "{path}: {:?} should be named {:?}",
                    String::from_utf8_lossy(id),
                    String::from_utf8_lossy(name)
                );
            }
            for id in &unlinked {
                table.prefetch(code(id));
                assert_eq!(table.person(code(id)), code(id), "{path}: unlinked {:?}", String::from_utf8_lossy(id));
            }
        }
    }

    #[test]
    fn random_graphs_match_the_naive_names() {
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        for round in 0..60 {
            let people = [0, 1, 2, 5, 30, 200, 1500][round % 7];
            let exotic_one_in = [2, 5, 50, 2000][round % 4];
            let pairs = random_pairs(&mut random, people, exotic_one_in);
            check(&mut random, &pairs);
        }
    }

    #[test]
    fn exotic_ids_are_compared_by_their_bytes() {
        let id = |text: &str| text.as_bytes().to_vec();
        let pairs = vec![
            // Upper case sorts first: the exotic ID is the name.
            (id("u5"), id("U9")),
            (id("u1"), id("u5")),
            // The exotic IDs here are not the smallest of their groups.
            (id("u7"), id("u7A")),
            (id("u7 "), id("u7A")),
            (id("u8zzzzzzzz"), id("u8")),
            // Only exotic IDs, and one that is a prefix of the other.
            (id("Ub"), id("Ua")),
            (id("Ua"), id("Uaa")),
            // A self-link and a repeat the other way round change nothing.
            (id("U9"), id("U9")),
            (id("U9"), id("u5")),
            (id("u3"), id("u3")),
        ];
        let names = naive_names(&pairs);
        assert_eq!(names[&id("u1")], id("U9"));
        assert_eq!(names[&id("u7A")], id("u7"));
        assert_eq!(names[&id("u7 ")], id("u7"));
        assert_eq!(names[&id("u8zzzzzzzz")], id("u8"));
        assert_eq!(names[&id("Ub")], id("Ua"));
        assert_eq!(names[&id("u3")], id("u3"));
        check(&mut Random(7), &pairs);
    }

    #[test]
    fn long_chains_and_big_groups() {
        let users = Interner::default();
        let code = |number: usize| pattern_code(format!("u{number:07}").as_bytes(), b'u').unwrap();
        // A chain that always links a smaller ID on, the order that makes the
        // deepest trees, then the same IDs upward, and one hub with many spokes.
        let count = 30_000;
        let down: Vec<(u64, u64)> = (1..count).rev().map(|at| (code(at), code(at - 1))).collect();
        let up: Vec<(u64, u64)> = (1..count).map(|at| (code(count + at - 1), code(count + at))).collect();
        let hub: Vec<(u64, u64)> = (1..count).map(|at| (code(3 * count), code(2 * count + at))).collect();
        for max_slots in [FOREST_MAX_SLOTS, 0] {
            let table = build_within(&[down.clone(), Vec::new(), up.clone(), hub.clone()], &users, max_slots);
            for at in 0..count {
                assert_eq!(table.person(code(at)), code(0));
                assert_eq!(table.person(code(count + at)), code(count));
            }
            for at in 1..count {
                assert_eq!(table.person(code(2 * count + at)), code(2 * count + 1));
            }
            assert_eq!(table.person(code(3 * count)), code(2 * count + 1));
            assert_eq!(table.person(code(4 * count)), code(4 * count));
        }
    }

    #[test]
    fn prefetch_changes_no_result() {
        let code = |id: &[u8]| pattern_code(id, b'u').unwrap();
        let pairs = vec![vec![(code(b"u5"), code(b"u3")), (code(b"u3"), code(b"u9"))], vec![(code(b"u7"), code(b"u7"))]];
        let table = build(&pairs, &Interner::default());
        let expected = [(b"u3", b"u3"), (b"u5", b"u3"), (b"u9", b"u3"), (b"u7", b"u7"), (b"u1", b"u1")];
        for (user, _) in expected {
            table.prefetch(code(user));
        }
        for (user, person) in expected {
            assert_eq!(table.person(code(user)), code(person));
        }
        // An empty table still has a line to prefetch.
        let empty = build(&[], &Interner::default());
        empty.prefetch(code(b"u1"));
        assert_eq!(empty.person(code(b"u1")), code(b"u1"));
    }
}

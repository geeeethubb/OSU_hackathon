//! Linked IDs: union-find over the link pairs, then a flat lookup table from
//! a user code to the code of its person's name (the byte-wise smallest ID).
//!
//! The union-find lives inside one hash table (`Forest`): a node holds an ID
//! and the slot of its parent, so finding an ID and reaching its tree node is
//! one memory access, and the lines that the pairs coming up will need are
//! prefetched while the current pair is joined.
//!
//! A build can also be given a grouping of the IDs (`build_grouped`). A link
//! component is then simple when all its members are in one group and complex
//! otherwise, and `LinkTable::lookup` reports which with the name.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use super::codes::{id_cmp, mix, unmix, Interner, CODE_BITS, CODE_MASK, EXOTIC_BASE};
use crate::reference::FastHasher;

const HASH_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
/// Set in a stored name when the ID's link component is complex.
const COMPLEX_BIT: u64 = 1 << 63;

/// Open-addressing table of `(user code, name code)` for every linked ID whose
/// person is named by a different ID, and for every member of a complex
/// component, its own name included; those names carry `COMPLEX_BIT`.
/// Anything not in the table names itself and is simple.
pub struct LinkTable {
    slots: Vec<[u64; 2]>,
    mask: usize,
    shift: u32,
    /// Every member of every complex component.
    complex: Vec<u64>,
}

impl LinkTable {
    fn with_capacity(entries: usize) -> Self {
        let bits = (entries.max(4) * 2).next_power_of_two().trailing_zeros();
        let capacity = 1usize << bits;
        Self { slots: vec![[0, 0]; capacity], mask: capacity - 1, shift: 64 - bits, complex: Vec::new() }
    }

    /// `name` is the name code, with `COMPLEX_BIT` for a complex component.
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
        self.lookup(user).0
    }

    /// The name code of the person `user` belongs to, and whether that
    /// person's link component is complex, in one probe. An ID that is in no
    /// link line names itself and is simple.
    #[inline(always)]
    pub fn lookup(&self, user: u64) -> (u64, bool) {
        let mut at = (user.wrapping_mul(HASH_MULTIPLIER) >> self.shift) as usize;
        loop {
            // SAFETY: `at` is always masked into the table.
            let slot = unsafe { self.slots.get_unchecked(at) };
            if slot[0] == user {
                return (slot[1] & CODE_MASK, slot[1] & COMPLEX_BIT != 0);
            }
            if slot[0] == 0 {
                return (user, false);
            }
            at = (at + 1) & self.mask;
        }
    }

    /// The codes of all members of complex components, in no particular
    /// order, each once. Empty after a plain `build`.
    pub fn complex_members(&self) -> &[u64] {
        &self.complex
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

/// How many pairs ahead of the one being located the home lines are prefetched.
const HOME_LOOKAHEAD: usize = 32;
/// Pairs wait this many deep between being located and being joined, while
/// the lines of their parents load. A power of two.
const JOIN_DELAY: usize = 16;
/// How many slots ahead of the one being named its parent is prefetched.
const SCAN_LOOKAHEAD: usize = 24;
/// Renamed IDs wait this many deep while their lookup-table slots load.
const INSERT_BATCH: usize = 32;

/// Slots in one line of the forest: a cache line of 8-byte nodes.
const LINE_SLOTS: usize = 8;
/// A node is stored at most this many lines after its home line.
const MAX_SPILL: usize = 15;
/// A node, from the low bits up: the slot of its parent (`parent_bits`, which
/// grows with the forest); the bits of the ID's hash that its home line does
/// not give (which shrink as fast, so they always end at `SPILL_SHIFT`); how
/// many lines after its home line the node sits (4 bits); two flags at the top.
const SPILL_SHIFT: u32 = CODE_BITS + 4;
const BELOW_SPILL: u64 = (1 << SPILL_SHIFT) - 1;
/// On a root: its tree is complex.
const MARKED: u64 = 1 << 62;
/// On every node, so that none is 0, which is the empty slot.
const OCCUPIED: u64 = 1 << 63;
/// A forest has 2^bits home lines, with bits between these (1 KiB to 8 GiB).
const MIN_LINE_BITS: u32 = 4;
const MAX_LINE_BITS: u32 = 27;

/// Bit `at` is set when node `at` of the line is empty or, under `key_mask`,
/// equals `key`.
#[inline(always)]
fn line_hits(line: *const u64, key_mask: u64, key: u64) -> u32 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    // SAFETY: a line is `LINE_SLOTS` nodes of the forest and starts on a
    // 64-byte boundary, so both halves are readable and 32-byte aligned.
    unsafe {
        use core::arch::x86_64::*;
        let low = _mm256_load_si256(line as *const __m256i);
        let high = _mm256_load_si256(line.add(4) as *const __m256i);
        let mask = _mm256_set1_epi64x(key_mask as i64);
        let want = _mm256_set1_epi64x(key as i64);
        let zero = _mm256_setzero_si256();
        let low_hits =
            _mm256_or_si256(_mm256_cmpeq_epi64(_mm256_and_si256(low, mask), want), _mm256_cmpeq_epi64(low, zero));
        let high_hits =
            _mm256_or_si256(_mm256_cmpeq_epi64(_mm256_and_si256(high, mask), want), _mm256_cmpeq_epi64(high, zero));
        let low_bits = _mm256_movemask_pd(_mm256_castsi256_pd(low_hits));
        let high_bits = _mm256_movemask_pd(_mm256_castsi256_pd(high_hits));
        (low_bits | (high_bits << 4)) as u32
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    line_hits_plain(line, key_mask, key)
}

/// `line_hits` without vector instructions.
#[cfg(any(test, not(all(target_arch = "x86_64", target_feature = "avx2"))))]
fn line_hits_plain(line: *const u64, key_mask: u64, key: u64) -> u32 {
    let mut hits = 0;
    for at in 0..LINE_SLOTS {
        // SAFETY: a line is `LINE_SLOTS` nodes of the forest.
        let node = unsafe { *line.add(at) };
        hits |= u32::from(node & key_mask == key || node == 0) << at;
    }
    hits
}

/// Union-find over the linked IDs, stored in one hash table of 8-byte nodes
/// in lines of eight. An ID's hash picks its home line, and the node sits in
/// the first line from there that has room. The node keeps only the hash bits
/// its home line does not give, which leaves room for its parent's slot at
/// any table size. A root is its own parent. Joining always puts the
/// byte-wise smaller root on top, so the root of a tree is the name of its
/// person. A root is `MARKED` when its tree is complex.
struct Forest<'a> {
    /// Owns the memory; `nodes` starts at its first 64-byte boundary.
    _store: Vec<u64>,
    nodes: *mut u64,
    slot_count: usize,
    parent_bits: u32,
    /// Hash bits below the ones that pick the home line.
    rem_bits: u32,
    parent_mask: u64,
    /// The bits of a node that say which ID it holds: occupied, spill, hash.
    key_mask: u64,
    /// Exotic IDs by index, to compare them by their bytes.
    exotic: &'a [Box<[u8]>],
    /// An ID found no room within `MAX_SPILL` lines of its home line; the
    /// forest is of no use from then on.
    overflowed: bool,
    /// Joins that made one tree of two. Each leaves one more ID that is not
    /// a root, so this is the number of IDs named by another ID, whatever
    /// the order of the lines and however many repeat or link an ID to itself.
    merges: usize,
    /// Trees whose root is `MARKED`.
    complex_trees: usize,
}

impl<'a> Forest<'a> {
    fn new(line_bits: u32, exotic: &'a [Box<[u8]>]) -> Self {
        debug_assert!((MIN_LINE_BITS..=MAX_LINE_BITS).contains(&line_bits));
        // Lines after the last home line take what spills out of it.
        let slot_count = ((1usize << line_bits) + MAX_SPILL + 1) * LINE_SLOTS;
        let mut store = vec![0u64; slot_count + LINE_SLOTS];
        let skew = (64 - store.as_ptr() as usize % 64) % 64 / 8;
        // SAFETY: `skew` is below `LINE_SLOTS`, the words added above.
        let nodes = unsafe { store.as_mut_ptr().add(skew) };
        // The slots number under 2^(line_bits + 4), since there are at least 16 home lines.
        let parent_bits = line_bits + 4;
        let parent_mask = (1u64 << parent_bits) - 1;
        Forest {
            _store: store,
            nodes,
            slot_count,
            parent_bits,
            rem_bits: CODE_BITS - line_bits,
            parent_mask,
            key_mask: (OCCUPIED | BELOW_SPILL | (MAX_SPILL as u64) << SPILL_SHIFT) & !parent_mask,
            exotic,
            overflowed: false,
            merges: 0,
            complex_trees: 0,
        }
    }

    #[inline(always)]
    fn node(&self, slot: usize) -> u64 {
        debug_assert!(slot < self.slot_count);
        // SAFETY: every slot number in use is below `slot_count`.
        unsafe { *self.nodes.add(slot) }
    }

    #[inline(always)]
    fn set_node(&mut self, slot: usize, node: u64) {
        debug_assert!(slot < self.slot_count);
        // SAFETY: as in `node`.
        unsafe { *self.nodes.add(slot) = node }
    }

    #[inline(always)]
    fn prefetch_home(&self, code: u64) {
        prefetch_line(self.nodes.wrapping_add((mix(code) >> self.rem_bits) as usize * LINE_SLOTS));
    }

    /// Starts loading the node of slot `at`'s parent.
    #[inline(always)]
    fn prefetch_parent(&self, at: usize) {
        prefetch_line(self.nodes.wrapping_add((self.node(at) & self.parent_mask) as usize));
    }

    /// The code of the ID in `slot`, whose node is `node`.
    #[inline(always)]
    fn code_at(&self, slot: usize, node: u64) -> u64 {
        let home_line = (slot / LINE_SLOTS) as u64 - ((node >> SPILL_SHIFT) & MAX_SPILL as u64);
        unmix((home_line << self.rem_bits) | ((node & BELOW_SPILL) >> self.parent_bits))
    }

    /// The slot of `code`, which becomes a new root if it was not there.
    /// Sets `overflowed` instead when there is no room for it.
    #[inline(always)]
    fn locate(&mut self, code: u64) -> usize {
        let hash = mix(code);
        let mut line = (hash >> self.rem_bits) as usize;
        // The hash is below 2^CODE_BITS, so this keeps its low `rem_bits`.
        let mut key = OCCUPIED | ((hash << self.parent_bits) & BELOW_SPILL);
        for _ in 0..=MAX_SPILL {
            let hits = line_hits(self.nodes.wrapping_add(line * LINE_SLOTS), self.key_mask, key);
            if hits != 0 {
                // A line fills from its first slot, so the lowest hit is the
                // ID itself if it is there, and the first empty slot if not.
                let slot = line * LINE_SLOTS + hits.trailing_zeros() as usize;
                let node = self.node(slot);
                self.set_node(slot, if node == 0 { key | slot as u64 } else { node });
                return slot;
            }
            line += 1;
            key += 1 << SPILL_SHIFT;
        }
        self.overflowed = true;
        0
    }

    /// The root of the tree that the occupied slot `at` is in.
    #[inline(always)]
    fn find(&mut self, at: usize) -> usize {
        let parent = (self.node(at) & self.parent_mask) as usize;
        let grandparent = (self.node(parent) & self.parent_mask) as usize;
        if grandparent == parent {
            // `at` is a root or the child of one, as nearly always.
            return parent;
        }
        self.find_far(at)
    }

    /// `find` for deeper trees. Halves the path on the way.
    #[inline(never)]
    fn find_far(&mut self, mut at: usize) -> usize {
        loop {
            let node = self.node(at);
            let parent = (node & self.parent_mask) as usize;
            let grandparent = (self.node(parent) & self.parent_mask) as usize;
            if grandparent == parent {
                return parent;
            }
            self.set_node(at, (node & !self.parent_mask) | grandparent as u64);
            at = grandparent;
        }
    }

    /// Marks the tree that slot `at` is in as complex.
    #[cold]
    fn mark_tree(&mut self, at: usize) {
        let root = self.find(at);
        let node = self.node(root);
        self.complex_trees += usize::from(node & MARKED == 0);
        self.set_node(root, node | MARKED);
    }

    /// Makes the trees of slots `a` and `b` one, rooted at the smaller ID.
    #[inline(always)]
    fn join(&mut self, a: usize, b: usize, group: &impl Fn(u64) -> Option<u64>) {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        let (node_a, node_b) = (self.node(a), self.node(b));
        let (code_a, code_b) = (self.code_at(a, node_a), self.code_at(b, node_b));
        // Pattern codes order like their IDs; exotic IDs are compared by bytes.
        let a_is_smaller = if code_a.max(code_b) < EXOTIC_BASE {
            code_a < code_b
        } else {
            id_cmp(code_a, code_b, b'u', self.exotic) == Ordering::Less
        };
        let (root, root_node, child, child_node) =
            if a_is_smaller { (a, node_a, b, node_b) } else { (b, node_b, a, node_a) };
        self.set_node(child, (child_node & !self.parent_mask) | root as u64);
        self.merges += 1;
        // A tree with no mark is one ID, or has all its members in its root's
        // group. So two unmarked trees make an unmarked tree exactly when
        // their roots share a group.
        let same_group = matches!((group(code_a), group(code_b)), (Some(one), Some(other)) if one == other);
        if !same_group || (node_a | node_b) & MARKED != 0 {
            self.set_node(root, root_node | MARKED);
            // One complex tree where there were none, one or two.
            self.complex_trees += 1;
            self.complex_trees -= usize::from(node_a & MARKED != 0) + usize::from(node_b & MARKED != 0);
        }
    }
}

/// Joins every pair, names each group by its byte-wise smallest ID.
pub fn build(pairs: &[Vec<(u64, u64)>], users: &Interner) -> LinkTable {
    // One group for everybody: every component is simple.
    build_grouped(pairs, users, &|_| Some(0))
}

/// `build`, which also sorts the link components into simple and complex.
/// `group` puts an ID's code in a group, or in none. A component is simple
/// when all its members are in one and the same group, and complex when any
/// member is in no group or two members are in different groups (an ID
/// linked only to itself is a component of one). `LinkTable::lookup` returns
/// the flag with the name, and `LinkTable::complex_members` lists the members
/// of the complex components. Names do not depend on `group`.
pub fn build_grouped(
    pairs: &[Vec<(u64, u64)>],
    users: &Interner,
    group: &impl Fn(u64) -> Option<u64>,
) -> LinkTable {
    build_within(pairs, users, group, Limits::NORMAL)
}

/// What a build may use, so that the tests can reach every path.
#[derive(Clone, Copy)]
struct Limits {
    /// The first forest is this many times half the size it should be.
    squeeze: u32,
    /// No forest has more than 2^this home lines.
    max_line_bits: u32,
}

impl Limits {
    const NORMAL: Limits = Limits { squeeze: 0, max_line_bits: MAX_LINE_BITS };
}

fn build_within(
    pairs: &[Vec<(u64, u64)>],
    users: &Interner,
    group: &impl Fn(u64) -> Option<u64>,
    limits: Limits,
) -> LinkTable {
    let pair_count: usize = pairs.iter().map(Vec::len).sum();
    // Exotic IDs (upper case and the like) sort by their bytes, not their code.
    let exotic = users.snapshot();
    // A pair brings at most two new IDs: room for all of them at 8/9 full,
    // rounded up to a power of two, which leaves a usual input half full.
    let lines = (pair_count * 2 + pair_count / 4) / LINE_SLOTS + 1;
    let mut line_bits = lines.next_power_of_two().trailing_zeros().saturating_sub(limits.squeeze).max(MIN_LINE_BITS);
    // IDs whose hashes crowd one place can overflow a forest. Twice the
    // room nearly always cures that; if not, the hash map takes any input.
    for _ in 0..2 {
        if line_bits > limits.max_line_bits {
            break;
        }
        if let Some(table) = build_in_forest(pairs, &exotic, group, line_bits) {
            return table;
        }
        line_bits += 1;
    }
    build_general(pairs, &exotic, group)
}

/// The build in a forest of 2^`line_bits` home lines, or `None` if an ID
/// found no room in it.
fn build_in_forest(
    pairs: &[Vec<(u64, u64)>],
    exotic: &[Box<[u8]>],
    group: &impl Fn(u64) -> Option<u64>,
    line_bits: u32,
) -> Option<LinkTable> {
    let mut forest = Forest::new(line_bits, exotic);
    let mut located = [(0u32, 0u32); JOIN_DELAY];
    for list in pairs {
        // Three steps, each on a different pair, so that the memory a step
        // needs was asked for by an earlier one: the home lines of the pair
        // `HOME_LOOKAHEAD` ahead are prefetched; this pair's IDs are located
        // (and added if new) and their parents' nodes prefetched; and the
        // pair located `JOIN_DELAY` ago is joined.
        for at in 0..list.len() + JOIN_DELAY {
            if let Some(&(a, b)) = list.get(at + HOME_LOOKAHEAD) {
                forest.prefetch_home(a);
                forest.prefetch_home(b);
            }
            if at >= JOIN_DELAY {
                let (a, b) = located[at % JOIN_DELAY];
                forest.join(a as usize, b as usize, group);
            }
            if let Some(&(code_a, code_b)) = list.get(at) {
                let (a, b) = (forest.locate(code_a), forest.locate(code_b));
                if forest.overflowed {
                    return None;
                }
                // No join ever looks at an ID that is linked only to itself.
                if code_a == code_b && group(code_a).is_none() {
                    forest.mark_tree(a);
                }
                forest.prefetch_parent(a);
                forest.prefetch_parent(b);
                located[at % JOIN_DELAY] = (a as u32, b as u32);
            }
        }
    }

    // The table holds every ID that is not the root of its tree, under the
    // root's name, and the root of every complex tree under its own. The
    // joins counted both, so no pass over the forest is needed to size it.
    let entries = forest.merges + forest.complex_trees;
    let mut table = LinkTable::with_capacity(entries);
    let mut batch = [(0u64, 0u64); INSERT_BATCH];
    let mut waiting = 0;
    let mut stored = 0;
    for slot in 0..forest.slot_count {
        if slot + SCAN_LOOKAHEAD < forest.slot_count {
            forest.prefetch_parent(slot + SCAN_LOOKAHEAD);
        }
        let node = forest.node(slot);
        if node == 0 {
            continue;
        }
        let root = forest.find(slot);
        let root_node = forest.node(root);
        let complex = root_node & MARKED != 0;
        if root == slot && !complex {
            continue;
        }
        let (user, name) = (forest.code_at(slot, node), forest.code_at(root, root_node));
        if complex {
            table.complex.push(user);
        }
        table.prefetch(user);
        batch[waiting] = (user, if complex { name | COMPLEX_BIT } else { name });
        waiting += 1;
        stored += 1;
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
    debug_assert_eq!(stored, entries, "the joins miscounted the table's entries");
    Some(table)
}

fn find(parent: &mut [u32], mut at: u32) -> u32 {
    while parent[at as usize] != at {
        let grandparent = parent[parent[at as usize] as usize];
        parent[at as usize] = grandparent;
        at = grandparent;
    }
    at
}

/// The same result with a hash map and dense indices: slower, but it takes
/// any input.
fn build_general(
    pairs: &[Vec<(u64, u64)>],
    exotic: &[Box<[u8]>],
    group: &impl Fn(u64) -> Option<u64>,
) -> LinkTable {
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

    let mut name: Vec<u64> = codes.clone();
    let mut root_of: Vec<u32> = Vec::with_capacity(codes.len());
    for at in 0..codes.len() {
        let root = find(&mut parent, at as u32);
        root_of.push(root);
        if id_cmp(codes[at], name[root as usize], b'u', exotic) == Ordering::Less {
            name[root as usize] = codes[at];
        }
    }
    // A component is simple when every member is in its root's group.
    let mut complex = vec![false; codes.len()];
    for at in 0..codes.len() {
        let root = root_of[at] as usize;
        if !matches!((group(codes[at]), group(codes[root])), (Some(one), Some(other)) if one == other) {
            complex[root] = true;
        }
    }
    let stored = |at: usize| name[root_of[at] as usize] != codes[at] || complex[root_of[at] as usize];
    let mut table = LinkTable::with_capacity((0..codes.len()).filter(|&at| stored(at)).count());
    for at in 0..codes.len() {
        if stored(at) {
            let root = root_of[at] as usize;
            if complex[root] {
                table.complex.push(codes[at]);
            }
            table.insert(codes[at], if complex[root] { name[root] | COMPLEX_BIT } else { name[root] });
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fast::codes::{id_code, pattern_code};
    use std::collections::{HashMap, HashSet};

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
    /// The group of every ID that some link line names.
    type Groups = HashMap<Id, Option<u64>>;

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

    /// The members of the complex components, straight from the definition:
    /// list each component's groups, then look for a missing or a second one.
    fn naive_complex(names: &HashMap<Id, Id>, groups: &Groups) -> HashSet<Id> {
        let mut seen: HashMap<&Id, Vec<Option<u64>>> = HashMap::new();
        for (id, name) in names {
            seen.entry(name).or_default().push(groups[id]);
        }
        let complex = |name: &Id| seen[name].iter().any(|group| group.is_none() || *group != seen[name][0]);
        names.iter().filter(|(_, name)| complex(name)).map(|(id, _)| id.clone()).collect()
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
    /// way round and self-links, all shuffled. Every person is a group; half
    /// of the people have some IDs in no group or in another one.
    fn random_pairs(random: &mut Random, people: usize, exotic_one_in: usize) -> (Vec<(Id, Id)>, Groups) {
        let mut pairs: Vec<(Id, Id)> = Vec::new();
        let mut groups = Groups::new();
        let devices: Vec<Id> = (0..3).map(|_| random_id(random, exotic_one_in)).collect();
        for person in 0..people {
            let chain = random.chance(40);
            let size = if chain { 20 + random.below(200) } else { 1 + random.below(8) };
            let ids: Vec<Id> = (0..size).map(|_| random_id(random, exotic_one_in)).collect();
            let tidy = random.chance(2);
            for id in &ids {
                let group = if tidy || !random.chance(6) {
                    Some(person as u64)
                } else if random.chance(2) {
                    None
                } else {
                    Some(1_000_000 + random.below(3) as u64)
                };
                // The same bytes can be drawn twice: the first group stays.
                groups.entry(id.clone()).or_insert(group);
            }
            for at in 1..size {
                let earlier = if chain { at - 1 } else { random.below(at) };
                pairs.push((ids[earlier].clone(), ids[at].clone()));
            }
            if random.chance(12) {
                let device = random.below(devices.len());
                groups.entry(devices[device].clone()).or_insert(Some(2_000_000 + device as u64));
                pairs.push((ids[random.below(size)].clone(), devices[device].clone()));
            }
            if size == 1 || random.chance(20) {
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
        (pairs, groups)
    }

    /// The build paths, by the limits that select them: a forest of the
    /// usual size; one that starts at half that, which overflows on some
    /// inputs and is then doubled; one far too small, which overflows twice
    /// on all but tiny inputs and leaves the work to the hash map; and the
    /// hash map alone.
    const PATHS: [(&str, Limits); 4] = [
        ("forest", Limits::NORMAL),
        ("forest from half size", Limits { squeeze: 1, ..Limits::NORMAL }),
        ("forest far too small", Limits { squeeze: 6, ..Limits::NORMAL }),
        ("hash map", Limits { squeeze: 0, max_line_bits: 0 }),
    ];

    /// Builds on every path and checks every linked ID, and some unlinked
    /// ones, against the naive names and flags.
    fn check(random: &mut Random, pairs: &[(Id, Id)], groups: &Groups) {
        let users = Interner::default();
        let code = |id: &[u8]| id_code(id, b'u', &users);
        // Spread the lines over per-thread lists, some of them empty.
        let mut lists: Vec<Vec<(u64, u64)>> = vec![Vec::new(); 1 + random.below(9)];
        for (a, b) in pairs {
            let list = random.below(lists.len());
            lists[list].push((code(a), code(b)));
        }
        let group_of_code: HashMap<u64, Option<u64>> = groups.iter().map(|(id, &group)| (code(id), group)).collect();
        // A build may only ask for the group of an ID it was given.
        let group = |code: u64| group_of_code[&code];
        let names = naive_names(pairs);
        let complex = naive_complex(&names, groups);
        let complex_codes: HashSet<u64> = complex.iter().map(|id| code(id)).collect();
        let unlinked: Vec<Id> = (0..200).map(|_| random_id(random, 4)).filter(|id| !names.contains_key(id)).collect();
        for (path, limits) in PATHS {
            let table = build_within(&lists, &users, &group, limits);
            for (id, name) in &names {
                let shown = String::from_utf8_lossy(id);
                assert_eq!(table.lookup(code(id)), (code(name), complex.contains(id)), "{path}: {shown:?}");
                assert_eq!(table.person(code(id)), code(name), "{path}: {shown:?}");
            }
            for id in &unlinked {
                table.prefetch(code(id));
                let shown = String::from_utf8_lossy(id);
                assert_eq!(table.lookup(code(id)), (code(id), false), "{path}: unlinked {shown:?}");
            }
            assert_eq!(table.complex_members().len(), complex_codes.len(), "{path}: complex members listed once");
            assert_eq!(table.complex_members().iter().copied().collect::<HashSet<u64>>(), complex_codes, "{path}");
        }
        // Without groups the names are the same and nothing is complex.
        let table = build(&lists, &users);
        for (id, name) in &names {
            assert_eq!(table.lookup(code(id)), (code(name), false));
        }
        assert!(table.complex_members().is_empty());
    }

    #[test]
    fn random_graphs_match_the_naive_names_and_flags() {
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        for round in 0..60 {
            let people = [0, 1, 2, 5, 30, 200, 1500][round % 7];
            let exotic_one_in = [2, 5, 50, 2000][round % 4];
            let (pairs, groups) = random_pairs(&mut random, people, exotic_one_in);
            check(&mut random, &pairs, &groups);
        }
    }

    #[test]
    fn names_and_flags_by_hand() {
        let id = |text: &str| text.as_bytes().to_vec();
        let lines = [
            // Upper case sorts first: the exotic ID is the name. One group: simple.
            ("u5", "U9"),
            ("u1", "u5"),
            // A self-link and a repeat the other way round change nothing.
            ("U9", "U9"),
            ("U9", "u5"),
            // Exotic IDs that are not the smallest; `u7A` is in no group: complex.
            ("u7", "u7A"),
            ("u7 ", "u7A"),
            // Two groups: complex.
            ("u8zzzzzzzz", "u8"),
            // Only exotic IDs, one a prefix of the other, one group: simple.
            ("Ub", "Ua"),
            ("Ua", "Uaa"),
            // IDs linked only to themselves: simple in a group, complex in none.
            ("u3", "u3"),
            ("u4", "u4"),
        ];
        let groups: Groups = [
            ("u5", Some(1)),
            ("U9", Some(1)),
            ("u1", Some(1)),
            ("u7", Some(2)),
            ("u7A", None),
            ("u7 ", Some(2)),
            ("u8zzzzzzzz", Some(3)),
            ("u8", Some(4)),
            ("Ua", Some(5)),
            ("Ub", Some(5)),
            ("Uaa", Some(5)),
            ("u3", Some(6)),
            ("u4", None),
        ]
        .into_iter()
        .map(|(text, group)| (id(text), group))
        .collect();
        let expected = [
            ("u5", "U9", false),
            ("U9", "U9", false),
            ("u1", "U9", false),
            ("u7", "u7", true),
            ("u7A", "u7", true),
            ("u7 ", "u7", true),
            ("u8zzzzzzzz", "u8", true),
            ("u8", "u8", true),
            ("Ua", "Ua", false),
            ("Ub", "Ua", false),
            ("Uaa", "Ua", false),
            ("u3", "u3", false),
            ("u4", "u4", true),
            // In no link line at all.
            ("u9", "u9", false),
            ("U1", "U1", false),
        ];
        let pairs: Vec<(Id, Id)> = lines.iter().map(|(a, b)| (id(a), id(b))).collect();
        // The naive helpers agree with the expectations written out above.
        let names = naive_names(&pairs);
        let complex = naive_complex(&names, &groups);
        for (user, name, flag) in expected.iter().filter(|(user, _, _)| names.contains_key(&id(user))) {
            assert_eq!((&names[&id(user)], complex.contains(&id(user))), (&id(name), *flag), "{user}");
        }
        let users = Interner::default();
        let code = |text: &str| id_code(text.as_bytes(), b'u', &users);
        let list: Vec<(u64, u64)> = lines.iter().map(|(a, b)| (code(a), code(b))).collect();
        let group_of_code: HashMap<u64, Option<u64>> =
            groups.iter().map(|(id, &group)| (id_code(id, b'u', &users), group)).collect();
        for (path, limits) in PATHS {
            let table = build_within(&[list.clone()], &users, &|code| group_of_code[&code], limits);
            for (user, name, flag) in expected {
                assert_eq!(table.lookup(code(user)), (code(name), flag), "{path}: {user}");
                assert_eq!(table.person(code(user)), code(name), "{path}: {user}");
            }
            let mut members: Vec<u64> = table.complex_members().to_vec();
            members.sort_unstable();
            let mut expected_members: Vec<u64> = ["u7", "u7A", "u7 ", "u8zzzzzzzz", "u8", "u4"].map(code).to_vec();
            expected_members.sort_unstable();
            assert_eq!(members, expected_members, "{path}");
        }
        check(&mut Random(7), &pairs, &groups);
    }

    #[test]
    fn nothing_but_complex_singletons() {
        // Every ID is linked only to itself and is in no group: each is a
        // complex component of one, stored under its own name, so the table
        // must make room for IDs that are not renamed.
        let users = Interner::default();
        let code = |number: usize| pattern_code(format!("u{number}").as_bytes(), b'u').unwrap();
        let list: Vec<(u64, u64)> = (0..1000).map(|at| (code(at), code(at))).collect();
        for (path, limits) in PATHS {
            let table = build_within(&[list.clone()], &users, &|_| None, limits);
            for at in 0..1000 {
                assert_eq!(table.lookup(code(at)), (code(at), true), "{path}");
            }
            assert_eq!(table.lookup(code(1000)), (code(1000), false), "{path}");
            assert_eq!(table.complex_members().len(), 1000, "{path}");
        }
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
        let lists = [down, Vec::new(), up, hub];
        // The hub is in no group, which makes its whole component complex;
        // each chain is one group.
        let hub_code = code(3 * count);
        let group = |member: u64| if member == hub_code { None } else { Some(u64::from(member >= code(count))) };
        for (path, limits) in PATHS {
            let table = build_within(&lists, &users, &group, limits);
            for at in 0..count {
                assert_eq!(table.lookup(code(at)), (code(0), false), "{path}");
                assert_eq!(table.lookup(code(count + at)), (code(count), false), "{path}");
            }
            for at in 1..count {
                assert_eq!(table.lookup(code(2 * count + at)), (code(2 * count + 1), true), "{path}");
            }
            assert_eq!(table.lookup(hub_code), (code(2 * count + 1), true), "{path}");
            assert_eq!(table.lookup(code(4 * count)), (code(4 * count), false), "{path}");
            assert_eq!(table.complex_members().len(), count, "{path}");
        }
    }

    #[test]
    fn a_forest_without_room_says_so() {
        // 5000 IDs cannot fit the smallest forest (256 slots), and half of
        // them cannot fit one of half the usual size.
        let code = |number: usize| pattern_code(format!("u{number}").as_bytes(), b'u').unwrap();
        let list: Vec<(u64, u64)> = (0..2500).map(|at| (code(2 * at), code(2 * at + 1))).collect();
        assert!(build_in_forest(&[list.clone()], &[], &|_| Some(0), MIN_LINE_BITS).is_none());
        assert!(build_in_forest(&[list.clone()], &[], &|_| Some(0), 9).is_none());
        let table = build_in_forest(&[list], &[], &|_| Some(0), 10).expect("room for 5000 IDs in 8192 slots");
        for at in 0..2500 {
            assert_eq!(table.lookup(code(2 * at)), (code(2 * at), false));
            assert_eq!(table.lookup(code(2 * at + 1)), (code(2 * at), false));
        }
    }

    #[test]
    fn line_hits_agree_with_the_plain_version() {
        #[repr(align(64))]
        struct Line([u64; LINE_SLOTS]);
        let mut random = Random(99);
        for round in 0..20_000 {
            // A few distinct keys, so that hits are common, with noise in the
            // bits the mask leaves out.
            let parent_bits = 8 + (round % 20) as u32;
            let parent_mask = (1u64 << parent_bits) - 1;
            let key_mask = (OCCUPIED | BELOW_SPILL | (MAX_SPILL as u64) << SPILL_SHIFT) & !parent_mask;
            let keys: Vec<u64> = (0..3).map(|_| (random.next() & key_mask) | OCCUPIED).collect();
            let mut line = Line([0; LINE_SLOTS]);
            for node in line.0.iter_mut() {
                *node = match random.below(4) {
                    0 => 0,
                    1 => random.next() | OCCUPIED,
                    _ => keys[random.below(3)] | (random.next() & (parent_mask | MARKED)),
                };
            }
            for &key in &keys {
                let plain = line_hits_plain(line.0.as_ptr(), key_mask, key);
                assert_eq!(line_hits(line.0.as_ptr(), key_mask, key), plain);
            }
        }
    }

    #[test]
    fn prefetch_changes_no_result() {
        let code = |id: &[u8]| pattern_code(id, b'u').unwrap();
        let pairs =
            vec![vec![(code(b"u5"), code(b"u3")), (code(b"u3"), code(b"u9"))], vec![(code(b"u7"), code(b"u7"))]];
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

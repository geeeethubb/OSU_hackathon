//! Linked IDs: union-find over the link pairs, then a flat lookup table from
//! a user code to the code of its person's name (the byte-wise smallest ID).
//!
//! The union-find lives inside one open-addressing table (`Forest`): a slot
//! holds an ID's code and the slot of its parent, so finding an ID and
//! reaching its tree node is one memory access, and the home slots of the
//! pairs coming up are prefetched while the current pair is joined.
//!
//! A build can also be given a grouping of the IDs (`build_grouped`). A link
//! component is then simple when all its members are in one group and complex
//! otherwise, and `LinkTable::lookup` reports which with the name.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use super::codes::{id_cmp, mix, Interner, CODE_BITS, CODE_MASK, EXOTIC_BASE};
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

    /// TEMPORARY (the links branch brings the real one): calls
    /// `visit(member code, name code, complex)` for every stored ID, and for
    /// the name of every simple component (possibly more than once).
    pub fn for_each_member(&self, mut visit: impl FnMut(u64, u64, bool)) {
        for slot in &self.slots {
            if slot[0] != 0 {
                let complex = slot[1] & COMPLEX_BIT != 0;
                visit(slot[0], slot[1] & CODE_MASK, complex);
                if !complex {
                    visit(slot[1] & CODE_MASK, slot[1] & CODE_MASK, false);
                }
            }
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

/// How many pairs ahead of the one being joined the home slots are prefetched.
const PAIR_LOOKAHEAD: usize = 16;
/// How many slots ahead of the one being named its parent is prefetched.
const SCAN_LOOKAHEAD: usize = 24;
/// Renamed IDs wait this many deep while their lookup-table slots load.
const INSERT_BATCH: usize = 32;

/// One slot of a forest: an ID's code and the slot of its parent. A root is
/// its own parent, and the empty slot is all zero bits (codes are never 0).
trait Node: Copy {
    const EMPTY: Self;
    /// The most slots a forest of these nodes can have.
    const MAX_SLOTS: usize;
    fn new(code: u64, parent: usize) -> Self;
    fn is_empty(self) -> bool;
    fn code(self) -> u64;
    fn parent(self) -> usize;
    fn with_parent(self, parent: usize) -> Self;
}

/// Parent bits of a narrow node; the code is above them.
const NARROW_PARENT_BITS: u32 = 64 - CODE_BITS;
const NARROW_PARENT_MASK: u64 = (1 << NARROW_PARENT_BITS) - 1;

/// Eight bytes, `code << 22 | parent`: half the memory of a wide node, for
/// forests of up to 2^22 slots (about 1.86M link lines).
impl Node for u64 {
    const EMPTY: Self = 0;
    const MAX_SLOTS: usize = 1 << NARROW_PARENT_BITS;

    #[inline(always)]
    fn new(code: u64, parent: usize) -> Self {
        (code << NARROW_PARENT_BITS) | parent as u64
    }

    #[inline(always)]
    fn is_empty(self) -> bool {
        self == 0
    }

    #[inline(always)]
    fn code(self) -> u64 {
        self >> NARROW_PARENT_BITS
    }

    #[inline(always)]
    fn parent(self) -> usize {
        (self & NARROW_PARENT_MASK) as usize
    }

    #[inline(always)]
    fn with_parent(self, parent: usize) -> Self {
        (self & !NARROW_PARENT_MASK) | parent as u64
    }
}

/// Sixteen bytes, `[code, parent]`, for larger forests.
impl Node for [u64; 2] {
    const EMPTY: Self = [0, 0];
    const MAX_SLOTS: usize = 1 << 32;

    #[inline(always)]
    fn new(code: u64, parent: usize) -> Self {
        [code, parent as u64]
    }

    #[inline(always)]
    fn is_empty(self) -> bool {
        self[0] == 0
    }

    #[inline(always)]
    fn code(self) -> u64 {
        self[0]
    }

    #[inline(always)]
    fn parent(self) -> usize {
        self[1] as usize
    }

    #[inline(always)]
    fn with_parent(self, parent: usize) -> Self {
        [self[0], parent as u64]
    }
}

/// Union-find over the linked IDs, stored in one open-addressing table of
/// nodes. Joining always puts the byte-wise smaller root on top, so the root
/// of a tree is the name of its person.
struct Forest<'a, N: Node> {
    slots: Vec<N>,
    /// One bit per slot, meaningful for roots: the tree is complex.
    complex: Vec<u64>,
    /// Exotic IDs by index, to compare them by their bytes.
    exotic: &'a [Box<[u8]>],
}

impl<N: Node> Forest<'_, N> {
    #[inline(always)]
    fn is_complex(&self, at: usize) -> bool {
        // SAFETY: `at` is a slot, and the bitmap has a bit for every slot.
        unsafe { (*self.complex.get_unchecked(at >> 6) >> (at & 63)) & 1 != 0 }
    }

    #[cold]
    fn set_complex(&mut self, at: usize) {
        self.complex[at >> 6] |= 1 << (at & 63);
    }

    /// Where the probe for `code` starts.
    #[inline(always)]
    fn home(&self, code: u64) -> usize {
        // `mix` is below 2^CODE_BITS, so the result is below the slot count.
        ((u128::from(mix(code)) * self.slots.len() as u128) >> CODE_BITS) as usize
    }

    /// The slot of `code`, which becomes a new root if it was not there.
    #[inline(always)]
    fn locate(&mut self, code: u64) -> usize {
        let mut at = self.home(code);
        loop {
            // SAFETY: `at` is always below the slot count.
            let node = unsafe { *self.slots.get_unchecked(at) };
            if node.code() == code {
                return at;
            }
            if node.is_empty() {
                // SAFETY: as above.
                unsafe { *self.slots.get_unchecked_mut(at) = N::new(code, at) };
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
                let node = *self.slots.get_unchecked(at);
                let parent = node.parent();
                if parent == at {
                    return at;
                }
                let grandparent = self.slots.get_unchecked(parent).parent();
                if grandparent == parent {
                    return parent;
                }
                *self.slots.get_unchecked_mut(at) = node.with_parent(grandparent);
                at = grandparent;
            }
        }
    }

    /// Makes the trees of slots `a` and `b` one, rooted at the smaller ID.
    #[inline(always)]
    fn join(&mut self, a: usize, b: usize, group: &impl Fn(u64) -> Option<u64>) {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        let (node_a, node_b) = (self.slots[a], self.slots[b]);
        let (code_a, code_b) = (node_a.code(), node_b.code());
        // Pattern codes order like their IDs; exotic IDs are compared by bytes.
        let a_is_smaller = if code_a < EXOTIC_BASE && code_b < EXOTIC_BASE {
            code_a < code_b
        } else {
            id_cmp(code_a, code_b, b'u', self.exotic) == Ordering::Less
        };
        let root = if a_is_smaller {
            self.slots[b] = node_b.with_parent(a);
            a
        } else {
            self.slots[a] = node_a.with_parent(b);
            b
        };
        // Every member of a simple tree is in its root's group, so two simple
        // trees make a simple tree exactly when their roots share a group.
        // The mark stays on this slot when the tree is joined to another;
        // `settle_marks` moves it to the final root.
        let same_group = matches!((group(code_a), group(code_b)), (Some(one), Some(other)) if one == other);
        if !same_group {
            self.set_complex(root);
        }
    }

    /// Marks the root of every tree that has a marked slot, and returns how
    /// many roots are marked. Call when all pairs are joined; from then on
    /// `is_complex(root)` says whether a join made the tree complex.
    fn settle_marks(&mut self) -> usize {
        for word in 0..self.complex.len() {
            let mut marks = self.complex[word];
            while marks != 0 {
                let at = word * 64 + marks.trailing_zeros() as usize;
                marks &= marks - 1;
                let root = self.find(at);
                self.set_complex(root);
            }
        }
        let mut roots = 0;
        for word in 0..self.complex.len() {
            let mut marks = self.complex[word];
            while marks != 0 {
                let at = word * 64 + marks.trailing_zeros() as usize;
                marks &= marks - 1;
                roots += usize::from(self.slots[at].parent() == at);
            }
        }
        roots
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
    build_within(pairs, users, group, <u64 as Node>::MAX_SLOTS, <[u64; 2] as Node>::MAX_SLOTS)
}

/// `build_grouped`, with the two forest sizes limited to `narrow_max` and
/// `wide_max` slots (parameters so the tests can reach every path with small
/// inputs).
fn build_within(
    pairs: &[Vec<(u64, u64)>],
    users: &Interner,
    group: &impl Fn(u64) -> Option<u64>,
    narrow_max: usize,
    wide_max: usize,
) -> LinkTable {
    let pair_count: usize = pairs.iter().map(Vec::len).sum();
    // A pair brings at most two new IDs, so the forest stays under 8/9 full
    // and always has an empty slot to end a probe.
    let slot_count = pair_count * 2 + pair_count / 4 + 16;
    if slot_count <= narrow_max {
        build_in_forest::<u64>(pairs, users, group, slot_count)
    } else if slot_count <= wide_max {
        build_in_forest::<[u64; 2]>(pairs, users, group, slot_count)
    } else {
        build_general(pairs, users, group)
    }
}

fn build_in_forest<N: Node>(
    pairs: &[Vec<(u64, u64)>],
    users: &Interner,
    group: &impl Fn(u64) -> Option<u64>,
    slot_count: usize,
) -> LinkTable {
    // Exotic IDs (upper case and the like) sort by their bytes, not their code.
    let exotic = users.snapshot();
    let mut forest =
        Forest { slots: vec![N::EMPTY; slot_count], complex: vec![0u64; slot_count / 64 + 1], exotic: &exotic };
    let mut self_links = 0;
    for list in pairs {
        for (at, &(a, b)) in list.iter().enumerate() {
            if let Some(&(later_a, later_b)) = list.get(at + PAIR_LOOKAHEAD) {
                prefetch_line(forest.slots.as_ptr().wrapping_add(forest.home(later_a)));
                prefetch_line(forest.slots.as_ptr().wrapping_add(forest.home(later_b)));
            }
            self_links += usize::from(a == b);
            let (a, b) = (forest.locate(a), forest.locate(b));
            forest.join(a, b, group);
        }
    }

    // The table holds every ID that is not the root of its tree, renamed to
    // the root, and the roots of the complex trees. A tree that no join
    // marked has all its members in its root's group, so the one case left
    // is a root that is in no group. Such a root is alone in its tree, which
    // takes a line that links an ID to itself, so the self-links bound their
    // number and the table can be sized before they are found.
    let marked_roots = forest.settle_marks();
    let renamed =
        forest.slots.iter().enumerate().filter(|&(at, node)| !node.is_empty() && node.parent() != at).count();
    let mut table = LinkTable::with_capacity(renamed + marked_roots + self_links);
    let mut batch = [(0u64, 0u64); INSERT_BATCH];
    let mut waiting = 0;
    for at in 0..slot_count {
        if let Some(later) = forest.slots.get(at + SCAN_LOOKAHEAD) {
            prefetch_line(forest.slots.as_ptr().wrapping_add(later.parent()));
        }
        let node = forest.slots[at];
        if node.is_empty() {
            continue;
        }
        let (root, complex) = if node.parent() == at {
            let complex = forest.is_complex(at) || group(node.code()).is_none();
            if !complex {
                continue;
            }
            (at, true)
        } else {
            let root = forest.find(at);
            (root, forest.is_complex(root))
        };
        let (user, name) = (node.code(), forest.slots[root].code());
        if complex {
            table.complex.push(user);
        }
        table.prefetch(user);
        batch[waiting] = (user, if complex { name | COMPLEX_BIT } else { name });
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
fn build_general(pairs: &[Vec<(u64, u64)>], users: &Interner, group: &impl Fn(u64) -> Option<u64>) -> LinkTable {
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

    /// The three build paths, by the slot limits that select them.
    const PATHS: [(&str, usize, usize); 3] =
        [("narrow forest", usize::MAX, usize::MAX), ("wide forest", 0, usize::MAX), ("general", 0, 0)];

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
        for (path, narrow_max, wide_max) in PATHS {
            let table = build_within(&lists, &users, &group, narrow_max, wide_max);
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
        for (path, narrow_max, wide_max) in PATHS {
            let table = build_within(&[list.clone()], &users, &|code| group_of_code[&code], narrow_max, wide_max);
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
        for (path, narrow_max, wide_max) in PATHS {
            let table = build_within(&[list.clone()], &users, &|_| None, narrow_max, wide_max);
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
        for (path, narrow_max, wide_max) in PATHS {
            let table = build_within(&lists, &users, &group, narrow_max, wide_max);
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

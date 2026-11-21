//! Linked IDs: union-find over the link pairs, then a flat lookup table from
//! a user code to the code of its person's name (the byte-wise smallest ID).

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use super::codes::{id_cmp, Interner};
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

fn find(parent: &mut [u32], mut at: u32) -> u32 {
    while parent[at as usize] != at {
        let grandparent = parent[parent[at as usize] as usize];
        parent[at as usize] = grandparent;
        at = grandparent;
    }
    at
}

/// Joins every pair, names each group by its byte-wise smallest ID.
pub fn build(pairs: &[Vec<(u64, u64)>], users: &Interner) -> LinkTable {
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

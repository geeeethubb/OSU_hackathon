//! Linked IDs: union-find over the link pairs, then a lookup table from a
//! user code to the code of its person's name (the byte-wise smallest ID).
//!
//! The union-find lives inside one hash table (`Forest`): a node holds an ID
//! and the slot of its parent, so finding an ID and reaching its tree node is
//! one memory access, and the lines that the pairs coming up will need are
//! prefetched while the current pair is joined.
//!
//! The lookup table (`Table`) is written from the forest in one pass, in
//! the order of the IDs' hashes: a bitmap that says which IDs have an entry,
//! and the entries packed with no gaps. It is about a quarter of the size of
//! a hash table of the same IDs, and most IDs that have no entry never get
//! past the bitmap.
//!
//! A build can also be given a grouping of the IDs (`build_grouped`). A link
//! component is then simple when all its members are in one group and complex
//! otherwise, and `LinkTable::lookup` reports which with the name.
//!
//! Index mode (`build_indexed`, `build_sorted`) groups the IDs by their user
//! index, and there nearly every line links two IDs of one user. Such lines
//! need no table at all. They are sorted into shards of a few thousand users
//! (`Lines`), a word a line, and a shard is settled by one pass over its
//! lines that notes, per user, which aliases were seen and which of them
//! have a line to a lower alias: when all but the lowest have one, the
//! user's IDs are one component, and the user's summary entry (the name and
//! the aliases of that component) answers every lookup for its IDs. The few
//! users this leaves open have their lines looked at one by one. Only the
//! lines that leave a user, and all lines of the users they touch or whose
//! links one entry cannot describe, go through a forest and into a table,
//! which is small.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use super::codes::{id_cmp, mix, unmix, Exotic, Interner, CODE_BITS, CODE_MASK, EXOTIC_BASE, P37};
use super::keys::{Keys, MASK as NUMBER_MASK};
use crate::reference::FastHasher;

/// Set in a name on its way into the table when the component is complex.
const COMPLEX_BIT: u64 = 1 << 63;

/// An entry of the table, from the low bits up: the complex flag; how many
/// positions after its home position the entry sits; the bits of the ID's
/// hash that the home position does not give; and the name code.
const ENTRY_COMPLEX: u64 = 1;
const ENTRY_NAME_SHIFT: u32 = 64 - CODE_BITS;
const ENTRY_KEY_MASK: u64 = (1 << ENTRY_NAME_SHIFT) - 2;
/// The table has 2^bits positions, with bits between these. At the fewest an
/// entry has 19 hash bits and 2 displacement bits beside the flag.
const MIN_POSITION_BITS: u32 = 23;
const MAX_POSITION_BITS: u32 = CODE_BITS - 1;
/// A lookup looks at up to 2^this entries; more displacement goes to the stash.
const MAX_DISPLACEMENT_BITS: u32 = 4;

/// The lookup table: an entry for every linked ID whose person is named by a
/// different ID, and for every member of a complex component, its own name
/// included. Anything without an entry names itself and is simple.
///
/// An ID's hash gives its home position among 2^23 or more. `bits` has a bit
/// for every position that holds an entry, and the entries themselves are
/// packed in position order in `entries`, so most IDs without an entry are
/// answered by one bit of a small bitmap, and an entry costs 8 bytes with no
/// empty slots between. An entry sits at its home position or just after it,
/// and says how far; the position and the hash bits it keeps identify its ID.
struct Table {
    bits: Vec<u64>,
    /// For every 256 positions: the entries before them (from bit 24 up),
    /// and the entries of the block before its words 1, 2 and 3 (8 bits each).
    blocks: Vec<u64>,
    entries: Vec<u64>,
    /// Hash bits below the ones that pick the home position.
    rem_bits: u32,
    displacement_bits: u32,
    /// The entries that found no room close enough to their home position:
    /// ID to name, with `COMPLEX_BIT`. Empty unless hashes crowd.
    stash: HashMap<u64, u64>,
    /// Every member of every complex component.
    complex: Vec<u64>,
    /// The name of every simple component of two or more IDs. These IDs
    /// have no entry, since they name themselves.
    simple_names: Vec<u64>,
}

/// Timing only, wrong answers: `SS_DIAG_BITMAP=1` (see `Table::probe`).
fn diag_bitmap() -> bool {
    diag_bitmap_mode() != 0
}

/// 1: bitmap and rank words from 512 cached bytes; 2: the same, with about
/// 40% of probes taking an entry at a random rank (the real rate of IDs with
/// an entry), so that only the bitmap and rank misses are gone.
fn diag_bitmap_mode() -> u32 {
    static MODE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| std::env::var("SS_DIAG_BITMAP").ok().and_then(|value| value.parse().ok()).unwrap_or(0))
}

impl Table {
    /// Starts loading what `lookup(user)` will read, so that a lookup a few
    /// records later finds it in the cache. Changes no result.
    ///
    /// The bitmap is small and stays cached; the entry is what a lookup
    /// would wait for. So this reads the ID's bit itself and prefetches the
    /// entry when there is one. It takes no branch on the bit, which is as
    /// good as random.
    #[inline(always)]
    fn prefetch(&self, user: u64) {
        let home = (mix(user) >> self.rem_bits) as usize;
        // SAFETY: `bits` has a word for every home position.
        let word = unsafe { *self.bits.get_unchecked(home >> 6) };
        // The entry, when there is one; otherwise this table, which is in the cache.
        let set = (word >> (home & 63)) & 1 != 0;
        let entry = self.entries.as_ptr().wrapping_add(self.rank(home, word));
        prefetch_line(if set { entry } else { self as *const Self as *const u64 });
    }

    /// `prefetch` and the first half of `lookup` in one: starts loading the
    /// ID's entry and returns the ID's hash and, with the home bit on top,
    /// the entry's rank, so that `lookup_probed` does not work them out
    /// again.
    #[inline(always)]
    fn probe(&self, user: u64) -> [u64; 2] {
        let hash = mix(user);
        // Timing only: the bitmap and rank words of the first 4,096
        // positions (512 bytes, always cached) stand in for the real ones.
        let home = (hash >> self.rem_bits) as usize & if diag_bitmap() { 4095 } else { usize::MAX };
        // SAFETY: `bits` has a word for every home position.
        let word = unsafe { *self.bits.get_unchecked(home >> 6) };
        let mut set = (word >> (home & 63)) & 1;
        let mut rank = self.rank(home, word);
        if diag_bitmap_mode() == 2 && !self.entries.is_empty() {
            set = u64::from((hash >> 9) % 5 < 2);
            rank = ((hash >> 20) % self.entries.len() as u64) as usize;
        }
        let entry = self.entries.as_ptr().wrapping_add(rank);
        prefetch_line(if set != 0 { entry } else { self as *const Self as *const u64 });
        [hash, (rank as u64) | (set << 63)]
    }

    /// `lookup(user)` from what `probe(user)` returned.
    #[inline(always)]
    fn lookup_probed(&self, user: u64, probe: [u64; 2]) -> (u64, bool) {
        let [hash, at] = probe;
        if at >> 63 == 0 {
            return (user, false);
        }
        let rank = (at & !(1 << 63)) as usize;
        let home = (hash >> self.rem_bits) as usize;
        let key = (hash << (self.displacement_bits + 1)) & ENTRY_KEY_MASK;
        // SAFETY: a set bit has an entry at its rank.
        let entry = unsafe { *self.entries.get_unchecked(rank) };
        if entry & ENTRY_KEY_MASK == key {
            return (entry >> ENTRY_NAME_SHIFT, entry & ENTRY_COMPLEX != 0);
        }
        if diag_bitmap() {
            return (user, false);
        }
        self.lookup_further(user, home, rank, key)
    }

    /// The name code of the person `user` belongs to. Codes are never 0.
    #[cfg(test)]
    fn person(&self, user: u64) -> u64 {
        self.lookup(user).0
    }

    /// The name code of the person `user` belongs to, and whether that
    /// person's link component is complex. An ID that is in no link line
    /// names itself and is simple.
    #[inline(always)]
    fn lookup(&self, user: u64) -> (u64, bool) {
        // The same hash as the forest's, so the forest's order is the table's.
        let hash = mix(user);
        let home = (hash >> self.rem_bits) as usize;
        // SAFETY: `bits` has a word for every home position.
        let word = unsafe { *self.bits.get_unchecked(home >> 6) };
        if (word >> (home & 63)) & 1 == 0 {
            return (user, false);
        }
        let rank = self.rank(home, word);
        let key = (hash << (self.displacement_bits + 1)) & ENTRY_KEY_MASK;
        // SAFETY: a set bit has an entry at its rank.
        let entry = unsafe { *self.entries.get_unchecked(rank) };
        if entry & ENTRY_KEY_MASK == key {
            return (entry >> ENTRY_NAME_SHIFT, entry & ENTRY_COMPLEX != 0);
        }
        self.lookup_further(user, home, rank, key)
    }

    /// How many entries are at positions before `home`, whose word of `bits` is `word`.
    #[inline(always)]
    fn rank(&self, home: usize, word: u64) -> usize {
        // SAFETY: `blocks` has a word for every four words of `bits`.
        let block = unsafe { *self.blocks.get_unchecked(home >> 8) };
        let in_block = ((block << 8) >> (8 * ((home >> 6) & 3))) & 0xFF;
        (block >> 24) as usize + in_block as usize + (word & ((1u64 << (home & 63)) - 1)).count_ones() as usize
    }

    /// The rest of `lookup` when the entry at the home position is another
    /// ID's: the entries at the positions after it, then the stash.
    #[inline(never)]
    fn lookup_further(&self, user: u64, home: usize, rank: usize, key: u64) -> (u64, bool) {
        for step in 1..1usize << self.displacement_bits {
            let at = home + step;
            // SAFETY: `bits` has room for the positions after the last home position.
            if (unsafe { *self.bits.get_unchecked(at >> 6) } >> (at & 63)) & 1 == 0 {
                return (user, false);
            }
            // SAFETY: the positions from `home` to `at` all hold entries, in rank order.
            let entry = unsafe { *self.entries.get_unchecked(rank + step) };
            if entry & ENTRY_KEY_MASK == key | (step as u64) << 1 {
                return (entry >> ENTRY_NAME_SHIFT, entry & ENTRY_COMPLEX != 0);
            }
        }
        match self.stash.get(&user) {
            Some(&name) => (name & CODE_MASK, name & COMPLEX_BIT != 0),
            None => (user, false),
        }
    }

    /// The codes of all members of complex components, in no particular
    /// order, each once. Empty after a plain `build`.
    fn complex_members(&self) -> &[u64] {
        &self.complex
    }

    /// Calls `visit(member code, name code, complex)` once for every ID of
    /// every link component that has two or more IDs or is complex, the ID
    /// that names the component included. These are all the IDs for which
    /// `lookup` can return anything but `(itself, false)`, and the names of
    /// their components. An ID that is alone in a simple component (linked
    /// only to itself, and in a group) is not visited. In no particular order.
    fn for_each_member(&self, mut visit: impl FnMut(u64, u64, bool)) {
        let rem_mask = (1u64 << self.rem_bits) - 1;
        let mut rank = 0;
        for (index, &word) in self.bits.iter().enumerate() {
            let mut left = word;
            while left != 0 {
                let at = (index * 64 + left.trailing_zeros() as usize) as u64;
                left &= left - 1;
                let entry = self.entries[rank];
                rank += 1;
                // The position less the displacement is the home position:
                // the high bits of the ID's hash, above the bits the entry kept.
                let key = (entry & ENTRY_KEY_MASK) >> 1;
                let home = at - (key & ((1 << self.displacement_bits) - 1));
                let hash = (home << self.rem_bits) | ((key >> self.displacement_bits) & rem_mask);
                visit(unmix(hash), entry >> ENTRY_NAME_SHIFT, entry & ENTRY_COMPLEX != 0);
            }
        }
        for (&user, &name) in &self.stash {
            visit(user, name & CODE_MASK, name & COMPLEX_BIT != 0);
        }
        for &name in &self.simple_names {
            visit(name, name, false);
        }
    }
}

/// Bits of a user number below the user index: which of the user's IDs it is.
const ALIAS_BITS: u32 = 9;
const ALIAS_MASK: u64 = (1 << ALIAS_BITS) - 1;
/// User indexes per shard of an index-mode build, as a shift: the lines of a
/// shard and what is noted of each of its users stay in the second-level
/// cache.
const SHARD_BITS: u32 = 13;
const SHARD_USERS: usize = 1 << SHARD_BITS;
/// The summaries have a word of bits for every 64 users.
const SHARD_WORDS: usize = SHARD_USERS / 64;
/// Set in a user's summary: the user has one link component of two or more
/// IDs, all of its own, named by the code in the low bits, and word 1 has a
/// bit for the alias of each of its members.
pub const SIMPLE: u64 = 1 << 63;
/// A user's summary when its two words cannot describe the user's IDs: one
/// of them is in a complex component, they are in two components, or a
/// linked one has an alias of 64 or more. Each ID is then looked up.
pub const SLOW: u64 = 1 << 62;

/// What the links say, to look up: the person of an ID (`lookup`, `person`),
/// and in index mode the summary of a user index (`summary`).
///
/// After `build` and `build_grouped` this is one table of all linked IDs.
/// After `build_indexed` the summaries answer for the IDs of every user they
/// can describe, and the table holds the rest: the IDs of the users whose
/// summary is `SLOW`, and the linked IDs that are no user's.
pub struct LinkTable {
    table: Table,
    indexed: Option<Indexed>,
}

/// The summaries of an index-mode build. Most user indexes have no links
/// (the bound on the indexes is far above the users there are), so a bit per
/// user says whether it has an entry, and the entries are packed.
struct Indexed {
    keys: Keys,
    /// For every 64 user indexes: a bit for each user that has an entry, and
    /// where in `entries` the entries of these users are, in user order.
    words: Vec<[u64; 2]>,
    /// The summaries that are not all zero.
    entries: Vec<[u64; 2]>,
}

impl Indexed {
    /// The summary of a user index below the keys' bound.
    #[inline(always)]
    fn entry(&self, user: u64) -> [u64; 2] {
        let Some(&[bits, first]) = self.words.get((user >> 6) as usize) else {
            return [0, 0];
        };
        let bit = 1u64 << (user & 63);
        if bits & bit == 0 {
            return [0, 0];
        }
        self.entries[first as usize + (bits & (bit - 1)).count_ones() as usize]
    }

    /// The answer of `lookup` when the summaries have it: for an ID of a
    /// user whose entry is not `SLOW`.
    #[inline(always)]
    fn lookup(&self, code: u64) -> Option<(u64, bool)> {
        let number = user_number_of_code(&self.keys, code)?;
        let [head, mask] = self.entry(number >> ALIAS_BITS);
        if head & SLOW != 0 {
            return None;
        }
        // In the user's one component, or linked to no other ID.
        let alias = number & ALIAS_MASK;
        let linked = head & SIMPLE != 0 && alias < 64 && (mask >> alias) & 1 != 0;
        Some((if linked { head & CODE_MASK } else { code }, false))
    }
}

/// For two base-37 digits of a code, `first * 37 + second`: the number that
/// the two characters are in base 36, or `NO_PAIR` when either digit is 0
/// (no character there).
static PAIR_VALUE: [u16; 37 * 37] = pair_values();
const NO_PAIR: u16 = 1 << 15;

const fn pair_values() -> [u16; 37 * 37] {
    let mut values = [NO_PAIR; 37 * 37];
    let mut first = 1;
    while first < 37 {
        let mut second = 1;
        while second < 37 {
            values[first * 37 + second] = ((first - 1) * 36 + (second - 1)) as u16;
            second += 1;
        }
        first += 1;
    }
    values
}

/// `keys.user_number_of_code(code)`: the user number behind the code of a
/// generated ID. Nearly all generated IDs have eight characters, and for
/// those this reads the characters two at a time; the keys decide the rest.
#[inline(always)]
fn user_number_of_code(keys: &Keys, code: u64) -> Option<u64> {
    if code >= EXOTIC_BASE {
        return None;
    }
    // Four pairs of base-37 digits, each below 37 * 37.
    let (high, low) = (code / P37[4], code % P37[4]);
    let first = (high / P37[2]) as usize;
    let pairs = [
        PAIR_VALUE[first],
        PAIR_VALUE[(high % P37[2]) as usize],
        PAIR_VALUE[(low / P37[2]) as usize],
        PAIR_VALUE[(low % P37[2]) as usize],
    ];
    // Fewer than eight characters, or `0` as the first of them.
    if (pairs[0] | pairs[1] | pairs[2] | pairs[3]) & NO_PAIR != 0 || first < 2 * 37 {
        return keys.user_number_of_code(code);
    }
    let value = ((u64::from(pairs[0]) * 1296 + u64::from(pairs[1])) * 1296 + u64::from(pairs[2])) * 1296
        + u64::from(pairs[3]);
    if value > NUMBER_MASK {
        return None;
    }
    keys.user_number(value)
}

impl LinkTable {
    /// Starts loading what `lookup(user)` will read, so that a lookup a few
    /// records later finds it in the cache. Changes no result.
    #[inline(always)]
    pub fn prefetch(&self, user: u64) {
        match &self.indexed {
            None => self.table.prefetch(user),
            Some(indexed) => self.prefetch_indexed(indexed, user),
        }
    }

    /// `prefetch` that also hands back where the user's entry is (its hash,
    /// home bit and rank), so that `person_probed` does not work it out
    /// again. After an index-mode build it is `prefetch`.
    #[inline(always)]
    pub fn probe(&self, user: u64) -> [u64; 2] {
        match &self.indexed {
            None => self.table.probe(user),
            Some(indexed) => {
                self.prefetch_indexed(indexed, user);
                [0, 0]
            }
        }
    }

    /// `mix(person(user))`, given what `probe(user)` returned. When the
    /// person is named by `user` itself (an ID in no link line, or the one
    /// that names its component), that is the probe's hash, `mix(user)`,
    /// already: no second mix.
    #[inline(always)]
    pub fn person_mixed_probed(&self, user: u64, probe: [u64; 2]) -> u64 {
        match &self.indexed {
            None => {
                let name = self.table.lookup_probed(user, probe).0;
                if name == user {
                    probe[0]
                } else {
                    mix(name)
                }
            }
            Some(indexed) => mix(self.lookup_indexed(indexed, user).0),
        }
    }

    /// `person(user)`, given what `probe(user)` returned.
    #[inline(always)]
    pub fn person_probed(&self, user: u64, probe: [u64; 2]) -> u64 {
        match &self.indexed {
            None => self.table.lookup_probed(user, probe).0,
            Some(indexed) => self.lookup_indexed(indexed, user).0,
        }
    }

    /// `prefetch` after an index-mode build. Not inlined, and neither is
    /// `lookup_indexed`: after any other build every lookup goes to the
    /// table, and the loops that do them should carry nothing else.
    #[inline(never)]
    fn prefetch_indexed(&self, indexed: &Indexed, user: u64) {
        match user_number_of_code(&indexed.keys, user) {
            // The word of bits of the user; its entry, if any, is one more read.
            Some(number) => prefetch_line(indexed.words.as_ptr().wrapping_add((number >> (ALIAS_BITS + 6)) as usize)),
            None => self.table.prefetch(user),
        }
    }

    /// The name code of the person `user` belongs to. Codes are never 0.
    #[inline]
    pub fn person(&self, user: u64) -> u64 {
        self.lookup(user).0
    }

    /// The name code of the person `user` belongs to, and whether that
    /// person's link component is complex. An ID that is in no link line
    /// names itself and is simple.
    #[inline(always)]
    pub fn lookup(&self, user: u64) -> (u64, bool) {
        match &self.indexed {
            None => self.table.lookup(user),
            Some(indexed) => self.lookup_indexed(indexed, user),
        }
    }

    /// `lookup` after an index-mode build: the summaries answer for the IDs
    /// of the users they can describe, the table for the rest.
    #[inline(never)]
    fn lookup_indexed(&self, indexed: &Indexed, user: u64) -> (u64, bool) {
        match indexed.lookup(user) {
            Some(answer) => answer,
            None => self.table.lookup(user),
        }
    }

    /// The codes of all members of complex components, in no particular
    /// order, each once. Empty after a plain `build`.
    pub fn complex_members(&self) -> &[u64] {
        self.table.complex_members()
    }

    /// Calls `visit(member code, name code, complex)` once for every ID of
    /// every link component that has two or more IDs or is complex, the ID
    /// that names the component included. These are all the IDs for which
    /// `lookup` can return anything but `(itself, false)`, and the names of
    /// their components. An ID that is alone in a simple component (linked
    /// only to itself, and in a group) is not visited. In no particular order.
    ///
    /// After `build_indexed` this is slow: an ID that only a summary knows
    /// has to be worked out from its user index and alias.
    pub fn for_each_member(&self, mut visit: impl FnMut(u64, u64, bool)) {
        self.table.for_each_member(&mut visit);
        let Some(indexed) = &self.indexed else {
            return;
        };
        for (at, &[bits, first]) in indexed.words.iter().enumerate() {
            let mut users = bits;
            let mut entry = first as usize;
            while users != 0 {
                let user = (at * 64) as u64 + u64::from(users.trailing_zeros());
                users &= users - 1;
                let [head, mask] = indexed.entries[entry];
                entry += 1;
                if head & SIMPLE == 0 {
                    continue;
                }
                let mut aliases = mask;
                while aliases != 0 {
                    let alias = u64::from(aliases.trailing_zeros());
                    aliases &= aliases - 1;
                    visit(indexed.keys.code_of_user_number(user << ALIAS_BITS | alias), head & CODE_MASK, false);
                }
            }
        }
    }

    /// Index mode: what the links say about the IDs of user index `user`.
    /// `[name code | SIMPLE, alias mask]` when the user has one link
    /// component of two or more IDs, all the user's own and of aliases below
    /// 64: the name of that component and a bit for the alias of each
    /// member. `[0, 0]` when none of the user's IDs is in a component of two
    /// or more. `[SLOW, 0]` for every other user, for a user index that is
    /// out of range, and for every user after a build that is not indexed.
    #[inline(always)]
    pub fn summary(&self, user: u64) -> [u64; 2] {
        match &self.indexed {
            Some(indexed) if user < indexed.keys.user_bound => indexed.entry(user),
            _ => [SLOW, 0],
        }
    }
}

/// Fills a `Table` from entries that come in the order of their hashes.
struct TableWriter {
    table: Table,
    /// The first position after the last entry.
    next_free: usize,
}

impl TableWriter {
    /// A writer for about `entries` entries.
    fn new(entries: usize) -> Self {
        // At most a quarter of the positions hold an entry.
        let position_bits = (entries * 4).next_power_of_two().trailing_zeros();
        Self::with_position_bits(entries, position_bits.clamp(MIN_POSITION_BITS, MAX_POSITION_BITS))
    }

    fn with_position_bits(entries: usize, position_bits: u32) -> Self {
        let displacement_bits = (position_bits + ENTRY_NAME_SHIFT - CODE_BITS - 1).min(MAX_DISPLACEMENT_BITS);
        // A word of padding for the positions that entries of the last home
        // positions are displaced to, and one more so that the last block is whole.
        let words = (1usize << position_bits) / 64 + 4;
        let table = Table {
            bits: vec![0; words],
            blocks: vec![0; words / 4],
            entries: Vec::with_capacity(entries),
            rem_bits: CODE_BITS - position_bits,
            displacement_bits,
            stash: HashMap::new(),
            complex: Vec::new(),
            simple_names: Vec::new(),
        };
        Self { table, next_free: 0 }
    }

    /// Adds the entry of the ID with hash `hash`: its name, with
    /// `COMPLEX_BIT` for a complex component. Hashes must not decrease.
    #[inline(always)]
    fn push(&mut self, hash: u64, name: u64) {
        let table = &mut self.table;
        let home = (hash >> table.rem_bits) as usize;
        // The first free position from home on. Everything from home up to
        // it holds entries, which is what a lookup relies on.
        let at = home.max(self.next_free);
        let step = at - home;
        if step >> table.displacement_bits != 0 {
            table.stash.insert(unmix(hash), name);
            return;
        }
        // SAFETY: `at` is less than 2^`MAX_DISPLACEMENT_BITS` positions after
        // a home position, inside the padding of `bits`.
        unsafe { *table.bits.get_unchecked_mut(at >> 6) |= 1 << (at & 63) };
        let key = ((hash << table.displacement_bits) | step as u64) << 1 & ENTRY_KEY_MASK;
        table.entries.push((name & CODE_MASK) << ENTRY_NAME_SHIFT | key | name >> 63);
        self.next_free = at + 1;
    }

    fn finish(self) -> Table {
        let mut table = self.table;
        let mut before = 0u64;
        for (block, packed) in table.blocks.iter_mut().enumerate() {
            let words = &table.bits[block * 4..block * 4 + 4];
            let first = u64::from(words[0].count_ones());
            let second = first + u64::from(words[1].count_ones());
            let third = second + u64::from(words[2].count_ones());
            *packed = before << 24 | first | second << 8 | third << 16;
            before = before + third + u64::from(words[3].count_ones());
        }
        debug_assert_eq!(before as usize, table.entries.len());
        table
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
/// How many lines ahead of the one being named the parents are prefetched.
const SCAN_LOOKAHEAD: usize = 12;

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
/// On a root: another tree was joined under it, so its tree has two or more IDs.
const JOINED: u64 = 1 << 61;
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
    exotic: &'a Exotic,
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
    fn new(line_bits: u32, exotic: &'a Exotic) -> Self {
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

    /// The hash of the ID in `slot`, whose node is `node`.
    #[inline(always)]
    fn hash_at(&self, slot: usize, node: u64) -> u64 {
        let home_line = (slot / LINE_SLOTS) as u64 - ((node >> SPILL_SHIFT) & MAX_SPILL as u64);
        (home_line << self.rem_bits) | ((node & BELOW_SPILL) >> self.parent_bits)
    }

    /// The code of the ID in `slot`, whose node is `node`.
    #[inline(always)]
    fn code_at(&self, slot: usize, node: u64) -> u64 {
        unmix(self.hash_at(slot, node))
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
        let mut root_node = root_node | JOINED;
        if !same_group || (node_a | node_b) & MARKED != 0 {
            root_node |= MARKED;
            // One complex tree where there were none, one or two.
            self.complex_trees += 1;
            self.complex_trees -= usize::from(node_a & MARKED != 0) + usize::from(node_b & MARKED != 0);
        }
        self.set_node(root, root_node);
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
    LinkTable { table: build_within(pairs, users, group, Limits::NORMAL), indexed: None }
}

/// The build of index mode. The generated IDs are grouped by their user
/// index (`keys.user_number_of_code(code) >> 9`), every other ID is in no
/// group, and beside names and flags the result has the `summary` of every
/// user index. `helpers` is how many threads to use beside the caller's.
///
/// A line between two IDs of one user is INSIDE that user; any other line
/// makes its component complex, and the users at its ends are TOUCHED. An
/// untouched user's IDs are in no line but its own inside lines, so its
/// components are found from those alone, a shard of users at a time and in
/// parallel; when they are one component with aliases below 64, the summary
/// entry says all there is to say about the user. Every other user (touched,
/// or with links one entry cannot describe) is handed over whole: all its
/// lines go, with the lines that are not inside a user, through the build
/// that `build_grouped` does, into the one table. A component that touches
/// such a user has all its lines there, since a line inside another user
/// cannot reach it.
pub fn build_indexed(pairs: &[Vec<(u64, u64)>], users: &Interner, keys: &Keys, helpers: usize) -> LinkTable {
    build_indexed_within(pairs, users, keys, helpers, Limits::NORMAL)
}

/// `build_indexed` from lines that were sorted as they were read: each
/// thread that reads link lines adds them to a `Lines` of its own, made with
/// the same keys, and this builds from all of them. The result is that of
/// `build_indexed` from the same lines as pairs.
pub fn build_sorted(lines: &[Lines], users: &Interner, keys: &Keys, helpers: usize) -> LinkTable {
    build_sorted_within(lines, users, keys, helpers, Limits::NORMAL)
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
) -> Table {
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
    exotic: &Exotic,
    group: &impl Fn(u64) -> Option<u64>,
    line_bits: u32,
) -> Option<Table> {
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
    // root's name, and the root of every complex tree under its own; the
    // joins counted both. It wants them in the order of their hashes, which
    // is the order of the forest's lines but for the nodes that spilled. A
    // node is never stored past a line with room, so a line with an empty
    // slot ends a run: everything seen so far hashes below everything after.
    let mut writer = TableWriter::new(forest.merges + forest.complex_trees);
    let mut complex = Vec::new();
    let mut simple_names = Vec::new();
    let mut run: Vec<(u64, u64)> = Vec::new();
    for first in (0..forest.slot_count).step_by(LINE_SLOTS) {
        let ahead = first + SCAN_LOOKAHEAD * LINE_SLOTS;
        if ahead < forest.slot_count {
            for slot in ahead..ahead + LINE_SLOTS {
                forest.prefetch_parent(slot);
            }
        }
        // Neither half of the test matches a key of all ones: only empty slots hit.
        let empty = line_hits(forest.nodes.wrapping_add(first), 0, u64::MAX);
        let mut occupied = !empty & ((1 << LINE_SLOTS) - 1);
        while occupied != 0 {
            let slot = first + occupied.trailing_zeros() as usize;
            occupied &= occupied - 1;
            let node = forest.node(slot);
            let root = forest.find(slot);
            let root_node = forest.node(root);
            let is_complex = root_node & MARKED != 0;
            if root == slot && !is_complex {
                // A root that names others has no entry; it is only listed.
                if node & JOINED != 0 {
                    simple_names.push(forest.code_at(slot, node));
                }
                continue;
            }
            let hash = forest.hash_at(slot, node);
            let name = forest.code_at(root, root_node) | if is_complex { COMPLEX_BIT } else { 0 };
            if is_complex {
                complex.push(unmix(hash));
            }
            // Runs are a few entries long: keep them sorted as they grow.
            let mut at = run.len();
            run.push((hash, name));
            while at > 0 && run[at - 1].0 > hash {
                run[at] = run[at - 1];
                at -= 1;
            }
            run[at] = (hash, name);
        }
        if empty != 0 {
            for &(hash, name) in &run {
                writer.push(hash, name);
            }
            run.clear();
        }
    }
    for &(hash, name) in &run {
        writer.push(hash, name);
    }
    let mut table = writer.finish();
    debug_assert_eq!(
        table.entries.len() + table.stash.len(),
        forest.merges + forest.complex_trees,
        "the joins miscounted the table's entries"
    );
    table.complex = complex;
    table.simple_names = simple_names;
    Some(table)
}

/// Pairs per task when the lines are sorted into inside and across.
const SORT_CHUNK: usize = 1 << 13;
/// With more user indexes than this the lines are not sorted into shards:
/// a thread's buckets and the bitmaps over all users would take too much.
/// (User numbers have 40 bits, so a user index always fits 32.)
const MAX_USER_BOUND: u64 = 1 << 28;
/// A line inside a user is one word when both aliases fit four bits, as
/// nearly all do: the smaller of the two codes, the aliases from `LINE_A` and
/// `LINE_B` up, and from `LINE_USER` up the user index within its shard. The
/// two IDs are then known from the user and the aliases.
///
/// Any other line is two words. The first has the code of one ID, `LONG_LINE`
/// where the aliases would be (15 and 15, which no line has, since the IDs of
/// a line differ), and the user; the second has the code of the other ID,
/// from `LONG_A` and `LONG_B` up the aliases in full, and `LINE_MORE`, which
/// no first word has.
const LINE_A: u32 = CODE_BITS;
const LINE_B: u32 = CODE_BITS + 4;
const LINE_USER: u32 = CODE_BITS + 8;
const LONG_LINE: u64 = 0xFF << CODE_BITS;
const LONG_A: u32 = CODE_BITS;
const LONG_B: u32 = CODE_BITS + ALIAS_BITS;
const LINE_MORE: u64 = 1 << 63;
const _: () = assert!(LINE_USER + SHARD_BITS <= 63 && LONG_B + ALIAS_BITS <= 63);

/// One thread's share of the link lines of an index-mode build, sorted as
/// they are added: the lines inside a user by the shard of the user, the
/// others apart.
pub struct Lines {
    /// The lines inside a user, by the shard of the user. No shards at all
    /// when the keys have more users than the shards take.
    shards: Vec<Vec<u64>>,
    /// The lines that are not inside a user.
    across: Vec<(u64, u64)>,
    /// The users at the ends of those lines.
    touched: Vec<u32>,
}

impl Lines {
    /// For the lines of a build with these keys.
    pub fn new(keys: &Keys) -> Self {
        let shard_count = match keys.user_bound > MAX_USER_BOUND {
            // Too many users for shards: every line is kept as it is, and
            // `build_sorted` makes one table of all.
            true => 0,
            // A word of bits for every 64 users, a shard for every `SHARD_WORDS` words.
            false => ((keys.user_bound >> 6) as usize + 1).div_ceil(SHARD_WORDS),
        };
        Self { shards: vec![Vec::new(); shard_count], across: Vec::new(), touched: Vec::new() }
    }

    /// Adds the line that links the IDs with the codes `code_a` and
    /// `code_b`. `number_a` and `number_b` are their user numbers: what
    /// `keys.user_number_of_code` says of each code, which is `None` for an
    /// ID that is not a generated ID of a user index below the keys' bound.
    #[inline(always)]
    pub fn add(&mut self, code_a: u64, number_a: Option<u64>, code_b: u64, number_b: Option<u64>) {
        if code_a == code_b {
            // An ID linked to itself: nothing, unless it is no user's ID,
            // which is then a complex component of one.
            if number_a.is_none() {
                self.across.push((code_a, code_b));
            }
            return;
        }
        if let (Some(one), Some(other)) = (number_a, number_b) {
            let user = one >> ALIAS_BITS;
            if user == other >> ALIAS_BITS {
                if let Some(shard) = self.shards.get_mut((user >> SHARD_BITS) as usize) {
                    let (alias_a, alias_b) = (one & ALIAS_MASK, other & ALIAS_MASK);
                    let local = (user & (SHARD_USERS as u64 - 1)) << LINE_USER;
                    if alias_a.max(alias_b) < 16 {
                        shard.push(code_a.min(code_b) | alias_a << LINE_A | alias_b << LINE_B | local);
                    } else {
                        let more = code_b | alias_a << LONG_A | alias_b << LONG_B | LINE_MORE;
                        shard.extend([code_a | LONG_LINE | local, more]);
                    }
                    return;
                }
            }
        }
        self.across.push((code_a, code_b));
        for number in [number_a, number_b].into_iter().flatten() {
            self.touched.push((number >> ALIAS_BITS) as u32);
        }
    }

    /// The lines inside the users of a shard. (None if these lines were
    /// made with other keys than the build's.)
    fn shard(&self, shard: usize) -> &[u64] {
        self.shards.get(shard).map_or(&[], Vec::as_slice)
    }

    /// `add` for a line whose user numbers are still to be worked out.
    #[inline(always)]
    pub fn add_codes(&mut self, keys: &Keys, code_a: u64, code_b: u64) {
        let number_a = user_number_of_code(keys, code_a);
        let number_b = if code_a == code_b { number_a } else { user_number_of_code(keys, code_b) };
        self.add(code_a, number_a, code_b, number_b);
    }
}

/// The room for the summary entries of all users. The threads fill it a
/// shard at a time: each takes the entries that its shard needs.
struct EntryRoom<'a> {
    entries: *mut [u64; 2],
    capacity: usize,
    taken: std::sync::atomic::AtomicUsize,
    owner: std::marker::PhantomData<&'a mut [[u64; 2]]>,
}

// SAFETY: `take` hands every entry out at most once, so no two threads have
// the same one.
unsafe impl Sync for EntryRoom<'_> {}

impl<'a> EntryRoom<'a> {
    fn new(entries: &'a mut [[u64; 2]]) -> Self {
        Self {
            entries: entries.as_mut_ptr(),
            capacity: entries.len(),
            taken: std::sync::atomic::AtomicUsize::new(0),
            owner: std::marker::PhantomData,
        }
    }

    /// The next `count` entries, which no other caller gets, and where in
    /// the room they start.
    fn take(&self, count: usize) -> (usize, &'a mut [[u64; 2]]) {
        let first = self.taken.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
        assert!(count <= self.capacity && first <= self.capacity - count, "more summary entries than lines");
        // SAFETY: the range lies in the slice this room was made from (just
        // checked), which is borrowed for `'a`, and `taken` has moved past
        // it, so no other call returns any part of it.
        (first, unsafe { std::slice::from_raw_parts_mut(self.entries.add(first), count) })
    }

    /// How many entries were taken in all.
    fn taken(&self) -> usize {
        self.taken.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Runs `work` on up to `helpers` new threads and on this one, and returns
/// what each returned. `work` shares out the tasks itself, so a thread the
/// system does not give only leaves more for the others.
fn on_threads<T: Send>(helpers: usize, work: impl Fn() -> T + Sync) -> Vec<T> {
    std::thread::scope(|scope| {
        let threads: Vec<_> =
            (0..helpers).filter_map(|_| std::thread::Builder::new().spawn_scoped(scope, &work).ok()).collect();
        let mut results = vec![work()];
        results.extend(threads.into_iter().map(|thread| thread.join().expect("a link build thread")));
        results
    })
}

fn build_indexed_within(
    pairs: &[Vec<(u64, u64)>],
    users: &Interner,
    keys: &Keys,
    helpers: usize,
    limits: Limits,
) -> LinkTable {
    // Every line is inside a user or not; the lines inside go to the user's shard.
    let tasks: Vec<(usize, usize)> = pairs
        .iter()
        .enumerate()
        .flat_map(|(list, lines)| (0..lines.len()).step_by(SORT_CHUNK).map(move |start| (list, start)))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let sorted = on_threads(helpers, || {
        let mut sorted = Lines::new(keys);
        while let Some(&(list, start)) = tasks.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
            let lines = &pairs[list];
            for &(code_a, code_b) in &lines[start..(start + SORT_CHUNK).min(lines.len())] {
                sorted.add_codes(keys, code_a, code_b);
            }
        }
        sorted
    });
    build_sorted_within(&sorted, users, keys, helpers, limits)
}

fn build_sorted_within(sorted: &[Lines], users: &Interner, keys: &Keys, helpers: usize, limits: Limits) -> LinkTable {
    let user_of = |code: u64| keys.user_index_of_code(code);
    if keys.user_bound > MAX_USER_BOUND {
        // No shards (see `Lines::new`): one table, and every user is looked
        // up. The lines left out, of a user's ID to itself, say nothing.
        let all: Vec<Vec<(u64, u64)>> = sorted.iter().map(|sorted| sorted.across.clone()).collect();
        return LinkTable { table: build_within(&all, users, &user_of, limits), indexed: None };
    }
    let word_count = (keys.user_bound >> 6) as usize + 1;

    // A touched user has an ID in a complex component.
    let mut touched = vec![0u64; word_count];
    for &user in sorted.iter().flat_map(|sorted| &sorted.touched) {
        // (A user index out of range would be a wrong number given to `Lines::add`.)
        if let Some(word) = touched.get_mut((user >> 6) as usize) {
            *word |= 1 << (user & 63);
        }
    }

    // The shards, each done by one thread. A user with an entry has a line
    // inside it (a word or two) or is touched, so there are no more entries
    // than this; the pages that no entry is written to stay untouched.
    let inside: usize = sorted.iter().flat_map(|sorted| &sorted.shards).map(Vec::len).sum();
    let touches: usize = sorted.iter().map(|sorted| sorted.touched.len()).sum();
    let mut words = vec![[0u64; 2]; word_count];
    let mut entries = vec![[0u64; 2]; inside + touches];
    let (handed_over, taken) = {
        let room = EntryRoom::new(&mut entries);
        let shards = std::sync::Mutex::new(words.chunks_mut(SHARD_WORDS).enumerate());
        let handed_over = on_threads(helpers, || {
            let mut scratch = ShardScratch::new();
            let mut handed_over: Vec<(u64, u64)> = Vec::new();
            loop {
                // The lock is held while a shard is taken, not while it is done.
                let shard = shards.lock().expect("a link build thread").next();
                let Some((shard, words)) = shard else {
                    break;
                };
                scratch.summarize(keys, shard, words, sorted, &touched, &room, &mut handed_over);
            }
            handed_over
        });
        (handed_over, room.taken())
    };
    entries.truncate(taken);

    let mut rest: Vec<(u64, u64)> = Vec::new();
    for lines in &handed_over {
        rest.extend(lines);
    }
    for sorted in sorted {
        rest.extend(&sorted.across);
    }
    // The threads share out the work as it comes: sorted, the table does
    // not depend on which did what.
    rest.sort_unstable();
    let table = build_within(&[rest], users, &user_of, limits);
    LinkTable { table, indexed: Some(Indexed { keys: *keys, words, entries }) }
}

/// What a thread keeps from shard to shard.
struct ShardScratch {
    /// What the lines say of each user of the shard, in four words: a bit
    /// for each alias seen; a bit for each alias with a line to a lower
    /// alias; `CODE_MASK` less the smallest code seen; and 1 if an alias was
    /// 64 or more, which the first two words leave out. All zero for a user
    /// without lines, and between shards.
    users: Box<[[u64; 4]; SHARD_USERS]>,
    /// A bit for each user of the shard that has lines. All zero between shards.
    lined: [u64; SHARD_WORDS],
    /// The users whose lines are looked at one by one, in order: the user,
    /// its entry among the shard's, and whether it is `SLOW` whatever they say.
    open: Vec<(usize, usize, bool)>,
    /// A bit for each of them. All zero between shards.
    opened: [u64; SHARD_WORDS],
    /// For each of them: how many words of lines it has, then where its
    /// next word goes in `lines`. All zero between shards.
    places: Box<[usize; SHARD_USERS]>,
    /// Their lines as they come, and then those of a user together.
    picked: Vec<u64>,
    lines: Vec<u64>,
    /// For each alias below 64 of one user, the aliases it is linked to.
    /// All zero between users.
    rows: [u64; 64],
}

impl ShardScratch {
    fn new() -> Self {
        Self {
            users: vec![[0u64; 4]; SHARD_USERS].into_boxed_slice().try_into().expect("a place for each user"),
            lined: [0; SHARD_WORDS],
            open: Vec::new(),
            opened: [0; SHARD_WORDS],
            places: vec![0usize; SHARD_USERS].into_boxed_slice().try_into().expect("a place for each user"),
            picked: Vec::new(),
            lines: Vec::new(),
            rows: [0; 64],
        }
    }

    /// The summaries of the users of `shard`: fills in its `words`, and
    /// takes the entries they need from `room` and fills them. The lines of
    /// the users that are touched, or whose links one entry cannot describe,
    /// are added to `handed_over`.
    #[allow(clippy::too_many_arguments)]
    fn summarize(
        &mut self,
        keys: &Keys,
        shard: usize,
        words: &mut [[u64; 2]],
        sorted: &[Lines],
        touched: &[u64],
        room: &EntryRoom,
        handed_over: &mut Vec<(u64, u64)>,
    ) {
        let touched = &touched[shard * SHARD_WORDS..][..words.len()];
        let local_of = |line: u64| (line >> LINE_USER) as usize & (SHARD_USERS - 1);

        // What the lines say of each user, line by line as they come.
        for lines in sorted.iter().map(|sorted| sorted.shard(shard)) {
            for (at, &line) in lines.iter().enumerate() {
                if line & LINE_MORE != 0 {
                    continue;
                }
                let local = local_of(line);
                self.lined[local >> 6] |= 1 << (local & 63);
                let user = &mut self.users[local];
                user[2] = user[2].max(CODE_MASK ^ (line & CODE_MASK));
                let both = if line & LONG_LINE == LONG_LINE {
                    let more = lines[at + 1];
                    user[2] = user[2].max(CODE_MASK ^ (more & CODE_MASK));
                    let (alias_a, alias_b) = ((more >> LONG_A) & ALIAS_MASK, (more >> LONG_B) & ALIAS_MASK);
                    if alias_a.max(alias_b) >= 64 {
                        user[3] = 1;
                        continue;
                    }
                    1 << alias_a | 1 << alias_b
                } else {
                    alias_bits(line)
                };
                user[0] |= both;
                // The higher alias of the two has a line to a lower one.
                user[1] |= both & (both - 1);
            }
        }

        // The users with an entry: those with lines and those that are touched.
        let entry_count: u32 = self.lined.iter().zip(touched).map(|(lined, touched)| (lined | touched).count_ones()).sum();
        if entry_count == 0 {
            return;
        }
        let (first, entries) = room.take(entry_count as usize);

        // Every user with an entry, in order.
        let mut next = 0;
        for (at, word) in words.iter_mut().enumerate() {
            let lined = std::mem::take(&mut self.lined[at]);
            let bits = lined | touched[at];
            if bits == 0 {
                continue;
            }
            *word = [bits, (first + next) as u64];
            let mut users = bits;
            while users != 0 {
                let bit = users.trailing_zeros();
                users &= users - 1;
                let local = at * 64 + bit as usize;
                let [seen, lower, smallest, wide] = std::mem::take(&mut self.users[local]);
                let is_touched = (touched[at] >> bit) & 1 != 0;
                // When every alias but the lowest has a line to a lower
                // alias, each is linked to the lowest through lower and
                // lower ones: the user's IDs are one component. Generated
                // links are like that. Any other user with lines has them
                // looked at one by one.
                let surely_one = lower == seen & seen.wrapping_sub(1) && wide == 0;
                entries[next] = if surely_one && !is_touched {
                    [(CODE_MASK ^ smallest) | SIMPLE, seen]
                } else {
                    if (lined >> bit) & 1 != 0 {
                        self.open.push((local, next, is_touched || wide != 0));
                        self.opened[at] |= 1 << bit;
                    }
                    [SLOW, 0]
                };
                next += 1;
            }
        }
        if self.open.is_empty() {
            return;
        }

        // The lines of the open users: picked out as they come, then
        // grouped by user, the users in order.
        self.picked.clear();
        for lines in sorted.iter().map(|sorted| sorted.shard(shard)) {
            for (at, &line) in lines.iter().enumerate() {
                let local = local_of(line);
                if line & LINE_MORE != 0 || (self.opened[local >> 6] >> (local & 63)) & 1 == 0 {
                    continue;
                }
                let long = line & LONG_LINE == LONG_LINE;
                self.picked.push(line);
                if long {
                    self.picked.push(lines[at + 1]);
                }
                self.places[local] += 1 + usize::from(long);
            }
        }
        let mut start = 0;
        for &(local, _, _) in &self.open {
            start += std::mem::replace(&mut self.places[local], start);
        }
        if self.lines.len() < self.picked.len() {
            self.lines.resize(self.picked.len(), 0);
        }
        let mut at = 0;
        while at < self.picked.len() {
            let line = self.picked[at];
            let place = &mut self.places[local_of(line)];
            let size = 1 + usize::from(line & LONG_LINE == LONG_LINE);
            self.lines[*place..][..size].copy_from_slice(&self.picked[at..at + size]);
            *place += size;
            at += size;
        }
        // `places` now has where the words of each open user end, and they
        // start where those of the open user before it end.
        let mut start = 0;
        for &(local, entry, surely_slow) in &self.open {
            let end = std::mem::take(&mut self.places[local]);
            let lines = &self.lines[start..end];
            start = end;
            self.opened[local >> 6] = 0;
            // (Touched, or with an alias that a mask has no bit for.)
            let one_component = if surely_slow { None } else { one_component(lines, &mut self.rows) };
            match one_component {
                Some((name, mask)) => entries[entry] = [name | SIMPLE, mask],
                None => hand_over(keys, (shard * SHARD_USERS + local) as u64, lines, handed_over),
            }
        }
        self.open.clear();
    }
}

/// The bits of the two aliases of a line of one word.
#[inline(always)]
fn alias_bits(line: u64) -> u64 {
    1 << ((line >> LINE_A) & 15) | 1 << ((line >> LINE_B) & 15)
}

/// If the lines, all inside one user and with no alias of 64 or more, make
/// one component: the smallest of its codes and the mask of its aliases.
/// Every alias gets a row of the aliases it is linked to, and the rows are
/// walked from one alias on. `rows` is all zero before and after.
fn one_component(lines: &[u64], rows: &mut [u64; 64]) -> Option<(u64, u64)> {
    let (mut seen, mut smallest) = (0u64, u64::MAX);
    let mut at = 0;
    while at < lines.len() {
        // Generated IDs have pattern codes, which order like the IDs.
        let line = lines[at];
        smallest = smallest.min(line & CODE_MASK);
        let (alias_a, alias_b) = if line & LONG_LINE == LONG_LINE {
            let more = lines[at + 1];
            at += 2;
            smallest = smallest.min(more & CODE_MASK);
            ((more >> LONG_A) & ALIAS_MASK, (more >> LONG_B) & ALIAS_MASK)
        } else {
            at += 1;
            ((line >> LINE_A) & 15, (line >> LINE_B) & 15)
        };
        // (An alias of 64 or more has no row; the caller sees to such users.)
        rows[alias_a as usize] |= 1 << alias_b;
        rows[alias_b as usize] |= 1 << alias_a;
        seen |= 1 << alias_a | 1 << alias_b;
    }
    // The aliases that the rows reach from the lowest one; a row is cleared
    // when its alias is reached.
    let mut reached = seen & seen.wrapping_neg();
    let mut fresh = reached;
    while fresh != 0 {
        let alias = fresh.trailing_zeros() as usize;
        fresh &= fresh - 1;
        let new = std::mem::take(&mut rows[alias]) & !reached;
        reached |= new;
        fresh |= new;
    }
    let mut left = seen & !reached;
    while left != 0 {
        rows[left.trailing_zeros() as usize] = 0;
        left &= left - 1;
    }
    (reached == seen).then_some((smallest, seen))
}

/// Adds the lines of user index `user` to `out`, as pairs of codes.
fn hand_over(keys: &Keys, user: u64, lines: &[u64], out: &mut Vec<(u64, u64)>) {
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        if line & LONG_LINE == LONG_LINE {
            out.push((line & CODE_MASK, lines[at + 1] & CODE_MASK));
            at += 2;
        } else {
            // The IDs of a short line, from the user and their aliases.
            let id = |shift: u32| keys.code_of_user_number(user << ALIAS_BITS | (line >> shift) & 15);
            out.push((id(LINE_A), id(LINE_B)));
            at += 1;
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

/// The same result with a hash map and dense indices: slower, but it takes
/// any input.
fn build_general(
    pairs: &[Vec<(u64, u64)>],
    exotic: &Exotic,
    group: &impl Fn(u64) -> Option<u64>,
) -> Table {
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
    let mut several = vec![false; codes.len()];
    for at in 0..codes.len() {
        let root = root_of[at] as usize;
        if !matches!((group(codes[at]), group(codes[root])), (Some(one), Some(other)) if one == other) {
            complex[root] = true;
        }
        several[root] |= at != root;
    }
    let stored = |at: usize| name[root_of[at] as usize] != codes[at] || complex[root_of[at] as usize];
    let mut members: Vec<u64> = Vec::new();
    let mut entries: Vec<(u64, u64)> = Vec::new();
    for at in 0..codes.len() {
        if stored(at) {
            let root = root_of[at] as usize;
            if complex[root] {
                members.push(codes[at]);
            }
            entries.push((mix(codes[at]), if complex[root] { name[root] | COMPLEX_BIT } else { name[root] }));
        }
    }
    entries.sort_unstable();
    let mut writer = TableWriter::new(entries.len());
    for &(hash, name) in &entries {
        writer.push(hash, name);
    }
    let mut table = writer.finish();
    table.complex = members;
    for root in 0..codes.len() {
        if root_of[root] as usize == root && several[root] && !complex[root] {
            table.simple_names.push(name[root]);
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
        // What `for_each_member` must visit: every ID of every component
        // that has two or more IDs or is complex.
        let mut sizes: HashMap<&Id, usize> = HashMap::new();
        for name in names.values() {
            *sizes.entry(name).or_default() += 1;
        }
        let members = |with_groups: bool| -> HashMap<u64, (u64, bool)> {
            names
                .iter()
                .filter(|(id, name)| sizes[name] >= 2 || (with_groups && complex.contains(*id)))
                .map(|(id, name)| (code(id), (code(name), with_groups && complex.contains(id))))
                .collect()
        };
        let visited = |table: &Table, path: &str| -> HashMap<u64, (u64, bool)> {
            let mut seen = HashMap::new();
            table.for_each_member(|member, name, flag| {
                assert!(seen.insert(member, (name, flag)).is_none(), "{path}: an ID visited twice");
            });
            seen
        };
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
            assert_eq!(visited(&table, path), members(true), "{path}: for_each_member");
        }
        // Without groups the names are the same and nothing is complex.
        let table = build(&lists, &users);
        for (id, name) in &names {
            assert_eq!(table.lookup(code(id)), (code(name), false));
        }
        assert!(table.complex_members().is_empty());
        assert_eq!(visited(&table.table, "plain build"), members(false), "plain build: for_each_member");
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
            // Every ID above but `u3`, alone in a simple component, and the
            // two that are in no link line; the names `U9` and `Ua` included.
            let mut visited: Vec<(u64, u64, bool)> = Vec::new();
            table.for_each_member(|member, name, flag| visited.push((member, name, flag)));
            visited.sort_unstable();
            let mut expected_visits: Vec<(u64, u64, bool)> = expected
                .iter()
                .filter(|(user, _, _)| !["u3", "u9", "U1"].contains(user))
                .map(|&(user, name, flag)| (code(user), code(name), flag))
                .collect();
            expected_visits.sort_unstable();
            assert_eq!(visited, expected_visits, "{path}");
        }
        check(&mut Random(7), &pairs, &groups);
    }

    /// Keys like the generator's, for user indexes below `user_bound`.
    fn test_keys(random: &mut Random, user_bound: u64) -> Keys {
        let key = random.next() & crate::fast::keys::MASK;
        Keys { event_key: 1, user_key: key, index_bound: 1 << 20, user_bound, index_shift: 14 }
    }

    /// The summary of every user index below the keys' bound, read off a
    /// table one member at a time, as index mode first did it. An entry
    /// with `SLOW` says nothing else.
    fn summary_from_members(table: &Table, keys: &Keys) -> Vec<[u64; 2]> {
        let mut users = vec![[0u64; 2]; keys.user_bound as usize];
        table.for_each_member(|member, name, complex| {
            let Some(number) = keys.user_number_of_code(member) else {
                return;
            };
            let entry = &mut users[(number >> ALIAS_BITS) as usize];
            let alias = number & ALIAS_MASK;
            if complex || alias >= 64 {
                entry[0] |= SLOW;
            } else if entry[0] & SIMPLE == 0 {
                entry[0] |= SIMPLE | name;
                entry[1] |= 1 << alias;
            } else if entry[0] & CODE_MASK == name {
                entry[1] |= 1 << alias;
            } else {
                entry[0] |= SLOW;
            }
        });
        users.iter().map(|&entry| if entry[0] & SLOW != 0 { [SLOW, 0] } else { entry }).collect()
    }

    /// Link lines of generated IDs of the users below `users`, by code:
    /// small trees and long chains inside a user, users whose links fall in
    /// two components, aliases of 64 and more, lines to other users (some of
    /// them the user's only link), lines to IDs that are no user's, repeats
    /// either way round and self-links, shuffled over a few lists.
    fn random_user_lines(random: &mut Random, keys: &Keys, exotic: &Interner, users: u64) -> Vec<Vec<(u64, u64)>> {
        let id = |user: u64, alias: u64| keys.code_of_user_number(user << ALIAS_BITS | alias);
        let mut lines: Vec<(u64, u64)> = Vec::new();
        for user in 0..users {
            if random.chance(4) {
                continue;
            }
            let chain = random.chance(30);
            let size = if chain { 20 + random.below(280) } else { 1 + random.below(8) } as u64;
            // Aliases from 0 up, or from somewhere higher, across 64 at times.
            let first = if random.chance(8) { random.below(512 - size as usize) as u64 } else { 0 };
            let split = if random.chance(15) { 1 + random.below(size as usize) as u64 } else { size };
            for at in 1..size {
                // Leaving one line out makes two components of the user's IDs.
                if at == split {
                    continue;
                }
                let earlier = if chain { at - 1 } else { random.below(at as usize) as u64 };
                lines.push((id(user, first + earlier), id(user, first + at)));
            }
            let mine = id(user, first + random.below(size as usize) as u64);
            if size == 1 || random.chance(15) {
                lines.push((mine, mine));
            }
            if random.chance(25) {
                // To another user: sometimes to one of its linked IDs, sometimes not.
                let other = random.below(users as usize) as u64;
                lines.push((mine, id(other, random.below(10) as u64)));
            }
            if random.chance(40) {
                let text = format!("X{}", random.below(50));
                let other = id_code(text.as_bytes(), b'u', exotic);
                lines.push(if random.chance(2) { (mine, other) } else { (other, mine) });
                if random.chance(3) {
                    lines.push((other, other));
                }
            }
            if random.chance(60) {
                // A line that only touches the user: from an ID with no other link.
                lines.push((id(user, 511), id((user + 1) % users, 510)));
            }
        }
        for at in 0..lines.len() {
            if random.chance(12) {
                let (a, b) = lines[at];
                lines.push(if random.chance(2) { (a, b) } else { (b, a) });
            }
        }
        random.shuffle(&mut lines);
        let mut lists: Vec<Vec<(u64, u64)>> = vec![Vec::new(); 1 + random.below(9)];
        for line in lines {
            let list = random.below(lists.len());
            lists[list].push(line);
        }
        lists
    }

    /// The sharded build against the single grouped build of the same lines
    /// and the summary read off it: every ID of the lines, IDs of no line,
    /// the complex members, the visits, and every user index.
    fn check_shards(random: &mut Random, keys: &Keys, exotic: &Interner, lists: &[Vec<(u64, u64)>]) {
        let single = build_within(lists, exotic, &|code| keys.user_index_of_code(code), Limits::NORMAL);
        let summary = summary_from_members(&single, keys);
        let mut codes: Vec<u64> = lists.iter().flatten().flat_map(|&(a, b)| [a, b]).collect();
        // IDs of no line: of users in and out of range, and no user's.
        for _ in 0..300 {
            let user = random.below(keys.user_bound as usize * 2 + 2) as u64;
            codes.push(keys.code_of_user_number(user << ALIAS_BITS | random.below(512) as u64));
            codes.push(id_code(format!("Y{}", random.below(1000)).as_bytes(), b'u', exotic));
        }
        let visits = |visit: &dyn Fn(&mut dyn FnMut(u64, u64, bool))| -> HashMap<u64, (u64, bool)> {
            let mut seen = HashMap::new();
            visit(&mut |member, name, flag| {
                assert!(seen.insert(member, (name, flag)).is_none(), "an ID visited twice");
            });
            seen
        };
        let expected_visits = visits(&|visit| single.for_each_member(visit));
        let mut expected_complex: Vec<u64> = single.complex_members().to_vec();
        expected_complex.sort_unstable();
        // The same lines given with their user numbers, as a parser would,
        // and shared out differently.
        let mut given: Vec<Lines> = (0..1 + random.below(3)).map(|_| Lines::new(keys)).collect();
        for &(a, b) in lists.iter().flatten() {
            let lines = random.below(given.len());
            given[lines].add(a, keys.user_number_of_code(a), b, keys.user_number_of_code(b));
        }
        for (helpers, from_pairs) in [(0, true), (1, true), (3, true), (0, false), (2, false)] {
            for (path, limits) in PATHS {
                let table = match from_pairs {
                    true => build_indexed_within(lists, exotic, keys, helpers, limits),
                    false => build_sorted_within(&given, exotic, keys, helpers, limits),
                };
                let path = format!("{path}, {}", if from_pairs { "from pairs" } else { "from lines" });
                for &code in &codes {
                    table.prefetch(code);
                    assert_eq!(table.lookup(code), single.lookup(code), "{path}, {helpers} helpers: {code}");
                    assert_eq!(table.person(code), single.lookup(code).0, "{path}, {helpers} helpers: {code}");
                }
                let mut complex: Vec<u64> = table.complex_members().to_vec();
                complex.sort_unstable();
                assert_eq!(complex, expected_complex, "{path}, {helpers} helpers");
                assert_eq!(visits(&|visit| table.for_each_member(visit)), expected_visits, "{path}, {helpers} helpers");
                for user in 0..keys.user_bound {
                    assert_eq!(table.summary(user), summary[user as usize], "{path}, {helpers} helpers: user {user}");
                }
                assert_eq!(table.summary(keys.user_bound), [SLOW, 0], "{path}");
                assert_eq!(table.summary(u64::MAX), [SLOW, 0], "{path}");
            }
        }
    }

    #[test]
    fn shards_match_the_single_build() {
        let mut random = Random(0xA076_1D64_78BD_642F);
        for round in 0..40 {
            // Users over one shard or several, some past the keys' bound.
            let users = [1, 2, 7, 60, 700, 5000, 20_000][round % 7];
            let user_bound = [users, users + 5, users - users / 4, users / 2 + 1][round % 4];
            let keys = test_keys(&mut random, user_bound);
            let exotic = Interner::default();
            let lists = random_user_lines(&mut random, &keys, &exotic, users);
            check_shards(&mut random, &keys, &exotic, &lists);
        }
        // No lines at all.
        let keys = test_keys(&mut random, 100);
        check_shards(&mut random, &keys, &Interner::default(), &[]);
    }

    #[test]
    fn the_quick_number_of_a_code_is_the_keys_own() {
        let mut random = Random(0x1234_5678_9ABC_DEF1);
        let exotic = Interner::default();
        let keys = test_keys(&mut random, 1 << 22);
        let small = test_keys(&mut random, 3);
        let mut codes: Vec<u64> = vec![0, 1, 36, 37, 38, P37[4] - 1, P37[4], P37[4] + 1, EXOTIC_BASE - 1, EXOTIC_BASE, EXOTIC_BASE + 5, CODE_MASK];
        // The IDs of users in and out of range, long and short.
        for user in (0..2000u64).chain((1 << 22) - 3..(1 << 22) + 3) {
            for alias in [0, 1, 63, 64, 511] {
                codes.push(keys.code_of_user_number(user << ALIAS_BITS | alias));
            }
        }
        for _ in 0..200_000 {
            // Any code below the exotic ones, and any number as an ID.
            codes.push(random.next() % EXOTIC_BASE);
            codes.push(crate::fast::keys::code37_of_value(random.next() & NUMBER_MASK));
            codes.push(crate::fast::keys::code37_of_value(random.next() & NUMBER_MASK >> random.below(40)));
            // Texts of every length, with `0` in front or not, or one character too high.
            let length = 1 + random.below(8);
            let text: Vec<u8> = (0..length)
                .map(|at| {
                    let digit = if at == 0 && random.chance(4) { 0 } else { random.below(36) };
                    b"0123456789abcdefghijklmnopqrstuvwxyz"[digit]
                })
                .collect();
            let mut id = vec![b'u'];
            id.extend(&text);
            codes.push(id_code(&id, b'u', &exotic));
        }
        let mut found = 0;
        for &code in &codes {
            for keys in [&keys, &small] {
                let expected = keys.user_number_of_code(code);
                assert_eq!(user_number_of_code(keys, code), expected, "code {code}");
                found += usize::from(expected.is_some());
            }
        }
        assert!(found > 10_000, "{found} codes with a user number");
    }

    #[test]
    fn more_users_than_the_shards_take() {
        let mut random = Random(77);
        let keys = test_keys(&mut random, MAX_USER_BOUND + 1);
        let exotic = Interner::default();
        let id = |user: u64, alias: u64| keys.code_of_user_number(user << ALIAS_BITS | alias);
        let no_users = id_code(b"Ux", b'u', &exotic);
        let lines = vec![
            (id(0, 0), id(0, 1)),
            (id(0, 1), id(0, 2)),
            (id(1, 0), id(2, 0)),
            (id(3, 5), id(3, 5)),
            (id(MAX_USER_BOUND, 7), id(MAX_USER_BOUND, 8)),
            (id(4, 0), no_users),
            (no_users, no_users),
        ];
        let single = build_within(&[lines.clone()], &exotic, &|code| keys.user_index_of_code(code), Limits::NORMAL);
        let mut expected_complex: Vec<u64> = single.complex_members().to_vec();
        expected_complex.sort_unstable();
        let mut given = Lines::new(&keys);
        for &(a, b) in &lines {
            given.add(a, keys.user_number_of_code(a), b, keys.user_number_of_code(b));
        }
        for table in [build_indexed(&[lines.clone()], &exotic, &keys, 1), build_sorted(&[given], &exotic, &keys, 0)] {
            for &(a, b) in &lines {
                assert_eq!(table.lookup(a), single.lookup(a));
                assert_eq!(table.lookup(b), single.lookup(b));
            }
            assert_eq!(table.lookup(id(9, 9)), (id(9, 9), false));
            let mut complex: Vec<u64> = table.complex_members().to_vec();
            complex.sort_unstable();
            assert_eq!(complex, expected_complex);
            // Every user is looked up.
            for user in [0, 1, 3, 4, 9, MAX_USER_BOUND, u64::MAX] {
                assert_eq!(table.summary(user), [SLOW, 0]);
            }
        }
        assert_eq!(single.lookup(id(0, 2)).0, id(0, 0).min(id(0, 1)).min(id(0, 2)));
        assert_eq!(single.lookup(id(MAX_USER_BOUND, 8)), (id(MAX_USER_BOUND, 7).min(id(MAX_USER_BOUND, 8)), false));
    }

    #[test]
    fn shards_by_hand() {
        let mut random = Random(5);
        let keys = test_keys(&mut random, 11);
        let exotic = Interner::default();
        let id = |user: u64, alias: u64| keys.code_of_user_number(user << ALIAS_BITS | alias);
        let no_users = id_code(b"Ux", b'u', &exotic);
        let lines = vec![
            // One component of three inside user 0.
            (id(0, 0), id(0, 1)),
            (id(0, 1), id(0, 2)),
            // Two components inside user 1.
            (id(1, 0), id(1, 1)),
            (id(1, 2), id(1, 3)),
            // Users 2 and 3 share a component; user 3 has one of its own too.
            (id(2, 0), id(3, 0)),
            (id(3, 4), id(3, 5)),
            // A member with an alias of 64 or more in user 4.
            (id(4, 0), id(4, 1)),
            (id(4, 1), id(4, 70)),
            // Linked only to itself in user 5.
            (id(5, 9), id(5, 9)),
            // A component of two and an ID linked only to itself in user 6.
            (id(6, 63), id(6, 5)),
            (id(6, 6), id(6, 6)),
            // Linked to an ID that is no user's in user 7.
            (id(7, 0), no_users),
            (no_users, no_users),
            // A cycle with repeats, either way round, in user 8.
            (id(8, 0), id(8, 1)),
            (id(8, 1), id(8, 2)),
            (id(8, 2), id(8, 0)),
            (id(8, 1), id(8, 0)),
            (id(8, 0), id(8, 0)),
            // The smallest alias that the mask has no bit for, in user 9.
            (id(9, 3), id(9, 64)),
            // User 12 is past the bound: its IDs are no user's.
            (id(12, 0), id(12, 1)),
        ];
        let smallest = |codes: &[u64]| *codes.iter().min().unwrap();
        let expected = [
            [smallest(&[id(0, 0), id(0, 1), id(0, 2)]) | SIMPLE, 0b111],
            [SLOW, 0],
            [SLOW, 0],
            [SLOW, 0],
            [SLOW, 0],
            [0, 0],
            [smallest(&[id(6, 63), id(6, 5)]) | SIMPLE, 1 << 63 | 1 << 5],
            [SLOW, 0],
            [smallest(&[id(8, 0), id(8, 1), id(8, 2)]) | SIMPLE, 0b111],
            [SLOW, 0],
            [0, 0],
        ];
        for helpers in [0, 2] {
            let table = build_indexed(&[lines.clone()], &exotic, &keys, helpers);
            for (user, entry) in expected.iter().enumerate() {
                assert_eq!(table.summary(user as u64), *entry, "user {user}");
            }
            assert_eq!(table.summary(11), [SLOW, 0]);
            assert_eq!(table.lookup(id(0, 2)), (smallest(&[id(0, 0), id(0, 1), id(0, 2)]), false));
            assert_eq!(table.lookup(id(0, 3)), (id(0, 3), false));
            assert_eq!(table.lookup(id(1, 3)), (id(1, 2).min(id(1, 3)), false));
            assert_eq!(table.lookup(id(2, 0)), (id(2, 0).min(id(3, 0)), true));
            assert_eq!(table.lookup(id(3, 5)), (id(3, 4).min(id(3, 5)), false));
            assert_eq!(table.lookup(id(4, 70)), (smallest(&[id(4, 0), id(4, 1), id(4, 70)]), false));
            assert_eq!(table.lookup(id(5, 9)), (id(5, 9), false));
            assert_eq!(table.lookup(id(7, 0)), (no_users, true));
            assert_eq!(table.lookup(no_users), (no_users, true));
            assert_eq!(table.lookup(id(12, 1)), (id(12, 0).min(id(12, 1)), true));
        }
        check_shards(&mut random, &keys, &exotic, &[lines]);
    }

    #[test]
    fn lines_of_one_word_and_of_two() {
        let mut random = Random(11);
        let keys = test_keys(&mut random, 40);
        let exotic = Interner::default();
        let id = |user: u64, alias: u64| keys.code_of_user_number(user << ALIAS_BITS | alias);
        let smallest = |user: u64, aliases: &[u64]| aliases.iter().map(|&alias| id(user, alias)).min().unwrap();
        let mask = |aliases: &[u64]| aliases.iter().fold(0u64, |mask, &alias| mask | 1 << alias);
        // Per user: its lines as pairs of aliases, and the aliases of its one component, if it is one.
        let users: Vec<(Vec<(u64, u64)>, Option<Vec<u64>>)> = vec![
            // One line, the largest aliases of a word, either way round.
            (vec![(14, 15)], Some(vec![14, 15])),
            (vec![(15, 0)], Some(vec![0, 15])),
            // One line of two words: an alias of 16, of 63, both large.
            (vec![(3, 16)], Some(vec![3, 16])),
            (vec![(63, 2)], Some(vec![2, 63])),
            (vec![(40, 20)], Some(vec![20, 40])),
            // One line with an alias the mask has no bit for.
            (vec![(0, 64)], None),
            (vec![(511, 510)], None),
            // Two lines of a word each: sharing an ID, the same line twice, the same line turned, apart.
            (vec![(0, 1), (1, 2)], Some(vec![0, 1, 2])),
            (vec![(5, 6), (5, 6)], Some(vec![5, 6])),
            (vec![(5, 6), (6, 5)], Some(vec![5, 6])),
            (vec![(0, 1), (2, 3)], None),
            // A word and two words: sharing an ID, apart, with a wide alias.
            (vec![(0, 1), (1, 30)], Some(vec![0, 1, 30])),
            (vec![(30, 1), (0, 1)], Some(vec![0, 1, 30])),
            (vec![(0, 1), (16, 17)], None),
            (vec![(0, 1), (1, 100)], None),
            // Three and more: a path given out of order, a cycle, a star, two parts, a part that joins late.
            (vec![(0, 1), (2, 3), (1, 2)], Some(vec![0, 1, 2, 3])),
            (vec![(0, 1), (1, 2), (2, 0)], Some(vec![0, 1, 2])),
            (vec![(9, 1), (9, 2), (9, 3), (9, 33), (9, 5)], Some(vec![1, 2, 3, 5, 9, 33])),
            (vec![(0, 1), (2, 3), (3, 4)], None),
            (vec![(0, 1), (2, 3), (4, 5), (5, 0), (3, 4)], Some(vec![0, 1, 2, 3, 4, 5])),
            (vec![(0, 1), (2, 3), (4, 5), (5, 0)], None),
            // Many, with a wide alias first, in the middle, last.
            (vec![(0, 200), (0, 1), (1, 2)], None),
            (vec![(0, 1), (1, 200), (1, 2)], None),
            (vec![(0, 1), (1, 2), (2, 64)], None),
            // All 64 aliases in a path from the top down, and with one alias more.
            ((1..64).rev().map(|alias| (alias, alias - 1)).collect(), Some((0..64).collect())),
            ((1..65).rev().map(|alias| (alias, alias - 1)).collect(), None),
        ];
        let mut lines: Vec<(u64, u64)> = Vec::new();
        for (user, (pairs, _)) in users.iter().enumerate() {
            lines.extend(pairs.iter().map(|&(a, b)| (id(user as u64, a), id(user as u64, b))));
        }
        // A user with lines after one that is handed over, the rows of bits clean again.
        random.shuffle(&mut lines);
        for helpers in [0, 2] {
            let table = build_indexed(&[lines.clone()], &exotic, &keys, helpers);
            for (user, (pairs, component)) in users.iter().enumerate() {
                let user = user as u64;
                let expected = match component {
                    Some(aliases) => [smallest(user, aliases) | SIMPLE, mask(aliases)],
                    None => [SLOW, 0],
                };
                assert_eq!(table.summary(user), expected, "user {user}: {pairs:?}");
            }
            assert_eq!(table.summary(users.len() as u64), [0, 0]);
        }
        check_shards(&mut random, &keys, &exotic, &[lines]);
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
        assert!(build_in_forest(&[list.clone()], &Exotic::default(), &|_| Some(0), MIN_LINE_BITS).is_none());
        assert!(build_in_forest(&[list.clone()], &Exotic::default(), &|_| Some(0), 9).is_none());
        let table = build_in_forest(&[list], &Exotic::default(), &|_| Some(0), 10).expect("room for 5000 IDs in 8192 slots");
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
    fn ids_whose_hashes_crowd_one_place() {
        // `unmix` of consecutive numbers gives IDs with consecutive hashes:
        // one home line in any forest, one home position in the table. Sixty
        // of them fit a forest and overflow the table's displacement into
        // its stash; three hundred overflow the forest too.
        let users = Interner::default();
        for count in [60, 300] {
            let codes: Vec<u64> = (1..)
                .map(|low| unmix((0x155_5555 << 17) | low))
                .filter(|&code| code < EXOTIC_BASE)
                .take(count + 40)
                .collect();
            let (linked, unlinked) = codes.split_at(count);
            let list: Vec<(u64, u64)> = linked.windows(2).map(|pair| (pair[0], pair[1])).collect();
            let name = *linked.iter().min().unwrap();
            for (path, limits) in PATHS {
                for complex in [false, true] {
                    let group = |_: u64| if complex { None } else { Some(0) };
                    let table = build_within(&[list.clone()], &users, &group, limits);
                    assert!(table.stash.len() >= count - 20, "{path}: {} in the stash", table.stash.len());
                    for &code in linked {
                        assert_eq!(table.lookup(code), (name, complex), "{path}");
                    }
                    for &code in unlinked {
                        assert_eq!(table.lookup(code), (code, false), "{path}");
                    }
                    let mut visited: Vec<u64> = Vec::new();
                    table.for_each_member(|member, to, flag| {
                        assert_eq!((to, flag), (name, complex), "{path}");
                        visited.push(member);
                    });
                    visited.sort_unstable();
                    let mut expected = linked.to_vec();
                    expected.sort_unstable();
                    assert_eq!(visited, expected, "{path}");
                    assert_eq!(table.complex_members().len(), if complex { count } else { 0 }, "{path}");
                }
            }
        }
    }

    #[test]
    fn tables_of_every_size_answer_like_a_map() {
        let mut random = Random(4242);
        for position_bits in [23, 24, 25, 26, 27, 28] {
            // Random hashes, and clumps of nearby ones to displace entries.
            let mut hashes: Vec<u64> = (0..3000).map(|_| random.next() & CODE_MASK).collect();
            for _ in 0..200 {
                let base = random.next() & CODE_MASK & !0xFFFF;
                hashes.extend((0..1 + random.below(40)).map(|_| base | (random.next() & 0xFFFF)));
            }
            hashes.sort_unstable();
            hashes.dedup();
            let mut expected: HashMap<u64, (u64, bool)> = HashMap::new();
            let mut writer = TableWriter::with_position_bits(hashes.len(), position_bits);
            for &hash in &hashes {
                let (name, complex) = (1 + (random.next() & (CODE_MASK >> 1)), random.chance(3));
                writer.push(hash, name | if complex { COMPLEX_BIT } else { 0 });
                expected.insert(unmix(hash), (name, complex));
            }
            let table = writer.finish();
            assert_eq!(table.entries.len() + table.stash.len(), hashes.len());
            for (&user, &answer) in &expected {
                table.prefetch(user);
                assert_eq!(table.lookup(user), answer, "{position_bits} bits");
            }
            // IDs without an entry, many of them next to ones that have one.
            for &hash in &hashes {
                for other in [hash ^ 1, hash ^ (1 << 20), hash.wrapping_add(1 << table.rem_bits) & CODE_MASK] {
                    let user = unmix(other);
                    if !expected.contains_key(&user) {
                        assert_eq!(table.lookup(user), (user, false), "{position_bits} bits");
                    }
                }
            }
            let mut visited: HashMap<u64, (u64, bool)> = HashMap::new();
            table.for_each_member(|member, name, complex| {
                assert!(visited.insert(member, (name, complex)).is_none());
            });
            assert_eq!(visited, expected, "{position_bits} bits");
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

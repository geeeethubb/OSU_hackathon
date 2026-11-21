//! Index mode: events in index order, most persons sessionized where they lie.
//!
//! The generator hands out event indexes user by user, in time order, and
//! all IDs of a user share a user index (see `keys`). When the data shows
//! both, a partition of consecutive indexes holds whole users one after the
//! other: putting each record in the slot of its index picks the winning copy
//! of every event without hashing, and reading the slots in order gives each
//! user's events already grouped (a "run").
//!
//! A person whose linked IDs all have one user index (a simple link
//! component, or an ID in no link) has all its events in that user's run and
//! all its clicks under that user index in the click index, so it goes
//! straight to the session kernel. Everything else takes the general path
//! through the person partitions: IDs of complex components, IDs that are not
//! generated ones, and events whose own ID is not a generated one.
//!
//! What this relies on is checked, not assumed:
//! - "each user's events are one run" holds when user indexes never decrease
//!   along the index order. `Runner` checks that inside a partition and
//!   `chains` across partitions; a violation makes the engine start over on
//!   the general path.
//! - Time order inside a run is not relied on: the kernel orders the events.
//! - A run that reaches a partition's edge may continue in the next one, so
//!   first and last runs are set aside and joined afterwards (`chains`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::arena::PartList;
use super::codes::{unmix, CODE_MASK};
use super::dedup::{forward, forward_click, forward_named, PassBOut};
use super::keys::{Keys, USER_NUMBER};
use super::links::LinkTable;
use super::parse::{INDEX_SLOTS, RAW_CLICK_WORDS};
use super::sessions::{PassCOut, ScratchC, EVENT_SPARE};

const INGEST_MASK: u64 = 0x1F_FFFF;
const TS_MASK: u64 = 0x7FFF_FFFF;
/// IDs per user index.
const ALIASES: u64 = 512;
/// Set in a resolved name when the ID belongs to a complex component.
const COMPLEX: u64 = 1 << 63;

/// One winning event: the two slot words (see `parse::emit_indexed`) and the
/// purchase word.
pub type Record = [u64; 3];

/// Where a partition's records are placed by index.
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

/// The valid clicks of generated user IDs, grouped by user index.
pub struct ClickIndex {
    /// `ends[user]` is one past the user's last click in `clicks`; its first
    /// is at `ends[user - 1]` (0 for user 0).
    ends: Vec<u32>,
    /// `[ts since seven days before the window (32 low) | alias (9 bits
    /// above), click code (42 low) | campaign]`.
    clicks: Vec<[u64; 2]>,
}

impl ClickIndex {
    #[inline]
    pub fn of(&self, user: u64) -> &[[u64; 2]] {
        let user = user as usize;
        if user >= self.ends.len() {
            return &[];
        }
        let start = if user == 0 { 0 } else { self.ends[user - 1] as usize };
        &self.clicks[start..self.ends[user] as usize]
    }

    pub fn len(&self) -> usize {
        self.clicks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clicks.is_empty()
    }
}

/// Builds a `ClickIndex` in two parallel passes over the parsed clicks with a
/// serial step between them: `count` every batch, `layout`, `scatter` every
/// batch, `finish`.
pub struct ClickIndexBuilder {
    /// Per user index: its click count, then where its next click goes.
    cursors: Vec<AtomicU32>,
    clicks: Vec<[u64; 2]>,
}

/// The click array while several threads fill disjoint entries of it.
#[derive(Clone, Copy)]
pub struct ClickTarget(*mut [u64; 2]);

// SAFETY: every entry is written by exactly one thread (the cursors hand each
// position out once), and nothing reads the array until all have finished.
unsafe impl Send for ClickTarget {}
unsafe impl Sync for ClickTarget {}

impl ClickIndexBuilder {
    pub fn new(keys: &Keys) -> Self {
        // Zeroed pages stay untouched until a user index in them is counted.
        let mut zeros = std::mem::ManuallyDrop::new(vec![0u32; keys.user_bound as usize + 1]);
        // SAFETY: `AtomicU32` has the size, alignment and bit validity of
        // `u32`, and the vector's buffer changes hands exactly once.
        let cursors =
            unsafe { Vec::from_raw_parts(zeros.as_mut_ptr() as *mut AtomicU32, zeros.len(), zeros.capacity()) };
        Self { cursors, clicks: Vec::new() }
    }

    /// Counts a batch of parsed clicks (blocks of `[user, click code and
    /// campaign, ts]`, the user as index mode's `emit_click` stores it). A
    /// click of an ID that is not a generated one goes to its person's
    /// partition instead. Returns the highest user index seen plus one.
    pub fn count(&self, links: &LinkTable, blocks: &[&[u64]], out: &mut PassBOut) -> u64 {
        let mut users = 0;
        for block in blocks {
            for click in block.chunks_exact(RAW_CLICK_WORDS) {
                if click[0] & USER_NUMBER != 0 {
                    let user = (click[0] & !USER_NUMBER) >> 9;
                    // SAFETY: `emit_click` only stores numbers whose index
                    // is below `user_bound`, the length of `cursors`.
                    unsafe { self.cursors.get_unchecked(user as usize) }.fetch_add(1, Ordering::Relaxed);
                    users = users.max(user + 1);
                } else {
                    forward_click(out, links.person(click[0]), click[2], click[1]);
                }
            }
        }
        users
    }

    /// Turns the counts of the first `users` user indexes into start
    /// positions and makes room for the clicks.
    pub fn layout(&mut self, users: u64) -> ClickTarget {
        self.cursors.truncate(users as usize);
        let mut total = 0u32;
        for cursor in &mut self.cursors {
            let count = *cursor.get_mut();
            *cursor.get_mut() = total;
            total += count;
        }
        self.clicks = Vec::with_capacity(total as usize);
        // SAFETY: `scatter` writes each of the `total` entries exactly once
        // before `finish` hands the vector out, and `[u64; 2]` has no invalid values.
        #[allow(clippy::uninit_vec)]
        unsafe {
            self.clicks.set_len(total as usize)
        };
        ClickTarget(self.clicks.as_mut_ptr())
    }

    /// Puts a batch's clicks in their users' ranges.
    pub fn scatter(&self, blocks: &[&[u64]], target: ClickTarget) {
        for block in blocks {
            for click in block.chunks_exact(RAW_CLICK_WORDS) {
                if click[0] & USER_NUMBER != 0 {
                    let number = click[0] & !USER_NUMBER;
                    // SAFETY: the same clicks were counted, so the user's
                    // cursor exists and stays inside the user's range.
                    unsafe {
                        let at = self.cursors.get_unchecked((number >> 9) as usize).fetch_add(1, Ordering::Relaxed);
                        *target.0.add(at as usize) = [click[2] | ((number & (ALIASES - 1)) << 32), click[1]];
                    }
                }
            }
        }
    }

    pub fn finish(self) -> ClickIndex {
        // Every cursor has moved to its user's end.
        let ends = self.cursors.into_iter().map(AtomicU32::into_inner).collect();
        ClickIndex { ends, clicks: self.clicks }
    }
}

/// Winning events whose own ID is not a generated one (they went through the
/// general dedup pass) but whose person is simple: they join their user's run.
#[derive(Default)]
pub struct Extras {
    by_user: HashMap<u64, (AtomicBool, Vec<Record>)>,
}

impl Extras {
    /// Sorts the general dedup pass's survivors: `lists[thread][partition]`
    /// are its person lists. Events of simple persons with a generated name
    /// go to their user's extras; everything else is written to `out` again.
    pub fn collect(keys: &Keys, links: &LinkTable, lists: &[Vec<PartList>], out: &mut PassBOut) -> Self {
        let mut extras = Self::default();
        for per_thread in lists {
            for (part, list) in per_thread.iter().enumerate() {
                for block in list.blocks() {
                    let mut at = 0;
                    while at < block.len() {
                        // One word per event, plus one for a purchase.
                        let key = block[at];
                        let is_purchase = key & 1;
                        let purchase = if is_purchase == 1 { block[at + 1] } else { 0 };
                        at += 1 + is_purchase as usize;
                        let name = unmix(((part as u64) << out.person_shift) | (key >> 32));
                        let word0 = (1 << 32) | (key & (TS_MASK << 1)) | 1;
                        match (links.lookup(name), keys.user_number_of_code(name)) {
                            ((_, false), Some(number)) => {
                                let entry = extras.by_user.entry(number >> 9).or_default();
                                entry.1.push([word0, (number << 22) | is_purchase, purchase]);
                            }
                            _ => forward_named(out, name, word0, is_purchase, purchase),
                        }
                    }
                }
            }
        }
        extras
    }

    pub fn len(&self) -> usize {
        self.by_user.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_user.is_empty()
    }
}

/// What every thread reads while runs are processed.
pub struct Shared<'a> {
    pub keys: &'a Keys,
    pub links: &'a LinkTable,
    pub clicks: &'a ClickIndex,
    pub extras: &'a Extras,
    pub click_exotic: &'a [Box<[u8]>],
    /// Index partitions and worker threads, to size the output columns.
    pub parts: usize,
    pub threads: usize,
}

/// The clicks of complex components' generated IDs, sent to their persons'
/// partitions: the general path needs them, and no run visits a user
/// because of its clicks.
pub fn forward_complex_clicks(keys: &Keys, links: &LinkTable, clicks: &ClickIndex, out: &mut PassBOut) {
    for &code in links.complex_members() {
        let Some(number) = keys.user_number_of_code(code) else {
            continue;
        };
        let name = links.person(code);
        let alias = number & (ALIASES - 1);
        for click in clicks.of(number >> 9) {
            if (click[0] >> 32) == alias {
                forward_click(out, name, click[0] & 0xFFFF_FFFF, click[1]);
            }
        }
    }
}

/// The first and last run of one partition, which may continue next door.
pub struct Edge {
    part: usize,
    first_user: u64,
    first: Vec<Record>,
    /// `u64::MAX` when the partition has a single run (it is `first`).
    last_user: u64,
    last: Vec<Record>,
}

/// Joins the edge runs of neighbouring partitions into whole runs, in index
/// order. `None` when user indexes decrease somewhere: the data is not laid
/// out the way index mode needs.
pub fn chains(mut edges: Vec<Edge>) -> Option<Vec<(u64, Vec<Record>)>> {
    edges.sort_unstable_by_key(|edge| edge.part);
    let mut whole: Vec<(u64, Vec<Record>)> = Vec::new();
    let mut open: Option<(u64, Vec<Record>)> = None;
    for edge in edges {
        match &mut open {
            Some((user, records)) if *user == edge.first_user => records.extend_from_slice(&edge.first),
            _ => {
                if let Some(previous) = open.take() {
                    if edge.first_user < previous.0 {
                        return None;
                    }
                    whole.push(previous);
                }
                open = Some((edge.first_user, edge.first));
            }
        }
        if edge.last_user != u64::MAX {
            // The runner checked the order of the runs inside a partition:
            // this is a later user.
            whole.extend(open.take());
            open = Some((edge.last_user, edge.last));
        }
    }
    whole.extend(open);
    Some(whole)
}

/// One thread's state for the index pass.
pub struct Runner {
    place: ScratchI,
    kernel: ScratchC,
    /// Per alias: the run that resolved it last, and what it resolved to
    /// (the person's name code, with `COMPLEX` for a complex component).
    aliases: Vec<[u64; 2]>,
    run: u64,
    records: Vec<Record>,
    merged: Vec<Record>,
    ts: Vec<u32>,
    purchases: Vec<[u64; 2]>,
    clicks: Vec<[u64; 2]>,
    /// Events of a run's further simple persons: `[name, ts << 1 | is purchase, purchase word]`.
    others: Vec<Record>,
    pub edges: Vec<Edge>,
    /// User indexes decreased inside a partition.
    pub violated: bool,
    /// Counters for `SS_TIMING`: inline persons, their events, events sent to the general path.
    pub inline_persons: u64,
    pub inline_events: u64,
    pub general_events: u64,
    /// With `measure`: nanoseconds spent placing records and inside the session kernel.
    pub measure: bool,
    pub place_nanos: u64,
    pub kernel_nanos: u64,
    pub total_nanos: u64,
}

impl Runner {
    pub fn new(kernel: ScratchC) -> Self {
        Self {
            place: ScratchI::new(),
            kernel,
            aliases: vec![[0; 2]; ALIASES as usize],
            run: 0,
            records: Vec::new(),
            merged: Vec::new(),
            ts: Vec::new(),
            purchases: Vec::new(),
            clicks: Vec::new(),
            others: Vec::new(),
            edges: Vec::new(),
            violated: false,
            inline_persons: 0,
            inline_events: 0,
            general_events: 0,
            measure: false,
            place_nanos: 0,
            kernel_nanos: 0,
            total_nanos: 0,
        }
    }

    /// The winners of one index partition: whole runs are processed, the
    /// first and the last run are set aside as the partition's `Edge`.
    pub fn partition(
        &mut self,
        shared: &Shared,
        part: usize,
        lists: &[PartList],
        b_out: &mut PassBOut,
        c_out: &mut PassCOut,
    ) {
        let started = self.measure.then(std::time::Instant::now);
        let any = place(lists, &mut self.place);
        if let Some(started) = started {
            self.place_nanos += started.elapsed().as_nanos() as u64;
        }
        if !any {
            return;
        }
        let mut current = u64::MAX;
        let mut edge = Edge { part, first_user: u64::MAX, first: Vec::new(), last_user: u64::MAX, last: Vec::new() };
        let mut records = std::mem::take(&mut self.records);
        records.clear();
        for word_index in 0..self.place.occupied.len() {
            let mut bits = self.place.occupied[word_index];
            while bits != 0 {
                let slot = word_index * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let [word0, word1] = self.place.slots[slot];
                // The winner's own valid bit decides: an invalid winner drops the event.
                if word0 & 1 == 0 {
                    continue;
                }
                if (word0 >> 32) & 1 == 0 {
                    // Not a generated user ID: `word1` holds its code.
                    forward(shared.links, b_out, word0, word1, self.place.purchases[slot]);
                    self.general_events += 1;
                    continue;
                }
                let user = word1 >> 31;
                if user != current {
                    if current != u64::MAX {
                        self.violated |= user < current;
                        if edge.first_user == u64::MAX {
                            edge.first_user = current;
                            edge.first = std::mem::take(&mut records);
                        } else {
                            self.process_run(shared, b_out, c_out, current, &records);
                            records.clear();
                        }
                    }
                    current = user;
                }
                records.push([word0, word1, self.place.purchases[slot]]);
            }
        }
        if current != u64::MAX {
            if edge.first_user == u64::MAX {
                edge.first_user = current;
                edge.first = std::mem::take(&mut records);
            } else {
                edge.last_user = current;
                edge.last = std::mem::take(&mut records);
            }
            self.edges.push(edge);
        }
        self.records = records;
        self.kernel.flush(c_out);
        if !c_out.names.is_empty() {
            // A thread handles about `parts / threads` partitions like this one.
            self.kernel.reserve_rows(c_out, c_out.names.len() * shared.parts / shared.threads * 3 / 2);
        }
        if let Some(started) = started {
            self.total_nanos += started.elapsed().as_nanos() as u64;
        }
    }

    /// Adds the assists still pending after runs processed outside `partition`.
    pub fn flush(&mut self, c_out: &mut PassCOut) {
        self.kernel.flush(c_out);
    }

    /// Hands the session kernel back, for the general pass that follows.
    pub fn into_kernel(self) -> ScratchC {
        self.kernel
    }

    /// One user's events (`word1 >> 31` is the same user index in all of
    /// them), in any order: each simple person in it goes to the session
    /// kernel, events of complex components go to their persons' partitions.
    pub fn process_run(
        &mut self,
        shared: &Shared,
        b_out: &mut PassBOut,
        c_out: &mut PassCOut,
        user: u64,
        records: &[Record],
    ) {
        let mut merged = std::mem::take(&mut self.merged);
        let extra = if shared.extras.is_empty() { None } else { shared.extras.by_user.get(&user) };
        let records = match extra {
            Some((used, more)) => {
                used.store(true, Ordering::Relaxed);
                merged.clear();
                merged.extend_from_slice(records);
                merged.extend_from_slice(more);
                &merged[..]
            }
            None => records,
        };
        self.run += 1;
        self.ts.clear();
        self.purchases.clear();
        self.others.clear();
        // The first simple person met; codes are never 0.
        let mut first = 0u64;
        for &[word0, word1, purchase] in records {
            let name = resolve(&mut self.aliases, self.run, shared, word1 >> 22);
            let ts = (word0 >> 1) & TS_MASK;
            let is_purchase = word1 & 1;
            if name & COMPLEX != 0 {
                forward_named(b_out, name & CODE_MASK, word0, is_purchase, purchase);
                self.general_events += 1;
                continue;
            }
            if first == 0 {
                first = name;
            }
            if name == first {
                self.ts.push(ts as u32);
                if is_purchase == 1 {
                    self.purchases.push([ts, purchase]);
                }
            } else {
                self.others.push([name, (ts << 1) | is_purchase, purchase]);
            }
        }
        if first != 0 {
            self.person(shared, c_out, user, first);
            if !self.others.is_empty() {
                // Rare: IDs of one user that no link joins are separate persons.
                let mut others = std::mem::take(&mut self.others);
                others.sort_unstable_by_key(|other| other[0]);
                let mut from = 0;
                while from < others.len() {
                    let name = others[from][0];
                    let to = from + others[from..].iter().take_while(|other| other[0] == name).count();
                    self.ts.clear();
                    self.purchases.clear();
                    for &[_, stamp, purchase] in &others[from..to] {
                        self.ts.push((stamp >> 1) as u32);
                        if stamp & 1 == 1 {
                            self.purchases.push([stamp >> 1, purchase]);
                        }
                    }
                    self.person(shared, c_out, user, name);
                    from = to;
                }
                self.others = others;
            }
        }
        self.merged = merged;
    }

    /// The simple person `name` of `user`, whose events are in `self.ts` and
    /// `self.purchases`: adds its clicks and runs the session kernel.
    fn person(&mut self, shared: &Shared, c_out: &mut PassCOut, user: u64, name: u64) {
        self.clicks.clear();
        for click in shared.clicks.of(user) {
            // A click belongs to the person its own ID resolves to.
            let number = (user << 9) | (click[0] >> 32);
            if resolve(&mut self.aliases, self.run, shared, number) == name {
                self.clicks.push([click[0] & 0xFFFF_FFFF, click[1]]);
            }
        }
        let count = self.ts.len();
        self.inline_persons += 1;
        self.inline_events += count as u64;
        self.ts.resize(count + EVENT_SPARE, 0);
        let started = self.measure.then(std::time::Instant::now);
        self.kernel.one_person(c_out, shared.click_exotic, name, &mut self.ts, count, &self.purchases, &self.clicks);
        if let Some(started) = started {
            self.kernel_nanos += started.elapsed().as_nanos() as u64;
        }
    }
}

/// What the generated user ID `number` resolves to in run `run`: its
/// person's name code, with `COMPLEX` when the link component is complex.
/// One table probe per ID and run.
#[inline(always)]
fn resolve(aliases: &mut [[u64; 2]], run: u64, shared: &Shared, number: u64) -> u64 {
    let entry = &mut aliases[(number & (ALIASES - 1)) as usize];
    if entry[0] != run {
        let (name, complex) = shared.links.lookup(shared.keys.code_of_user_number(number));
        *entry = [run, name | if complex { COMPLEX } else { 0 }];
    }
    entry[1]
}

/// The users that have extras but no run of their own.
pub fn leftover_extras(shared: &Shared, runner: &mut Runner, b_out: &mut PassBOut, c_out: &mut PassCOut) {
    for (&user, (used, _)) in &shared.extras.by_user {
        if !used.load(Ordering::Relaxed) {
            runner.process_run(shared, b_out, c_out, user, &[]);
        }
    }
    runner.flush(c_out);
}

/// Diagnostic (`SS_STATS=1`): how well the data fits index mode's assumptions.
pub fn stats(keys: &Keys, links: &LinkTable, indexed_lists: &[Vec<PartList>], parts: usize) {
    let mut scratch = ScratchI::new();
    let mut lists: Vec<PartList> = Vec::new();
    let (mut occupied, mut valid, mut numbered, mut other_user) = (0u64, 0u64, 0u64, 0u64);
    let (mut user_back, mut runs, mut ts_back_runs) = (0u64, 0u64, 0u64);
    let (mut complex_events, mut multi_name_runs) = (0u64, 0u64);
    let mut previous_user = u64::MAX;
    let mut previous_ts = 0u64;
    let mut run_ts_back = false;
    let mut names: Vec<u64> = Vec::new();
    for part in 0..parts {
        lists.clear();
        lists.extend(indexed_lists.iter().map(|per_thread| per_thread[part]));
        if !place(&lists, &mut scratch) {
            continue;
        }
        for word_index in 0..scratch.occupied.len() {
            let mut bits = scratch.occupied[word_index];
            while bits != 0 {
                let slot = word_index * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                occupied += 1;
                let [word0, word1] = scratch.slots[slot];
                if word0 & 1 == 0 {
                    continue;
                }
                valid += 1;
                if (word0 >> 32) & 1 == 0 {
                    other_user += 1;
                    continue;
                }
                numbered += 1;
                let user = word1 >> 31;
                let ts = (word0 >> 1) & TS_MASK;
                if user != previous_user {
                    runs += 1;
                    ts_back_runs += u64::from(run_ts_back);
                    multi_name_runs += u64::from(names.len() > 1);
                    run_ts_back = false;
                    names.clear();
                    user_back += u64::from(previous_user != u64::MAX && user < previous_user);
                    previous_user = user;
                    previous_ts = 0;
                }
                run_ts_back |= ts < previous_ts;
                previous_ts = ts;
                let (name, complex) = links.lookup(keys.code_of_user_number(word1 >> 22));
                complex_events += u64::from(complex);
                if !complex && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    eprintln!(
        "SS_STATS occupied={occupied} valid={valid} numbered={numbered} other_user={other_user} runs={runs} \
         user_back={user_back} ts_back_runs={ts_back_runs} complex_events={complex_events} multi_name_runs={multi_name_runs}"
    );
}

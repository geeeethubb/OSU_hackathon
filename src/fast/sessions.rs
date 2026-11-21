//! Pass C, one person partition at a time: sessions, attribution, assists
//! and the daily rows.
//!
//! A partition is never sorted as a whole. Its events, purchases and clicks
//! are grouped by person with a counting sort (a small hash table gives every
//! person of the partition a dense index): through staging arrays in one
//! pass over the lists, or for a huge partition in two passes without them.
//! Then, per person:
//!
//! * Sessions. Up to 16 timestamps are sorted by a fixed network of
//!   compare-exchanges. More are summarized per half-hour bucket (min, max,
//!   count): two events of one bucket are less than 30 minutes apart, so a
//!   bucket never holds a session break, and walking the occupied buckets in
//!   order a new session starts exactly where
//!   `bucket.min - previous_bucket.max` is over 30 minutes. The occupied
//!   buckets are put in order by the same network when there are at most
//!   16, and read back from flag bytes otherwise. A caller that already has
//!   a person's events in time order (`ScratchC::one_person_in_order`) gets
//!   the sessions from one pass over them instead.
//! * Purchases go to the session that holds their timestamp: the last one
//!   that starts at or before it.
//! * Clicks. A click at `t` can credit or assist exactly the sessions with
//!   `t <= start <= t + 7 days`, a contiguous run of the person's sessions.
//!   The click that credits a session is the latest one at or before its
//!   start, so it is enough to keep, per session, the best click among those
//!   whose run begins there and to carry it forward while it is in reach.
//!   A campaign assists the union of its clicks' runs, kept as one bit per
//!   session and counted per day with popcounts. No click is ever paired
//!   with every session it reaches.
//!
//! Most persons are small, and what they cost is mispredicted branches, so
//! the per-person loops avoid data-dependent branches: values are written
//! before the condition that keeps them is known, and counts are added
//! whether or not they are zero.
//!
//! Rows of one person are emitted together, in day order; persons come in the
//! order they first appear in the partition's lists.

use std::cmp::Ordering;

use super::arena::PartList;
use super::codes::{id_cmp, unmix, CODE_MASK};
use super::Ctx;
use crate::util::{DAY_MS, SESSION_GAP_MS, WEEK_MS};

const TS_MASK: u64 = 0x7FFF_FFFF;
const GAP: u32 = SESSION_GAP_MS as u32;
const WEEK: u32 = WEEK_MS as u32;
const DAY: u32 = DAY_MS as u32;

/// Half-hour buckets a 31-bit timestamp can fall into.
const BUCKETS: usize = (TS_MASK / SESSION_GAP_MS as u64) as usize + 1;
/// Days a 31-bit timestamp can fall on, for a window that starts late in a day.
const MAX_ROWS: usize = ((TS_MASK + DAY_MS as u64) / DAY_MS as u64) as usize + 1;
/// A bucket summary `[min, max, count, unused]` that no event has touched.
const EMPTY_CELL: [u32; 4] = [u32::MAX, 0, 0, 0];
/// Session starts are compared this many at a time.
const LANES: usize = 8;
/// The most events the sorting network takes.
const NETWORK_MAX: usize = 2 * LANES;
/// Spare words after the grouped events, so that a full network input can be
/// read at any person's first event.
const EVENT_PAD: usize = NETWORK_MAX;

pub const NO_CATEGORY: u32 = u32::MAX;

/// Development aid: `true` times the phases below with the CPU's time stamp
/// counter, and every worker prints one line of totals when it ends.
const PROFILE: bool = false;
const PHASES: [&str; 11] = [
    "read_events",
    "read_clicks",
    "scatter",
    "sessions",
    "rows",
    "purchases",
    "click_setup",
    "clicks",
    "credit",
    "assists",
    "emit",
];

struct Profile {
    last: u64,
    ticks: [u64; PHASES.len()],
    born: (std::time::Instant, u64),
    /// When the current partition began, and the longest one so far: `(ticks, partition)`.
    began: u64,
    longest: (u64, usize),
}

#[inline(always)]
fn time_stamp() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: reading the time stamp counter has no preconditions.
    unsafe {
        std::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

impl Profile {
    fn new() -> Self {
        Self {
            last: 0,
            ticks: [0; PHASES.len()],
            born: (std::time::Instant::now(), time_stamp()),
            began: 0,
            longest: (0, 0),
        }
    }

    #[inline(always)]
    fn start(&mut self) {
        if PROFILE {
            self.last = time_stamp();
            self.began = self.last;
        }
    }

    /// Notes how long the partition that `start` began has taken.
    #[inline(always)]
    fn end(&mut self, part: usize) {
        if PROFILE {
            self.longest = self.longest.max((time_stamp().wrapping_sub(self.began), part));
        }
    }

    /// Charges the time since the previous call to `phase`.
    #[inline(always)]
    fn lap(&mut self, phase: usize) {
        if PROFILE {
            let now = time_stamp();
            self.ticks[phase] += now.wrapping_sub(self.last);
            self.last = now;
        }
    }

    fn report(&self) {
        let ticks_per_ms = (time_stamp() - self.born.1) as f64 / (self.born.0.elapsed().as_secs_f64() * 1e3);
        let mut line = String::from("SESSIONS_PROFILE");
        let mut total = 0.0;
        for (name, &ticks) in PHASES.iter().zip(&self.ticks) {
            let ms = ticks as f64 / ticks_per_ms;
            total += ms;
            line += &format!(" {name}={ms:.2}");
        }
        let (ticks, part) = self.longest;
        eprintln!("{line} total={total:.2} longest={:.2} longest_part={part}", ticks as f64 / ticks_per_ms);
    }
}

/// What one worker produces in pass C.
pub struct PassCOut {
    /// user_daily rows, column by column.
    pub names: Vec<u64>,
    pub days: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub top_category: Vec<u32>,
    /// `[sessions, purchasing_sessions, revenue_cents, assisted_sessions]`
    /// per `(campaign, day)`, indexed `campaign * day_count + day`.
    pub campaign_days: Vec<[i64; 4]>,
}

impl PassCOut {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            names: Vec::new(),
            days: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top_category: Vec::new(),
            campaign_days: vec![[0; 4]; ctx.campaign_ids.len() * ctx.day_count],
        }
    }
}

/// What pass C needs from `Ctx`, in the form it uses.
struct Params {
    /// Epoch day of the window's first day.
    first_day: i64,
    /// Milliseconds from that day's midnight to `window_start`.
    day_offset: u32,
    day_count: usize,
    campaigns: usize,
    organic: usize,
    categories: usize,
    /// Per category, how many category names are byte-wise smaller: the
    /// order that decides a tie between two categories' sums.
    category_rank: Vec<u32>,
    /// A person's mixed name code is `partition << person_shift | local key`.
    person_shift: u32,
}

impl Params {
    fn new(
        window_start: i64,
        first_day: i64,
        day_count: usize,
        campaigns: usize,
        organic: usize,
        category_names: &[Box<[u8]>],
        person_shift: u32,
    ) -> Self {
        let day_offset = window_start - first_day * DAY_MS;
        assert!((0..DAY_MS).contains(&day_offset));
        let category_rank =
            category_names.iter().map(|name| category_names.iter().filter(|other| *other < name).count() as u32).collect();
        Self {
            first_day,
            day_offset: day_offset as u32,
            day_count,
            campaigns,
            organic,
            categories: category_names.len(),
            category_rank,
            person_shift,
        }
    }
}

/// Where the per-person strategies switch. Every setting gives the same
/// output; the tests run them all.
#[derive(Clone, Copy)]
struct Tuning {
    /// Use the fixed network (where the target has one): to sort up to
    /// `NETWORK_MAX` events, and to order up to `NETWORK_MAX` buckets of a
    /// bigger person.
    network: bool,
    /// A person with at most this many events that the network does not take
    /// is sorted by the standard library; a bigger one goes through the
    /// bucket summaries.
    sort_max: usize,
    /// For a person with more than `2 * LANES` sessions, session starts are
    /// compared one by one with every click while `sessions * clicks` is
    /// below this; above, a per-bucket table answers.
    lookup_min: usize,
    /// A partition with at most this many list words is grouped through the
    /// staging arrays, one pass over its lists. A bigger one is read twice
    /// instead (count, then fill): a little slower per record, but a huge
    /// partition then needs no staging room, which would be touched once
    /// and never used again.
    stage_max: usize,
}

impl Tuning {
    const DEFAULT: Tuning = Tuning { network: true, sort_max: 0, lookup_min: 2048, stage_max: 1 << 17 };
}

/// Asks for every cache line of a block. The lists were written by other
/// threads long ago and each block is only a few kilobytes, too short for the
/// hardware prefetcher to get ahead of the reader.
#[inline(always)]
fn prefetch(block: &[u64]) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        let mut at = 0;
        while at < block.len() {
            // SAFETY: a prefetch reads nothing; the address is inside the block.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(block.as_ptr().add(at) as *const i8) };
            at += 8;
        }
        if let Some(last) = block.last() {
            // SAFETY: as above.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(last as *const u64 as *const i8) };
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = block;
}

/// Calls `visit` with every block of the lists in order. The block after
/// the one being visited is asked for first, so that it arrives meanwhile.
#[inline(always)]
fn for_each_block<'a>(lists: &'a [PartList], mut visit: impl FnMut(&'a [u64])) {
    for list in lists {
        let mut blocks = list.blocks();
        let mut following = blocks.next();
        while let Some(block) = following {
            following = blocks.next();
            if let Some(next) = following {
                prefetch(next);
            }
            visit(block);
        }
    }
}

/// The persons of one partition and their rows, grouped.
#[derive(Default)]
struct Group {
    /// Open addressing: `(local key + 1) << 31 | person index`, 0 for empty.
    table: Vec<u64>,
    /// `32 - log2(table.len())`.
    shift: u32,
    /// Local key per person index, in order of first appearance.
    locals: Vec<u32>,
    /// Per person `[events, purchases, clicks]`: counts while the lists are
    /// read, then the end of the person's slice in the grouped arrays.
    ends: Vec<[u32; 3]>,
    /// Events in list order: `person index << 32 | ts`.
    staged: Vec<u64>,
    /// Purchases in list order: `[person index << 32 | ts, price | category << 32]`.
    staged_purchases: Vec<[u64; 2]>,
    /// Clicks in list order: `[person index << 32 | ts, click code | campaign << 42]`.
    staged_clicks: Vec<[u64; 2]>,
    /// Event timestamps, person after person, then `EVENT_PAD` spare words.
    events: Vec<u32>,
    /// `[ts, price | category << 32]`, person after person.
    purchases: Vec<[u64; 2]>,
    /// `[ts, click code | campaign << 42]`, person after person.
    clicks: Vec<[u64; 2]>,
}

const INDEX_MASK: u64 = 0x7FFF_FFFF;
const HASH: u32 = 0x9E37_79B1;

impl Group {
    /// Bytes of scratch held, the high-water mark over the partitions so far.
    fn bytes(&self) -> usize {
        held(&self.table)
            + held(&self.locals)
            + held(&self.ends)
            + held(&self.staged)
            + held(&self.staged_purchases)
            + held(&self.staged_clicks)
            + held(&self.events)
            + held(&self.purchases)
            + held(&self.clicks)
    }

    /// Empties the table, sized for about as many persons as the previous partition had.
    fn begin(&mut self) {
        let capacity = (self.locals.len() * 8).clamp(256, 1 << 24).next_power_of_two();
        self.table.clear();
        self.table.resize(capacity, 0);
        self.shift = 32 - capacity.trailing_zeros();
        self.locals.clear();
        self.ends.clear();
    }

    /// The index of the person with this local key, new or known.
    #[inline(always)]
    fn person(&mut self, local: u32) -> usize {
        let tag = u64::from(local) + 1;
        let mask = self.table.len() - 1;
        let mut slot = (local.wrapping_mul(HASH) >> self.shift) as usize;
        loop {
            // SAFETY: `slot` is below the table's power-of-two length.
            let entry = unsafe { *self.table.get_unchecked(slot) };
            if entry >> 31 == tag {
                return (entry & INDEX_MASK) as usize;
            }
            if entry == 0 {
                return self.insert(local, slot);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// The index of the person with this local key, if it has events.
    #[inline(always)]
    fn find(&self, local: u32) -> Option<usize> {
        let tag = u64::from(local) + 1;
        let mask = self.table.len() - 1;
        let mut slot = (local.wrapping_mul(HASH) >> self.shift) as usize;
        loop {
            // SAFETY: `slot` is below the table's power-of-two length.
            let entry = unsafe { *self.table.get_unchecked(slot) };
            if entry >> 31 == tag {
                return Some((entry & INDEX_MASK) as usize);
            }
            if entry == 0 {
                return None;
            }
            slot = (slot + 1) & mask;
        }
    }

    #[inline(never)]
    fn insert(&mut self, local: u32, slot: usize) -> usize {
        let index = self.locals.len();
        self.table[slot] = ((u64::from(local) + 1) << 31) | index as u64;
        self.locals.push(local);
        self.ends.push([0; 3]);
        if (index + 1) * 2 > self.table.len() {
            self.grow();
        }
        index
    }

    #[cold]
    fn grow(&mut self) {
        let capacity = self.table.len() * 2;
        self.table.clear();
        self.table.resize(capacity, 0);
        self.shift = 32 - capacity.trailing_zeros();
        let mask = capacity - 1;
        for (index, &local) in self.locals.iter().enumerate() {
            let mut slot = (local.wrapping_mul(HASH) >> self.shift) as usize;
            while self.table[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            self.table[slot] = ((u64::from(local) + 1) << 31) | index as u64;
        }
    }

    /// Reads the partition's event lists (`words` words in all): finds every
    /// person, counts its events and purchases, and stages both with the
    /// person's index.
    fn read_events(&mut self, lists: &[PartList], words: usize) {
        self.staged.clear();
        // At most one event per word.
        self.staged.reserve_exact(words);
        self.staged_purchases.clear();
        let staged = self.staged.as_mut_ptr();
        let mut count = 0;
        for_each_block(lists, |block| {
            assert!(count + block.len() <= words);
            let mut at = 0;
            while at < block.len() {
                let key = block[at];
                at += 1;
                let index = self.person((key >> 32) as u32);
                let head = ((index as u64) << 32) | ((key >> 1) & TS_MASK);
                // SAFETY: `index` was just returned for an entry of `ends`;
                // one word is staged per event, and the block's words fit in
                // what is reserved, as checked above.
                unsafe {
                    self.ends.get_unchecked_mut(index)[0] += 1;
                    *staged.add(count) = head;
                }
                count += 1;
                if key & 1 == 1 {
                    // A block holds whole records: the purchase's second word follows.
                    self.ends[index][1] += 1;
                    self.staged_purchases.push([head, block[at]]);
                    at += 1;
                }
            }
        });
        // SAFETY: every word below `count` was written above.
        unsafe { self.staged.set_len(count) };
    }

    /// Reads the partition's click lists (`words` words in all), keeping the
    /// clicks of persons that have events.
    fn read_clicks(&mut self, lists: &[PartList], words: usize) {
        self.staged_clicks.clear();
        self.staged_clicks.reserve_exact(words / 2);
        for_each_block(lists, |block| {
            // A block holds whole records: two words per click.
            for click in block.chunks_exact(2) {
                if let Some(index) = self.find((click[0] >> 32) as u32) {
                    self.ends[index][2] += 1;
                    self.staged_clicks.push([((index as u64) << 32) | ((click[0] >> 1) & TS_MASK), click[1]]);
                }
            }
        });
    }

    /// Turns the per-person counts into slice starts and gives the grouped
    /// arrays their lengths, with no more room than the biggest partition
    /// so far needed.
    fn lay_out(&mut self) {
        let mut totals = [0u32; 3];
        for end in &mut self.ends {
            for kind in 0..3 {
                let count = end[kind];
                end[kind] = totals[kind];
                totals[kind] += count;
            }
        }
        resize_exact(&mut self.events, totals[0] as usize + EVENT_PAD, 0);
        resize_exact(&mut self.purchases, totals[1] as usize, [0; 2]);
        resize_exact(&mut self.clicks, totals[2] as usize, [0; 2]);
    }

    /// Moves everything staged into per-person slices. Afterwards person
    /// `index` owns `ends[index - 1][kind]..ends[index][kind]` of each array.
    fn scatter(&mut self) {
        // Every slot is written by the loops that follow: the counts add up
        // to the staged rows.
        self.lay_out();
        for &word in &self.staged {
            let cursor = &mut self.ends[(word >> 32) as usize][0];
            self.events[*cursor as usize] = word as u32;
            *cursor += 1;
        }
        for row in &self.staged_purchases {
            let cursor = &mut self.ends[(row[0] >> 32) as usize][1];
            self.purchases[*cursor as usize] = [row[0] & TS_MASK, row[1]];
            *cursor += 1;
        }
        for row in &self.staged_clicks {
            let cursor = &mut self.ends[(row[0] >> 32) as usize][2];
            self.clicks[*cursor as usize] = [row[0] & TS_MASK, row[1]];
            *cursor += 1;
        }
    }

    /// The same grouping without the staging arrays, for a huge partition:
    /// one pass over the lists counts, a second one fills the slices.
    fn group_in_two_passes(&mut self, event_lists: &[PartList], click_lists: &[PartList]) {
        for_each_block(event_lists, |block| {
            let mut at = 0;
            while at < block.len() {
                let key = block[at];
                at += 1;
                let index = self.person((key >> 32) as u32);
                self.ends[index][0] += 1;
                if key & 1 == 1 {
                    self.ends[index][1] += 1;
                    at += 1;
                }
            }
        });
        for_each_block(click_lists, |block| {
            for click in block.chunks_exact(2) {
                if let Some(index) = self.find((click[0] >> 32) as u32) {
                    self.ends[index][2] += 1;
                }
            }
        });
        self.lay_out();
        for_each_block(event_lists, |block| {
            let mut at = 0;
            while at < block.len() {
                let key = block[at];
                at += 1;
                // Every person was met in the first pass.
                let index = self.person((key >> 32) as u32);
                let ts = (key >> 1) & TS_MASK;
                let cursor = &mut self.ends[index];
                self.events[cursor[0] as usize] = ts as u32;
                cursor[0] += 1;
                if key & 1 == 1 {
                    self.purchases[cursor[1] as usize] = [ts, block[at]];
                    cursor[1] += 1;
                    at += 1;
                }
            }
        });
        for_each_block(click_lists, |block| {
            for click in block.chunks_exact(2) {
                if let Some(index) = self.find((click[0] >> 32) as u32) {
                    let cursor = &mut self.ends[index][2];
                    self.clicks[*cursor as usize] = [(click[0] >> 1) & TS_MASK, click[1]];
                    *cursor += 1;
                }
            }
        });
    }
}

/// Empties the vector and gives it `length` copies of `value`, in room that
/// is not rounded up.
fn resize_exact<T: Clone>(vector: &mut Vec<T>, length: usize, value: T) {
    vector.clear();
    vector.reserve_exact(length);
    vector.resize(length, value);
}

/// The AVX2 kernels. The crate is compiled for x86-64-v3, so the intrinsics
/// need no per-function feature attributes.
#[cfg(target_arch = "x86_64")]
mod wide {
    use std::arch::x86_64::*;

    use super::{GAP, LANES, NETWORK_MAX};

    /// Keeps the smaller of each lane pair where `MASK` has a 0, the larger
    /// where it has a 1: one layer of compare-exchanges.
    #[inline(always)]
    unsafe fn exchange<const MASK: i32>(lanes: __m256i, partners: __m256i) -> __m256i {
        _mm256_blend_epi32::<MASK>(_mm256_min_epu32(lanes, partners), _mm256_max_epu32(lanes, partners))
    }

    /// Lanes `1 0 3 2 5 4 7 6`.
    #[inline(always)]
    unsafe fn swap_neighbours(lanes: __m256i) -> __m256i {
        _mm256_shuffle_epi32::<0b10_11_00_01>(lanes)
    }

    /// Lanes `2 3 0 1 6 7 4 5`.
    #[inline(always)]
    unsafe fn swap_pairs(lanes: __m256i) -> __m256i {
        _mm256_shuffle_epi32::<0b01_00_11_10>(lanes)
    }

    /// Lanes `3 2 1 0 7 6 5 4`.
    #[inline(always)]
    unsafe fn reverse_halves(lanes: __m256i) -> __m256i {
        _mm256_shuffle_epi32::<0b00_01_10_11>(lanes)
    }

    /// Lanes `4 5 6 7 0 1 2 3`.
    #[inline(always)]
    unsafe fn swap_halves(lanes: __m256i) -> __m256i {
        _mm256_permute2x128_si256::<0x01>(lanes, lanes)
    }

    /// Lanes `7 6 5 4 3 2 1 0`.
    #[inline(always)]
    unsafe fn reverse(lanes: __m256i) -> __m256i {
        _mm256_permutevar8x32_epi32(lanes, _mm256_setr_epi32(7, 6, 5, 4, 3, 2, 1, 0))
    }

    /// Sorts eight unsigned lanes: a bitonic sorter, six layers.
    #[inline(always)]
    unsafe fn sort8(mut lanes: __m256i) -> __m256i {
        // Sorted pairs, then sorted halves.
        lanes = exchange::<0b1010_1010>(lanes, swap_neighbours(lanes));
        lanes = exchange::<0b1100_1100>(lanes, reverse_halves(lanes));
        lanes = exchange::<0b1010_1010>(lanes, swap_neighbours(lanes));
        // The two halves merged.
        lanes = exchange::<0b1111_0000>(lanes, reverse(lanes));
        lanes = exchange::<0b1100_1100>(lanes, swap_pairs(lanes));
        exchange::<0b1010_1010>(lanes, swap_neighbours(lanes))
    }

    /// Sorts eight lanes that rise and then fall, or fall and then rise.
    #[inline(always)]
    unsafe fn merge8(mut lanes: __m256i) -> __m256i {
        lanes = exchange::<0b1111_0000>(lanes, swap_halves(lanes));
        lanes = exchange::<0b1100_1100>(lanes, swap_pairs(lanes));
        exchange::<0b1010_1010>(lanes, swap_neighbours(lanes))
    }

    /// Sorts sixteen unsigned values, the smaller eight first.
    ///
    /// # Safety
    /// Needs AVX2.
    #[inline(always)]
    pub unsafe fn sort16(low: __m256i, high: __m256i) -> (__m256i, __m256i) {
        // Rising then falling: the smaller of each pair are the eight
        // smallest, and both sets rise and fall again.
        let (low, high) = (sort8(low), reverse(sort8(high)));
        (merge8(_mm256_min_epu32(low, high)), merge8(_mm256_max_epu32(low, high)))
    }

    /// Sorts the first `count` (1 to 16) of the timestamps at `events` into
    /// `sorted`, pads the rest with `u32::MAX`, and returns one bit per
    /// position whose timestamp is more than 30 minutes after the one
    /// before it. Bit 0 is never set.
    ///
    /// # Safety
    /// Needs AVX2, and 16 readable words at `events`.
    #[inline(always)]
    pub unsafe fn sort_and_cut(events: *const u32, count: usize, sorted: &mut [u32; NETWORK_MAX]) -> u32 {
        let last = _mm256_set1_epi32(count as i32 - 1);
        // Lanes past the person's events become u32::MAX and sort to the end.
        let low = _mm256_or_si256(
            _mm256_loadu_si256(events as *const __m256i),
            _mm256_cmpgt_epi32(_mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7), last),
        );
        let high = _mm256_or_si256(
            _mm256_loadu_si256(events.add(LANES) as *const __m256i),
            _mm256_cmpgt_epi32(_mm256_setr_epi32(8, 9, 10, 11, 12, 13, 14, 15), last),
        );
        let (low, high) = sort16(low, high);
        _mm256_storeu_si256(sorted.as_mut_ptr() as *mut __m256i, low);
        _mm256_storeu_si256(sorted.as_mut_ptr().add(LANES) as *mut __m256i, high);
        // Each lane next to the one before it in the order; lane 0 next to itself.
        let shift = _mm256_setr_epi32(0, 0, 1, 2, 3, 4, 5, 6);
        let before_low = _mm256_permutevar8x32_epi32(low, shift);
        let before_high = _mm256_blend_epi32::<0b0000_0001>(
            _mm256_permutevar8x32_epi32(high, shift),
            _mm256_permutevar8x32_epi32(low, _mm256_set1_epi32(7)),
        );
        // Timestamps have 31 bits, so their differences compare as signed.
        let gap = _mm256_set1_epi32(GAP as i32);
        let cut_low = _mm256_cmpgt_epi32(_mm256_sub_epi32(low, before_low), gap);
        let cut_high = _mm256_cmpgt_epi32(_mm256_sub_epi32(high, before_high), gap);
        let cuts = _mm256_movemask_ps(_mm256_castsi256_ps(cut_low)) as u32
            | (_mm256_movemask_ps(_mm256_castsi256_ps(cut_high)) as u32) << LANES;
        cuts & ((1 << count) - 1)
    }

    /// Puts the first `count` (1 to 16) of the bucket numbers at `buckets`
    /// in order: `order` gets `bucket << 4 | position` ascending, then
    /// `u32::MAX` for the rest.
    ///
    /// # Safety
    /// Needs AVX2, and 16 readable words at `buckets`.
    #[inline(always)]
    pub unsafe fn order_buckets(buckets: *const u32, count: usize, order: &mut [u32; NETWORK_MAX]) {
        let last = _mm256_set1_epi32(count as i32 - 1);
        let (first_half, second_half) =
            (_mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7), _mm256_setr_epi32(8, 9, 10, 11, 12, 13, 14, 15));
        // Lanes past the person's buckets become u32::MAX and sort to the end.
        let low = _mm256_or_si256(
            _mm256_or_si256(_mm256_slli_epi32::<4>(_mm256_loadu_si256(buckets as *const __m256i)), first_half),
            _mm256_cmpgt_epi32(first_half, last),
        );
        let high = _mm256_or_si256(
            _mm256_or_si256(_mm256_slli_epi32::<4>(_mm256_loadu_si256(buckets.add(LANES) as *const __m256i)), second_half),
            _mm256_cmpgt_epi32(second_half, last),
        );
        let (low, high) = sort16(low, high);
        _mm256_storeu_si256(order.as_mut_ptr() as *mut __m256i, low);
        _mm256_storeu_si256(order.as_mut_ptr().add(LANES) as *mut __m256i, high);
    }

    /// Of `LANES * blocks` ascending starts padded with `u32::MAX`: how many
    /// are below `floor`, and how many are at most `ts`.
    ///
    /// # Safety
    /// Needs AVX2, and `LANES * blocks` readable words at `starts`.
    #[inline(always)]
    pub unsafe fn reach(starts: *const u32, blocks: usize, floor: u32, ts: u32) -> (usize, usize) {
        let (floor_lanes, ts_lanes) = (_mm256_set1_epi32(floor as i32), _mm256_set1_epi32(ts as i32));
        let (mut not_below, mut up_to) = (0, 0);
        for block in 0..blocks {
            let lanes = _mm256_loadu_si256(starts.add(LANES * block) as *const __m256i);
            // Unsigned `start >= floor` and `start <= ts`; the padding is never below and never up to.
            let above = _mm256_cmpeq_epi32(_mm256_max_epu32(lanes, floor_lanes), lanes);
            let within = _mm256_cmpeq_epi32(_mm256_min_epu32(lanes, ts_lanes), lanes);
            not_below += (_mm256_movemask_ps(_mm256_castsi256_ps(above)) as u32).count_ones();
            up_to += (_mm256_movemask_ps(_mm256_castsi256_ps(within)) as u32).count_ones();
        }
        (LANES * blocks - not_below as usize, up_to as usize)
    }

    /// One bit per byte of the 32 flags that is `0xFF`.
    ///
    /// # Safety
    /// Needs AVX2.
    #[inline(always)]
    pub unsafe fn flag_bits(flags: &[u8; 32]) -> u32 {
        _mm256_movemask_epi8(_mm256_loadu_si256(flags.as_ptr() as *const __m256i)) as u32
    }
}

/// Scratch for one person at a time.
struct Work {
    profile: Profile,
    /// Bucket summaries `[min, max, count, unused]`; all `EMPTY_CELL` between persons.
    cells: Vec<[u32; 4]>,
    /// The numbers of the person's buckets, in the order they were first
    /// met, with room to read `NETWORK_MAX` of them wherever they end.
    touched: Vec<u32>,
    /// `0xFF` per bucket with events, in whole 32-byte groups; all zero between persons.
    flags: Vec<u8>,
    /// Sort path: the index of each session's first event, and one past the last.
    first: Vec<u32>,
    /// Sessions of the person in start order: start and event count. After
    /// the starts come `LANES` times `u32::MAX`.
    start: Vec<u32>,
    count: Vec<u32>,
    /// Purchase cents and purchase count per session.
    revenue: Vec<i64>,
    purchases: Vec<u32>,
    /// Row (day) number per session.
    row: Vec<u8>,
    /// Per row: its first session (one more entry closes the last row), its
    /// day index and its events. One spare entry each for values written
    /// before the row is known to start.
    row_first: [u32; MAX_ROWS + 1],
    row_day: [u32; MAX_ROWS + 1],
    row_events: [i64; MAX_ROWS + 1],
    /// Per row, one bit per session of the row (persons of up to 64 sessions).
    row_bits: [u64; MAX_ROWS],
    /// Purchase cents per `(row, category)`; all zero between persons.
    category_sums: Vec<i64>,
    /// Per session: `(click ts + 1) << 32 | click index` of the best click
    /// among those whose reach begins at this session; 0 for none. One more
    /// entry takes the clicks after the last start.
    best: Vec<u64>,
    /// Credited campaign per session.
    credited: Vec<u32>,
    /// Per bucket: `starts before the bucket << 32 | the start inside it`
    /// (`u32::MAX` when it has none).
    lookup: Vec<u64>,
    /// Campaign -> its slot for the current person, from 1; all zero between persons.
    slot_of: Vec<u32>,
    /// Slot -> campaign; entry 0 is not used.
    slot_campaign: Vec<u32>,
    /// Per slot, one bit per session the campaign assists; all zero between persons.
    masks: Vec<u64>,
    /// Assists of the partition's persons so far that are not in the output
    /// yet: `campaign_days index << 32 | sessions`.
    pending: Vec<u64>,
    /// The output columns have been given their room.
    reserved: bool,
}

impl Work {
    /// Bytes of scratch held, the high-water mark over the persons so far.
    fn bytes(&self) -> usize {
        held(&self.cells)
            + held(&self.touched)
            + held(&self.flags)
            + held(&self.first)
            + held(&self.start)
            + held(&self.count)
            + held(&self.revenue)
            + held(&self.purchases)
            + held(&self.row)
            + held(&self.category_sums)
            + held(&self.best)
            + held(&self.credited)
            + held(&self.lookup)
            + held(&self.slot_of)
            + held(&self.slot_campaign)
            + held(&self.masks)
            + held(&self.pending)
    }

    fn new(params: &Params, tuning: Tuning) -> Self {
        // A bucket holds at most one session start.
        let sessions = BUCKETS.max(tuning.sort_max).max(NETWORK_MAX) + 1;
        Self {
            profile: Profile::new(),
            cells: vec![EMPTY_CELL; BUCKETS],
            touched: vec![0; BUCKETS + NETWORK_MAX],
            flags: vec![0; BUCKETS + 32],
            first: vec![0; tuning.sort_max.max(NETWORK_MAX) + 2],
            start: vec![0; sessions + LANES],
            count: vec![0; sessions],
            revenue: vec![0; sessions],
            purchases: vec![0; sessions],
            row: vec![0; sessions],
            row_first: [0; MAX_ROWS + 1],
            row_day: [0; MAX_ROWS + 1],
            row_events: [0; MAX_ROWS + 1],
            row_bits: [0; MAX_ROWS],
            category_sums: vec![0; MAX_ROWS * params.categories],
            best: vec![0; sessions + 1],
            credited: vec![0; sessions],
            lookup: vec![0; BUCKETS + 1],
            slot_of: vec![0; params.campaigns],
            slot_campaign: Vec::new(),
            masks: Vec::new(),
            pending: Vec::new(),
            reserved: false,
        }
    }
}

pub struct ScratchC {
    params: Params,
    tuning: Tuning,
    group: Group,
    work: Work,
    batch: Batch,
}

impl Drop for ScratchC {
    fn drop(&mut self) {
        if PROFILE {
            self.work.profile.report();
            let staged = held(&self.group.staged) + held(&self.group.staged_purchases) + held(&self.group.staged_clicks);
            let bytes = self.group.bytes() + self.work.bytes() + self.batch.bytes();
            eprintln!("SESSIONS_SCRATCH bytes={bytes} staged={staged}");
        }
    }
}

/// Bytes a vector holds on to.
fn held<T>(vector: &Vec<T>) -> usize {
    vector.capacity() * std::mem::size_of::<T>()
}

impl ScratchC {
    pub fn new(ctx: &Ctx) -> Self {
        let params = Params::new(
            ctx.window_start,
            ctx.first_day,
            ctx.day_count,
            ctx.campaign_ids.len(),
            ctx.organic as usize,
            &ctx.category_names,
            ctx.person_shift(),
        );
        Self::with(params, Tuning::DEFAULT)
    }

    fn with(params: Params, tuning: Tuning) -> Self {
        let work = Work::new(&params, tuning);
        Self { params, tuning, group: Group::default(), work, batch: Batch::new() }
    }

    /// One whole person outside the partition pass: index mode sessionizes a
    /// person where it finds it. `events` holds the person's `count`
    /// timestamps first, in any order, and is at least `count + EVENT_SPARE`
    /// long; `purchases` are `[ts, price | category << 32]` and `clicks`
    /// `[ts since seven days before the window, click code | campaign << 42]`.
    /// The person's assists stay pending until `flush`.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn one_person(
        &mut self,
        out: &mut PassCOut,
        click_exotic: &[Box<[u8]>],
        name: u64,
        events: &mut [u32],
        count: usize,
        purchases: &[[u64; 2]],
        clicks: &[[u64; 2]],
    ) {
        person(&self.params, self.tuning, &mut self.work, out, click_exotic, name, events, count, purchases, clicks);
    }

    /// `one_person` for a person whose events are already in time order:
    /// the sessions come from one pass over the timestamps, with no sort and
    /// no bucket summaries. `events` is exactly the person's timestamps (at
    /// least one; no spare words needed, and they are not changed). If they
    /// are not ascending after all, the person is done as by `one_person`,
    /// so the rows are the same either way. The assists stay pending until
    /// `flush`.
    #[inline]
    pub fn one_person_in_order(
        &mut self,
        out: &mut PassCOut,
        click_exotic: &[Box<[u8]>],
        name: u64,
        events: &[u32],
        purchases: &[[u64; 2]],
        clicks: &[[u64; 2]],
    ) {
        person_in_order(&self.params, self.tuning, &mut self.work, out, click_exotic, name, events, purchases, clicks);
    }

    /// Starts a person whose events the caller reads in time order and does
    /// not want to collect: `ts` is its first event. The person joins the
    /// batch; its further events go to `Person::add` with the slices of
    /// `batch_scratch`, and `end_person` or `abort_person` must follow
    /// before the next person starts. Rows and cells are the ones
    /// `one_person_in_order` gives for the same events; they reach `out`
    /// when the batch is full or at `finish_batch`.
    #[inline(always)]
    pub fn begin_person(&mut self, out: &mut PassCOut, click_exotic: &[Box<[u8]>], ts: u32) -> Person {
        if self.batch_is_full() {
            self.finish_batch(out, click_exotic);
        }
        let batch = &mut self.batch;
        let owner = (batch.persons as u64) << OWNER_SHIFT;
        batch.start[batch.sessions] = ts;
        batch.meta[batch.sessions] = owner | batch.events;
        Person {
            open: batch.sessions,
            owner,
            events: batch.events + 1,
            latest: ts,
            events_before: batch.events,
            purchases_before: batch.purchases.len(),
        }
    }

    /// Whether the batch has no room for another person: `begin_person`
    /// would finish it first.
    #[inline(always)]
    pub fn batch_is_full(&self) -> bool {
        self.batch.persons == BATCH_PERSONS || self.batch.sessions + PERSON_SESSIONS > BATCH_SESSIONS
    }

    /// What `Person::add` and `Person::purchase` write to: the batch's
    /// session starts, its session words and its purchases.
    #[inline(always)]
    pub fn batch_scratch(&mut self) -> (&mut [u32], &mut [u64], &mut Vec<[u64; 2]>) {
        (&mut self.batch.start, &mut self.batch.meta, &mut self.batch.purchases)
    }

    /// Ends the person `begin_person` started: its name code and its clicks,
    /// `[ts since seven days before the window, click code | campaign << 42]`.
    #[inline(always)]
    pub fn end_person(&mut self, person: Person, name: u64, clicks: impl Iterator<Item = [u64; 2]>) {
        let batch = &mut self.batch;
        let number = batch.persons;
        let clicks_before = batch.clicks.len();
        batch.clicks.extend(clicks);
        let has_clicks = batch.clicks.len() != clicks_before;
        batch.names[number] = name;
        batch.organic[number] = i64::from(!has_clicks);
        batch.first_session[number] = batch.sessions as u32;
        batch.click_ends[number + 1] = batch.clicks.len() as u32;
        // The persons with clicks: written every time, kept when there are any.
        batch.clicked[batch.clicked_count] = number as u32;
        batch.clicked_count += usize::from(has_clicks);
        batch.persons = number + 1;
        batch.sessions = person.open + 1;
        batch.events = person.events;
    }

    /// Drops the person `begin_person` started, as if it had never begun:
    /// its events were not in time order after all (`Person::add` said so),
    /// or the caller will hand it over in another way.
    #[inline]
    pub fn abort_person(&mut self, person: Person) {
        self.batch.purchases.truncate(person.purchases_before);
    }

    /// Rows and cells of the persons in the batch. Must be called before
    /// `flush` and before any use of the scratch for something else than
    /// whole persons; the assists stay pending until `flush`.
    pub fn finish_batch(&mut self, out: &mut PassCOut, click_exotic: &[Box<[u8]>]) {
        if self.batch.persons != 0 {
            finish_batch(&self.params, self.tuning, &mut self.work, &mut self.batch, out, click_exotic);
        }
    }

    /// Adds the assists that `one_person` left pending to `out`.
    pub fn flush(&mut self, out: &mut PassCOut) {
        assert_eq!(self.batch.persons, 0, "a batch of persons was left unfinished");
        apply_assists(&mut self.work.pending, &mut out.campaign_days);
    }

    /// Reserves room for `rows` more rows in `out`'s columns, the first time
    /// it is called; the partition pass then leaves the reservation alone.
    pub fn reserve_rows(&mut self, out: &mut PassCOut, rows: usize) {
        if !self.work.reserved {
            self.work.reserved = true;
            out.names.reserve(rows);
            out.days.reserve(rows);
            out.sessions.reserve(rows);
            out.events.reserve(rows);
            out.revenue.reserve(rows);
            out.top_category.reserve(rows);
        }
    }
}

/// Readable words `ScratchC::one_person` needs after a person's events.
pub const EVENT_SPARE: usize = EVENT_PAD;

/// The lowest `count` bits (`count` up to 64).
#[inline(always)]
fn low_bits(count: usize) -> u64 {
    if count >= 64 {
        u64::MAX
    } else {
        (1 << count) - 1
    }
}

/// How many of the ascending `starts` are `<= limit`.
#[inline(always)]
fn starts_up_to(starts: &[u32], limit: u32) -> usize {
    if starts.len() <= 2 * LANES {
        starts.iter().filter(|&&start| start <= limit).count()
    } else {
        starts.partition_point(|&start| start <= limit)
    }
}

/// The sessions of one person while its events arrive one at a time in time
/// order. `start` and `count` are the scratch's session starts and counts.
#[derive(Clone, Copy)]
struct Sessions {
    /// The number of the open session.
    open: usize,
    /// Events before the open session, and events so far.
    first: u32,
    events: u32,
    /// The latest timestamp.
    latest: u32,
}

impl Sessions {
    /// Starts a person at its first event.
    #[inline(always)]
    fn begin(start: &mut [u32], ts: u32) -> Self {
        start[0] = ts;
        Self { open: 0, first: 0, events: 1, latest: ts }
    }

    /// The person's next event. `false`, with nothing changed, when it is
    /// earlier than the one before it: the events are not in time order,
    /// and the person has to be done from all its timestamps instead.
    #[inline(always)]
    fn add(&mut self, start: &mut [u32], count: &mut [u32], ts: u32) -> bool {
        if ts < self.latest {
            return false;
        }
        // The open session's count and the next one's start are written
        // before the gap decides whether a session starts here.
        let cut = ts - self.latest > GAP;
        count[self.open] = self.events - self.first;
        start[self.open + 1] = ts;
        self.first = if cut { self.events } else { self.first };
        self.open += usize::from(cut);
        self.events += 1;
        self.latest = ts;
        true
    }

    /// Ends the person: writes the last session's count and returns the
    /// number of sessions.
    #[inline(always)]
    fn close(self, count: &mut [u32]) -> usize {
        count[self.open] = self.events - self.first;
        self.open + 1
    }
}

/// Most persons a batch holds, and most sessions.
const BATCH_PERSONS: usize = 32;
const BATCH_SESSIONS: usize = 2048;
/// Most sessions of one person whose timestamps are in order and at most
/// `TS_MASK`: every session but the first begins more than `GAP` later.
const PERSON_SESSIONS: usize = BUCKETS;
const _: () = assert!(PERSON_SESSIONS < BATCH_SESSIONS);
/// A batch session's word: the person's number in the batch, above the
/// number of the batch's events before the session.
const OWNER_SHIFT: u32 = 48;
const EVENTS_MASK: u64 = (1 << OWNER_SHIFT) - 1;

/// Persons whose events arrive in time order (`ScratchC::begin_person`),
/// kept until there are enough of them to do their rows together.
///
/// One person at a time costs a handful of loops that each run a few times
/// and end in a mispredicted branch: over its sessions for the rows, over
/// its rows for the output. Here the sessions of all persons of the batch
/// stand one after the other, the person is one more field of a session,
/// and the same work is three passes over the whole batch: sessions to
/// rows, purchases, rows to output. Only attribution still goes person by
/// person, for the persons that have clicks.
struct Batch {
    /// The batch's sessions, person after person: start, and
    /// `person << OWNER_SHIFT | events of the batch before the session`.
    /// Both have one entry more than `BATCH_SESSIONS`: the next session's
    /// values are written before it is known to begin, and one more word
    /// closes the last session.
    start: Vec<u32>,
    meta: Vec<u64>,
    sessions: usize,
    events: u64,
    /// Per person: its name code, 1 if it has no clicks (its sessions are
    /// organic) and 0 if it has, and its first session; where its clicks
    /// end in `clicks` is at `person + 1`.
    names: Vec<u64>,
    organic: Vec<i64>,
    first_session: Vec<u32>,
    click_ends: Vec<u32>,
    persons: usize,
    /// The persons with clicks, and one spare entry.
    clicked: Vec<u32>,
    clicked_count: usize,
    clicks: Vec<[u64; 2]>,
    /// `[session, price | category << 32]`, in session order.
    purchases: Vec<[u64; 2]>,
    /// Per session: purchase cents, purchases, and its row.
    revenue: Vec<i64>,
    bought: Vec<u32>,
    row_of: Vec<u32>,
    /// Per row: its first session (one more entry closes the last row), its
    /// day index and its person, each with a spare entry for values written
    /// before the row is known to begin; purchase cents, sessions with a
    /// purchase and top category.
    row_first: Vec<u32>,
    row_day: Vec<u32>,
    row_owner: Vec<u32>,
    row_revenue: Vec<i64>,
    row_buying: Vec<i64>,
    row_top: Vec<u32>,
}

impl Batch {
    fn new() -> Self {
        Self {
            start: vec![0; BATCH_SESSIONS + 1],
            meta: vec![0; BATCH_SESSIONS + 1],
            sessions: 0,
            events: 0,
            names: vec![0; BATCH_PERSONS],
            organic: vec![0; BATCH_PERSONS],
            first_session: vec![0; BATCH_PERSONS + 1],
            click_ends: vec![0; BATCH_PERSONS + 1],
            persons: 0,
            clicked: vec![0; BATCH_PERSONS + 1],
            clicked_count: 0,
            clicks: Vec::new(),
            purchases: Vec::new(),
            revenue: vec![0; BATCH_SESSIONS],
            bought: vec![0; BATCH_SESSIONS],
            row_of: vec![0; BATCH_SESSIONS],
            row_first: vec![0; BATCH_SESSIONS + 1],
            row_day: vec![0; BATCH_SESSIONS + 1],
            row_owner: vec![0; BATCH_SESSIONS + 1],
            row_revenue: vec![0; BATCH_SESSIONS],
            row_buying: vec![0; BATCH_SESSIONS],
            row_top: vec![0; BATCH_SESSIONS],
        }
    }

    /// Bytes of scratch held.
    fn bytes(&self) -> usize {
        held(&self.start)
            + held(&self.meta)
            + held(&self.names)
            + held(&self.organic)
            + held(&self.first_session)
            + held(&self.click_ends)
            + held(&self.clicked)
            + held(&self.clicks)
            + held(&self.purchases)
            + held(&self.revenue)
            + held(&self.bought)
            + held(&self.row_of)
            + held(&self.row_first)
            + held(&self.row_day)
            + held(&self.row_owner)
            + held(&self.row_revenue)
            + held(&self.row_buying)
            + held(&self.row_top)
    }
}

/// A person of the batch while its events arrive, one at a time and in time
/// order (`ScratchC::begin_person`).
#[derive(Clone, Copy, Default)]
pub struct Person {
    /// The open session's place in the batch.
    open: usize,
    /// The person's number in the batch, as it stands in a session word.
    owner: u64,
    /// Events of the batch up to the person's latest one.
    events: u64,
    /// The latest timestamp.
    latest: u32,
    /// The batch's events and purchases before the person.
    events_before: u64,
    purchases_before: usize,
}

impl Person {
    /// The person's next event; `start` and `meta` are the first two of
    /// `ScratchC::batch_scratch`. `false`, with nothing changed, when it is
    /// earlier than the one before it: the events are not in time order,
    /// and the person has to be aborted and done from all its timestamps.
    /// Timestamps must be at most `TS_MASK`.
    #[inline(always)]
    pub fn add(&mut self, start: &mut [u32], meta: &mut [u64], ts: u32) -> bool {
        if ts < self.latest {
            return false;
        }
        // The next session's start and word are written before the gap
        // decides whether a session begins here.
        let cut = ts - self.latest > GAP;
        start[self.open + 1] = ts;
        meta[self.open + 1] = self.owner | self.events;
        self.open += usize::from(cut);
        self.events += 1;
        self.latest = ts;
        true
    }

    /// The event just given is a purchase: `word` is `price | category << 32`
    /// and `purchases` the third of `ScratchC::batch_scratch`.
    #[inline(always)]
    pub fn purchase(&self, purchases: &mut Vec<[u64; 2]>, word: u64) {
        purchases.push([self.open as u64, word]);
    }

    /// How many events the person has so far.
    #[inline(always)]
    pub fn events(&self) -> u64 {
        self.events - self.events_before
    }
}

/// Rows and cells of the persons in the batch; the batch is empty afterwards.
fn finish_batch(
    params: &Params,
    tuning: Tuning,
    work: &mut Work,
    batch: &mut Batch,
    out: &mut PassCOut,
    click_exotic: &[Box<[u8]>],
) {
    let (sessions, persons) = (batch.sessions, batch.persons);
    batch.meta[sessions] = (persons as u64) << OWNER_SHIFT | batch.events;
    batch.first_session[persons] = sessions as u32;

    // Sessions to rows: one row per person and day with a session start.
    // Every session writes the row it would begin; the write stays only
    // where the person or the day changes.
    let mut rows = 0;
    let (mut previous_owner, mut previous_day) = (u32::MAX, u32::MAX);
    for session in 0..sessions {
        let owner = (batch.meta[session] >> OWNER_SHIFT) as u32;
        let day = (batch.start[session] + params.day_offset) / DAY;
        batch.row_first[rows] = session as u32;
        batch.row_day[rows] = day;
        batch.row_owner[rows] = owner;
        rows += usize::from((owner != previous_owner) | (day != previous_day));
        batch.row_of[session] = (rows - 1) as u32;
        previous_owner = owner;
        previous_day = day;
    }
    batch.row_first[rows] = sessions as u32;

    // Purchases: per session for the campaign cells, per row for the output.
    // They come in session order, so a row's purchases come together.
    batch.row_revenue[..rows].fill(0);
    batch.row_buying[..rows].fill(0);
    batch.row_top[..rows].fill(NO_CATEGORY);
    batch.revenue[..sessions].fill(0);
    batch.bought[..sessions].fill(0);
    let sums = &mut work.category_sums[..params.categories];
    let (mut current_row, mut previous_session) = (usize::MAX, u64::MAX);
    for &[session, word] in &batch.purchases {
        let row = batch.row_of[session as usize] as usize;
        if row != current_row {
            if current_row != usize::MAX {
                batch.row_top[current_row] = top_category(&params.category_rank, sums);
            }
            current_row = row;
        }
        let price = (word & 0xFFFF_FFFF) as i64;
        sums[(word >> 32) as usize] += price;
        batch.revenue[session as usize] += price;
        batch.bought[session as usize] += 1;
        batch.row_revenue[row] += price;
        batch.row_buying[row] += i64::from(session != previous_session);
        previous_session = session;
    }
    if current_row != usize::MAX {
        batch.row_top[current_row] = top_category(&params.category_rank, sums);
    }

    // Attribution, for the persons with clicks: each one's sessions and rows
    // are laid out the way one person's are, and its sessions go to the
    // cells of the campaigns they are credited to.
    for &person in &batch.clicked[..batch.clicked_count] {
        let person = person as usize;
        let (from, to) = (batch.first_session[person] as usize, batch.first_session[person + 1] as usize);
        let count = to - from;
        let (first_row, end_row) = (batch.row_of[from] as usize, batch.row_of[to - 1] as usize + 1);
        let person_rows = end_row - first_row;
        work.start[..count].copy_from_slice(&batch.start[from..to]);
        work.start[count..count + LANES].fill(u32::MAX);
        for row in 0..=person_rows {
            work.row_first[row] = batch.row_first[first_row + row] - from as u32;
        }
        work.row_day[..person_rows].copy_from_slice(&batch.row_day[first_row..end_row]);
        let clicks = &batch.clicks[batch.click_ends[person] as usize..batch.click_ends[person + 1] as usize];
        if count <= 64 {
            attribute_narrow(params, tuning, work, click_exotic, count, person_rows, clicks);
        } else {
            attribute_wide(params, work, click_exotic, count, person_rows, clicks);
        }
        for session in 0..count {
            let day = batch.row_day[batch.row_of[from + session] as usize] as usize;
            let cell = &mut out.campaign_days[work.credited[session] as usize * params.day_count + day];
            cell[0] += 1;
            cell[1] += i64::from(batch.bought[from + session] > 0);
            cell[2] += batch.revenue[from + session];
        }
    }

    // Rows to output, a column at a time.
    let row_sessions = |row: usize| batch.row_first[row + 1] - batch.row_first[row];
    let events_before = |row: usize| batch.meta[batch.row_first[row] as usize] & EVENTS_MASK;
    out.names.extend((0..rows).map(|row| batch.names[batch.row_owner[row] as usize]));
    out.days.extend((0..rows).map(|row| (params.first_day + i64::from(batch.row_day[row])) as i32));
    out.sessions.extend((0..rows).map(|row| i64::from(row_sessions(row))));
    out.events.extend((0..rows).map(|row| (events_before(row + 1) - events_before(row)) as i64));
    out.revenue.extend_from_slice(&batch.row_revenue[..rows]);
    out.top_category.extend_from_slice(&batch.row_top[..rows]);
    // The sessions of a person without clicks are organic, a row at a time.
    let organic = &mut out.campaign_days[params.organic * params.day_count..][..params.day_count];
    for row in 0..rows {
        let weight = batch.organic[batch.row_owner[row] as usize];
        let cell = &mut organic[batch.row_day[row] as usize];
        cell[0] += weight * i64::from(row_sessions(row));
        cell[1] += weight * batch.row_buying[row];
        cell[2] += weight * batch.row_revenue[row];
    }

    batch.sessions = 0;
    batch.events = 0;
    batch.persons = 0;
    batch.clicked_count = 0;
    batch.clicks.clear();
    batch.purchases.clear();
}

/// Sessions of one person whose timestamps are already ascending, in one
/// pass over them. Returns how many, or `None` at the first timestamp that
/// is smaller than the one before it.
fn sessions_in_order(events: &[u32], work: &mut Work) -> Option<usize> {
    let (start, count) = (&mut work.start[..], &mut work.count[..]);
    let mut sessions = Sessions::begin(start, events[0]);
    for &ts in &events[1..] {
        if !sessions.add(start, count, ts) {
            return None;
        }
    }
    Some(sessions.close(count))
}

/// Sessions of one person by sorting its timestamps. Returns how many.
fn sessions_by_sort(events: &mut [u32], work: &mut Work) -> usize {
    events.sort_unstable();
    let first = &mut work.first[..events.len() + 1];
    let mut sessions = 0;
    first[0] = 0;
    for at in 1..events.len() {
        // Written before the gap is looked at; it stays only if a session starts here.
        first[sessions + 1] = at as u32;
        sessions += usize::from(events[at] - events[at - 1] > GAP);
    }
    sessions += 1;
    first[sessions] = events.len() as u32;
    for session in 0..sessions {
        work.start[session] = events[first[session] as usize];
        work.count[session] = first[session + 1] - first[session];
    }
    sessions
}

/// Sessions of one person with at most `NETWORK_MAX` events, sorted by the
/// fixed network. `events` is the person's first event and whatever follows
/// it. Returns how many sessions.
#[cfg(target_arch = "x86_64")]
fn sessions_by_network(events: &mut [u32], count: usize, work: &mut Work) -> usize {
    assert!((1..=NETWORK_MAX).contains(&count) && events.len() >= NETWORK_MAX);
    let mut sorted = [0u32; NETWORK_MAX];
    // SAFETY: 16 words are readable at `events`, as just checked.
    let mut cuts = unsafe { wide::sort_and_cut(events.as_ptr(), count, &mut sorted) };
    let mut session = 0;
    let mut first = 0;
    work.start[0] = sorted[0];
    while cuts != 0 {
        let at = cuts.trailing_zeros();
        cuts &= cuts - 1;
        work.count[session] = at - first;
        session += 1;
        work.start[session] = sorted[at as usize];
        first = at;
    }
    work.count[session] = count as u32 - first;
    session + 1
}

/// Where there is no fixed network, the standard sort does the same.
#[cfg(not(target_arch = "x86_64"))]
fn sessions_by_network(events: &mut [u32], count: usize, work: &mut Work) -> usize {
    sessions_by_sort(&mut events[..count], work)
}

/// One bit per bucket of `flags[base..base + 32]` that has events; clears them.
#[inline(always)]
fn take_flags(flags: &mut [u8], base: usize) -> u32 {
    let group: &mut [u8; 32] = (&mut flags[base..base + 32]).try_into().unwrap();
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the crate is built with AVX2.
    let bits = unsafe { wide::flag_bits(group) };
    #[cfg(not(target_arch = "x86_64"))]
    let bits = group.iter().enumerate().fold(0u32, |bits, (at, &flag)| bits | u32::from(flag >> 7) << at);
    *group = [0; 32];
    bits
}

/// The session being built while a person's bucket summaries are taken in
/// time order.
struct Walk {
    /// Index of the open session.
    open: usize,
    /// Its events so far.
    events: u32,
    /// The latest timestamp seen.
    latest: u32,
}

impl Walk {
    /// Starts with the person's earliest bucket.
    #[inline(always)]
    fn first(cell: [u32; 4], start: &mut [u32]) -> Walk {
        start[0] = cell[0];
        Walk { open: 0, events: cell[2], latest: cell[1] }
    }

    /// Takes the next bucket in time order. The open session's count and
    /// the next session's start are written before the gap decides whether
    /// a session starts here.
    #[inline(always)]
    fn next(&mut self, cell: [u32; 4], start: &mut [u32], count: &mut [u32]) {
        // Buckets come in time order, so `cell.min >= self.latest`.
        let cut = cell[0] - self.latest > GAP;
        count[self.open] = self.events;
        start[self.open + 1] = cell[0];
        self.open += usize::from(cut);
        self.events = if cut { cell[2] } else { self.events + cell[2] };
        self.latest = cell[1];
    }

    /// Closes the last session; returns how many there are.
    #[inline(always)]
    fn finish(self, count: &mut [u32]) -> usize {
        count[self.open] = self.events;
        self.open + 1
    }
}

/// Sessions of one person from half-hour bucket summaries. Returns how many.
fn sessions_by_buckets(events: &[u32], work: &mut Work, network: bool) -> usize {
    let mut touched = 0;
    for &ts in events {
        // Timestamps have 31 bits. The mask changes nothing for them and
        // keeps the index in range for any other value.
        let bucket = ((ts & TS_MASK as u32) / GAP) as usize;
        // SAFETY: a 31-bit value gives `bucket < BUCKETS`, the length of
        // `cells`; `touched` counts different buckets, so it is at most
        // `BUCKETS`, less than the length of `work.touched`.
        unsafe {
            let cell = work.cells.get_unchecked_mut(bucket);
            // The bucket is noted every time; the note stays the first time.
            *work.touched.get_unchecked_mut(touched) = bucket as u32;
            touched += usize::from(cell[2] == 0);
            cell[0] = cell[0].min(ts);
            cell[1] = cell[1].max(ts);
            cell[2] += 1;
        }
    }
    #[cfg(target_arch = "x86_64")]
    if network && touched <= NETWORK_MAX {
        // Few buckets: put their numbers in order with the fixed network.
        let mut order = [0u32; NETWORK_MAX];
        // SAFETY: `work.touched` has `NETWORK_MAX` words from its start.
        unsafe { wide::order_buckets(work.touched.as_ptr(), touched, &mut order) };
        let earliest = std::mem::replace(&mut work.cells[(order[0] >> 4) as usize], EMPTY_CELL);
        let mut walk = Walk::first(earliest, &mut work.start);
        for &entry in &order[1..touched] {
            let cell = std::mem::replace(&mut work.cells[(entry >> 4) as usize], EMPTY_CELL);
            walk.next(cell, &mut work.start, &mut work.count);
        }
        return walk.finish(&mut work.count);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = network;
    // Many buckets: flag each one and read the flags back in bucket order.
    let (mut low, mut high) = (usize::MAX, 0);
    for &bucket in &work.touched[..touched] {
        let bucket = bucket as usize;
        work.flags[bucket] = 0xFF;
        low = low.min(bucket);
        high = high.max(bucket);
    }
    // The earliest bucket opens the first session.
    work.flags[low] = 0;
    let earliest = std::mem::replace(&mut work.cells[low], EMPTY_CELL);
    let mut walk = Walk::first(earliest, &mut work.start);
    let mut base = low;
    while base <= high {
        let mut bits = take_flags(&mut work.flags, base);
        while bits != 0 {
            let bucket = base + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let cell = std::mem::replace(&mut work.cells[bucket], EMPTY_CELL);
            walk.next(cell, &mut work.start, &mut work.count);
        }
        base += 32;
    }
    walk.finish(&mut work.count)
}

/// Sets bits `from..to` (`from < to`) of a bit set of 64-bit words.
#[inline(always)]
fn set_bits(words: &mut [u64], from: usize, to: usize) {
    let (first, last) = (from >> 6, (to - 1) >> 6);
    let head = u64::MAX << (from & 63);
    let tail = u64::MAX >> (63 - ((to - 1) & 63));
    if first == last {
        words[first] |= head & tail;
    } else {
        words[first] |= head;
        for word in &mut words[first + 1..last] {
            *word = u64::MAX;
        }
        words[last] |= tail;
    }
}

/// How many of bits `from..to` (`from < to`) of a bit set are set.
#[inline(always)]
fn count_bits(words: &[u64], from: usize, to: usize) -> u32 {
    let (first, last) = (from >> 6, (to - 1) >> 6);
    let head = u64::MAX << (from & 63);
    let tail = u64::MAX >> (63 - ((to - 1) & 63));
    if first == last {
        (words[first] & head & tail).count_ones()
    } else {
        let middle: u32 = words[first + 1..last].iter().map(|word| word.count_ones()).sum();
        (words[first] & head).count_ones() + middle + (words[last] & tail).count_ones()
    }
}

/// The category with the highest sum; a tie goes to the byte-wise smallest
/// name. Clears the sums.
fn top_category(rank: &[u32], sums: &mut [i64]) -> u32 {
    let mut best = 0;
    for candidate in 1..sums.len() {
        let better =
            sums[candidate] > sums[best] || (sums[candidate] == sums[best] && rank[candidate] < rank[best]);
        if better {
            best = candidate;
        }
    }
    sums.fill(0);
    best as u32
}

/// Fills `lookup` for the ascending `starts`: per bucket, how many starts
/// come before it and the start inside it. Returns the index of the entry
/// that stands for every later bucket.
fn build_lookup(lookup: &mut [u64], starts: &[u32]) -> usize {
    // Starts are more than a bucket apart, so a bucket holds at most one.
    let mut limit = 0;
    for (session, &start) in starts.iter().enumerate() {
        let own = (start / GAP) as usize;
        while limit < own {
            lookup[limit] = (session as u64) << 32 | u64::from(u32::MAX);
            limit += 1;
        }
        lookup[own] = (session as u64) << 32 | u64::from(start);
        limit = own + 1;
    }
    lookup[limit] = (starts.len() as u64) << 32 | u64::from(u32::MAX);
    limit
}

/// How many starts are below `floor` and how many are at most `ts`, from the table.
#[inline(always)]
fn reach_by_lookup(lookup: &[u64], limit: usize, floor: u32, ts: u32) -> (usize, usize) {
    let low = lookup[((floor / GAP) as usize).min(limit)];
    let high = lookup[((ts / GAP) as usize).min(limit)];
    ((low >> 32) as usize + usize::from((low as u32) < floor), (high >> 32) as usize + usize::from(high as u32 <= ts))
}

/// How many of the `LANES * blocks` starts (padded with `u32::MAX`) are
/// below `floor` and how many are at most `ts`, by comparing them all.
#[inline(always)]
fn reach_by_count(starts: &[u32], blocks: usize, floor: u32, ts: u32) -> (usize, usize) {
    let starts = &starts[..LANES * blocks];
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `starts` has `LANES * blocks` words.
    return unsafe { wide::reach(starts.as_ptr(), blocks, floor, ts) };
    #[cfg(not(target_arch = "x86_64"))]
    {
        let (mut below, mut up_to) = (0, 0);
        for &start in starts {
            below += usize::from(start < floor);
            up_to += usize::from(start <= ts);
        }
        (below, up_to)
    }
}

/// Records one click whose reach is sessions `from..to` as a candidate for
/// crediting session `from`. Of the clicks that begin at one session the
/// latest wins; in one millisecond, the byte-wise smallest click ID.
///
/// A click that reaches no session (`from == to`) is recorded too. It is
/// older than every click that does reach session `from`, and when it is
/// the only one there the carried click it hides is older still, out of
/// reach like itself, so the sweep's own reach test gives the same answer.
#[inline(always)]
fn offer(best: &mut [u64], clicks: &[[u64; 2]], click_exotic: &[Box<[u8]>], at: usize, from: usize) {
    let click = clicks[at];
    let candidate = (click[0] + 1) << 32 | at as u64;
    let current = best[from];
    if current >> 32 == candidate >> 32 {
        let other = clicks[(current & 0xFFFF_FFFF) as usize][1];
        if id_cmp(click[1] & CODE_MASK, other & CODE_MASK, b'k', click_exotic) == Ordering::Less {
            best[from] = candidate;
        }
    } else {
        best[from] = current.max(candidate);
    }
}

/// The slot of a click's campaign for the current person, a new one the
/// first time. `slots` counts the slots given out.
#[inline(always)]
fn slot_for(slot_of: &mut [u32], slot_campaign: &mut [u32], slots: &mut u32, campaign: usize) -> usize {
    let seen = slot_of[campaign];
    *slots += u32::from(seen == 0);
    let slot = if seen == 0 { *slots } else { seen };
    slot_of[campaign] = slot;
    // Written again each time: cheaper than asking whether the slot is new.
    slot_campaign[slot as usize] = campaign as u32;
    slot as usize
}

/// Credit per session from the best clicks: the click that credits a session
/// is the latest one at or before its start, which is the best of the
/// session's own or else the one carried over. It counts while
/// `start - 7 days <= ts`; with `ts` counted from seven days before the
/// window and `start` from the window, that is `start <= ts`.
fn credit(work: &mut Work, clicks: &[[u64; 2]], sessions: usize, organic: u32) {
    let mut carried = 0u64;
    for session in 0..sessions {
        let own = work.best[session];
        carried = if own != 0 { own } else { carried };
        // With nothing carried this reads click 0 and then keeps `organic`.
        let campaign = (clicks[(carried & 0xFFFF_FFFF) as usize][1] >> 42) as u32;
        work.credited[session] = if (carried >> 32) as u32 > work.start[session] { campaign } else { organic };
    }
}

/// Credit and assists of a person with clicks and at most 64 sessions: one
/// word of session bits per campaign.
fn attribute_narrow(
    params: &Params,
    tuning: Tuning,
    work: &mut Work,
    click_exotic: &[Box<[u8]>],
    sessions: usize,
    rows: usize,
    clicks: &[[u64; 2]],
) {
    let most_slots = clicks.len().min(params.campaigns);
    if work.masks.len() <= most_slots {
        work.masks.resize(most_slots + 1, 0);
    }
    if work.slot_campaign.len() <= most_slots {
        work.slot_campaign.resize(most_slots + 1, 0);
    }
    work.best[..=sessions].fill(0);
    for row in 0..rows {
        work.row_bits[row] = low_bits(work.row_first[row + 1] as usize) & !low_bits(work.row_first[row] as usize);
    }
    let blocks = sessions.div_ceil(LANES);
    let use_lookup = sessions > 2 * LANES && sessions * clicks.len() >= tuning.lookup_min;
    let limit = if use_lookup { build_lookup(&mut work.lookup, &work.start[..sessions]) } else { 0 };
    work.profile.lap(6);

    let mut slots = 0;
    for (at, click) in clicks.iter().enumerate() {
        // Both in ms: `ts` since `window_start - 7 days`, starts since
        // `window_start`. The click reaches the sessions with
        // `ts - 7 days <= start <= ts`: sessions `from..to`.
        let ts = click[0] as u32;
        let floor = ts.saturating_sub(WEEK);
        let (from, to) = if use_lookup {
            reach_by_lookup(&work.lookup, limit, floor, ts)
        } else {
            reach_by_count(&work.start, blocks, floor, ts)
        };
        offer(&mut work.best, clicks, click_exotic, at, from);
        let slot = slot_for(&mut work.slot_of, &mut work.slot_campaign, &mut slots, (click[1] >> 42) as usize);
        work.masks[slot] |= low_bits(to) & !low_bits(from);
    }
    work.profile.lap(7);

    credit(work, clicks, sessions, params.organic as u32);
    work.profile.lap(8);

    // Assists per campaign and day, kept for the end of the partition. Every
    // (campaign, row) pair is written; the write stays only if the count is not zero.
    work.pending.reserve(slots as usize * rows);
    let mut length = work.pending.len();
    let pending = work.pending.as_mut_ptr();
    for slot in 1..=slots as usize {
        let bits = std::mem::take(&mut work.masks[slot]);
        let campaign = work.slot_campaign[slot] as usize;
        work.slot_of[campaign] = 0;
        let base = (campaign * params.day_count) as u64;
        for row in 0..rows {
            let assisted = (bits & work.row_bits[row]).count_ones();
            // SAFETY: at most `slots * rows` entries are written and that many are reserved.
            unsafe { *pending.add(length) = (base + u64::from(work.row_day[row])) << 32 | u64::from(assisted) };
            length += usize::from(assisted != 0);
        }
    }
    // SAFETY: every entry below `length` was written above or earlier.
    unsafe { work.pending.set_len(length) };
    work.profile.lap(9);
}

/// Credit and assists of a person with clicks and more than 64 sessions:
/// several words of session bits per campaign.
fn attribute_wide(
    params: &Params,
    work: &mut Work,
    click_exotic: &[Box<[u8]>],
    sessions: usize,
    rows: usize,
    clicks: &[[u64; 2]],
) {
    let words = sessions.div_ceil(64);
    let most_slots = clicks.len().min(params.campaigns);
    if work.masks.len() < (most_slots + 1) * words {
        work.masks.resize((most_slots + 1) * words, 0);
    }
    if work.slot_campaign.len() <= most_slots {
        work.slot_campaign.resize(most_slots + 1, 0);
    }
    work.best[..=sessions].fill(0);
    let limit = build_lookup(&mut work.lookup, &work.start[..sessions]);
    work.profile.lap(6);

    let mut slots = 0;
    for (at, click) in clicks.iter().enumerate() {
        let ts = click[0] as u32;
        let (from, to) = reach_by_lookup(&work.lookup, limit, ts.saturating_sub(WEEK), ts);
        offer(&mut work.best, clicks, click_exotic, at, from);
        let slot = slot_for(&mut work.slot_of, &mut work.slot_campaign, &mut slots, (click[1] >> 42) as usize);
        if from < to {
            set_bits(&mut work.masks[slot * words..(slot + 1) * words], from, to);
        }
    }
    work.profile.lap(7);

    credit(work, clicks, sessions, params.organic as u32);
    work.profile.lap(8);

    for slot in 1..=slots as usize {
        let campaign = work.slot_campaign[slot] as usize;
        work.slot_of[campaign] = 0;
        let base = campaign * params.day_count;
        let bits = &mut work.masks[slot * words..(slot + 1) * words];
        for row in 0..rows {
            let assisted = count_bits(bits, work.row_first[row] as usize, work.row_first[row + 1] as usize);
            if assisted != 0 {
                work.pending.push(((base + work.row_day[row] as usize) as u64) << 32 | u64::from(assisted));
            }
        }
        bits.fill(0);
    }
    work.profile.lap(9);
}

/// Everything for one person: its rows and its share of the campaign cells.
/// `events` starts at the person's first event and runs to the end of the
/// grouped events and their padding (`EVENT_PAD` words after the last
/// person's last event); the person has the first `count`, in any order.
#[allow(clippy::too_many_arguments)]
fn person(
    params: &Params,
    tuning: Tuning,
    work: &mut Work,
    out: &mut PassCOut,
    click_exotic: &[Box<[u8]>],
    name: u64,
    events: &mut [u32],
    count: usize,
    purchases: &[[u64; 2]],
    clicks: &[[u64; 2]],
) {
    let sessions = if tuning.network && count <= NETWORK_MAX {
        sessions_by_network(events, count, work)
    } else if count <= tuning.sort_max {
        sessions_by_sort(&mut events[..count], work)
    } else {
        sessions_by_buckets(&events[..count], work, tuning.network)
    };
    work.profile.lap(3);
    finish_person(params, tuning, work, out, click_exotic, name, sessions, purchases, clicks);
}

/// Everything for one person whose events are already in time order: its
/// sessions come from one pass over the timestamps, with no sort and no
/// bucket summaries. `events` is exactly the person's events (at least
/// one). If they turn out not to be in order, the person is done as by
/// `person`, so the rows are the same either way.
#[allow(clippy::too_many_arguments)]
fn person_in_order(
    params: &Params,
    tuning: Tuning,
    work: &mut Work,
    out: &mut PassCOut,
    click_exotic: &[Box<[u8]>],
    name: u64,
    events: &[u32],
    purchases: &[[u64; 2]],
    clicks: &[[u64; 2]],
) {
    let sessions = match sessions_in_order(events, work) {
        Some(sessions) => sessions,
        None => sessions_by_buckets(events, work, tuning.network),
    };
    work.profile.lap(3);
    finish_person(params, tuning, work, out, click_exotic, name, sessions, purchases, clicks);
}

/// The rest of a person once `work.start` and `work.count` hold its
/// `sessions` sessions: rows, purchases, attribution and output.
#[allow(clippy::too_many_arguments)]
fn finish_person(
    params: &Params,
    tuning: Tuning,
    work: &mut Work,
    out: &mut PassCOut,
    click_exotic: &[Box<[u8]>],
    name: u64,
    sessions: usize,
    purchases: &[[u64; 2]],
    clicks: &[[u64; 2]],
) {
    work.start[sessions..sessions + LANES].fill(u32::MAX);

    // One row per day with a session start. Every session writes the row it
    // would start; the write stays only where the day changes.
    let mut rows = 0;
    let mut previous_day = u32::MAX;
    for session in 0..sessions {
        let day = (work.start[session] + params.day_offset) / DAY;
        work.row_first[rows] = session as u32;
        work.row_day[rows] = day;
        work.row_events[rows] = 0;
        rows += usize::from(day != previous_day);
        work.row_events[rows - 1] += i64::from(work.count[session]);
        work.row[session] = (rows - 1) as u8;
        previous_day = day;
    }
    work.row_first[rows] = sessions as u32;
    work.profile.lap(4);

    let has_purchases = !purchases.is_empty();
    if has_purchases {
        work.revenue[..sessions].fill(0);
        work.purchases[..sessions].fill(0);
        for purchase in purchases {
            // The session that holds the purchase: the last one that starts at or before it.
            let session = starts_up_to(&work.start[..sessions], purchase[0] as u32).saturating_sub(1);
            let price = (purchase[1] & 0xFFFF_FFFF) as i64;
            work.revenue[session] += price;
            work.purchases[session] += 1;
            work.category_sums[work.row[session] as usize * params.categories + (purchase[1] >> 32) as usize] += price;
        }
    }
    work.profile.lap(5);

    let has_clicks = !clicks.is_empty();
    if has_clicks {
        if sessions <= 64 {
            attribute_narrow(params, tuning, work, click_exotic, sessions, rows, clicks);
        } else {
            attribute_wide(params, work, click_exotic, sessions, rows, clicks);
        }
        for session in 0..sessions {
            let day = work.row_day[work.row[session] as usize] as usize;
            let cell = &mut out.campaign_days[work.credited[session] as usize * params.day_count + day];
            cell[0] += 1;
            if has_purchases {
                cell[1] += i64::from(work.purchases[session] > 0);
                cell[2] += work.revenue[session];
            }
        }
    }

    out.names.reserve(rows);
    out.days.reserve(rows);
    out.sessions.reserve(rows);
    out.events.reserve(rows);
    out.revenue.reserve(rows);
    out.top_category.reserve(rows);
    for row in 0..rows {
        let (from, to) = (work.row_first[row] as usize, work.row_first[row + 1] as usize);
        let day = work.row_day[row] as usize;
        let (mut revenue, mut bought, mut buying_sessions) = (0i64, 0u32, 0i64);
        if has_purchases {
            for session in from..to {
                revenue += work.revenue[session];
                bought += work.purchases[session];
                buying_sessions += i64::from(work.purchases[session] > 0);
            }
        }
        let top = if bought == 0 {
            NO_CATEGORY
        } else {
            top_category(
                &params.category_rank,
                &mut work.category_sums[row * params.categories..(row + 1) * params.categories],
            )
        };
        out.names.push(name);
        out.days.push((params.first_day + day as i64) as i32);
        out.sessions.push((to - from) as i64);
        out.events.push(work.row_events[row]);
        out.revenue.push(revenue);
        out.top_category.push(top);
        if !has_clicks {
            let cell = &mut out.campaign_days[params.organic * params.day_count + day];
            cell[0] += (to - from) as i64;
            cell[1] += buying_sessions;
            cell[2] += revenue;
        }
    }
    work.profile.lap(10);
}

/// Sessions, attribution and daily rows for one person partition.
pub fn sessions_partition(
    ctx: &Ctx,
    part: usize,
    event_lists: &[PartList],
    click_lists: &[PartList],
    click_exotic: &[Box<[u8]>],
    out: &mut PassCOut,
    scratch: &mut ScratchC,
) {
    let _ = ctx;
    partition_core(part, event_lists, click_lists, click_exotic, out, scratch);
}

/// `sessions_partition` without the `Ctx`: everything it needs is in the scratch.
fn partition_core(
    part: usize,
    event_lists: &[PartList],
    click_lists: &[PartList],
    click_exotic: &[Box<[u8]>],
    out: &mut PassCOut,
    scratch: &mut ScratchC,
) {
    let ScratchC { params, tuning, group, work, batch } = scratch;
    assert_eq!(batch.persons, 0, "a batch of persons was left unfinished");
    // The first block of every list, while the lists are measured.
    for list in event_lists.iter().chain(click_lists) {
        if let Some(block) = list.blocks().next() {
            prefetch(block);
        }
    }
    work.profile.start();
    let event_words: usize = event_lists.iter().map(PartList::words).sum();
    if event_words == 0 {
        return;
    }
    let click_words: usize = click_lists.iter().map(PartList::words).sum();
    // Person indexes and slice ends are kept in 31 and 32 bits.
    assert!(event_words <= INDEX_MASK as usize && click_words <= u32::MAX as usize);
    group.begin();
    if event_words + click_words <= tuning.stage_max {
        group.read_events(event_lists, event_words);
        work.profile.lap(0);
        group.read_clicks(click_lists, click_words);
        work.profile.lap(1);
        group.scatter();
    } else {
        group.group_in_two_passes(event_lists, click_lists);
    }
    work.profile.lap(2);

    let mut starts = [0usize; 3];
    for index in 0..group.locals.len() {
        let ends = group.ends[index].map(|end| end as usize);
        let name = unmix(((part as u64) << params.person_shift) | u64::from(group.locals[index]));
        person(
            params,
            *tuning,
            work,
            out,
            click_exotic,
            name,
            &mut group.events[starts[0]..],
            ends[0] - starts[0],
            &group.purchases[starts[1]..ends[1]],
            &group.clicks[starts[2]..ends[2]],
        );
        starts = ends;
    }
    apply_assists(&mut work.pending, &mut out.campaign_days);
    work.profile.lap(9);
    work.profile.end(part);
    if !work.reserved {
        // The first partition with rows tells how many rows a partition
        // has. Room for half of all partitions' rows is asked for at once:
        // growing the six columns step by step would copy them again and
        // again, and memory that is reserved but never written costs nothing.
        work.reserved = true;
        let rows = out.names.len() << (super::codes::CODE_BITS - params.person_shift) >> 1;
        out.names.reserve(rows);
        out.days.reserve(rows);
        out.sessions.reserve(rows);
        out.events.reserve(rows);
        out.revenue.reserve(rows);
        out.top_category.reserve(rows);
    }
}

/// Adds the pending assists to their cells. The cells are spread over a
/// table bigger than the nearest caches, so each one is asked for a few
/// entries before it is needed.
fn apply_assists(pending: &mut Vec<u64>, cells: &mut [[i64; 4]]) {
    const AHEAD: usize = 12;
    for at in 0..pending.len() {
        #[cfg(target_arch = "x86_64")]
        if let Some(&later) = pending.get(at + AHEAD) {
            use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            // SAFETY: a prefetch reads nothing, whatever the address.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(cells.as_ptr().wrapping_add((later >> 32) as usize) as *const i8) };
        }
        let entry = pending[at];
        cells[(entry >> 32) as usize][3] += (entry & 0xFFFF_FFFF) as i64;
    }
    pending.clear();
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::fast::arena::{Arena, PartWriter};
    use crate::fast::codes::{id_bytes, pattern_code, EXOTIC_BASE};

    /// SplitMix64: small, seedable, good enough to shuffle test data.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        /// Both ends included.
        fn range(&mut self, low: u64, high: u64) -> u64 {
            low + self.below(high - low + 1)
        }

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[self.below(items.len() as u64) as usize]
        }

        fn one_in(&mut self, count: u64) -> bool {
            self.below(count) == 0
        }

        fn shuffle<T>(&mut self, items: &mut [T]) {
            for at in (1..items.len()).rev() {
                items.swap(at, self.below(at as u64 + 1) as usize);
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct Event {
        local: u32,
        /// Milliseconds since `window_start`.
        ts: u32,
        /// `(price, category)`.
        purchase: Option<(u32, u32)>,
    }

    #[derive(Clone, Copy, Debug)]
    struct Click {
        local: u32,
        /// Milliseconds since `window_start - 7 days`.
        ts: u32,
        code: u64,
        campaign: u32,
    }

    /// What `Ctx` holds for a run.
    struct World {
        window_start: i64,
        /// `window_end - window_start`.
        length: u32,
        first_day: i64,
        day_count: usize,
        category_names: Vec<Box<[u8]>>,
        /// Campaigns including `organic`, the last one.
        campaigns: usize,
        exotic: Vec<Box<[u8]>>,
    }

    impl World {
        fn new(window_start: i64, length: u32, categories: &[&str], campaigns: usize, exotic: &[&str]) -> Self {
            let first_day = window_start.div_euclid(DAY_MS);
            let boxed = |names: &[&str]| names.iter().map(|name| name.as_bytes().into()).collect();
            World {
                window_start,
                length,
                first_day,
                day_count: ((window_start + i64::from(length) - 1).div_euclid(DAY_MS) - first_day + 1) as usize,
                category_names: boxed(categories),
                campaigns,
                exotic: boxed(exotic),
            }
        }
    }

    struct Partition {
        part: usize,
        events: Vec<Event>,
        clicks: Vec<Click>,
    }

    /// Both output tables in a form that does not depend on row order.
    #[derive(Debug, PartialEq)]
    struct Tables {
        /// `(name, day) -> (sessions, events, revenue, top category)`.
        rows: BTreeMap<(u64, i32), (i64, i64, i64, u32)>,
        campaign_days: Vec<[i64; 4]>,
    }

    const TEST_PERSON_SHIFT: u32 = 32;

    /// rules.md read literally for one partition: sort each person's events,
    /// cut sessions at gaps over 30 minutes, and try every click of the
    /// person against every session.
    fn reference(world: &World, partition: &Partition, tables: &mut Tables) {
        let organic = world.campaigns - 1;
        let click_id = |code: u64| {
            let mut bytes = Vec::new();
            id_bytes(code, b'k', &world.exotic, &mut bytes);
            bytes
        };
        let mut people: BTreeMap<u32, Vec<Event>> = BTreeMap::new();
        for event in &partition.events {
            people.entry(event.local).or_default().push(*event);
        }
        for (&local, events) in &mut people {
            events.sort_by_key(|event| event.ts);
            let name = unmix(((partition.part as u64) << TEST_PERSON_SHIFT) | u64::from(local));
            let clicks: Vec<&Click> = partition.clicks.iter().filter(|click| click.local == local).collect();
            let mut category_sums: BTreeMap<(i32, u32), i64> = BTreeMap::new();
            let mut at = 0;
            while at < events.len() {
                let start = i64::from(events[at].ts);
                let day = (world.window_start + start).div_euclid(DAY_MS);
                let day_index = (day - world.first_day) as usize;
                let (mut last, mut count, mut revenue, mut bought) = (start, 0i64, 0i64, 0);
                while at < events.len() && i64::from(events[at].ts) - last <= SESSION_GAP_MS {
                    last = i64::from(events[at].ts);
                    count += 1;
                    if let Some((price, category)) = events[at].purchase {
                        revenue += i64::from(price);
                        bought += 1;
                        *category_sums.entry((day as i32, category)).or_default() += i64::from(price);
                    }
                    at += 1;
                }
                let row = tables.rows.entry((name, day as i32)).or_insert((0, 0, 0, NO_CATEGORY));
                row.0 += 1;
                row.1 += count;
                row.2 += revenue;

                let mut credited: Option<&Click> = None;
                let mut assisting = BTreeSet::new();
                for click in &clicks {
                    let click_time = i64::from(click.ts) - WEEK_MS;
                    if click_time <= start && start - click_time <= WEEK_MS {
                        assisting.insert(click.campaign);
                        let better = match credited {
                            None => true,
                            Some(best) => {
                                click.ts > best.ts || (click.ts == best.ts && click_id(click.code) < click_id(best.code))
                            }
                        };
                        if better {
                            credited = Some(click);
                        }
                    }
                }
                let campaign = credited.map_or(organic, |click| click.campaign as usize);
                let cell = &mut tables.campaign_days[campaign * world.day_count + day_index];
                cell[0] += 1;
                cell[1] += i64::from(bought > 0);
                cell[2] += revenue;
                for campaign in assisting {
                    tables.campaign_days[campaign as usize * world.day_count + day_index][3] += 1;
                }
            }
            // Highest sum of the day; a tie goes to the byte-wise smallest name.
            let mut tops: BTreeMap<i32, (i64, u32)> = BTreeMap::new();
            for (&(day, category), &sum) in &category_sums {
                let top = tops.entry(day).or_insert((sum, category));
                let names = &world.category_names;
                if sum > top.0 || (sum == top.0 && names[category as usize] < names[top.1 as usize]) {
                    *top = (sum, category);
                }
            }
            for (day, (_, category)) in tops {
                tables.rows.get_mut(&(name, day)).unwrap().3 = category;
            }
        }
    }

    /// Runs pass C over the partitions with one scratch and one output, as a
    /// worker does, with the rows cut into lists and blocks at random.
    fn run(world: &World, tuning: Tuning, partitions: &[Partition], rng: &mut Rng) -> Tables {
        let params = Params::new(
            world.window_start,
            world.first_day,
            world.day_count,
            world.campaigns,
            world.campaigns - 1,
            &world.category_names,
            TEST_PERSON_SHIFT,
        );
        let mut scratch = ScratchC::with(params, tuning);
        let mut out = PassCOut {
            names: Vec::new(),
            days: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top_category: Vec::new(),
            campaign_days: vec![[0; 4]; world.campaigns * world.day_count],
        };
        for partition in partitions {
            let workers = rng.range(1, 4) as usize;
            // Two header words per block, and room for at least one record.
            let event_block = rng.pick(&[4, 5, 7, 512]);
            let click_block = rng.pick(&[4, 5, 6, 256]);
            let mut arenas: Vec<Arena> = (0..workers).map(|_| Arena::new()).collect();
            let mut event_writers = vec![PartWriter::EMPTY; workers];
            let mut click_writers = vec![PartWriter::EMPTY; workers];
            for event in &partition.events {
                let worker = rng.below(workers as u64) as usize;
                let key =
                    (u64::from(event.local) << 32) | (u64::from(event.ts) << 1) | u64::from(event.purchase.is_some());
                let words = 1 + usize::from(event.purchase.is_some());
                let at = event_writers[worker].reserve(&mut arenas[worker], event_block, words);
                // SAFETY: `reserve` returned room for `words` words.
                unsafe {
                    *at = key;
                    if let Some((price, category)) = event.purchase {
                        *at.add(1) = u64::from(price) | (u64::from(category) << 32);
                    }
                }
            }
            for click in &partition.clicks {
                let worker = rng.below(workers as u64) as usize;
                let key = (u64::from(click.local) << 32) | (u64::from(click.ts) << 1);
                let at = click_writers[worker].reserve(&mut arenas[worker], click_block, 2);
                // SAFETY: `reserve` returned room for two words.
                unsafe {
                    *at = key;
                    *at.add(1) = click.code | (u64::from(click.campaign) << 42);
                }
            }
            let event_lists: Vec<PartList> = event_writers.iter_mut().map(PartWriter::finish).collect();
            let click_lists: Vec<PartList> = click_writers.iter_mut().map(PartWriter::finish).collect();
            partition_core(partition.part, &event_lists, &click_lists, &world.exotic, &mut out, &mut scratch);
        }
        tables_of(&out)
    }

    /// A worker's output as tables.
    fn tables_of(out: &PassCOut) -> Tables {
        let count = out.names.len();
        for length in [out.days.len(), out.sessions.len(), out.events.len(), out.revenue.len(), out.top_category.len()]
        {
            assert_eq!(length, count);
        }
        let mut tables = Tables { rows: BTreeMap::new(), campaign_days: out.campaign_days.clone() };
        for at in 0..count {
            let key = (out.names[at], out.days[at]);
            let row = (out.sessions[at], out.events[at], out.revenue[at], out.top_category[at]);
            assert!(tables.rows.insert(key, row).is_none(), "two rows for one person and day");
        }
        tables
    }

    /// How `run_in_order` hands a person to the kernel.
    #[derive(Clone, Copy, PartialEq)]
    enum Entry {
        /// All timestamps at once, sorted.
        Sorted,
        /// All timestamps at once, as they come (the fallback, mostly).
        AsTheyCome,
        /// One event at a time into the batch, sorted or as they come; a
        /// person that turns out not to be in order is taken out of the
        /// batch again and handed over whole.
        Streamed(bool),
    }

    /// Runs every person through the entries for a caller that keeps each
    /// person's events together.
    fn run_in_order(world: &World, tuning: Tuning, partitions: &[Partition], entry: Entry) -> Tables {
        let params = Params::new(
            world.window_start,
            world.first_day,
            world.day_count,
            world.campaigns,
            world.campaigns - 1,
            &world.category_names,
            TEST_PERSON_SHIFT,
        );
        let mut scratch = ScratchC::with(params, tuning);
        let mut out = PassCOut {
            names: Vec::new(),
            days: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top_category: Vec::new(),
            campaign_days: vec![[0; 4]; world.campaigns * world.day_count],
        };
        // Decides where a batch is finished early.
        let mut rng = Rng(partitions.iter().map(|partition| partition.events.len() as u64).sum());
        for partition in partitions {
            // Per person: its events with their purchase words, and its clicks.
            let mut people: BTreeMap<u32, (Vec<(u32, Option<u64>)>, Vec<[u64; 2]>)> = BTreeMap::new();
            for event in &partition.events {
                let word = event.purchase.map(|(price, category)| u64::from(price) | (u64::from(category) << 32));
                people.entry(event.local).or_default().0.push((event.ts, word));
            }
            for click in &partition.clicks {
                if let Some(person) = people.get_mut(&click.local) {
                    person.1.push([u64::from(click.ts), click.code | (u64::from(click.campaign) << 42)]);
                }
            }
            for (&local, (stream, clicks)) in &mut people {
                if matches!(entry, Entry::Sorted | Entry::Streamed(true)) {
                    stream.sort_by_key(|&(ts, _)| ts);
                }
                let name = unmix(((partition.part as u64) << TEST_PERSON_SHIFT) | u64::from(local));
                if let Entry::Streamed(_) = entry {
                    if give_to_batch(&mut scratch, &mut out, &world.exotic, name, stream, clicks) {
                        if rng.one_in(7) {
                            scratch.finish_batch(&mut out, &world.exotic);
                        }
                        continue;
                    }
                }
                let events: Vec<u32> = stream.iter().map(|&(ts, _)| ts).collect();
                let purchases: Vec<[u64; 2]> =
                    stream.iter().filter_map(|&(ts, word)| word.map(|word| [u64::from(ts), word])).collect();
                scratch.one_person_in_order(&mut out, &world.exotic, name, &events, &purchases, clicks);
            }
            scratch.finish_batch(&mut out, &world.exotic);
            scratch.flush(&mut out);
        }
        tables_of(&out)
    }

    /// Hands one person to the batch, event by event. `false` when its
    /// events are not in time order: the batch is then as it was before.
    fn give_to_batch(
        scratch: &mut ScratchC,
        out: &mut PassCOut,
        exotic: &[Box<[u8]>],
        name: u64,
        stream: &[(u32, Option<u64>)],
        clicks: &[[u64; 2]],
    ) -> bool {
        let mut person = scratch.begin_person(out, exotic, stream[0].0);
        let (start, meta, bought) = scratch.batch_scratch();
        if let Some(word) = stream[0].1 {
            person.purchase(bought, word);
        }
        for &(ts, purchase) in &stream[1..] {
            if !person.add(start, meta, ts) {
                scratch.abort_person(person);
                return false;
            }
            if let Some(word) = purchase {
                person.purchase(bought, word);
            }
        }
        assert_eq!(person.events(), stream.len() as u64);
        scratch.end_person(person, name, clicks.iter().copied());
        true
    }

    fn expected(world: &World, partitions: &[Partition]) -> Tables {
        let mut tables =
            Tables { rows: BTreeMap::new(), campaign_days: vec![[0; 4]; world.campaigns * world.day_count] };
        for partition in partitions {
            reference(world, partition, &mut tables);
        }
        tables
    }

    /// A distinct generator-style click ID per `serial`, as its code.
    fn click_code(serial: u64) -> u64 {
        const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        // A bijection on 0..36^5, so short and long IDs mix.
        let mut value = serial.wrapping_mul(2_654_435_761) % 60_466_176;
        let mut id = vec![b'k'];
        loop {
            id.push(DIGITS[(value % 36) as usize]);
            value /= 36;
            if value == 0 {
                break;
            }
        }
        pattern_code(&id, b'k').unwrap()
    }

    /// Session starts of a person's timestamps, the plain way.
    fn starts_of(mut ts: Vec<u32>) -> Vec<u32> {
        ts.sort_unstable();
        let mut starts = vec![ts[0]];
        for pair in ts.windows(2) {
            if pair[1] - pair[0] > GAP {
                starts.push(pair[1]);
            }
        }
        starts
    }

    /// One person's timestamps: walks forward in steps chosen to land on
    /// every kind of edge, then shuffles.
    fn person_times(world: &World, rng: &mut Rng) -> Vec<u32> {
        let length = u64::from(world.length);
        let day_offset = (world.window_start - world.first_day * DAY_MS) as u64;
        let (gap, day) = (u64::from(GAP), u64::from(DAY));
        let style = rng.below(12);
        let budget = match style {
            0..=4 => rng.range(1, 6),
            5..=7 => rng.range(5, 60),
            8 | 9 => rng.range(40, 400),
            10 => rng.range(300, 3000),
            _ => rng.range(600, 1500),
        } as usize;
        let mut ts = match rng.below(4) {
            0 => 0,
            1 => rng.below(length),
            2 => (gap - 1).min(length - 1),
            _ => rng.below(gap.min(length)),
        };
        let mut times = Vec::new();
        while times.len() < budget {
            times.push(ts as u32);
            let step = if style == 11 {
                // As many sessions as a window can hold.
                rng.pick(&[gap + 1, gap + 1, gap + 1, gap + 2, 0])
            } else {
                match rng.below(16) {
                    0 | 1 => 0,
                    2 => 1,
                    3 => gap - 1,
                    4 => gap,
                    5 => gap + 1,
                    6 => gap + 2,
                    7 => 2 * gap,
                    8 => 2 * gap + 1,
                    // The next bucket edge and the millisecond before it.
                    9 => (ts / gap + 1) * gap - ts,
                    10 => (ts / gap + 1) * gap - 1 - ts,
                    // The next UTC midnight and the millisecond before it.
                    11 => day - (ts + day_offset) % day,
                    12 => day - (ts + day_offset) % day - 1,
                    13 => rng.below(120_000),
                    14 => rng.below(3 * gap),
                    _ => rng.below(3 * day),
                }
            };
            if ts + step >= length {
                if rng.one_in(3) {
                    times.push((length - 1) as u32);
                }
                break;
            }
            ts += step;
        }
        rng.shuffle(&mut times);
        times
    }

    fn random_partition(world: &World, part: usize, serial: &mut u64, rng: &mut Rng) -> Partition {
        let mut partition = Partition { part, events: Vec::new(), clicks: Vec::new() };
        let persons = match rng.below(10) {
            0 => rng.range(150, 700),
            1 => 1,
            _ => rng.range(2, 40),
        };
        let categories = world.category_names.len() as u64;
        let real_campaigns = world.campaigns as u64 - 1;
        let reach = i64::from(world.length) + WEEK_MS;
        let mut locals = BTreeSet::new();
        let mut exotic_left: Vec<u64> = (0..world.exotic.len() as u64).collect();
        for _ in 0..persons {
            let local = match rng.below(40) {
                0 => 0,
                1 => u32::MAX,
                _ => rng.next() as u32,
            };
            if !locals.insert(local) {
                continue;
            }
            // In a partition of many persons most are small.
            let times = if persons > 100 && !rng.one_in(50) {
                let mut times = person_times(world, rng);
                times.truncate(rng.range(1, 12) as usize);
                times
            } else {
                person_times(world, rng)
            };
            let buys = rng.pick(&[0, 0, 0, 6, 6, 2, 1]);
            for &ts in &times {
                let purchase = (buys != 0 && rng.one_in(buys))
                    .then(|| (rng.pick(&[1, 1, 2, 3, 5, 100, 250, 10_000_000]), rng.below(categories) as u32));
                partition.events.push(Event { local, ts, purchase });
            }
            let starts = starts_of(times);
            let count = match rng.below(8) {
                0 | 1 => 0,
                2..=4 => rng.range(1, 8),
                5 | 6 => rng.range(8, 80),
                _ => rng.range(80, 1200),
            };
            let pool = rng.pick(&[1, 3, real_campaigns]).min(real_campaigns);
            let mut previous = 0i64;
            for _ in 0..count {
                // The session start on the click clock, and a click around it.
                let start = i64::from(rng.pick(&starts)) + WEEK_MS;
                let ts = match rng.below(13) {
                    0 | 1 => start,
                    2 => start + 1,
                    3 => start - 1,
                    4 => start - WEEK_MS,
                    5 => start - WEEK_MS - 1,
                    6 => start - WEEK_MS + 1,
                    7 => start - rng.below(DAY_MS as u64) as i64,
                    8 => start - rng.below((WEEK_MS + SESSION_GAP_MS) as u64) as i64,
                    9 => previous,
                    10 => start / SESSION_GAP_MS * SESSION_GAP_MS,
                    _ => rng.below(reach as u64) as i64,
                }
                .clamp(0, reach - 1);
                previous = ts;
                let code = if !exotic_left.is_empty() && rng.one_in(6) {
                    EXOTIC_BASE + exotic_left.swap_remove(rng.below(exotic_left.len() as u64) as usize)
                } else {
                    *serial += 1;
                    click_code(*serial)
                };
                let click = Click { local, ts: ts as u32, code, campaign: rng.below(pool) as u32 };
                // Copies of a click have identical values.
                for _ in 0..rng.pick(&[1, 1, 1, 1, 1, 1, 2, 3]) {
                    partition.clicks.push(click);
                }
            }
        }
        // Clicks of persons without events are nobody's.
        for _ in 0..rng.below(4) {
            let local = rng.next() as u32;
            if !locals.contains(&local) {
                *serial += 1;
                let ts = rng.below(reach as u64) as u32;
                partition.clicks.push(Click { local, ts, code: click_code(*serial), campaign: 0 });
            }
        }
        rng.shuffle(&mut partition.events);
        rng.shuffle(&mut partition.clicks);
        partition
    }

    fn random_world(rng: &mut Rng) -> World {
        let day = DAY_MS;
        let length = rng.pick(&[14 * day, 14 * day, 17 * day, 3 * day + 1_025_000, 2 * day, 3_600_000, 1_860_000]);
        let offset = match rng.below(5) {
            0 | 1 => 0,
            2 => 26_000_000,
            3 => day - 1,
            _ => rng.below(day as u64) as i64,
        };
        let mut names = vec!["shoes", "books", "Toys", "zebra", "a", "B", "unknown"];
        rng.shuffle(&mut names);
        names.truncate(rng.range(1, 7) as usize);
        let exotic: &[&str] = if rng.one_in(2) { &["k", "K7", "k7Z", " k7", "k_", "kzzzzzzzzz"] } else { &[] };
        let window_start = rng.range(19_000, 21_500) as i64 * day + offset;
        World::new(window_start, length as u32, &names, rng.pick(&[2, 3, 5, 40, 400]), exotic)
    }

    #[test]
    fn matches_the_literal_rules_on_random_partitions() {
        let worlds = if cfg!(debug_assertions) { 40 } else { 400 };
        for seed in 0..worlds {
            let mut rng = Rng(seed);
            let world = random_world(&mut rng);
            let mut serial = 0;
            // A worker gets each partition once, in rising order.
            let first_part = rng.below(1000) as usize;
            let partitions: Vec<Partition> = (0..rng.range(1, 5) as usize)
                .map(|at| random_partition(&world, first_part + at, &mut serial, &mut rng))
                .collect();
            let want = expected(&world, &partitions);
            let tuning = Tuning {
                network: rng.one_in(2),
                sort_max: rng.pick(&[0, 1, 2, 5, 16, 32, 100, 100_000]),
                lookup_min: rng.pick(&[0, 1, 64, 512, usize::MAX]),
                stage_max: rng.pick(&[0, 200, usize::MAX]),
            };
            let got = run(&world, tuning, &partitions, &mut rng);
            assert!(got == want, "seed {seed}: tables differ");
            // The default settings on the same data.
            let got = run(&world, Tuning::DEFAULT, &partitions, &mut rng);
            assert!(got == want, "seed {seed}: tables differ with the default tuning");
            // The entry for events that are already in time order, and its
            // fallback when they are not.
            for entry in [Entry::Sorted, Entry::AsTheyCome, Entry::Streamed(true), Entry::Streamed(false)] {
                assert!(run_in_order(&world, tuning, &partitions, entry) == want, "seed {seed}: one of the in-order entries");
            }
        }
    }

    /// One user, one partition, the official fixtures' window and three campaigns.
    fn tables_for(times: &[u32], clicks: &[(u32, u64, u32)]) -> (World, Tables) {
        let world = World::new(1_790_812_800_000, 14 * DAY, &["shoes", "books"], 3, &[]);
        let events = times.iter().map(|&ts| Event { local: 7, ts, purchase: None }).collect();
        let clicks = clicks.iter().map(|&(ts, code, campaign)| Click { local: 7, ts, code, campaign }).collect();
        let partitions = [Partition { part: 3, events, clicks }];
        let got = run(&world, Tuning::DEFAULT, &partitions, &mut Rng(1));
        assert!(got == expected(&world, &partitions));
        (world, got)
    }

    #[test]
    fn gaps_at_bucket_edges() {
        let sessions = |times: &[u32]| tables_for(times, &[]).1.rows.values().map(|row| row.0).sum::<i64>();
        // Exactly 30 minutes joins, one millisecond more splits, wherever the bucket edges fall.
        for base in [0, 1, GAP - 1, GAP, GAP + 1, 5 * GAP - 1, 5 * GAP] {
            assert_eq!(sessions(&[base, base + GAP - 1]), 1);
            assert_eq!(sessions(&[base, base + GAP]), 1);
            assert_eq!(sessions(&[base, base + GAP + 1]), 2);
            assert_eq!(sessions(&[base + GAP, base, base + 2 * GAP, base + 3 * GAP + 1, base + 3 * GAP + 1]), 2);
        }
        // A session over midnight is one row, on its start day.
        let (_, tables) = tables_for(&[DAY - 1, DAY, DAY + GAP, DAY + 2 * GAP + 1], &[]);
        let rows: Vec<_> = tables.rows.values().copied().collect();
        assert_eq!(rows, [(1, 3, 0, NO_CATEGORY), (1, 1, 0, NO_CATEGORY)]);
    }

    #[test]
    fn click_reach_and_ties() {
        let start = 3 * DAY + 5 * GAP;
        // On the click clock the session starts at `start + WEEK`. Returns the
        // credited campaign and every campaign's assists.
        let credited = |clicks: &[(u32, u64, u32)]| {
            let (world, tables) = tables_for(&[start, start + 1], clicks);
            let cell = |campaign: usize| tables.campaign_days[campaign * world.day_count + 3];
            let campaign = (0..world.campaigns).find(|&campaign| cell(campaign)[0] == 1).unwrap();
            let assists: Vec<i64> = (0..world.campaigns).map(|campaign| cell(campaign)[3]).collect();
            (campaign, assists)
        };
        let (a, b) = (click_code(1), click_code(2));
        assert_eq!(credited(&[]), (2, vec![0, 0, 0]));
        assert_eq!(credited(&[(start + WEEK, a, 0)]), (0, vec![1, 0, 0]));
        assert_eq!(credited(&[(start + WEEK + 1, a, 0)]), (2, vec![0, 0, 0]));
        assert_eq!(credited(&[(start, a, 1)]), (1, vec![0, 1, 0]));
        assert_eq!(credited(&[(start - 1, a, 1)]), (2, vec![0, 0, 0]));
        // The later click wins; both campaigns assist, each once.
        let clicks = [(start + 5, a, 0), (start + 6, b, 1), (start + 5, a, 0), (start + 7, click_code(3), 1)];
        assert_eq!(credited(&clicks), (1, vec![1, 1, 0]));
        // In one millisecond the smaller click ID wins, whichever comes first.
        let (small, large) = if a < b { (a, b) } else { (b, a) };
        assert_eq!(credited(&[(start + 5, small, 0), (start + 5, large, 1)]), (0, vec![1, 1, 0]));
        assert_eq!(credited(&[(start + 5, large, 1), (start + 5, small, 0)]), (0, vec![1, 1, 0]));
    }

    /// Every tuning the per-person strategies can run under.
    fn tunings() -> Vec<Tuning> {
        let mut tunings = Vec::new();
        for network in [true, false] {
            for sort_max in [0, 40] {
                for lookup_min in [0, 2048, usize::MAX] {
                    let stage_max = if tunings.len() % 2 == 0 { usize::MAX } else { 0 };
                    tunings.push(Tuning { network, sort_max, lookup_min, stage_max });
                }
            }
        }
        tunings
    }

    /// One person given in absolute milliseconds: events `(ts, purchase)` and
    /// clicks `(ts, click ID, campaign)`, with campaigns `A` (0) and `B` (1)
    /// and categories `A` (0) and `B` (1). Checks every tuning against the
    /// literal rules and returns the number of sessions.
    fn sessions_of_case(window_start: i64, events: &[(i64, Option<(u32, u32)>)], clicks: &[(i64, &str, u32)]) -> i64 {
        let mut exotic: Vec<&str> = Vec::new();
        let clicks: Vec<Click> = clicks
            .iter()
            .map(|&(ts, id, campaign)| {
                let code = pattern_code(id.as_bytes(), b'k').unwrap_or_else(|| {
                    let at = exotic.iter().position(|known| *known == id).unwrap_or_else(|| {
                        exotic.push(id);
                        exotic.len() - 1
                    });
                    EXOTIC_BASE + at as u64
                });
                Click { local: 11, ts: (ts - (window_start - WEEK_MS)) as u32, code, campaign }
            })
            .collect();
        let events = events
            .iter()
            .map(|&(ts, purchase)| Event { local: 11, ts: (ts - window_start) as u32, purchase })
            .collect();
        let world = World::new(window_start, 14 * DAY, &["A", "B"], 3, &exotic);
        let partitions = [Partition { part: 5, events, clicks }];
        let want = expected(&world, &partitions);
        for tuning in tunings() {
            assert!(run(&world, tuning, &partitions, &mut Rng(window_start as u64)) == want);
            for entry in [Entry::Sorted, Entry::AsTheyCome, Entry::Streamed(true), Entry::Streamed(false)] {
                assert!(run_in_order(&world, tuning, &partitions, entry) == want);
            }
        }
        want.rows.values().map(|row| row.0).sum()
    }

    /// The named cases of `checks/attack_rewrites.py`, for every offset of
    /// the window start from midnight that it uses.
    #[test]
    fn named_cases_of_the_rewrite_attack() {
        const MIDNIGHT: i64 = 1_790_812_800_000;
        let (gap, day, week) = (SESSION_GAP_MS, DAY_MS, WEEK_MS);
        let view = |ts: i64| (ts, None);
        for offset in [0, 1, gap - 1, 26_000_000, 123_457, day - 1] {
            let w = MIDNIGHT + offset;
            // A bucket edge of the grid that starts at the window start.
            let edge = w + 20 * gap;
            let minute = 60_000;
            let plain: [(&[i64], i64); 8] = [
                (&[edge, edge + gap], 1),
                (&[edge - 1, edge + gap - 1], 1),
                (&[edge, edge + 2 * gap - 1], 2),
                (&[edge - 1, edge + gap], 2),
                (&[edge, edge + gap + 1], 2),
                (&[edge - 1, edge], 1),
                (
                    &[
                        edge - 1,
                        edge - 1,
                        edge,
                        edge,
                        edge,
                        edge + gap,
                        edge + gap,
                        edge + gap,
                        edge + 2 * gap + 1,
                        edge + 2 * gap + 1,
                    ],
                    2,
                ),
                (&[edge + 7, edge + 7 + gap, edge + 7 + 2 * gap], 1),
            ];
            for (times, sessions) in plain {
                let events: Vec<_> = times.iter().map(|&ts| view(ts)).collect();
                assert_eq!(sessions_of_case(w, &events, &[]), sessions, "offset {offset}, times {times:?}");
            }
            // A bucket's min and max are not its first and last line.
            let events: Vec<_> = [29, 1, 58, 31, 89, 88].iter().map(|&at| view(edge + at * minute)).collect();
            assert_eq!(sessions_of_case(w, &events, &[(edge + 15 * minute, "k", 0)]), 1);
            // The first and last instants of the window and of the click window.
            let last = w + 14 * day - 1;
            assert_eq!(sessions_of_case(w, &[view(w), view(last)], &[(w - week, "a", 0), (last, "b", 1)]), 2);
            // A session across UTC midnight, with purchases on both sides.
            let midnight = MIDNIGHT + 3 * day;
            let events = [
                view(midnight - 600_000),
                (midnight, Some((5, 0))),
                (midnight + gap - 1, Some((7, 1))),
                view(midnight + 2 * gap),
            ];
            assert_eq!(sessions_of_case(w, &events, &[(midnight - 600_000, "k", 0)]), 2);
            // Two clicks of one campaign whose 7-day reaches touch, almost
            // touch, or leave a hole of one millisecond.
            let t = MIDNIGHT + 2 * day - 1 - week;
            for (second, start) in
                [(t + week, t + week), (t + week + 1, t + week), (t + week + 2, t + week + 1), (t + week + 1, t + week + 1)]
            {
                assert_eq!(sessions_of_case(w, &[view(start)], &[(t, "k1", 0), (second, "k2", 0)]), 1);
            }
            // A reach that ends exactly at a midnight.
            let events = [view(MIDNIGHT + 9 * day), view(MIDNIGHT + 9 * day + gap + 1)];
            assert_eq!(sessions_of_case(w, &events, &[(MIDNIGHT + 2 * day, "k", 0)]), 2);
            // A tie between campaigns: the loser of the tie still assists.
            let clicks = [
                (MIDNIGHT + 5 * day, "k10", 0),
                (MIDNIGHT + 5 * day, "k9", 1),
                (MIDNIGHT + 4 * day, "K", 0),
                (MIDNIGHT + 5 * day, "k10", 0),
            ];
            assert_eq!(sessions_of_case(w, &[view(MIDNIGHT + 6 * day)], &clicks), 1);
        }
        // A heavy person: an event every two hours for 14 days, a click of A
        // every hour for 21 days, clicks of B every hour for two days, every
        // click three times.
        let hour = 3_600_000;
        let events: Vec<_> = (0..14 * 24).step_by(2).map(|at| view(MIDNIGHT + at * hour)).collect();
        let ids: Vec<String> = (0..21 * 24)
            .map(|at| format!("h{at:04}"))
            .chain((0..48).map(|at| if at < 24 { format!("g{at:02}") } else { format!("i{at:02}") }))
            .collect();
        let mut clicks: Vec<(i64, &str, u32)> = Vec::new();
        for _ in 0..3 {
            for at in 0..21 * 24 {
                clicks.push((MIDNIGHT - week + at as i64 * hour, &ids[at], 0));
            }
            for at in 0..48 {
                clicks.push((MIDNIGHT + 4 * day + at as i64 * hour, &ids[21 * 24 + at], 1));
            }
        }
        assert_eq!(sessions_of_case(MIDNIGHT, &events, &clicks), 168);
    }

    /// Batches against one person at a time, on shapes the random worlds
    /// rarely have: persons with hundreds of sessions (more than a word of
    /// session bits, and enough to fill a batch before it has its persons),
    /// rows on every day, purchases in many sessions, persons taken out of
    /// the batch again, and whole persons done in between.
    #[test]
    fn batches_match_one_person_at_a_time() {
        let world = World::new(1_790_812_800_000 + 3_600_000, 21 * DAY, &["shoes", "books", "toys"], 6, &[]);
        let params = || {
            Params::new(
                world.window_start,
                world.first_day,
                world.day_count,
                world.campaigns,
                world.campaigns - 1,
                &world.category_names,
                TEST_PERSON_SHIFT,
            )
        };
        let new_out = || PassCOut {
            names: Vec::new(),
            days: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top_category: Vec::new(),
            campaign_days: vec![[0; 4]; world.campaigns * world.day_count],
        };
        let rounds = if cfg!(debug_assertions) { 6 } else { 60 };
        for seed in 0..rounds {
            let mut rng = Rng(1000 + seed);
            let tuning = Tuning { lookup_min: rng.pick(&[0, 64, 2048, usize::MAX]), ..Tuning::DEFAULT };
            let (mut batched, mut single) = (ScratchC::with(params(), tuning), ScratchC::with(params(), tuning));
            let (mut out_batched, mut out_single) = (new_out(), new_out());
            let mut serial = 0;
            for number in 0..rng.range(1, 150) {
                // A person: events in time order with purchases, and clicks.
                let wanted = rng.pick(&[1, 1, 2, 3, 5, 8, 20, 70, 300, 1500]);
                let mut stream: Vec<(u32, Option<u64>)> = Vec::new();
                let steps: &[u32] = rng.pick(&[
                    &[GAP + 1, GAP + 1, GAP + 2, 1, 60_000][..],
                    &[0, 1, 60_000, GAP - 1, GAP, GAP + 1, GAP + 1, 3 * GAP, DAY / 3, DAY, 2 * DAY][..],
                    &[0, 1, 1000, 60_000, GAP - 1, GAP][..],
                ]);
                let early = rng.pick(&[2, 50]);
                let mut ts = rng.below(u64::from(world.length) / early) as u32;
                let purchase_rate = rng.pick(&[2, 9, 50]);
                while stream.len() < wanted && ts < world.length {
                    let word = rng
                        .one_in(purchase_rate)
                        .then(|| rng.below(5000) | rng.below(world.category_names.len() as u64) << 32);
                    stream.push((ts, word));
                    ts += rng.pick(steps);
                }
                let mut clicks: Vec<[u64; 2]> = Vec::new();
                for _ in 0..rng.pick(&[0, 0, 1, 2, 4, 12, 40]) {
                    serial += 1;
                    let click_ts = rng.below(u64::from(world.length) + u64::from(WEEK));
                    clicks.push([click_ts, click_code(serial) | rng.below(world.campaigns as u64 - 1) << 42]);
                }
                let name = unmix(u64::from(number as u32));
                let events: Vec<u32> = stream.iter().map(|&(ts, _)| ts).collect();
                let purchases: Vec<[u64; 2]> =
                    stream.iter().filter_map(|&(ts, word)| word.map(|word| [u64::from(ts), word])).collect();
                single.one_person_in_order(&mut out_single, &world.exotic, name, &events, &purchases, &clicks);
                match rng.below(10) {
                    // Done whole, between the persons of the batch.
                    0 => batched.one_person_in_order(&mut out_batched, &world.exotic, name, &events, &purchases, &clicks),
                    // Begun in the batch with an event out of place, taken out again.
                    1 if stream.len() > 1 && stream[stream.len() - 1].0 > 0 => {
                        let mut broken = stream.clone();
                        let last = broken.len() - 1;
                        broken.insert(rng.range(1, last as u64) as usize, (broken[last].0, Some(77)));
                        broken[last + 1].0 -= 1;
                        assert!(!give_to_batch(&mut batched, &mut out_batched, &world.exotic, name, &broken, &clicks));
                        batched.one_person_in_order(&mut out_batched, &world.exotic, name, &events, &purchases, &clicks);
                    }
                    _ => assert!(give_to_batch(&mut batched, &mut out_batched, &world.exotic, name, &stream, &clicks)),
                }
                if rng.one_in(40) {
                    batched.finish_batch(&mut out_batched, &world.exotic);
                    batched.flush(&mut out_batched);
                }
            }
            batched.finish_batch(&mut out_batched, &world.exotic);
            batched.flush(&mut out_batched);
            single.flush(&mut out_single);
            assert!(tables_of(&out_batched) == tables_of(&out_single), "seed {seed}");
        }
    }

    /// The in-order pass against the sort path, for short and long persons
    /// and gaps on both sides of 30 minutes; and its refusal of anything
    /// that is not ascending.
    #[test]
    fn in_order_pass_matches_the_sort() {
        let params = Params::new(0, 0, 14, 3, 2, &[], TEST_PERSON_SHIFT);
        let tuning = Tuning { sort_max: 4096, ..Tuning::DEFAULT };
        let mut work = Work::new(&params, tuning);
        let mut rng = Rng(11);
        for round in 0..4000 {
            let count = if round % 4 == 0 { rng.range(1, 400) } else { rng.range(1, 70) } as usize;
            let mut ts = rng.below(1_000_000) as u32;
            let mut events = Vec::with_capacity(count);
            for _ in 0..count {
                events.push(ts);
                let step = rng.pick(&[0, 0, 1, 1000, 60_000, GAP - 1, GAP, GAP + 1, GAP + 2, 2 * GAP]);
                ts = (ts + step).min(TS_MASK as u32);
            }
            let sessions = sessions_in_order(&events, &mut work).expect("ascending");
            let got: Vec<(u32, u32)> = (0..sessions).map(|at| (work.start[at], work.count[at])).collect();
            let mut sorted = events.clone();
            let sessions = sessions_by_sort(&mut sorted, &mut work);
            let want: Vec<(u32, u32)> = (0..sessions).map(|at| (work.start[at], work.count[at])).collect();
            assert_eq!(got, want, "round {round}");
            // One timestamp out of place anywhere must be noticed.
            if count >= 2 {
                let at = rng.range(1, count as u64 - 1) as usize;
                let mut broken = events.clone();
                if broken[at - 1] > 0 {
                    broken[at] = broken[at - 1] - 1;
                    assert!(sessions_in_order(&broken, &mut work).is_none(), "round {round}");
                }
            }
        }
    }

    /// A network of compare-exchanges sorts every input if it sorts every
    /// input of zeros and ones.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_network_sorts_every_zero_one_input() {
        use std::arch::x86_64::*;
        for pattern in 0u32..1 << NETWORK_MAX {
            let input: [u32; NETWORK_MAX] = std::array::from_fn(|lane| pattern >> lane & 1);
            let mut output = [0u32; NETWORK_MAX];
            // SAFETY: the arrays hold two registers each; the crate is built with AVX2.
            unsafe {
                let (low, high) = wide::sort16(
                    _mm256_loadu_si256(input.as_ptr() as *const __m256i),
                    _mm256_loadu_si256(input.as_ptr().add(LANES) as *const __m256i),
                );
                _mm256_storeu_si256(output.as_mut_ptr() as *mut __m256i, low);
                _mm256_storeu_si256(output.as_mut_ptr().add(LANES) as *mut __m256i, high);
            }
            let zeros = NETWORK_MAX - pattern.count_ones() as usize;
            for (lane, &value) in output.iter().enumerate() {
                assert_eq!(value, u32::from(lane >= zeros), "pattern {pattern:#06x}");
            }
        }
    }

    /// The AVX2 kernels against plain code, on values at the ends of their ranges.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn kernels_match_plain_code() {
        let mut rng = Rng(7);
        let edge = |rng: &mut Rng| match rng.below(6) {
            0 => 0,
            1 => TS_MASK as u32,
            2 => GAP * rng.below(1193) as u32,
            3 => GAP * rng.below(1193) as u32 + GAP - 1,
            _ => rng.below(TS_MASK + 1) as u32,
        };
        for _ in 0..20_000 {
            let count = rng.range(1, NETWORK_MAX as u64) as usize;
            // Whatever follows the first `count` words must not matter.
            let mut events: [u32; NETWORK_MAX] = std::array::from_fn(|_| rng.next() as u32);
            let step = rng.pick(&[0, 1, GAP - 1, GAP, GAP + 1, 3 * GAP]);
            let mut ts = edge(&mut rng) % (TS_MASK as u32 - 16 * 3 * GAP);
            for event in &mut events[..count] {
                *event = ts;
                let jump = rng.below(2 * u64::from(GAP)) as u32;
                ts += rng.pick(&[0, step, step, jump]);
            }
            rng.shuffle(&mut events[..count]);
            let mut want = events[..count].to_vec();
            want.sort_unstable();
            let want_cuts = (1..count).fold(0u32, |cuts, at| cuts | u32::from(want[at] - want[at - 1] > GAP) << at);
            let mut sorted = [0u32; NETWORK_MAX];
            // SAFETY: `events` has `NETWORK_MAX` words.
            let cuts = unsafe { wide::sort_and_cut(events.as_ptr(), count, &mut sorted) };
            assert_eq!(&sorted[..count], &want[..]);
            assert!(sorted[count..].iter().all(|&pad| pad == u32::MAX));
            assert_eq!(cuts, want_cuts);

            // Bucket numbers, all different as a person's are.
            let mut buckets: [u32; NETWORK_MAX] = std::array::from_fn(|_| rng.next() as u32);
            let mut chosen = BTreeSet::new();
            while chosen.len() < count {
                let any = rng.below(BUCKETS as u64) as u32;
                chosen.insert(rng.pick(&[0, BUCKETS as u32 - 1, any]));
            }
            let mut picked: Vec<u32> = chosen.iter().copied().collect();
            rng.shuffle(&mut picked);
            buckets[..count].copy_from_slice(&picked);
            let mut order = [0u32; NETWORK_MAX];
            // SAFETY: `buckets` has `NETWORK_MAX` words.
            unsafe { wide::order_buckets(buckets.as_ptr(), count, &mut order) };
            for (rank, &bucket) in chosen.iter().enumerate() {
                assert_eq!(order[rank] >> 4, bucket);
                assert_eq!(buckets[(order[rank] & 15) as usize], bucket);
            }
            assert!(order[count..].iter().all(|&pad| pad == u32::MAX));

            // Reach over padded ascending starts.
            let blocks = rng.range(1, 8) as usize;
            let sessions = rng.range(LANES as u64 * (blocks as u64 - 1) + 1, (LANES * blocks) as u64) as usize;
            let mut starts: Vec<u32> = (0..sessions).map(|_| edge(&mut rng)).collect();
            starts.sort_unstable();
            starts.resize(LANES * blocks, u32::MAX);
            let (floor, ts) = (edge(&mut rng), edge(&mut rng));
            // SAFETY: `starts` has `LANES * blocks` words.
            let (below, up_to) = unsafe { wide::reach(starts.as_ptr(), blocks, floor, ts) };
            assert_eq!(below, starts[..sessions].iter().filter(|&&start| start < floor).count());
            assert_eq!(up_to, starts[..sessions].iter().filter(|&&start| start <= ts).count());
        }
    }
}

//! Kill test 2 for the movement of event records: how many streams should
//! the parsers write?
//!
//! Every scheme turns N records that arrive in random index order (plus 3%
//! second copies) into winner slots in index order ("smallest ingest wins")
//! and reads the winners in order. Parsing is left out.
//!
//! lists:  the engine today. Producers append to one block list per 2^14-index
//!         partition (about 1,200 streams at 20M); then each partition is
//!         placed in a slot table that stays in the cache and read.
//! two:    two levels. Producers append to one list per COARSE partition
//!         (2^18 indexes: about 77 streams at 20M). Then, per coarse
//!         partition, a thread counts its records per fine partition,
//!         scatters them into a scratch buffer it reuses (so that copy never
//!         leaves the cache hierarchy), and places and reads each fine
//!         partition as in "lists".
//! owned:  one stream per owner thread. Producers append to one list per
//!         owner (a contiguous range of indexes); each owner then writes its
//!         records straight into its part of one big slot array with plain
//!         stores, prefetching the slot a few records ahead, and reads it.
//!
//! usage: scatter2 RECORDS THREADS ROUNDS
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

const SHIFT: u32 = 14;
const COARSE: u32 = 4;
const BLOCK: usize = 255;

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `count` zeroed values of `T` in 2 MiB-aligned memory with a huge-page hint.
fn huge<T>(count: usize) -> &'static mut [T] {
    let bytes = (count * std::mem::size_of::<T>()).max(1);
    let layout = std::alloc::Layout::from_size_align(bytes.next_multiple_of(2 << 20), 2 << 20).unwrap();
    // SAFETY: the layout is not empty; zeroed bytes are valid for the integer
    // types this is used with; the memory is never freed.
    unsafe {
        let block = std::alloc::alloc_zeroed(layout);
        assert!(!block.is_null());
        #[cfg(target_os = "linux")]
        {
            extern "C" {
                fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
            }
            madvise(block, layout.size(), 14);
        }
        std::slice::from_raw_parts_mut(block as *mut T, count)
    }
}

/// The record of index `index`: ingest in bits 1..22 of word1, payload above.
#[inline(always)]
fn record(index: u64, copy: u64) -> [u64; 2] {
    let payload = index.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 24;
    [index, (payload << 22) | ((1000 + copy * 7) << 1)]
}

/// Block lists carved from one arena: what a producer thread fills.
struct Lists {
    arena: &'static mut [[u64; 2]],
    used: usize,
    cursor: Vec<u32>,
    room: Vec<u32>,
    blocks: Vec<Vec<u32>>,
}

impl Lists {
    fn new(records: usize, parts: usize) -> Self {
        Lists {
            arena: huge(records + parts * BLOCK),
            used: 0,
            cursor: vec![0; parts],
            room: vec![0; parts],
            blocks: (0..parts).map(|_| Vec::with_capacity(16)).collect(),
        }
    }

    #[inline(always)]
    fn push(&mut self, part: usize, value: [u64; 2]) {
        if self.room[part] == 0 {
            self.blocks[part].push(self.used as u32);
            self.cursor[part] = self.used as u32;
            self.room[part] = BLOCK as u32;
            self.used += BLOCK;
        }
        self.arena[self.cursor[part] as usize] = value;
        self.cursor[part] += 1;
        self.room[part] -= 1;
    }

    fn each(&self, part: usize, mut visit: impl FnMut([u64; 2])) {
        let blocks = &self.blocks[part];
        for (at, &start) in blocks.iter().enumerate() {
            let end = if at + 1 == blocks.len() { self.cursor[part] as usize } else { start as usize + BLOCK };
            for &pair in &self.arena[start as usize..end] {
                visit(pair);
            }
        }
    }
}

#[inline(always)]
fn place(slots: &mut [[u64; 2]], [word0, word1]: [u64; 2]) {
    let slot = &mut slots[(word0 & ((1 << SHIFT) - 1)) as usize];
    if slot[1] == 0 || (word1 & 0x3F_FFFF) < (slot[1] & 0x3F_FFFF) {
        *slot = [word0, word1];
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let records: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(20_000_000);
    let threads: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(8);
    let rounds: usize = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(3);
    let mut order: Vec<u32> = (0..records as u32).collect();
    let mut state = 12345u64;
    for at in (1..order.len()).rev() {
        order.swap(at, (splitmix(&mut state) % (at as u64 + 1)) as usize);
    }
    let copies: Vec<u32> = (0..records / 33).map(|_| (splitmix(&mut state) % records as u64) as u32).collect();
    let parts = (records >> SHIFT) + 1;
    let coarse_parts = (records >> (SHIFT + COARSE)) + 1;
    let share = |items: usize, thread: usize| (thread * items / threads, (thread + 1) * items / threads);
    let mine = (order.len() + copies.len()) / threads + 1;
    // Producers of every scheme run the same loop over the arrival order.
    let produce = |stream_parts: usize, part_of: &(dyn Fn(u64) -> usize + Sync)| -> Vec<Lists> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|thread| {
                    let (order, copies) = (&order, &copies);
                    scope.spawn(move || {
                        let mut lists = Lists::new(mine, stream_parts);
                        let (from, to) = share(order.len(), thread);
                        for &index in &order[from..to] {
                            lists.push(part_of(u64::from(index)), record(u64::from(index), 1));
                        }
                        let (from, to) = share(copies.len(), thread);
                        for &index in &copies[from..to] {
                            lists.push(part_of(u64::from(index)), record(u64::from(index), 0));
                        }
                        lists
                    })
                })
                .collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        })
    };
    let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1e3;
    for round in 0..rounds {
        // ---- lists
        let started = Instant::now();
        let lists = produce(parts, &|index| (index >> SHIFT) as usize);
        let lists_produced = started.elapsed();
        let next = AtomicUsize::new(0);
        let total = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let (lists, next, total) = (&lists, &next, &total);
                scope.spawn(move || {
                    let mut slots = vec![[0u64; 2]; 1 << SHIFT];
                    let mut sum = 0u64;
                    loop {
                        let part = next.fetch_add(1, Ordering::Relaxed);
                        if part >= parts {
                            break;
                        }
                        slots.fill([0, 0]);
                        for list in lists {
                            list.each(part, |pair| place(&mut slots, pair));
                        }
                        for slot in &slots {
                            sum = sum.wrapping_add(slot[1] >> 22);
                        }
                    }
                    total.fetch_add(sum, Ordering::Relaxed);
                });
            }
        });
        let lists_total = started.elapsed();
        let lists_sum = total.load(Ordering::Relaxed);
        drop(lists);

        // ---- two levels
        let started = Instant::now();
        let coarse = produce(coarse_parts, &|index| (index >> (SHIFT + COARSE)) as usize);
        let two_produced = started.elapsed();
        let next = AtomicUsize::new(0);
        let total = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let (coarse, next, total) = (&coarse, &next, &total);
                scope.spawn(move || {
                    let fine = 1usize << COARSE;
                    let mut slots = vec![[0u64; 2]; 1 << SHIFT];
                    let mut scratch: Vec<[u64; 2]> = Vec::new();
                    let mut sum = 0u64;
                    loop {
                        let part = next.fetch_add(1, Ordering::Relaxed);
                        if part >= coarse_parts {
                            break;
                        }
                        // Count per fine partition, then scatter into the scratch buffer.
                        let mut counts = [0usize; 1 << COARSE];
                        for list in coarse {
                            list.each(part, |pair| counts[((pair[0] >> SHIFT) as usize) & (fine - 1)] += 1);
                        }
                        let mut starts = [0usize; (1 << COARSE) + 1];
                        for at in 0..fine {
                            starts[at + 1] = starts[at] + counts[at];
                        }
                        scratch.clear();
                        scratch.resize(starts[fine], [0, 0]);
                        let mut cursors = starts;
                        for list in coarse {
                            list.each(part, |pair| {
                                let at = ((pair[0] >> SHIFT) as usize) & (fine - 1);
                                scratch[cursors[at]] = pair;
                                cursors[at] += 1;
                            });
                        }
                        for at in 0..fine {
                            if counts[at] == 0 {
                                continue;
                            }
                            slots.fill([0, 0]);
                            for &pair in &scratch[starts[at]..starts[at + 1]] {
                                place(&mut slots, pair);
                            }
                            for slot in &slots {
                                sum = sum.wrapping_add(slot[1] >> 22);
                            }
                        }
                    }
                    total.fetch_add(sum, Ordering::Relaxed);
                });
            }
        });
        let two_total = started.elapsed();
        let two_sum = total.load(Ordering::Relaxed);
        drop(coarse);

        // ---- one stream per owner, plain stores with prefetch
        let started = Instant::now();
        let owner_of = |index: u64| (index as usize * threads / records).min(threads - 1);
        let streams = produce(threads, &owner_of);
        let owned_produced = started.elapsed();
        let slots: &'static mut [[u64; 2]] = huge(records);
        let slots_at = slots.as_mut_ptr() as usize;
        let total = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for owner in 0..threads {
                let (streams, total) = (&streams, &total);
                scope.spawn(move || {
                    // SAFETY: an owner only touches the slots of its own range of indexes.
                    let slots = unsafe { std::slice::from_raw_parts_mut(slots_at as *mut [u64; 2], records) };
                    const AHEAD: usize = 24;
                    for stream in streams {
                        let blocks = &stream.blocks[owner];
                        for (at, &start) in blocks.iter().enumerate() {
                            let end =
                                if at + 1 == blocks.len() { stream.cursor[owner] as usize } else { start as usize + BLOCK };
                            let block = &stream.arena[start as usize..end];
                            for (position, &[word0, word1]) in block.iter().enumerate() {
                                if let Some(later) = block.get(position + AHEAD) {
                                    #[cfg(target_arch = "x86_64")]
                                    // SAFETY: a prefetch reads nothing, whatever the address.
                                    unsafe {
                                        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
                                        _mm_prefetch(slots.as_ptr().wrapping_add(later[0] as usize) as *const i8, _MM_HINT_T0);
                                    }
                                    let _ = later;
                                }
                                let slot = &mut slots[word0 as usize];
                                if slot[1] == 0 || (word1 & 0x3F_FFFF) < (slot[1] & 0x3F_FFFF) {
                                    *slot = [word0, word1];
                                }
                            }
                        }
                    }
                    // The owner's range is contiguous: indexes owner*records/threads .. (owner+1)*records/threads.
                    let low = (owner * records).div_ceil(threads);
                    let high = (((owner + 1) * records).div_ceil(threads)).min(records);
                    let mut sum = 0u64;
                    for slot in &slots[low..high] {
                        sum = sum.wrapping_add(slot[1] >> 22);
                    }
                    total.fetch_add(sum, Ordering::Relaxed);
                });
            }
        });
        let owned_total = started.elapsed();
        let owned_sum = total.load(Ordering::Relaxed);
        drop(streams);

        println!(
            "round {round}: lists produce {:.1} + rest {:.1} = {:.1} ms | two-level produce {:.1} + rest {:.1} = {:.1} ms | owned produce {:.1} + rest {:.1} = {:.1} ms | same winners: {}",
            ms(lists_produced),
            ms(lists_total - lists_produced),
            ms(lists_total),
            ms(two_produced),
            ms(two_total - two_produced),
            ms(two_total),
            ms(owned_produced),
            ms(owned_total - owned_produced),
            ms(owned_total),
            lists_sum == two_sum && lists_sum == owned_sum,
        );
    }
}

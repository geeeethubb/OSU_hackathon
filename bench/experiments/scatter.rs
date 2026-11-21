//! Kill test for "partition-owned winner construction".
//!
//! Both schemes turn N records that arrive in random index order (as the CSV
//! rows do) into winner slots in index order, with the dedup rule "smallest
//! ingest wins", and then read the winners in order. Parsing is left out:
//! only the movement of the records differs between the schemes.
//!
//! lists: what the engine does. Each producer thread appends a record to the
//!   block list of its index partition (2^14 indexes, blocks of 255 records
//!   carved from one arena per thread); afterwards the threads take
//!   partitions one by one, place the partition's records in a slot table that
//!   stays in the cache, and read the slots.
//! direct: the best case of an owner design: no queues, no batches, no
//!   hand-off. Each producer writes the record straight into its final slot
//!   in one array that covers every index (an atomic "smaller ingest wins"
//!   update), and afterwards the threads read the array in order.
//!
//! lists-wc: the lists scheme with write combining: a record first goes into
//!   a 64-byte buffer of its partition (the buffers of all partitions stay in
//!   the cache), and a full buffer is written to the list with non-temporal
//!   stores, so the list memory is never read before it is written.
//!
//! All big arrays ask for transparent huge pages on Linux, as the engine's
//! allocator does.
//!
//! usage: scatter RECORDS THREADS ROUNDS
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

const SHIFT: u32 = 14;
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
    // and atomic types this is used with; the memory is never freed.
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

struct Lists {
    arena: &'static mut [[u64; 2]],
    used: usize,
    /// Per partition: where the next record goes, and how many still fit in its block.
    cursor: Vec<u32>,
    room: Vec<u32>,
    /// Per partition: the start of each of its blocks, in order.
    blocks: Vec<Vec<u32>>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let records: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(20_000_000);
    let threads: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(8);
    let rounds: usize = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(3);
    // The arrival order: every index once, shuffled, plus 3% second copies.
    let mut order: Vec<u32> = (0..records as u32).collect();
    let mut state = 12345u64;
    for at in (1..order.len()).rev() {
        order.swap(at, (splitmix(&mut state) % (at as u64 + 1)) as usize);
    }
    let copies: Vec<u32> = (0..records / 33).map(|_| (splitmix(&mut state) % records as u64) as u32).collect();
    let parts = (records >> SHIFT) + 1;
    let share = |items: usize, thread: usize| (thread * items / threads, (thread + 1) * items / threads);
    for round in 0..rounds {
        // ---- lists
        let started = Instant::now();
        let lists: Vec<Lists> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|thread| {
                    let (order, copies) = (&order, &copies);
                    scope.spawn(move || {
                        let mine = (order.len() + copies.len()) / threads + 1;
                        let mut lists = Lists {
                            arena: huge(mine + parts * BLOCK),
                            used: 0,
                            cursor: vec![0; parts],
                            room: vec![0; parts],
                            blocks: (0..parts).map(|_| Vec::with_capacity(16)).collect(),
                        };
                        let mut emit = |index: u64, copy: u64| {
                            let part = (index >> SHIFT) as usize;
                            if lists.room[part] == 0 {
                                lists.blocks[part].push(lists.used as u32);
                                lists.cursor[part] = lists.used as u32;
                                lists.room[part] = BLOCK as u32;
                                lists.used += BLOCK;
                            }
                            lists.arena[lists.cursor[part] as usize] = record(index, copy);
                            lists.cursor[part] += 1;
                            lists.room[part] -= 1;
                        };
                        let (from, to) = share(order.len(), thread);
                        for &index in &order[from..to] {
                            emit(u64::from(index), 1);
                        }
                        let (from, to) = share(copies.len(), thread);
                        for &index in &copies[from..to] {
                            emit(u64::from(index), 0);
                        }
                        lists
                    })
                })
                .collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        });
        let produced = started.elapsed();
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
                            let blocks = &list.blocks[part];
                            for (at, &start) in blocks.iter().enumerate() {
                                let end = if at + 1 == blocks.len() { list.cursor[part] as usize } else { start as usize + BLOCK };
                                for &[word0, word1] in &list.arena[start as usize..end] {
                                    let slot = &mut slots[(word0 & ((1 << SHIFT) - 1)) as usize];
                                    if slot[1] == 0 || (word1 & 0x3F_FFFF) < (slot[1] & 0x3F_FFFF) {
                                        *slot = [word0, word1];
                                    }
                                }
                            }
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

        // ---- lists with write combining and non-temporal stores
        let started = Instant::now();
        const WC_BLOCK: usize = 256;
        let wc_lists: Vec<(Lists, Vec<[[u64; 2]; 4]>, Vec<u8>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|thread| {
                    let (order, copies) = (&order, &copies);
                    scope.spawn(move || {
                        let mine = (order.len() + copies.len()) / threads + 1;
                        let mut lists = Lists {
                            arena: huge(mine + parts * WC_BLOCK),
                            used: 0,
                            cursor: vec![0; parts],
                            room: vec![0; parts],
                            blocks: (0..parts).map(|_| Vec::with_capacity(16)).collect(),
                        };
                        let staging: &'static mut [[[u64; 2]; 4]] = huge(parts);
                        let mut fill = vec![0u8; parts];
                        let mut emit = |index: u64, copy: u64| {
                            let part = (index >> SHIFT) as usize;
                            let at = fill[part] as usize;
                            staging[part][at] = record(index, copy);
                            if at == 3 {
                                if lists.room[part] == 0 {
                                    lists.blocks[part].push(lists.used as u32);
                                    lists.cursor[part] = lists.used as u32;
                                    lists.room[part] = WC_BLOCK as u32;
                                    lists.used += WC_BLOCK;
                                }
                                // SAFETY: the arena is 2 MiB aligned and the cursor a multiple of
                                // four records, so the target is a whole 64-byte line of the arena.
                                unsafe {
                                    use std::arch::x86_64::*;
                                    let from = staging[part].as_ptr() as *const __m256i;
                                    let to = lists.arena.as_mut_ptr().add(lists.cursor[part] as usize) as *mut __m256i;
                                    _mm256_stream_si256(to, _mm256_load_si256(from));
                                    _mm256_stream_si256(to.add(1), _mm256_load_si256(from.add(1)));
                                }
                                lists.cursor[part] += 4;
                                lists.room[part] -= 4;
                                fill[part] = 0;
                            } else {
                                fill[part] = at as u8 + 1;
                            }
                        };
                        let (from, to) = share(order.len(), thread);
                        for &index in &order[from..to] {
                            emit(u64::from(index), 1);
                        }
                        let (from, to) = share(copies.len(), thread);
                        for &index in &copies[from..to] {
                            emit(u64::from(index), 0);
                        }
                        // SAFETY: plain instruction; orders the streamed lines before the readers.
                        unsafe { std::arch::x86_64::_mm_sfence() };
                        (lists, staging.to_vec(), fill)
                    })
                })
                .collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        });
        let wc_produced = started.elapsed();
        let next = AtomicUsize::new(0);
        let total = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let (wc_lists, next, total) = (&wc_lists, &next, &total);
                scope.spawn(move || {
                    let mut slots = vec![[0u64; 2]; 1 << SHIFT];
                    let mut sum = 0u64;
                    loop {
                        let part = next.fetch_add(1, Ordering::Relaxed);
                        if part >= parts {
                            break;
                        }
                        slots.fill([0, 0]);
                        let mut put = |[word0, word1]: [u64; 2]| {
                            let slot = &mut slots[(word0 & ((1 << SHIFT) - 1)) as usize];
                            if slot[1] == 0 || (word1 & 0x3F_FFFF) < (slot[1] & 0x3F_FFFF) {
                                *slot = [word0, word1];
                            }
                        };
                        for (list, staging, fill) in wc_lists {
                            let blocks = &list.blocks[part];
                            for (at, &start) in blocks.iter().enumerate() {
                                let end =
                                    if at + 1 == blocks.len() { list.cursor[part] as usize } else { start as usize + WC_BLOCK };
                                for &pair in &list.arena[start as usize..end] {
                                    put(pair);
                                }
                            }
                            // What was still waiting in the partition's buffer.
                            for &pair in &staging[part][..fill[part] as usize] {
                                put(pair);
                            }
                        }
                        for slot in &slots {
                            sum = sum.wrapping_add(slot[1] >> 22);
                        }
                    }
                    total.fetch_add(sum, Ordering::Relaxed);
                });
            }
        });
        let wc_total = started.elapsed();
        let wc_sum = total.load(Ordering::Relaxed);
        drop(wc_lists);

        // ---- direct: one word per slot, ingest above the payload so that
        // "smaller wins" is one atomic minimum. Zero means empty, so a record
        // is stored inverted and the maximum is kept.
        let started = Instant::now();
        let slots: &[AtomicU64] = huge(records);
        std::thread::scope(|scope| {
            for thread in 0..threads {
                let (order, copies) = (&order, &copies);
                scope.spawn(move || {
                    let put = |index: u64, copy: u64| {
                        let [_, word1] = record(index, copy);
                        let packed = ((word1 & 0x3F_FFFF) << 42) | (word1 >> 22);
                        slots[index as usize].fetch_max(!packed, Ordering::Relaxed);
                    };
                    let (from, to) = share(order.len(), thread);
                    for &index in &order[from..to] {
                        put(u64::from(index), 1);
                    }
                    let (from, to) = share(copies.len(), thread);
                    for &index in &copies[from..to] {
                        put(u64::from(index), 0);
                    }
                });
            }
        });
        let produced_direct = started.elapsed();
        let total = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for thread in 0..threads {
                let total = &total;
                scope.spawn(move || {
                    let (from, to) = share(slots.len(), thread);
                    let mut sum = 0u64;
                    for slot in &slots[from..to] {
                        let stored = slot.load(Ordering::Relaxed);
                        if stored != 0 {
                            sum = sum.wrapping_add(!stored & ((1 << 42) - 1));
                        }
                    }
                    total.fetch_add(sum, Ordering::Relaxed);
                });
            }
        });
        let direct_total = started.elapsed();
        let direct_sum = total.load(Ordering::Relaxed);
        let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1e3;
        println!(
            "round {round}: lists produce {:.1} + place and read {:.1} = {:.1} ms | lists-wc produce {:.1} + place and read {:.1} = {:.1} ms | direct produce {:.1} + read {:.1} = {:.1} ms | same winners: {}",
            ms(produced),
            ms(lists_total - produced),
            ms(lists_total),
            ms(wc_produced),
            ms(wc_total - wc_produced),
            ms(wc_total),
            ms(produced_direct),
            ms(direct_total - produced_direct),
            ms(direct_total),
            lists_sum == direct_sum && lists_sum == wc_sum,
        );
    }
}

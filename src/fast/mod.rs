//! The optimized engine.
//!
//! Shape of a run:
//! 1. Parse (parallel over file chunks): every event line becomes a compact
//!    record in a partition chosen by its event ID; clicks and links go to
//!    per-thread lists. Linked IDs are resolved on a side thread meanwhile.
//! 2. Pass B (parallel over event-ID partitions): pick each event's winning
//!    copy, drop invalid winners, map users to persons, re-partition by person.
//! 3. Pass C (parallel over person partitions): sessions, attribution, assists
//!    and the daily rows.
//! 4. Write both Parquet tables.
//!
//! Anything the compact records cannot represent exactly makes `run` return
//! `Ok(false)`, and the caller runs the reference pipeline instead.

pub mod arena;
pub mod codes;
pub mod io;
pub mod keys;
pub mod links;
pub mod dedup;
pub mod indexed;
pub mod parse;
pub mod sessions;
pub mod simd;

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::fs;
use std::hash::BuildHasherDefault;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use self::arena::{PartList, PartWriter};
use self::codes::Interner;
use self::io::Mapped;
use self::parse::ParseOut;
use self::dedup::{PassBOut, ScratchB};
use self::sessions::{PassCOut, ScratchC, NO_CATEGORY};
use crate::parquet::{self, ColumnType, GroupWriter, Text, Written};
use crate::reference::{list_csv, FastHasher};
use crate::util::{for_each_data_line, parse_manifest, split_fields, Timing, DAY_MS};

/// Fewest and most partition bits. With at least 10, the rest of a mixed
/// code (the local key) fits the 32 bits the records give it.
pub const MIN_PART_BITS: u32 = 10;
pub const MAX_PART_BITS: u32 = 13;
/// Records a partition should hold, so that a partition and the tables built
/// over it stay inside a core's private cache whatever the input size.
const RECORDS_PER_PART: u64 = 24_000;
/// Bytes of an average event row, to turn file sizes into row counts.
const EVENT_ROW_BYTES: u64 = 77;
/// Words per list block.
pub const EVENT_BLOCK: usize = 512;
pub const PERSON_BLOCK: usize = 512;
pub const CLICK_BLOCK: usize = 256;

const EVENT_CHUNK: usize = 2 << 20;
const LINK_CHUNK: usize = 1 << 20;
/// Blocks of parsed clicks (about 170 clicks each) per click-mapping task.
const CLICK_BATCH: usize = 64;

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// Read-only state shared by every worker.
pub struct Ctx {
    pub window_start: i64,
    pub window_end: i64,
    /// Ingest times are stored as seconds since this.
    pub ingest_base_seconds: i64,
    /// Epoch day of the window's first day, and how many days it touches.
    pub first_day: i64,
    pub day_count: usize,
    /// Categories by index; the last one is `unknown`.
    pub category_names: Vec<Box<[u8]>>,
    pub unknown_category: u32,
    pub product_category: FastMap<Box<[u8]>, u32>,
    /// Category by number for products written `p<number>` with no leading
    /// zero, up to the largest such number in the file; `unknown` in the gaps.
    pub category_by_product_number: Vec<u32>,
    /// Campaigns by index; the last one is `organic`.
    pub campaign_ids: Vec<Box<[u8]>>,
    pub campaign_channels: Vec<Box<[u8]>>,
    pub campaign_index: FastMap<Box<[u8]>, u32>,
    /// Campaign index by number for IDs written `c_<number>` with no leading
    /// zero; `u32::MAX` where there is no such campaign.
    pub campaign_by_number: Vec<u32>,
    pub organic: u32,
    pub event_ids: Interner,
    pub user_ids: Interner,
    pub click_ids: Interner,
    #[cfg(target_arch = "x86_64")]
    pub ts_tables: simd::TsTables,
    /// Use the AVX2 row parsers (`SS_FAST=0` turns them off, for A/B runs).
    pub fast_rows: bool,
    /// Partition bits for pass A/B (by event ID) and for pass B/C (by
    /// person). Chosen from the input size; `SS_PART_BITS`, or
    /// `SS_EVENT_PART_BITS` / `SS_PERSON_PART_BITS`, override them.
    pub event_part_bits: u32,
    pub person_part_bits: u32,
    /// The generator's scramble keys (see `keys`), when the data has them:
    /// the engine then runs in index mode (see `indexed`). Data without them,
    /// and every run with `SS_STRUCT=0`, goes through the general path.
    pub keys: Option<keys::Keys>,
    /// Run the general parser next to every fast-parsed row and compare (`SS_VERIFY=1`).
    pub verify: bool,
    /// Diagnostic runs of the parse stage (`SS_DIAG`), to see where event
    /// parse time goes. Each level stops a row earlier: 7 parses rows fully
    /// but does not store them; 6 stops after both timestamps; 5 after the ID
    /// codes; 4 after field bounds; 3 after quote validation and comma
    /// positions; 2 after the row masks. Nothing is written in any of them.
    pub diag: u32,
    fallback: AtomicBool,
}

impl Ctx {
    #[cold]
    pub fn request_fallback(&self) {
        self.fallback.store(true, Ordering::Relaxed);
    }

    /// Partitions of pass A/B, and the shift that takes a mixed code to one.
    pub fn event_parts(&self) -> usize {
        1 << self.event_part_bits
    }

    pub fn event_shift(&self) -> u32 {
        codes::CODE_BITS - self.event_part_bits
    }

    /// Partitions of pass B/C, and the shift that takes a mixed code to one.
    pub fn person_parts(&self) -> usize {
        1 << self.person_part_bits
    }

    pub fn person_shift(&self) -> u32 {
        codes::CODE_BITS - self.person_part_bits
    }

    /// Sets the partition counts for `event_bytes` of event files.
    fn size_partitions(&mut self, event_bytes: u64) {
        let rows = event_bytes / EVENT_ROW_BYTES;
        let mut bits = MIN_PART_BITS;
        while bits < MAX_PART_BITS && (rows >> bits) > RECORDS_PER_PART {
            bits += 1;
        }
        let choice = |name: &str, default: u32| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .map_or(default, |bits| bits.clamp(MIN_PART_BITS, MAX_PART_BITS))
        };
        let bits = choice("SS_PART_BITS", bits);
        self.event_part_bits = choice("SS_EVENT_PART_BITS", bits);
        self.person_part_bits = choice("SS_PERSON_PART_BITS", bits);
    }

    /// The window of the manifest, with empty lookup tables: `install`
    /// adds those. `None`: this input is not for the engine.
    fn load_header(data: &Path) -> std::io::Result<Option<Ctx>> {
        let manifest = fs::read(data.join("manifest.json"))?;
        let Some((window_start, window_end)) = parse_manifest(&manifest) else {
            return Ok(None);
        };
        // Timestamps are stored in 31 bits relative to the window (events) or
        // to seven days before it (clicks).
        let length = window_end - window_start;
        if window_start < 8 * DAY_MS || length <= 0 || length > 17 * DAY_MS {
            return Ok(None);
        }

        let first_day = window_start.div_euclid(DAY_MS);
        let day_count = ((window_end - 1).div_euclid(DAY_MS) - first_day + 1) as usize;
        Ok(Some(Ctx {
            window_start,
            window_end,
            ingest_base_seconds: window_start.div_euclid(1_000) - 4 * 86_400,
            first_day,
            day_count,
            category_names: Vec::new(),
            unknown_category: 0,
            product_category: HashMap::default(),
            category_by_product_number: Vec::new(),
            campaign_ids: Vec::new(),
            campaign_channels: Vec::new(),
            campaign_index: HashMap::default(),
            campaign_by_number: Vec::new(),
            organic: 0,
            event_ids: Interner::default(),
            user_ids: Interner::default(),
            click_ids: Interner::default(),
            #[cfg(target_arch = "x86_64")]
            ts_tables: simd::TsTables::new(window_start, window_start.div_euclid(1_000) - 4 * 86_400),
            event_part_bits: MIN_PART_BITS,
            person_part_bits: MIN_PART_BITS,
            keys: None,
            fast_rows: !std::env::var_os("SS_FAST").is_some_and(|value| value == "0"),
            verify: std::env::var_os("SS_VERIFY").is_some_and(|value| value == "1"),
            diag: std::env::var("SS_DIAG").ok().and_then(|value| value.parse().ok()).unwrap_or(0),
            fallback: AtomicBool::new(false),
        }))
    }

    /// The lookup tables of products.csv and campaigns.csv. `None`: more
    /// campaigns or categories than the engine's records have room for.
    fn load_tables(data: &Path) -> std::io::Result<Option<Tables>> {
        let products = fs::read(data.join("products.csv"))?;
        let mut category_names: Vec<Box<[u8]>> = Vec::new();
        let mut category_index: FastMap<Box<[u8]>, u32> = HashMap::default();
        // A product line is at least a dozen bytes: room for all of them at once.
        let mut product_category: FastMap<Box<[u8]>, u32> =
            HashMap::with_capacity_and_hasher(products.len() / 12, Default::default());
        for_each_data_line(&products, |line| {
            if let Some([product, category]) = split_fields::<2>(line) {
                if product.is_empty() {
                    return;
                }
                // Categories are few: look one up before making a key of it.
                let at = match category_index.get(category) {
                    Some(&at) => at,
                    None => {
                        let next = category_names.len() as u32;
                        category_index.insert(category.into(), next);
                        category_names.push(category.into());
                        next
                    }
                };
                product_category.entry(product.into()).or_insert(at);
            }
        });
        let unknown_category = category_names.len() as u32;
        category_names.push((&b"unknown"[..]).into());
        let mut category_by_product_number: Vec<u32> = Vec::new();
        for (id, &category) in &product_category {
            let digits = id.strip_prefix(b"p").unwrap_or(&[]);
            let canonical = (1..=7).contains(&digits.len()) && digits[0] != b'0' && digits.iter().all(u8::is_ascii_digit);
            if !canonical {
                continue;
            }
            let number = digits.iter().fold(0usize, |number, &digit| number * 10 + usize::from(digit - b'0'));
            // Bigger numbers than this stay in the hash map only.
            if number < (1 << 22) {
                if category_by_product_number.len() <= number {
                    category_by_product_number.resize(number + 1, unknown_category);
                }
                category_by_product_number[number] = category;
            }
        }

        let campaigns = fs::read(data.join("campaigns.csv"))?;
        let mut campaign_ids: Vec<Box<[u8]>> = Vec::new();
        let mut campaign_channels: Vec<Box<[u8]>> = Vec::new();
        let mut campaign_index: FastMap<Box<[u8]>, u32> = HashMap::default();
        for_each_data_line(&campaigns, |line| {
            if let Some([campaign, channel]) = split_fields::<2>(line) {
                if campaign.is_empty() || campaign_index.contains_key(campaign) {
                    return;
                }
                campaign_index.insert(campaign.into(), campaign_ids.len() as u32);
                campaign_ids.push(campaign.into());
                campaign_channels.push(channel.into());
            }
        });
        let mut campaign_by_number: Vec<u32> = Vec::new();
        for (index, id) in campaign_ids.iter().enumerate() {
            let digits = id.strip_prefix(b"c_").unwrap_or(&[]);
            let canonical = (1..=7).contains(&digits.len()) && digits[0] != b'0' && digits.iter().all(u8::is_ascii_digit);
            if !canonical {
                continue;
            }
            let number = digits.iter().fold(0usize, |number, &digit| number * 10 + usize::from(digit - b'0'));
            // Bigger numbers than this stay in the hash map only.
            if number < (1 << 20) {
                if campaign_by_number.len() <= number {
                    campaign_by_number.resize(number + 1, u32::MAX);
                }
                campaign_by_number[number] = index as u32;
            }
        }
        let organic = campaign_ids.len() as u32;
        campaign_ids.push((&b"organic"[..]).into());
        campaign_channels.push((&b"organic"[..]).into());
        if campaign_ids.len() >= (1 << 22) || category_names.len() >= (1 << 16) {
            return Ok(None);
        }
        Ok(Some(Tables {
            category_names,
            unknown_category,
            product_category,
            category_by_product_number,
            campaign_ids,
            campaign_channels,
            campaign_index,
            campaign_by_number,
            organic,
        }))
    }

    /// The manifest and the tables in one go, for callers that are not in a hurry.
    pub fn load(data: &Path) -> std::io::Result<Option<Ctx>> {
        let (Some(mut ctx), Some(tables)) = (Self::load_header(data)?, Self::load_tables(data)?) else {
            return Ok(None);
        };
        ctx.install(tables);
        Ok(Some(ctx))
    }

    fn install(&mut self, tables: Tables) {
        self.category_names = tables.category_names;
        self.unknown_category = tables.unknown_category;
        self.product_category = tables.product_category;
        self.category_by_product_number = tables.category_by_product_number;
        self.campaign_ids = tables.campaign_ids;
        self.campaign_channels = tables.campaign_channels;
        self.campaign_index = tables.campaign_index;
        self.campaign_by_number = tables.campaign_by_number;
        self.organic = tables.organic;
    }
}

/// What `Ctx::load_tables` reads; `Ctx::install` moves it into the context.
struct Tables {
    category_names: Vec<Box<[u8]>>,
    unknown_category: u32,
    product_category: FastMap<Box<[u8]>, u32>,
    category_by_product_number: Vec<u32>,
    campaign_ids: Vec<Box<[u8]>>,
    campaign_channels: Vec<Box<[u8]>>,
    campaign_index: FastMap<Box<[u8]>, u32>,
    campaign_by_number: Vec<u32>,
    organic: u32,
}

/// One mutable slot per worker thread.
struct Slots<T>(Vec<UnsafeCell<T>>);

// SAFETY: each slot is only touched by the one thread whose index it is while
// a parallel phase runs, and by the coordinating thread between phases.
unsafe impl<T: Send> Sync for Slots<T> {}

impl<T> Slots<T> {
    fn new(count: usize, mut make: impl FnMut() -> T) -> Self {
        Self((0..count).map(|_| UnsafeCell::new(make())).collect())
    }

    /// # Safety
    /// No two live references to the same slot.
    #[allow(clippy::mut_from_ref)]
    unsafe fn get(&self, at: usize) -> &mut T {
        &mut *self.0[at].get()
    }

    fn into_inner(self) -> Vec<T> {
        self.0.into_iter().map(UnsafeCell::into_inner).collect()
    }
}

/// Runs `work(thread_index)` on `threads` threads and waits for all of them.
fn parallel(threads: usize, work: impl Fn(usize) + Sync) {
    if threads <= 1 {
        work(0);
        return;
    }
    std::thread::scope(|scope| {
        for thread in 1..threads {
            let work = &work;
            scope.spawn(move || work(thread));
        }
        work(0);
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Events,
    Clicks,
    Links,
}

#[derive(Clone, Copy)]
struct Task {
    kind: Kind,
    file: usize,
    start: usize,
    end: usize,
}

fn chunk_tasks(kind: Kind, files: &[Mapped], chunk: usize, tasks: &mut Vec<Task>) {
    for (file, mapped) in files.iter().enumerate() {
        let mut start = 0;
        while start < mapped.len() {
            let end = (start + chunk).min(mapped.len());
            tasks.push(Task { kind, file, start, end });
            start = end;
        }
    }
}

fn map_all(directory: &Path) -> std::io::Result<Vec<Mapped>> {
    list_csv(directory)?.iter().map(|path| Mapped::open(path)).collect()
}

/// How one attempt of the engine ended.
enum Attempt {
    /// Both tables are written.
    Done,
    /// Nothing was written; this input is for the reference pipeline.
    Declined,
    /// Nothing was written: index mode found the events not laid out the way
    /// it needs. The general path takes over.
    NotIndexed,
}

/// Runs the optimized engine. `Ok(false)` means nothing was written and the
/// caller must use the reference pipeline for this input.
pub fn run(data: &Path, output: &Path, threads: usize) -> std::io::Result<bool> {
    // Index mode is tried first unless `SS_STRUCT=0`; it only engages when
    // the data has the generator's keys and layout.
    let indexed = !std::env::var_os("SS_STRUCT").is_some_and(|value| value == "0");
    let mut attempt = run_once(data, output, threads, indexed)?;
    if matches!(attempt, Attempt::NotIndexed) {
        attempt = run_once(data, output, threads, false)?;
    }
    Ok(matches!(attempt, Attempt::Done))
}

fn run_once(data: &Path, output: &Path, threads: usize, indexed: bool) -> std::io::Result<Attempt> {
    let mut timing = Timing::new();
    let threads = threads.max(1);
    let Some(mut ctx) = Ctx::load_header(data)? else {
        return Ok(Attempt::Declined);
    };
    // Three things overlap from here: the lookup tables are read (events and
    // clicks need them, links do not), the input files are mapped, and the
    // links are parsed, so that resolving them starts as early as it can and
    // overlaps the whole of the much longer event parse.
    let (tables, click_files, front) = std::thread::scope(|scope| {
        let tables = scope.spawn(|| Ctx::load_tables(data));
        let clicks = scope.spawn(|| map_all(&data.join("clicks")));
        let links = scope.spawn(|| map_all(&data.join("links")));
        let front = (|| -> std::io::Result<_> {
            let event_files = map_all(&data.join("events"))?;
            let event_bytes: u64 = event_files.iter().map(|file| file.len() as u64).sum();
            ctx.size_partitions(event_bytes);
            if indexed {
                // The start of the largest events file is enough to find the keys.
                if let Some(largest) = event_files.iter().max_by_key(|file| file.len()) {
                    let excerpt = &largest.bytes()[..largest.len().min(1 << 16)];
                    ctx.keys = keys::recover(excerpt, event_bytes);
                }
            }
            if timing.enabled() {
                eprintln!("SS_TIMING note=keys {:x?}", ctx.keys);
            }
            let link_files = links.join().unwrap()?;
            timing.stage("startup");
            let parse_out: Slots<ParseOut> = Slots::new(threads, || ParseOut::new(&ctx));
            let mut link_tasks = Vec::new();
            chunk_tasks(Kind::Links, &link_files, LINK_CHUNK, &mut link_tasks);
            let next = AtomicUsize::new(0);
            parallel(threads, |thread| {
                // SAFETY: slot `thread` belongs to this thread for the whole phase.
                let out = unsafe { parse_out.get(thread) };
                while let Some(task) = link_tasks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    parse::parse_links_chunk(&ctx, link_files[task.file].bytes(), task.start, task.end, out);
                }
            });
            let link_pairs: Vec<Vec<(u64, u64)>> = (0..threads)
                .map(|thread| {
                    // SAFETY: the phase above has ended; no other reference exists.
                    std::mem::take(&mut unsafe { parse_out.get(thread) }.links)
                })
                .collect();
            Ok((event_files, parse_out, link_pairs))
        })();
        (tables.join().unwrap(), clicks.join().unwrap(), front)
    });
    let (event_files, parse_out, link_pairs) = front?;
    let Some(tables) = tables? else {
        return Ok(Attempt::Declined);
    };
    ctx.install(tables);
    let click_files = click_files?;
    let ctx = ctx;
    timing.stage("links_parse");

    let mut tasks = Vec::new();
    chunk_tasks(Kind::Events, &event_files, EVENT_CHUNK, &mut tasks);
    chunk_tasks(Kind::Clicks, &click_files, EVENT_CHUNK, &mut tasks);
    if std::env::var_os("SS_PRETOUCH").is_some_and(|value| value == "1") {
        // Diagnostic only: fault every input page in first, so the parse stage
        // that follows shows parsing without first-touch costs.
        let next = AtomicUsize::new(0);
        let checksum = AtomicU64::new(0);
        parallel(threads, |_| {
            let mut sum = 0u64;
            while let Some(task) = tasks.get(next.fetch_add(1, Ordering::Relaxed)) {
                let files = if task.kind == Kind::Events { &event_files } else { &click_files };
                let bytes = &files[task.file].bytes()[task.start..task.end];
                let full = std::env::var_os("SS_PRETOUCH_FULL").is_some();
                if full {
                    for chunk in bytes.chunks_exact(8) {
                        sum = sum.wrapping_add(u64::from_le_bytes(chunk.try_into().unwrap()));
                    }
                } else {
                    for at in (0..bytes.len()).step_by(4096) {
                        sum = sum.wrapping_add(u64::from(bytes[at]));
                    }
                }
            }
            checksum.fetch_add(sum, Ordering::Relaxed);
        });
        timing.detail("pretouch_checksum", (checksum.load(Ordering::Relaxed) % 1000) as f64);
        timing.stage("pretouch");
    }
    let next = AtomicUsize::new(0);
    let measure = timing.enabled();
    // Summed worker time per kind of task, in nanoseconds (diagnostics only).
    let event_nanos = AtomicU64::new(0);
    let click_nanos = AtomicU64::new(0);
    let link_build_nanos = AtomicU64::new(0);
    let (link_table, summary_thread) = std::thread::scope(|scope| {
        let builder = scope.spawn(|| {
            let started = Instant::now();
            // Index mode sorts the link components by user index: a
            // component inside one user index is simple.
            let table = match &ctx.keys {
                Some(keys) => {
                    links::build_grouped(&link_pairs, &ctx.user_ids, &|code| keys.user_index_of_code(code))
                }
                None => links::build(&link_pairs, &ctx.user_ids),
            };
            link_build_nanos.store(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            let table = std::sync::Arc::new(table);
            // Index mode: what the table says per user index, flat. Only the
            // index pass reads it, so it gets a thread of its own that may run
            // on after the parse has ended: with eight parse threads the
            // builder would otherwise be the last to finish.
            let summary = ctx.keys.map(|keys| {
                let table = std::sync::Arc::clone(&table);
                std::thread::spawn(move || {
                    let started = Instant::now();
                    let summary = indexed::Summary::build(&keys, &table);
                    (summary, started.elapsed())
                })
            });
            (table, summary)
        });
        parallel(threads, |thread| {
            // SAFETY: slot `thread` belongs to this thread for the whole phase.
            let out = unsafe { parse_out.get(thread) };
            let (mut events_spent, mut clicks_spent) = (0u64, 0u64);
            while let Some(task) = tasks.get(next.fetch_add(1, Ordering::Relaxed)) {
                let started = measure.then(Instant::now);
                if task.kind == Kind::Events {
                    parse::parse_events_chunk(&ctx, event_files[task.file].bytes(), task.start, task.end, out);
                } else {
                    parse::parse_clicks_chunk(&ctx, click_files[task.file].bytes(), task.start, task.end, out);
                }
                if let Some(started) = started {
                    let spent = started.elapsed().as_nanos() as u64;
                    if task.kind == Kind::Events {
                        events_spent += spent;
                    } else {
                        clicks_spent += spent;
                    }
                }
            }
            event_nanos.fetch_add(events_spent, Ordering::Relaxed);
            click_nanos.fetch_add(clicks_spent, Ordering::Relaxed);
        });
        timing.detail("parse_workers_done", timing.since_last());
        builder.join().unwrap()
    });
    let link_table: &links::LinkTable = &link_table;
    timing.detail("cpu_parse_events", event_nanos.load(Ordering::Relaxed) as f64 / 1e6);
    timing.detail("cpu_parse_clicks", click_nanos.load(Ordering::Relaxed) as f64 / 1e6);
    timing.detail("links_build", link_build_nanos.load(Ordering::Relaxed) as f64 / 1e6);
    timing.stage("parse");
    if ctx.diag != 0 {
        eprintln!("SS_DIAG={}: parse-only diagnostic, no output written", ctx.diag);
        timing.total();
        std::process::exit(3);
    }
    if ctx.fallback.load(Ordering::Relaxed) {
        return Ok(Attempt::Declined);
    }

    // Pass B.
    let mut parse_out = parse_out.into_inner();
    let event_lists: Vec<Vec<PartList>> =
        parse_out.iter_mut().map(|out| out.events.iter_mut().map(|writer| writer.finish()).collect()).collect();
    // Index mode: per worker, one list per index partition.
    let indexed_lists: Vec<Vec<PartList>> =
        parse_out.iter_mut().map(|out| out.indexed.iter_mut().map(|writer| writer.finish()).collect()).collect();
    let click_lists: Vec<PartList> = parse_out.iter_mut().map(|out| out.clicks.finish()).collect();
    // Index mode: per worker, the clicks of generated user IDs by user range.
    let user_click_lists: Vec<Vec<PartList>> =
        parse_out.iter_mut().map(|out| out.user_clicks.iter_mut().map(|writer| writer.finish()).collect()).collect();
    let click_blocks: Vec<&[u64]> = click_lists.iter().flat_map(PartList::blocks).collect();
    let click_batches: Vec<&[&[u64]]> = click_blocks.chunks(CLICK_BATCH).collect();
    let b_out: Slots<PassBOut> = Slots::new(threads, || PassBOut::new(&ctx));
    let event_parts = ctx.event_parts();
    let next = AtomicUsize::new(0);
    let winners_nanos = AtomicU64::new(0);
    let forward_nanos = AtomicU64::new(0);
    let click_map_nanos = AtomicU64::new(0);
    parallel(threads, |thread| {
        // SAFETY: slot `thread` belongs to this thread for the whole phase.
        let out = unsafe { b_out.get(thread) };
        let mut scratch = ScratchB::default();
        scratch.measure = measure;
        let mut lists: Vec<PartList> = Vec::with_capacity(event_lists.len());
        let mut clicks_spent = 0u64;
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            if at < event_parts {
                lists.clear();
                lists.extend(event_lists.iter().map(|per_thread| per_thread[at]));
                dedup::dedup_partition(link_table, &lists, out, &mut scratch);
            } else if let Some(batch) = click_batches.get(at - event_parts) {
                let started = measure.then(Instant::now);
                dedup::map_clicks(link_table, batch, out);
                if let Some(started) = started {
                    clicks_spent += started.elapsed().as_nanos() as u64;
                }
            } else {
                break;
            }
        }
        winners_nanos.fetch_add(scratch.winners_nanos, Ordering::Relaxed);
        forward_nanos.fetch_add(scratch.forward_nanos, Ordering::Relaxed);
        click_map_nanos.fetch_add(clicks_spent, Ordering::Relaxed);
    });
    timing.detail("cpu_dedup_winners", winners_nanos.load(Ordering::Relaxed) as f64 / 1e6);
    timing.detail("cpu_dedup_forward", forward_nanos.load(Ordering::Relaxed) as f64 / 1e6);
    timing.detail("cpu_click_map", click_map_nanos.load(Ordering::Relaxed) as f64 / 1e6);
    // Index mode: the click index, and the few events and clicks that are
    // already known to belong to the general path.
    let index_state = match &ctx.keys {
        Some(keys) => {
            // The clicks of generated user IDs, grouped by user index.
            // Counting walks every block of every list: all threads share it.
            let ranges = user_click_lists.first().map_or(0, Vec::len);
            let counts: Vec<AtomicU32> = (0..ranges).map(|_| AtomicU32::new(0)).collect();
            let next = AtomicUsize::new(0);
            parallel(threads, |_| loop {
                let from = next.fetch_add(64, Ordering::Relaxed);
                if from >= ranges {
                    break;
                }
                for range in from..(from + 64).min(ranges) {
                    let lists = user_click_lists.iter().map(|per_thread| per_thread[range]);
                    counts[range].store(indexed::ClickIndexBuilder::count(lists), Ordering::Relaxed);
                }
            });
            let counts: Vec<u32> = counts.into_iter().map(AtomicU32::into_inner).collect();
            let (builder, target) = indexed::ClickIndexBuilder::new(&counts);
            let ranges = builder.ranges();
            let next = AtomicUsize::new(0);
            parallel(threads, |_| {
                let mut lists: Vec<PartList> = Vec::with_capacity(user_click_lists.len());
                loop {
                    let range = next.fetch_add(1, Ordering::Relaxed);
                    if range >= ranges {
                        break;
                    }
                    lists.clear();
                    lists.extend(user_click_lists.iter().map(|per_thread| per_thread[range]));
                    builder.fill(range, &lists, target);
                }
            });
            let click_index = builder.finish();
            timing.detail("click_index_built", timing.since_last());
            // What the general dedup pass let through are the events whose
            // own ID is not a generated one. Take its lists back and sort
            // them: most belong to a simple person and join that user's run.
            let odd_lists: Vec<Vec<PartList>> = (0..threads)
                .map(|thread| {
                    // SAFETY: no parallel phase is running; no other reference exists.
                    let out = unsafe { b_out.get(thread) };
                    out.events.iter_mut().map(|writer| std::mem::replace(writer, PartWriter::EMPTY).finish()).collect()
                })
                .collect();
            // SAFETY: as above.
            let out = unsafe { b_out.get(0) };
            let extras = indexed::Extras::collect(keys, link_table, &odd_lists, out);
            indexed::forward_complex_clicks(keys, link_table, &click_index, out);
            Some((click_index, extras))
        }
        None => None,
    };
    timing.stage("dedup");

    let click_exotic = ctx.click_ids.snapshot();
    let user_exotic = ctx.user_ids.snapshot();
    let c_out: Slots<PassCOut> = Slots::new(threads, || PassCOut::new(&ctx));
    // The session kernels the index pass used, for the general pass after it.
    let kernels: Slots<Option<ScratchC>> = Slots::new(threads, || None);

    // Index mode: the winners of every index partition, in index order.
    let link_summary = summary_thread.map(|thread| thread.join().unwrap());
    if let Some((_, spent)) = &link_summary {
        timing.detail("links_summary", spent.as_secs_f64() * 1e3);
        timing.detail("links_summary_joined", timing.since_last());
    }
    if let (Some(keys), Some((summary, _)), Some((click_index, extras))) = (&ctx.keys, &link_summary, &index_state) {
        let parts = parse::index_parts(keys);
        if std::env::var_os("SS_STATS").is_some() {
            let odd: usize = parse_out.iter().map(|out| out.odd_event_users.len()).sum();
            eprintln!("SS_STATS odd_event_users={odd} extras={} click_index={}", extras.len(), click_index.len());
            indexed::stats(keys, link_table, &indexed_lists, parts);
        }
        let shared = indexed::Shared {
            keys,
            links: link_table,
            summary,
            clicks: click_index,
            extras,
            click_exotic: &click_exotic,
            parts,
            threads,
        };
        let runners: Slots<indexed::Runner> = Slots::new(threads, || {
            let mut runner = indexed::Runner::new(ScratchC::new(&ctx), keys.index_shift);
            runner.measure = measure;
            runner
        });
        let next = AtomicUsize::new(0);
        parallel(threads, |thread| {
            // SAFETY: slot `thread` of each belongs to this thread for the whole phase.
            let (b, c, runner) = unsafe { (b_out.get(thread), c_out.get(thread), runners.get(thread)) };
            let mut lists: Vec<PartList> = Vec::with_capacity(indexed_lists.len());
            loop {
                let part = next.fetch_add(1, Ordering::Relaxed);
                if part >= parts {
                    break;
                }
                lists.clear();
                lists.extend(indexed_lists.iter().map(|per_thread| per_thread[part]));
                runner.partition(&shared, part, &lists, b, c);
            }
        });
        timing.detail("index_partitions_done", timing.since_last());
        // Runs that touch a partition's edge, joined across partitions. This
        // is also where the layout check ends: user indexes must never
        // decrease along the index order (`SS_FORCE_RESTART` pretends they did).
        let mut edges = Vec::new();
        let mut violated = std::env::var_os("SS_FORCE_RESTART").is_some();
        for thread in 0..threads {
            // SAFETY: the phase above has ended; no other reference exists.
            let runner = unsafe { runners.get(thread) };
            edges.append(&mut runner.edges);
            violated |= runner.violated;
        }
        let Some(chains) = indexed::chains(edges).filter(|_| !violated) else {
            if timing.enabled() {
                eprintln!("SS_TIMING note=index layout check failed, starting over on the general path");
            }
            return Ok(Attempt::NotIndexed);
        };
        timing.detail("index_chains_joined", timing.since_last());
        let next = AtomicUsize::new(0);
        parallel(threads, |thread| {
            // SAFETY: slot `thread` of each belongs to this thread for the whole phase.
            let (b, c, runner) = unsafe { (b_out.get(thread), c_out.get(thread), runners.get(thread)) };
            let mut pieces: Vec<&[indexed::Record]> = Vec::new();
            while let Some((user, records)) = chains.get(next.fetch_add(1, Ordering::Relaxed)) {
                pieces.clear();
                pieces.extend(records.iter().map(Vec::as_slice));
                runner.process_run(&shared, b, c, *user, &pieces);
            }
            runner.flush(c);
        });
        {
            // SAFETY: no parallel phase is running; no other reference exists.
            let (b, c, runner) = unsafe { (b_out.get(0), c_out.get(0), runners.get(0)) };
            indexed::leftover_extras(&shared, runner, b, c);
        }
        let runners = runners.into_inner();
        timing.detail("index_chains_done", timing.since_last());
        if timing.enabled() {
            let sum = |count: fn(&indexed::Runner) -> u64| runners.iter().map(count).sum::<u64>();
            timing.detail("cpu_index_place", sum(|runner| runner.place_nanos) as f64 / 1e6);
            timing.detail("cpu_index_kernel", sum(|runner| runner.kernel_nanos) as f64 / 1e6);
            timing.detail("cpu_index_partitions", sum(|runner| runner.total_nanos) as f64 / 1e6);
            eprintln!(
                "SS_TIMING note=index inline_persons={} inline_events={} general_events={} chains={} extras={} click_index={}",
                sum(|runner| runner.inline_persons),
                sum(|runner| runner.inline_events),
                sum(|runner| runner.general_events),
                chains.len(),
                extras.len(),
                click_index.len(),
            );
        }
        for (thread, runner) in runners.into_iter().enumerate() {
            // SAFETY: no parallel phase is running; no other reference exists.
            *unsafe { kernels.get(thread) } = Some(runner.into_kernel());
        }
        timing.stage("indexed");
    }

    // Pass C.
    let mut b_out = b_out.into_inner();
    let person_events: Vec<Vec<PartList>> =
        b_out.iter_mut().map(|out| out.events.iter_mut().map(|writer| writer.finish()).collect()).collect();
    let person_clicks: Vec<Vec<PartList>> =
        b_out.iter_mut().map(|out| out.clicks.iter_mut().map(|writer| writer.finish()).collect()).collect();
    let next = AtomicUsize::new(0);
    parallel(threads, |thread| {
        // SAFETY: slot `thread` of each belongs to this thread for the whole phase.
        let (out, kernel) = unsafe { (c_out.get(thread), kernels.get(thread)) };
        let mut scratch = match kernel.take() {
            Some(mut kernel) => {
                // The index pass has sized the output columns already.
                kernel.reserve_rows(out, 0);
                kernel
            }
            None => ScratchC::new(&ctx),
        };
        let mut events: Vec<PartList> = Vec::with_capacity(person_events.len());
        let mut clicks: Vec<PartList> = Vec::with_capacity(person_clicks.len());
        loop {
            let part = next.fetch_add(1, Ordering::Relaxed);
            if part >= ctx.person_parts() {
                break;
            }
            events.clear();
            events.extend(person_events.iter().map(|per_thread| per_thread[part]));
            clicks.clear();
            clicks.extend(person_clicks.iter().map(|per_thread| per_thread[part]));
            sessions::sessions_partition(&ctx, part, &events, &clicks, &click_exotic, out, &mut scratch);
        }
    });
    timing.stage("sessions");

    // Output: one user_daily file per thread, one campaign_daily file.
    let c_out = c_out.into_inner();
    write_tables(&ctx, output, threads, &c_out, &user_exotic)?;
    timing.stage("output");
    timing.total();
    Ok(Attempt::Done)
}

/// Rows per row group of user_daily: small enough that a row group is
/// encoded in a buffer that stays in the cache, large enough that page
/// headers and the footer stay negligible.
const GROUP_ROWS: usize = parquet::MAX_GROUP_ROWS;

/// The columns written as GZIP pages when `SS_GZIP=1`: the ones whose bytes
/// an entropy code shrinks (IDs, dictionary indices of skewed counts, sums).
/// Off by default: it makes the files about 40% smaller for more CPU time in
/// the output stage.
const USER_DAILY_GZIP: [&str; 4] = ["user_id", "sessions", "events", "revenue_cents"];
const CAMPAIGN_DAILY_GZIP: [&str; 6] =
    ["campaign_id", "channel", "sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions"];

/// The columns of a table to compress: `all` when `SS_GZIP=1`, else none.
fn gzip_columns<'a>(all: &'a [&'a str]) -> &'a [&'a str] {
    if std::env::var_os("SS_GZIP").is_some_and(|value| value == "1") {
        all
    } else {
        &[]
    }
}

/// Two characters for every pair of base-37 digits, first digit in the low
/// byte; digit 0 ("no character") is a zero byte.
static DIGIT_PAIRS: [u16; 1369] = digit_pairs();

const fn digit_pairs() -> [u16; 1369] {
    const fn character(digit: usize) -> u16 {
        if digit == 0 {
            0
        } else if digit <= 10 {
            (b'0' as usize + digit - 1) as u16
        } else {
            (b'a' as usize + digit - 11) as u16
        }
    }
    let mut table = [0u16; 1369];
    let mut at = 0;
    while at < 1369 {
        table[at] = character(at / 37) | (character(at % 37) << 8);
        at += 1;
    }
    table
}

/// What `codes::decode_pattern` appends after the prefix, without a division
/// by a variable: the characters (first one in the lowest byte) and how many
/// there are.
#[inline]
fn pattern_characters(code: u64) -> (u64, usize) {
    debug_assert!(code < codes::EXOTIC_BASE);
    let high = code / codes::P37[4];
    let low = code % codes::P37[4];
    let pair = |digits: u64| u64::from(DIGIT_PAIRS[digits as usize]);
    let characters = pair(high / 1369) | (pair(high % 1369) << 16) | (pair(low / 1369) << 32) | (pair(low % 1369) << 48);
    // The ID ends at its first zero digit: find the first zero byte.
    let zero = characters.wrapping_sub(0x0101_0101_0101_0101) & !characters & 0x8080_8080_8080_8080;
    (characters, (zero.trailing_zeros() / 8) as usize)
}

/// The user ID behind a name code, for `GroupWriter::runs`.
#[inline]
fn user_name<'a>(code: u64, slot: &mut [u8; parquet::SHORT_TEXT], exotic: &'a [Box<[u8]>]) -> Text<'a> {
    if code >= codes::EXOTIC_BASE {
        return Text::Long(&exotic[(code - codes::EXOTIC_BASE) as usize]);
    }
    let (characters, count) = pattern_characters(code);
    slot[0] = b'u';
    slot[1..9].copy_from_slice(&characters.to_le_bytes());
    Text::Short(1 + count)
}

/// What the output stage did, summed over its threads; times in nanoseconds.
/// Printed as `SS_TIMING` details, so that a run shows where the stage's
/// time goes on the machine at hand.
#[derive(Default)]
struct OutputStats {
    /// Creating the user_daily files (with the writers' scratch memory).
    create: AtomicU64,
    /// Encoding and writing the user_daily row groups, footers included.
    rows: AtomicU64,
    /// Of `rows`, the time inside write calls.
    write: AtomicU64,
    /// The campaign table, all of it.
    campaign: AtomicU64,
    user_daily_bytes: AtomicU64,
    campaign_daily_bytes: AtomicU64,
    row_groups: AtomicU64,
}

/// Writes both tables. The threads share one list of work: the campaign
/// table, then every worker's user_daily rows in row-group-sized pieces. A
/// thread adds the pieces it takes to its own file, so the files come out
/// about equally large and all threads finish together, whatever the split
/// of rows between the workers of pass C was.
fn write_tables(
    ctx: &Ctx,
    output: &Path,
    threads: usize,
    workers: &[PassCOut],
    user_exotic: &[Box<[u8]>],
) -> std::io::Result<()> {
    let timing = Timing::new();
    let started = Instant::now();
    fs::create_dir_all(output.join("user_daily"))?;
    fs::create_dir_all(output.join("campaign_daily"))?;
    timing.detail("output_mkdir", started.elapsed().as_secs_f64() * 1e3);
    let stats = OutputStats::default();
    let mut pieces: Vec<(&PassCOut, usize, usize)> = Vec::new();
    for worker in workers {
        let mut start = 0;
        while start < worker.names.len() {
            let end = (start + GROUP_ROWS).min(worker.names.len());
            pieces.push((worker, start, end));
            start = end;
        }
    }
    let gzip = gzip_columns(&USER_DAILY_GZIP);
    // Task 0 is the campaign table, task `n + 1` is piece `n`.
    let write = |integer: ColumnType, first_task: usize| -> std::io::Result<bool> {
        let fields = [
            ("user_id", ColumnType::Str),
            ("day", ColumnType::Date),
            ("sessions", integer),
            ("events", integer),
            ("revenue_cents", integer),
            ("top_category", ColumnType::OptStr),
        ];
        let next = AtomicUsize::new(first_task);
        let too_narrow = AtomicBool::new(false);
        let failure: std::sync::Mutex<Option<std::io::Error>> = std::sync::Mutex::new(None);
        parallel(threads, |thread| {
            let work = || -> std::io::Result<()> {
                let path = output.join("user_daily").join(format!("part-{thread:03}.parquet"));
                let create = || -> std::io::Result<GroupWriter> {
                    let started = Instant::now();
                    let writer = GroupWriter::create(&path, &fields)?.gzip(gzip);
                    stats.create.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    Ok(writer)
                };
                // Thread 0 always writes a file, so the folder holds a table
                // even with no rows; so does every thread of a second
                // attempt, to replace what the first one left behind.
                let mut writer = None;
                if thread == 0 || integer == ColumnType::Int64 {
                    writer = Some(create()?);
                }
                let mut spent = 0;
                while !too_narrow.load(Ordering::Relaxed) {
                    let task = next.fetch_add(1, Ordering::Relaxed);
                    if task == 0 {
                        let started = Instant::now();
                        let written = write_campaign_daily(ctx, output, workers)?;
                        stats.campaign.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        stats.campaign_daily_bytes.fetch_add(written.bytes, Ordering::Relaxed);
                        continue;
                    }
                    let Some(&(rows, start, end)) = pieces.get(task - 1) else {
                        break;
                    };
                    let writer = match &mut writer {
                        Some(writer) => writer,
                        None => writer.insert(create()?),
                    };
                    let started = Instant::now();
                    if !write_user_daily(ctx, writer, rows, start, end, user_exotic)? {
                        too_narrow.store(true, Ordering::Relaxed);
                    }
                    spent += started.elapsed().as_nanos() as u64;
                }
                if let Some(writer) = writer {
                    if !too_narrow.load(Ordering::Relaxed) {
                        let started = Instant::now();
                        let written = writer.finish()?;
                        spent += started.elapsed().as_nanos() as u64;
                        stats.write.fetch_add(written.write_nanos, Ordering::Relaxed);
                        stats.user_daily_bytes.fetch_add(written.bytes, Ordering::Relaxed);
                        stats.row_groups.fetch_add(written.row_groups as u64, Ordering::Relaxed);
                    }
                }
                stats.rows.fetch_add(spent, Ordering::Relaxed);
                Ok(())
            };
            if let Err(error) = work() {
                failure.lock().unwrap().get_or_insert(error);
            }
        });
        match failure.into_inner().unwrap() {
            Some(error) => Err(error),
            None => Ok(!too_narrow.load(Ordering::Relaxed)),
        }
    };
    // The counts and sums are INT32 unless one of them does not fit; then
    // every user_daily file is written again with INT64 columns.
    if !write(ColumnType::Int32, 0)? {
        write(ColumnType::Int64, 1)?;
    }
    if timing.enabled() {
        let ms = |nanos: &AtomicU64| nanos.load(Ordering::Relaxed) as f64 / 1e6;
        timing.detail("cpu_output_create", ms(&stats.create));
        timing.detail("cpu_output_encode", ms(&stats.rows) - ms(&stats.write));
        timing.detail("cpu_output_write", ms(&stats.write));
        timing.detail("cpu_output_campaign", ms(&stats.campaign));
        eprintln!(
            "SS_OUTPUT user_daily_bytes={} user_daily_row_groups={} campaign_daily_bytes={}",
            stats.user_daily_bytes.load(Ordering::Relaxed),
            stats.row_groups.load(Ordering::Relaxed),
            stats.campaign_daily_bytes.load(Ordering::Relaxed),
        );
    }
    Ok(())
}

/// Adds the user_daily rows `start..end` of one worker to a file as one row
/// group. `Ok(false)`: the file has INT32 columns and a value does not fit.
fn write_user_daily(
    ctx: &Ctx,
    writer: &mut GroupWriter,
    rows: &PassCOut,
    start: usize,
    end: usize,
    user_exotic: &[Box<[u8]>],
) -> std::io::Result<bool> {
    let top_category = &rows.top_category[start..end];
    // Days with purchases go last in their row group: the other rows then
    // share one run of zero revenue and one run of null categories.
    writer.begin(top_category, |&category| category != NO_CATEGORY)?;
    writer.runs(&rows.names[start..end], |code, slot| user_name(code, slot, user_exotic))?;
    writer.dates(&rows.days[start..end])?;
    if !writer.ints(&rows.sessions[start..end])?
        || !writer.ints(&rows.events[start..end])?
        || !writer.sparse_ints(&rows.revenue[start..end])?
    {
        return Ok(false);
    }
    writer.strings(&ctx.category_names, top_category, NO_CATEGORY)?;
    Ok(true)
}

/// Rows per row group of campaign_daily. The table is small; small groups
/// keep the buffer its rows are gathered in small too.
const CAMPAIGN_ROWS: usize = 1 << 12;

/// campaign_daily while its rows are produced: the file, and the rows
/// gathered for its next row group, column by column.
struct CampaignTable<'a> {
    ctx: &'a Ctx,
    folder: std::path::PathBuf,
    gzip: &'a [&'a str],
    /// The fields of a file with INT64 columns.
    wide: &'a [(&'a str, ColumnType)],
    writer: GroupWriter<'a>,
    /// A count or sum did not fit INT32: `writer` is the second file.
    is_wide: bool,
    /// What the files finished so far hold.
    written: Written,
    campaign: Vec<u64>,
    /// An index into the list of channels seen so far.
    channel: Vec<u32>,
    day: Vec<i32>,
    /// sessions, purchasing_sessions, revenue_cents, assisted_sessions.
    columns: [Vec<i64>; 4],
}

impl CampaignTable<'_> {
    /// Adds the gathered rows to the file as one row group and forgets them.
    fn flush(&mut self, channels: &[&[u8]]) -> std::io::Result<()> {
        if self.day.is_empty() {
            return Ok(());
        }
        if !self.group(channels)? {
            // A count or sum over INT32: the file ends before this row group,
            // and the rest of the table goes to one with INT64 columns.
            if self.is_wide {
                return Err(std::io::Error::other("campaign_daily: a value does not fit INT64"));
            }
            self.is_wide = true;
            self.writer.abandon();
            let next = GroupWriter::create(&self.folder.join("part-000-wide.parquet"), self.wide)?.gzip(self.gzip);
            let finished = std::mem::replace(&mut self.writer, next).finish()?;
            self.add(finished);
            if !self.group(channels)? {
                return Err(std::io::Error::other("campaign_daily: a value does not fit INT64"));
            }
        }
        self.campaign.clear();
        self.channel.clear();
        self.day.clear();
        self.columns.iter_mut().for_each(Vec::clear);
        Ok(())
    }

    /// One row group of the gathered rows. `Ok(false)`: the file has INT32
    /// columns and a value does not fit.
    fn group(&mut self, channels: &[&[u8]]) -> std::io::Result<bool> {
        let ctx = self.ctx;
        self.writer.begin(&self.day, |_| false)?;
        self.writer.runs(&self.campaign, |id, _| Text::Long(&ctx.campaign_ids[id as usize]))?;
        self.writer.strings(channels, &self.channel, u32::MAX)?;
        self.writer.dates(&self.day)?;
        for column in &self.columns {
            if !self.writer.ints(column)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn add(&mut self, file: Written) {
        self.written.bytes += file.bytes;
        self.written.row_groups += file.row_groups;
        self.written.write_nanos += file.write_nanos;
    }
}

/// Writes campaign_daily: the workers' cells summed campaign by campaign, the
/// rows written a row group at a time. Returns what was written.
fn write_campaign_daily(ctx: &Ctx, output: &Path, workers: &[PassCOut]) -> std::io::Result<Written> {
    let fields = |integer: ColumnType| {
        [
            ("campaign_id", ColumnType::Str),
            ("channel", ColumnType::Str),
            ("day", ColumnType::Date),
            ("sessions", integer),
            ("purchasing_sessions", integer),
            ("revenue_cents", integer),
            ("assisted_sessions", integer),
        ]
    };
    // The counts and sums are INT32 unless one of them does not fit.
    let (narrow, wide) = (fields(ColumnType::Int32), fields(ColumnType::Int64));
    let folder = output.join("campaign_daily");
    let gzip = gzip_columns(&CAMPAIGN_DAILY_GZIP);
    let mut table = CampaignTable {
        ctx,
        writer: GroupWriter::create(&folder.join("part-000.parquet"), &narrow)?.gzip(gzip),
        folder,
        gzip,
        wide: &wide,
        is_wide: false,
        written: Written::default(),
        campaign: Vec::with_capacity(CAMPAIGN_ROWS),
        channel: Vec::with_capacity(CAMPAIGN_ROWS),
        day: Vec::with_capacity(CAMPAIGN_ROWS),
        columns: std::array::from_fn(|_| Vec::with_capacity(CAMPAIGN_ROWS)),
    };
    // Every channel once, in the order campaigns with rows first use them.
    let mut channels: Vec<&[u8]> = Vec::new();
    let mut channel_index: FastMap<&[u8], u32> = HashMap::default();
    let day_count = ctx.day_count.max(1);
    let mut totals = vec![[0i64; 4]; day_count];
    for id in 0..ctx.campaign_ids.len() {
        totals.fill([0; 4]);
        for worker in workers {
            for (total, cell) in totals.iter_mut().zip(&worker.campaign_days[id * day_count..(id + 1) * day_count]) {
                for at in 0..4 {
                    total[at] += cell[at];
                }
            }
        }
        let mut channel_of_campaign = None;
        for (offset, total) in totals.iter().enumerate() {
            // A row exists when at least one session was credited to or assisted by the campaign.
            if total[0] == 0 && total[3] == 0 {
                continue;
            }
            let index = *channel_of_campaign.get_or_insert_with(|| {
                let name = &ctx.campaign_channels[id][..];
                let next = channels.len() as u32;
                *channel_index.entry(name).or_insert_with(|| {
                    channels.push(name);
                    next
                })
            });
            table.campaign.push(id as u64);
            table.channel.push(index);
            table.day.push((ctx.first_day + offset as i64) as i32);
            for (column, value) in table.columns.iter_mut().zip(total) {
                column.push(*value);
            }
            if table.day.len() == CAMPAIGN_ROWS {
                table.flush(&channels)?;
            }
        }
    }
    table.flush(&channels)?;
    let CampaignTable { writer, mut written, .. } = table;
    let last = writer.finish()?;
    written.bytes += last.bytes;
    written.row_groups += last.row_groups;
    written.write_nanos += last.write_nanos;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_characters_match_decode_pattern() {
        let check = |code: u64| {
            let mut expected = Vec::new();
            codes::decode_pattern(code, b'u', &mut expected);
            let (characters, count) = pattern_characters(code);
            assert_eq!(&characters.to_le_bytes()[..count], &expected[1..], "code {code}");
            let mut slot = [0xEEu8; parquet::SHORT_TEXT];
            match user_name(code, &mut slot, &[]) {
                Text::Short(length) => assert_eq!(&slot[..length], &expected[..]),
                Text::Long(_) => panic!("a pattern code is short"),
            }
        };
        // Every ID of one or two characters, and the ends of the range.
        for first in 1..=36u64 {
            check(first * codes::P37[7]);
            for second in 0..=36u64 {
                check(first * codes::P37[7] + second * codes::P37[6]);
            }
        }
        for code in [0, 1, 36, 37, codes::P37[4] - 1, codes::P37[4], codes::P37[4] + 1, codes::EXOTIC_BASE - 1] {
            check(code);
        }
        let ids: [&[u8]; 10] =
            [b"u0", b"u00", b"u1", b"u10", b"u1z", b"uzzzzzzzz", b"ua", b"u3fezza6p", b"u00000000", b"uz0z0z0z"];
        for id in ids {
            let code = codes::pattern_code(id, b'u').unwrap();
            let mut slot = [0u8; parquet::SHORT_TEXT];
            match user_name(code, &mut slot, &[]) {
                Text::Short(length) => assert_eq!(&slot[..length], id),
                Text::Long(_) => panic!("a pattern code is short"),
            }
        }
        // Random codes, valid or not (a zero digit in the middle ends the ID).
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..2_000_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            check(state % codes::EXOTIC_BASE);
        }
        let exotic: Vec<Box<[u8]>> = vec![(&b"U1"[..]).into(), (&b" a long exotic user id "[..]).into()];
        let mut slot = [0u8; parquet::SHORT_TEXT];
        match user_name(codes::EXOTIC_BASE + 1, &mut slot, &exotic) {
            Text::Long(bytes) => assert_eq!(bytes, &exotic[1][..]),
            Text::Short(_) => panic!("an exotic code is long"),
        }
    }
}

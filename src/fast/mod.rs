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
pub mod links;
pub mod dedup;
pub mod parse;
pub mod sessions;
pub mod simd;

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::fs;
use std::hash::BuildHasherDefault;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use self::arena::PartList;
use self::codes::Interner;
use self::io::Mapped;
use self::parse::{ClickRecord, ParseOut};
use self::dedup::{PassBOut, ScratchB};
use self::sessions::{PassCOut, ScratchC, NO_CATEGORY};
use crate::parquet::{self, Column, OptStrColumn, StrColumn};
use crate::reference::{list_csv, FastHasher};
use crate::util::{for_each_data_line, parse_manifest, split_fields, Timing, DAY_MS};

pub const PART_BITS: u32 = 10;
pub const PARTS: usize = 1 << PART_BITS;
/// Bits of a mixed code left after the partition bits.
pub const LOCAL_BITS: u32 = codes::CODE_BITS - PART_BITS;
pub const LOCAL_MASK: u64 = (1 << LOCAL_BITS) - 1;
/// Words per list block.
pub const EVENT_BLOCK: usize = 512;
pub const PERSON_BLOCK: usize = 512;
pub const CLICK_BLOCK: usize = 256;

const EVENT_CHUNK: usize = 2 << 20;
const LINK_CHUNK: usize = 1 << 20;
const CLICK_BATCH: usize = 1 << 16;

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

    fn load(data: &Path) -> std::io::Result<Option<Ctx>> {
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

        let products = fs::read(data.join("products.csv"))?;
        let mut category_names: Vec<Box<[u8]>> = Vec::new();
        let mut category_index: FastMap<Box<[u8]>, u32> = HashMap::default();
        let mut product_category: FastMap<Box<[u8]>, u32> = HashMap::default();
        for_each_data_line(&products, |line| {
            if let Some([product, category]) = split_fields::<2>(line) {
                if product.is_empty() {
                    return;
                }
                let next = category_names.len() as u32;
                let at = *category_index.entry(category.into()).or_insert_with(|| {
                    category_names.push(category.into());
                    next
                });
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

        let first_day = window_start.div_euclid(DAY_MS);
        let day_count = ((window_end - 1).div_euclid(DAY_MS) - first_day + 1) as usize;
        Ok(Some(Ctx {
            window_start,
            window_end,
            ingest_base_seconds: window_start.div_euclid(1_000) - 4 * 86_400,
            first_day,
            day_count,
            category_names,
            unknown_category,
            product_category,
            category_by_product_number,
            campaign_ids,
            campaign_channels,
            campaign_index,
            campaign_by_number,
            organic,
            event_ids: Interner::default(),
            user_ids: Interner::default(),
            click_ids: Interner::default(),
            #[cfg(target_arch = "x86_64")]
            ts_tables: simd::TsTables::new(window_start, window_start.div_euclid(1_000) - 4 * 86_400),
            fast_rows: !std::env::var_os("SS_FAST").is_some_and(|value| value == "0"),
            verify: std::env::var_os("SS_VERIFY").is_some_and(|value| value == "1"),
            diag: std::env::var("SS_DIAG").ok().and_then(|value| value.parse().ok()).unwrap_or(0),
            fallback: AtomicBool::new(false),
        }))
    }
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

/// Runs the optimized engine. `Ok(false)` means nothing was written and the
/// caller must use the reference pipeline for this input.
pub fn run(data: &Path, output: &Path, threads: usize) -> std::io::Result<bool> {
    let mut timing = Timing::new();
    let threads = threads.max(1);
    let Some(ctx) = Ctx::load(data)? else {
        return Ok(false);
    };
    let event_files = map_all(&data.join("events"))?;
    let click_files = map_all(&data.join("clicks"))?;
    let link_files = map_all(&data.join("links"))?;
    timing.stage("startup");

    // Links first: they are small, and resolving them can then overlap the
    // much longer event parse.
    let parse_out: Slots<ParseOut> = Slots::new(threads, ParseOut::new);
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
    let link_table = std::thread::scope(|scope| {
        let builder = scope.spawn(|| {
            let started = Instant::now();
            let table = links::build(&link_pairs, &ctx.user_ids);
            link_build_nanos.store(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            table
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
        return Ok(false);
    }

    // Pass B.
    let mut parse_out = parse_out.into_inner();
    let event_lists: Vec<Vec<PartList>> =
        parse_out.iter_mut().map(|out| out.events.iter_mut().map(|writer| writer.finish()).collect()).collect();
    let mut click_batches: Vec<&[ClickRecord]> = Vec::new();
    for out in &parse_out {
        click_batches.extend(out.clicks.chunks(CLICK_BATCH));
    }
    let b_out: Slots<PassBOut> = Slots::new(threads, PassBOut::new);
    let next = AtomicUsize::new(0);
    parallel(threads, |thread| {
        // SAFETY: slot `thread` belongs to this thread for the whole phase.
        let out = unsafe { b_out.get(thread) };
        let mut scratch = ScratchB::default();
        let mut lists: Vec<PartList> = Vec::with_capacity(event_lists.len());
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            if at < PARTS {
                lists.clear();
                lists.extend(event_lists.iter().map(|per_thread| per_thread[at]));
                dedup::dedup_partition(&link_table, &lists, out, &mut scratch);
            } else if let Some(batch) = click_batches.get(at - PARTS) {
                dedup::map_clicks(&link_table, batch, out);
            } else {
                break;
            }
        }
    });
    timing.stage("dedup");

    // Pass C.
    let mut b_out = b_out.into_inner();
    let person_events: Vec<Vec<PartList>> =
        b_out.iter_mut().map(|out| out.events.iter_mut().map(|writer| writer.finish()).collect()).collect();
    let person_clicks: Vec<Vec<PartList>> =
        b_out.iter_mut().map(|out| out.clicks.iter_mut().map(|writer| writer.finish()).collect()).collect();
    let click_exotic = ctx.click_ids.snapshot();
    let user_exotic = ctx.user_ids.snapshot();
    let c_out: Slots<PassCOut> = Slots::new(threads, || PassCOut::new(&ctx));
    let next = AtomicUsize::new(0);
    parallel(threads, |thread| {
        // SAFETY: slot `thread` belongs to this thread for the whole phase.
        let out = unsafe { c_out.get(thread) };
        let mut scratch = ScratchC::new(&ctx);
        let mut events: Vec<PartList> = Vec::with_capacity(person_events.len());
        let mut clicks: Vec<PartList> = Vec::with_capacity(person_clicks.len());
        loop {
            let part = next.fetch_add(1, Ordering::Relaxed);
            if part >= PARTS {
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

    // Output: one user_daily file per worker, one campaign_daily file.
    let c_out = c_out.into_inner();
    fs::create_dir_all(output.join("user_daily"))?;
    fs::create_dir_all(output.join("campaign_daily"))?;
    let failure: std::sync::Mutex<Option<std::io::Error>> = std::sync::Mutex::new(None);
    parallel(threads, |thread| {
        let rows = &c_out[thread];
        // Worker 0 always writes, so the folder holds a table even with no rows.
        if rows.names.is_empty() && thread != 0 {
            return;
        }
        if let Err(error) = write_user_daily(&ctx, output, thread, rows, &user_exotic) {
            failure.lock().unwrap().get_or_insert(error);
        }
    });
    if let Some(error) = failure.into_inner().unwrap() {
        return Err(error);
    }
    write_campaign_daily(&ctx, output, &c_out)?;
    timing.stage("output");
    timing.total();
    Ok(true)
}

fn write_user_daily(
    ctx: &Ctx,
    output: &Path,
    thread: usize,
    rows: &PassCOut,
    user_exotic: &[Box<[u8]>],
) -> std::io::Result<()> {
    let count = rows.names.len();
    let mut user_id = StrColumn::default();
    let mut top_category = OptStrColumn::default();
    let mut name: Vec<u8> = Vec::with_capacity(32);
    for at in 0..count {
        name.clear();
        codes::id_bytes(rows.names[at], b'u', user_exotic, &mut name);
        user_id.push(&name);
        let category = rows.top_category[at];
        top_category.push(if category == NO_CATEGORY { None } else { Some(&ctx.category_names[category as usize][..]) });
    }
    parquet::write_file(
        &output.join("user_daily").join(format!("part-{thread:03}.parquet")),
        &[
            ("user_id", &Column::Str(user_id)),
            ("day", &Column::Date(rows.days.clone())),
            ("sessions", &Column::I64(rows.sessions.clone())),
            ("events", &Column::I64(rows.events.clone())),
            ("revenue_cents", &Column::I64(rows.revenue.clone())),
            ("top_category", &Column::OptStr(top_category)),
        ],
        count,
    )
}

fn write_campaign_daily(ctx: &Ctx, output: &Path, workers: &[PassCOut]) -> std::io::Result<()> {
    let cells = ctx.campaign_ids.len() * ctx.day_count;
    let mut totals = vec![[0i64; 4]; cells];
    for worker in workers {
        for (total, cell) in totals.iter_mut().zip(&worker.campaign_days) {
            for at in 0..4 {
                total[at] += cell[at];
            }
        }
    }
    let mut campaign_id = StrColumn::default();
    let mut channel = StrColumn::default();
    let mut day: Vec<i32> = Vec::new();
    let mut columns: [Vec<i64>; 4] = Default::default();
    for (at, total) in totals.iter().enumerate() {
        // A row exists when at least one session was credited to or assisted by the campaign.
        if total[0] == 0 && total[3] == 0 {
            continue;
        }
        let campaign = at / ctx.day_count;
        campaign_id.push(&ctx.campaign_ids[campaign]);
        channel.push(&ctx.campaign_channels[campaign]);
        day.push((ctx.first_day + (at % ctx.day_count) as i64) as i32);
        for (column, value) in columns.iter_mut().zip(total) {
            column.push(*value);
        }
    }
    let count = day.len();
    let [sessions, purchasing, revenue, assisted] = columns;
    parquet::write_file(
        &output.join("campaign_daily").join("part-000.parquet"),
        &[
            ("campaign_id", &Column::Str(campaign_id)),
            ("channel", &Column::Str(channel)),
            ("day", &Column::Date(day)),
            ("sessions", &Column::I64(sessions)),
            ("purchasing_sessions", &Column::I64(purchasing)),
            ("revenue_cents", &Column::I64(revenue)),
            ("assisted_sessions", &Column::I64(assisted)),
        ],
        count,
    )
}

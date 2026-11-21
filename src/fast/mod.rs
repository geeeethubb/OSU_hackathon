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
pub mod parse;
pub mod passes;

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::fs;
use std::hash::BuildHasherDefault;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use self::arena::PartList;
use self::codes::Interner;
use self::io::Mapped;
use self::parse::{ClickRecord, ParseOut};
use self::passes::{PassBOut, PassCOut, ScratchB, ScratchC, NO_CATEGORY};
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
    /// Campaigns by index; the last one is `organic`.
    pub campaign_ids: Vec<Box<[u8]>>,
    pub campaign_channels: Vec<Box<[u8]>>,
    pub campaign_index: FastMap<Box<[u8]>, u32>,
    pub organic: u32,
    pub event_ids: Interner,
    pub user_ids: Interner,
    pub click_ids: Interner,
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
            campaign_ids,
            campaign_channels,
            campaign_index,
            organic,
            event_ids: Interner::default(),
            user_ids: Interner::default(),
            click_ids: Interner::default(),
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
    let next = AtomicUsize::new(0);
    let link_table = std::thread::scope(|scope| {
        let builder = scope.spawn(|| links::build(&link_pairs, &ctx.user_ids));
        parallel(threads, |thread| {
            // SAFETY: slot `thread` belongs to this thread for the whole phase.
            let out = unsafe { parse_out.get(thread) };
            while let Some(task) = tasks.get(next.fetch_add(1, Ordering::Relaxed)) {
                if task.kind == Kind::Events {
                    parse::parse_events_chunk(&ctx, event_files[task.file].bytes(), task.start, task.end, out);
                } else {
                    parse::parse_clicks_chunk(&ctx, click_files[task.file].bytes(), task.start, task.end, out);
                }
            }
        });
        builder.join().unwrap()
    });
    timing.stage("parse");
    if ctx.fallback.load(Ordering::Relaxed) {
        return Ok(false);
    }

    // Pass B.
    let parse_out = parse_out.into_inner();
    let event_lists: Vec<Vec<PartList>> =
        parse_out.iter().map(|out| out.events.iter().map(|writer| writer.list(EVENT_BLOCK)).collect()).collect();
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
                passes::dedup_partition(&link_table, &lists, out, &mut scratch);
            } else if let Some(batch) = click_batches.get(at - PARTS) {
                passes::map_clicks(&link_table, batch, out);
            } else {
                break;
            }
        }
    });
    timing.stage("dedup");

    // Pass C.
    let b_out = b_out.into_inner();
    let person_events: Vec<Vec<PartList>> =
        b_out.iter().map(|out| out.events.iter().map(|writer| writer.list(PERSON_BLOCK)).collect()).collect();
    let person_clicks: Vec<Vec<PartList>> =
        b_out.iter().map(|out| out.clicks.iter().map(|writer| writer.list(CLICK_BLOCK)).collect()).collect();
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
            passes::sessions_partition(&ctx, part, &events, &clicks, &click_exotic, out, &mut scratch);
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

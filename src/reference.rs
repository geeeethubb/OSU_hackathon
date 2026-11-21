//! The simple, correct pipeline: a direct transcription of rules.md with
//! ordinary hash maps and sorts. It is the baseline, and it stays as the
//! correctness reference the optimized engine is compared against on seeds
//! that have no published answer.

use std::collections::HashMap;
use std::fs;
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::parquet::{self, Column, OptStrColumn, StrColumn};
use crate::util::{
    for_each_data_line, parse_manifest, parse_price, parse_ts, split_fields, Timing, DAY_MS, LATE_MS,
    SESSION_GAP_MS, WEEK_MS,
};

/// A fast, well-mixed hasher for byte-string keys (FxHash-style rounds with a
/// final avalanche so hashbrown's high and low bits are both usable).
#[derive(Default)]
pub struct FastHasher(u64);

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut hash = self.0;
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let word = u64::from_le_bytes(chunk.try_into().unwrap());
            hash = (hash.rotate_left(27) ^ word).wrapping_mul(K);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            hash = (hash.rotate_left(27) ^ u64::from_le_bytes(word) ^ ((rest.len() as u64) << 56)).wrapping_mul(K);
        }
        self.0 = hash;
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(27) ^ value).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        let mut hash = self.0;
        hash ^= hash >> 32;
        hash = hash.wrapping_mul(0xD6E8_FEB8_6659_FD93);
        hash ^= hash >> 32;
        hash
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

fn fast_map<K, V>(capacity: usize) -> FastMap<K, V> {
    HashMap::with_capacity_and_hasher(capacity, BuildHasherDefault::default())
}

/// Every `*.csv` file directly in `directory`, in name order.
pub fn list_csv(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let is_csv = path.extension().is_some_and(|extension| extension == "csv");
        if is_csv && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Reads all files, several at a time.
fn read_all(paths: &[PathBuf], threads: usize) -> io::Result<Vec<Vec<u8>>> {
    let slots: Vec<Mutex<Option<io::Result<Vec<u8>>>>> = paths.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1).min(paths.len().max(1)) {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = paths.get(at) else { break };
                *slots[at].lock().unwrap() = Some(fs::read(path));
            });
        }
    });
    slots.into_iter().map(|slot| slot.into_inner().unwrap().unwrap()).collect()
}

/// Runs `parse` over every buffer on a small thread pool, keeping buffer order.
fn parse_parallel<'a, T: Send>(
    buffers: &'a [Vec<u8>],
    threads: usize,
    parse: impl Fn(&'a [u8]) -> Vec<T> + Sync,
) -> Vec<Vec<T>> {
    let slots: Vec<Mutex<Option<Vec<T>>>> = buffers.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1).min(buffers.len().max(1)) {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(buffer) = buffers.get(at) else { break };
                *slots[at].lock().unwrap() = Some(parse(buffer));
            });
        }
    });
    slots.into_iter().map(|slot| slot.into_inner().unwrap().unwrap()).collect()
}

/// One parsed (well-formed, non-empty event_id) event line.
struct EventRow<'a> {
    event_id: &'a [u8],
    user_id: &'a [u8],
    ts: i64,
    /// Ingest time in ms; `i64::MAX` if it could not be read (never in valid data).
    ingest: i64,
    /// Purchase price, 0 for views and carts.
    price: u32,
    category: u32,
    /// Passes validation (step 3) and is not late (step 4).
    valid: bool,
}

/// One parsed click line that passed validation.
struct ClickRow<'a> {
    click_id: &'a [u8],
    user_id: &'a [u8],
    ts: i64,
    campaign: u32,
}

struct Event {
    person: u32,
    ts: i64,
    price: u32,
    category: u32,
}

struct Click<'a> {
    person: u32,
    ts: i64,
    click_id: &'a [u8],
    campaign: u32,
}

/// Interned user IDs with union-find over the links.
struct People<'a> {
    index: FastMap<&'a [u8], u32>,
    ids: Vec<&'a [u8]>,
    parent: Vec<u32>,
}

impl<'a> People<'a> {
    fn intern(&mut self, id: &'a [u8]) -> u32 {
        if let Some(&at) = self.index.get(id) {
            return at;
        }
        let at = self.ids.len() as u32;
        self.index.insert(id, at);
        self.ids.push(id);
        self.parent.push(at);
        at
    }

    fn find(&mut self, mut at: u32) -> u32 {
        while self.parent[at as usize] != at {
            let grandparent = self.parent[self.parent[at as usize] as usize];
            self.parent[at as usize] = grandparent;
            at = grandparent;
        }
        at
    }

    fn union(&mut self, a: u32, b: u32) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a.max(b) as usize] = a.min(b);
        }
    }
}

#[derive(Default, Clone, Copy)]
struct CampaignDay {
    sessions: i64,
    purchasing_sessions: i64,
    revenue_cents: i64,
    assisted_sessions: i64,
}

pub fn run(data: &Path, output: &Path, threads: usize) -> io::Result<()> {
    let mut timing = Timing::new();
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());

    let manifest = fs::read(data.join("manifest.json"))?;
    let (window_start, window_end) = parse_manifest(&manifest).ok_or_else(|| invalid("bad manifest.json"))?;

    // Products and categories. `unknown` is the category of every product not in the file.
    let products_buffer = fs::read(data.join("products.csv"))?;
    let mut category_names: Vec<&[u8]> = Vec::new();
    let mut category_index: FastMap<&[u8], u32> = fast_map(64);
    let mut product_category: FastMap<&[u8], u32> = fast_map(1 << 16);
    for_each_data_line(&products_buffer, |line| {
        if let Some([product, category]) = split_fields::<2>(line) {
            if product.is_empty() {
                return;
            }
            let next = category_names.len() as u32;
            let at = *category_index.entry(category).or_insert_with(|| {
                category_names.push(category);
                next
            });
            product_category.entry(product).or_insert(at);
        }
    });
    let unknown_category = category_names.len() as u32;
    category_names.push(b"unknown");

    // Campaigns. `organic` takes the index after the last real campaign.
    let campaigns_buffer = fs::read(data.join("campaigns.csv"))?;
    let mut campaign_ids: Vec<&[u8]> = Vec::new();
    let mut campaign_channels: Vec<&[u8]> = Vec::new();
    let mut campaign_index: FastMap<&[u8], u32> = fast_map(1 << 12);
    for_each_data_line(&campaigns_buffer, |line| {
        if let Some([campaign, channel]) = split_fields::<2>(line) {
            if campaign.is_empty() || campaign_index.contains_key(campaign) {
                return;
            }
            campaign_index.insert(campaign, campaign_ids.len() as u32);
            campaign_ids.push(campaign);
            campaign_channels.push(channel);
        }
    });
    let organic = campaign_ids.len() as u32;
    campaign_ids.push(b"organic");
    campaign_channels.push(b"organic");

    let link_buffers = read_all(&list_csv(&data.join("links"))?, threads)?;
    let event_buffers = read_all(&list_csv(&data.join("events"))?, threads)?;
    let click_buffers = read_all(&list_csv(&data.join("clicks"))?, threads)?;
    timing.stage("read");

    // Events, steps 1, 3 and 4 per line (dedup, step 2, comes next and only
    // uses event_id and ingest_ts; validity is carried along with each copy).
    let event_rows: Vec<Vec<EventRow>> = parse_parallel(&event_buffers, threads, |buffer| {
        let mut rows = Vec::with_capacity(buffer.len() / 70);
        for_each_data_line(buffer, |line| {
            let Some([event_id, user_id, ts, ingest_ts, event_type, product_id, price_cents]) =
                split_fields::<7>(line)
            else {
                return;
            };
            if event_id.is_empty() {
                return;
            }
            let ingest = parse_ts(ingest_ts).unwrap_or(i64::MAX);
            let ts = parse_ts(ts).filter(|&ts| window_start <= ts && ts < window_end);
            let is_purchase = event_type == b"purchase";
            let known_type = is_purchase || event_type == b"view" || event_type == b"cart";
            let price = if is_purchase { parse_price(price_cents) } else { Some(0) };
            let mut valid =
                !user_id.is_empty() && ts.is_some() && known_type && !product_id.is_empty() && price.is_some();
            if let (true, Some(ts)) = (valid, ts) {
                // Exactly 24 hours is kept; an ingest before ts is kept.
                if ingest.saturating_sub(ts) > LATE_MS {
                    valid = false;
                }
            }
            let category = if valid && is_purchase {
                product_category.get(product_id).copied().unwrap_or(unknown_category)
            } else {
                unknown_category
            };
            rows.push(EventRow {
                event_id,
                user_id,
                ts: ts.unwrap_or(0),
                ingest,
                price: price.unwrap_or(0),
                category,
                valid,
            });
        });
        rows
    });
    let click_rows: Vec<Vec<ClickRow>> = parse_parallel(&click_buffers, threads, |buffer| {
        let mut rows = Vec::with_capacity(buffer.len() / 45);
        for_each_data_line(buffer, |line| {
            let Some([click_id, user_id, campaign_id, ts]) = split_fields::<4>(line) else {
                return;
            };
            if click_id.is_empty() || user_id.is_empty() {
                return;
            }
            let Some(ts) = parse_ts(ts).filter(|&ts| window_start - WEEK_MS <= ts && ts < window_end) else {
                return;
            };
            let Some(&campaign) = campaign_index.get(campaign_id) else {
                return;
            };
            rows.push(ClickRow { click_id, user_id, ts, campaign });
        });
        rows
    });
    timing.stage("parse");

    // Linked IDs.
    let mut people = People { index: fast_map(1 << 16), ids: Vec::new(), parent: Vec::new() };
    for buffer in &link_buffers {
        for_each_data_line(buffer, |line| {
            if let Some([a, b]) = split_fields::<2>(line) {
                if a.is_empty() || b.is_empty() {
                    return;
                }
                let (a, b) = (people.intern(a), people.intern(b));
                people.union(a, b);
            }
        });
    }
    timing.stage("links");

    // Step 2: dedup on event_id, keeping the smallest ingest_ts.
    let total_rows: usize = event_rows.iter().map(Vec::len).sum();
    let mut kept: FastMap<&[u8], &EventRow> = fast_map(total_rows);
    for row in event_rows.iter().flatten() {
        kept.entry(row.event_id)
            .and_modify(|current| {
                if row.ingest < current.ingest {
                    *current = row;
                }
            })
            .or_insert(row);
    }
    let mut events: Vec<Event> = Vec::with_capacity(kept.len());
    for row in kept.values() {
        if row.valid {
            let person = people.intern(row.user_id);
            events.push(Event { person, ts: row.ts, price: row.price, category: row.category });
        }
    }
    drop(kept);
    timing.stage("dedup");

    // Clicks: dedup on click_id (copies are identical, keep the first seen).
    let total_clicks: usize = click_rows.iter().map(Vec::len).sum();
    let mut seen_clicks: FastMap<&[u8], ()> = fast_map(total_clicks);
    let mut clicks: Vec<Click> = Vec::with_capacity(total_clicks);
    for row in click_rows.iter().flatten() {
        if seen_clicks.insert(row.click_id, ()).is_none() {
            let person = people.intern(row.user_id);
            clicks.push(Click { person, ts: row.ts, click_id: row.click_id, campaign: row.campaign });
        }
    }
    drop(seen_clicks);

    // Resolve every ID to its person (the root), and name each person by the
    // byte-wise smallest ID in the group.
    let id_count = people.ids.len();
    let mut root_of: Vec<u32> = Vec::with_capacity(id_count);
    for at in 0..id_count as u32 {
        let root = people.find(at);
        root_of.push(root);
    }
    let mut name_of: Vec<&[u8]> = people.ids.clone();
    for at in 0..id_count {
        let root = root_of[at] as usize;
        if people.ids[at] < name_of[root] {
            name_of[root] = people.ids[at];
        }
    }
    for event in &mut events {
        event.person = root_of[event.person as usize];
    }
    for click in &mut clicks {
        click.person = root_of[click.person as usize];
    }
    events.sort_unstable_by_key(|event| (event.person, event.ts));
    // Latest ts last; within a ts the smallest click_id last, so the click
    // that credits a session is always the last one at or before its start.
    clicks.sort_unstable_by(|a, b| {
        (a.person, a.ts).cmp(&(b.person, b.ts)).then_with(|| b.click_id.cmp(a.click_id))
    });
    timing.stage("sort");

    // Sessions, attribution and both aggregations in one pass per person.
    let category_count = category_names.len();
    let mut user_id_column = StrColumn::default();
    let mut user_day: Vec<i32> = Vec::new();
    let mut user_sessions: Vec<i64> = Vec::new();
    let mut user_events: Vec<i64> = Vec::new();
    let mut user_revenue: Vec<i64> = Vec::new();
    let mut user_top = OptStrColumn::default();
    let mut campaign_days: FastMap<(u32, i32), CampaignDay> = fast_map(1 << 16);
    let mut assist_stamp: Vec<u64> = vec![0; campaign_ids.len()];
    let mut session_serial: u64 = 0;
    let mut category_revenue: Vec<i64> = vec![0; category_count];

    let mut event_at = 0;
    let mut click_at = 0;
    while event_at < events.len() {
        let person = events[event_at].person;
        let mut person_end = event_at;
        while person_end < events.len() && events[person_end].person == person {
            person_end += 1;
        }
        while click_at < clicks.len() && clicks[click_at].person < person {
            click_at += 1;
        }
        let clicks_start = click_at;
        while click_at < clicks.len() && clicks[click_at].person == person {
            click_at += 1;
        }
        let person_clicks = &clicks[clicks_start..click_at];
        let person_events = &events[event_at..person_end];
        let name = name_of[person as usize];

        // The open user_daily row: (day, sessions, events, revenue, purchases).
        let mut row: Option<(i32, i64, i64, i64, i64)> = None;
        let mut flush = |row: &mut Option<(i32, i64, i64, i64, i64)>, category_revenue: &mut Vec<i64>| {
            if let Some((day, sessions, event_count, revenue, purchases)) = row.take() {
                user_id_column.push(name);
                user_day.push(day);
                user_sessions.push(sessions);
                user_events.push(event_count);
                user_revenue.push(revenue);
                if purchases == 0 {
                    user_top.push(None);
                } else {
                    let mut best = 0;
                    for at in 1..category_count {
                        let better = category_revenue[at] > category_revenue[best]
                            || (category_revenue[at] == category_revenue[best]
                                && category_names[at] < category_names[best]);
                        if better {
                            best = at;
                        }
                    }
                    user_top.push(Some(category_names[best]));
                }
                category_revenue.iter_mut().for_each(|sum| *sum = 0);
            }
        };

        let mut at = 0;
        while at < person_events.len() {
            let start = person_events[at].ts;
            let day = start.div_euclid(DAY_MS) as i32;
            if row.is_some_and(|open| open.0 != day) {
                flush(&mut row, &mut category_revenue);
            }
            let open = row.get_or_insert((day, 0, 0, 0, 0));
            let mut last = start;
            let mut session_events = 0;
            let mut session_revenue = 0;
            let mut session_purchases = 0;
            while at < person_events.len() && person_events[at].ts - last <= SESSION_GAP_MS {
                let event = &person_events[at];
                last = event.ts;
                session_events += 1;
                if event.price > 0 {
                    session_revenue += i64::from(event.price);
                    session_purchases += 1;
                    category_revenue[event.category as usize] += i64::from(event.price);
                }
                at += 1;
            }
            open.1 += 1;
            open.2 += session_events;
            open.3 += session_revenue;
            open.4 += session_purchases;

            // Clicks with start - 7 days <= ts <= start.
            let first = person_clicks.partition_point(|click| click.ts < start - WEEK_MS);
            let end = person_clicks.partition_point(|click| click.ts <= start);
            let credited = if end > first { person_clicks[end - 1].campaign } else { organic };
            let cell = campaign_days.entry((credited, day)).or_default();
            cell.sessions += 1;
            cell.purchasing_sessions += i64::from(session_purchases > 0);
            cell.revenue_cents += session_revenue;
            session_serial += 1;
            for click in &person_clicks[first..end] {
                if assist_stamp[click.campaign as usize] != session_serial {
                    assist_stamp[click.campaign as usize] = session_serial;
                    campaign_days.entry((click.campaign, day)).or_default().assisted_sessions += 1;
                }
            }
        }
        flush(&mut row, &mut category_revenue);
        event_at = person_end;
    }
    timing.stage("sessions");

    let user_rows = user_day.len();
    fs::create_dir_all(output.join("user_daily"))?;
    parquet::write_file(
        &output.join("user_daily").join("part-0.parquet"),
        &[
            ("user_id", &Column::Str(user_id_column)),
            ("day", &Column::Date(user_day)),
            ("sessions", &Column::I64(user_sessions)),
            ("events", &Column::I64(user_events)),
            ("revenue_cents", &Column::I64(user_revenue)),
            ("top_category", &Column::OptStr(user_top)),
        ],
        user_rows,
    )?;

    let mut campaign_id_column = StrColumn::default();
    let mut channel_column = StrColumn::default();
    let mut campaign_day: Vec<i32> = Vec::new();
    let mut campaign_sessions: Vec<i64> = Vec::new();
    let mut campaign_purchasing: Vec<i64> = Vec::new();
    let mut campaign_revenue: Vec<i64> = Vec::new();
    let mut campaign_assisted: Vec<i64> = Vec::new();
    for (&(campaign, day), cell) in &campaign_days {
        campaign_id_column.push(campaign_ids[campaign as usize]);
        channel_column.push(campaign_channels[campaign as usize]);
        campaign_day.push(day);
        campaign_sessions.push(cell.sessions);
        campaign_purchasing.push(cell.purchasing_sessions);
        campaign_revenue.push(cell.revenue_cents);
        campaign_assisted.push(cell.assisted_sessions);
    }
    let campaign_rows = campaign_day.len();
    fs::create_dir_all(output.join("campaign_daily"))?;
    parquet::write_file(
        &output.join("campaign_daily").join("part-0.parquet"),
        &[
            ("campaign_id", &Column::Str(campaign_id_column)),
            ("channel", &Column::Str(channel_column)),
            ("day", &Column::Date(campaign_day)),
            ("sessions", &Column::I64(campaign_sessions)),
            ("purchasing_sessions", &Column::I64(campaign_purchasing)),
            ("revenue_cents", &Column::I64(campaign_revenue)),
            ("assisted_sessions", &Column::I64(campaign_assisted)),
        ],
        campaign_rows,
    )?;
    timing.stage("output");
    timing.total();
    Ok(())
}

/// Shared entry point for the binaries: `DATA_DIR`/`OUTPUT_DIR` from the
/// environment (defaults `/data` and `/output`), or two positional arguments.
pub fn directories_from_environment() -> (PathBuf, PathBuf) {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() == 2 {
        return (PathBuf::from(&arguments[0]), PathBuf::from(&arguments[1]));
    }
    let data = std::env::var_os("DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from);
    let output = std::env::var_os("OUTPUT_DIR").map_or_else(|| PathBuf::from("/output"), PathBuf::from);
    (data, output)
}

pub fn thread_count() -> usize {
    std::env::var("NCPU")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&count| count > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, usize::from))
}

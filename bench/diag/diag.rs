//! ss-diag: dependency-free diagnostics for the benchmark harness.
//!
//! One static binary, three subcommands:
//!
//!   diag noop
//!       Exits 0 at once. A fresh container running it measures what Docker
//!       itself costs between "container start" and "container exit".
//!
//!   diag readthrough [--mode M] [--threads N] [--chunk-kb K]
//!       Passes every byte of every file under $DATA_DIR (default /data)
//!       through this process with N threads (default: $NCPU, else the number
//!       of CPUs the process may use). A lower bound for anything that has to
//!       look at the whole input. Modes:
//!         read                pread() into a per-thread buffer, nothing else
//!         read-scan           pread(), then count '\n' in the buffer
//!         mmap-touch          mmap, touch one byte per 4 KiB page (fault cost only)
//!         mmap-scan           mmap, count '\n' over every byte (default)
//!         mmap-populate-scan  mmap, MADV_POPULATE_READ each chunk, then count '\n'
//!
//!   diag memtouch [--mb N] [--thp] [--populate] [--munmap] [--threads T]
//!       Maps N MiB of fresh anonymous memory (default 1024), writes one byte
//!       per 4 KiB page with T threads and exits without unmapping. In-process
//!       time is the page-fault cost; container time minus in-process time
//!       minus the noop floor is what the kernel needs to tear the memory down
//!       at exit, which the scorer times and SS_TIMING cannot see.
//!         --thp       start the region on a 2 MiB boundary and madvise(MADV_HUGEPAGE)
//!                     before touching it
//!         --populate  MAP_POPULATE instead of touching (the kernel faults it
//!                     all in during mmap, on one thread)
//!         --munmap    each thread munmaps its own part before the process
//!                     exits (teardown in parallel, inside the process)
//!
//!   diag sysinfo
//!       Prints what a process inside the container can see: CPUs, memory,
//!       cgroup limits, transparent huge pages, mounts, environment.
//!
//! Build: rustc --edition 2021 -C opt-level=3 -C panic=abort diag.rs
//! (bench/diag.Dockerfile does this and puts the result in an empty image).

use std::env;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

fn main() {
    let started = Instant::now();
    let args: Vec<String> = env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("noop") => 0,
        Some("readthrough") => match readthrough(&args[2..], started) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("diag readthrough: {error}");
                1
            }
        },
        Some("memtouch") => match memtouch(&args[2..], started) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("diag memtouch: {error}");
                1
            }
        },
        Some("sysinfo") => {
            sysinfo();
            0
        }
        _ => {
            eprintln!(
                "usage: diag noop | diag readthrough [--mode read|read-scan|mmap-touch|mmap-scan|mmap-populate-scan] \
                 [--threads N] [--chunk-kb K] | diag memtouch [--mb N] [--thp] [--populate] [--munmap] [--threads T] \
                 | diag sysinfo"
            );
            2
        }
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// readthrough
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Read,
    ReadScan,
    MmapTouch,
    MmapScan,
    MmapPopulateScan,
}

impl Mode {
    fn parse(text: &str) -> Option<Mode> {
        Some(match text {
            "read" => Mode::Read,
            "read-scan" => Mode::ReadScan,
            "mmap-touch" => Mode::MmapTouch,
            "mmap-scan" => Mode::MmapScan,
            "mmap-populate-scan" => Mode::MmapPopulateScan,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Read => "read",
            Mode::ReadScan => "read-scan",
            Mode::MmapTouch => "mmap-touch",
            Mode::MmapScan => "mmap-scan",
            Mode::MmapPopulateScan => "mmap-populate-scan",
        }
    }

    fn uses_mmap(self) -> bool {
        matches!(self, Mode::MmapTouch | Mode::MmapScan | Mode::MmapPopulateScan)
    }
}

struct Input {
    path: PathBuf,
    /// First path component below the data directory ("." for files directly in it).
    folder: String,
    length: u64,
}

/// A byte range of one file; the unit of work a thread takes.
#[derive(Clone, Copy)]
struct Chunk {
    file: usize,
    offset: u64,
    length: usize,
}

fn option_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|argument| argument == name)?;
    args.get(at + 1).map(String::as_str)
}

fn walk(directory: &Path, root: &Path, inputs: &mut Vec<Input>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            walk(&path, root, inputs)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let mut components = relative.components();
            let first = components.next().map(|part| part.as_os_str().to_string_lossy().into_owned());
            let folder = match (first, components.next()) {
                (Some(first), Some(_)) => first,
                _ => ".".to_string(),
            };
            inputs.push(Input { path, folder, length: metadata.len() });
        }
    }
    Ok(())
}

#[inline(never)]
fn count_newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&byte| byte == b'\n').count() as u64
}

fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
    }
}

#[cfg(unix)]
mod sys {
    use std::os::raw::{c_int, c_void};

    unsafe extern "C" {
        pub fn mmap(address: *mut c_void, length: usize, protection: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
        pub fn munmap(address: *mut c_void, length: usize) -> c_int;
        pub fn madvise(address: *mut c_void, length: usize, advice: c_int) -> c_int;
    }

    pub const PROT_READ: c_int = 1;
    pub const PROT_WRITE: c_int = 2;
    pub const MAP_PRIVATE: c_int = 2;
    pub const MAP_ANONYMOUS: c_int = 0x20;
    pub const MAP_POPULATE: c_int = 0x8000;
    pub const MADV_HUGEPAGE: c_int = 14;
    /// Linux 5.14+: fault the range in for reading, in one call.
    pub const MADV_POPULATE_READ: c_int = 22;
}

/// A whole file mapped read-only. Never unmapped: the process exits right
/// after, which is also what a pipeline would do.
#[derive(Clone, Copy)]
struct Mapping {
    address: *const u8,
    length: usize,
}

// The mappings are read-only and live until the process exits.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

#[cfg(unix)]
fn map_file(file: &File, length: u64) -> io::Result<Mapping> {
    use std::os::unix::io::AsRawFd;
    if length == 0 {
        return Ok(Mapping { address: std::ptr::null(), length: 0 });
    }
    // SAFETY: a fresh private read-only mapping of an open file.
    let address = unsafe {
        sys::mmap(std::ptr::null_mut(), length as usize, sys::PROT_READ, sys::MAP_PRIVATE, file.as_raw_fd(), 0)
    };
    if address as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(Mapping { address: address as *const u8, length: length as usize })
}

#[cfg(not(unix))]
fn map_file(_file: &File, _length: u64) -> io::Result<Mapping> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the mmap modes need Linux"))
}

#[cfg(unix)]
fn populate(address: *const u8, length: usize) -> bool {
    // SAFETY: the range lies inside a live mapping and starts on a page boundary.
    unsafe { sys::madvise(address as *mut _, length, sys::MADV_POPULATE_READ) == 0 }
}

#[cfg(not(unix))]
fn populate(_address: *const u8, _length: usize) -> bool {
    false
}

fn readthrough(args: &[String], started: Instant) -> io::Result<()> {
    let mode = match option_value(args, "--mode") {
        Some(text) => Mode::parse(text)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("unknown mode {text}")))?,
        None => Mode::MmapScan,
    };
    let threads = option_value(args, "--threads")
        .and_then(|text| text.parse::<usize>().ok())
        .or_else(|| env::var("NCPU").ok().and_then(|text| text.parse::<usize>().ok()))
        .filter(|&count| count > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    let chunk_kb = option_value(args, "--chunk-kb")
        .and_then(|text| text.parse::<usize>().ok())
        .filter(|&kb| kb > 0)
        .unwrap_or(1024);
    // A multiple of the page size, so mapped chunks start on page boundaries.
    let chunk_bytes = (chunk_kb * 1024).next_multiple_of(4096);
    let root = env::var_os("DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from);

    let mut inputs = Vec::new();
    walk(&root, &root, &mut inputs)?;
    inputs.sort_by(|a, b| a.path.cmp(&b.path));

    let mut files = Vec::with_capacity(inputs.len());
    let mut mappings = Vec::with_capacity(inputs.len());
    let mut chunks = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        let file = File::open(&input.path)?;
        if mode.uses_mmap() {
            mappings.push(map_file(&file, input.length)?);
        }
        files.push(file);
        let mut offset = 0u64;
        while offset < input.length {
            let length = (input.length - offset).min(chunk_bytes as u64) as usize;
            chunks.push(Chunk { file: index, offset, length });
            offset += length as u64;
        }
    }
    // Largest files first is not needed: chunks are equal-sized, so any order
    // spreads the work evenly over the threads.

    let next = AtomicUsize::new(0);
    let total_bytes = AtomicU64::new(0);
    let total_lines = AtomicU64::new(0);
    let total_touch = AtomicU64::new(0);
    let populate_failures = AtomicU64::new(0);
    let failure: std::sync::Mutex<Option<io::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut buffer = if mode.uses_mmap() { Vec::new() } else { vec![0u8; chunk_bytes] };
                let (mut bytes, mut lines, mut touch, mut failed_populates) = (0u64, 0u64, 0u64, 0u64);
                while let Some(&chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if mode.uses_mmap() {
                        let mapping = mappings[chunk.file];
                        debug_assert!(chunk.offset as usize + chunk.length <= mapping.length);
                        // SAFETY: the chunk lies inside the file's mapping.
                        let slice = unsafe {
                            std::slice::from_raw_parts(mapping.address.add(chunk.offset as usize), chunk.length)
                        };
                        match mode {
                            Mode::MmapTouch => {
                                let mut at = 0;
                                while at < slice.len() {
                                    // SAFETY: `at` is in bounds.
                                    touch += u64::from(unsafe { std::ptr::read_volatile(slice.as_ptr().add(at)) });
                                    at += 4096;
                                }
                            }
                            Mode::MmapPopulateScan => {
                                if !populate(slice.as_ptr(), slice.len()) {
                                    failed_populates += 1;
                                }
                                lines += count_newlines(slice);
                            }
                            _ => lines += count_newlines(slice),
                        }
                        bytes += slice.len() as u64;
                    } else {
                        let target = &mut buffer[..chunk.length];
                        let mut filled = 0;
                        while filled < target.len() {
                            match read_at(&files[chunk.file], &mut target[filled..], chunk.offset + filled as u64) {
                                Ok(0) => break,
                                Ok(count) => filled += count,
                                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                                Err(error) => {
                                    failure.lock().unwrap().get_or_insert(error);
                                    return;
                                }
                            }
                        }
                        if mode == Mode::ReadScan {
                            lines += count_newlines(&target[..filled]);
                        } else if filled > 0 {
                            touch += u64::from(target[filled - 1]);
                        }
                        bytes += filled as u64;
                    }
                }
                total_bytes.fetch_add(bytes, Ordering::Relaxed);
                total_lines.fetch_add(lines, Ordering::Relaxed);
                total_touch.fetch_add(touch, Ordering::Relaxed);
                populate_failures.fetch_add(failed_populates, Ordering::Relaxed);
            });
        }
    });
    if let Some(error) = failure.into_inner().unwrap() {
        return Err(error);
    }
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;

    let expected: u64 = inputs.iter().map(|input| input.length).sum();
    let read = total_bytes.load(Ordering::Relaxed);
    let mut folders: Vec<(String, u64, u64)> = Vec::new();
    for input in &inputs {
        match folders.iter_mut().find(|(name, _, _)| *name == input.folder) {
            Some(folder) => {
                folder.1 += 1;
                folder.2 += input.length;
            }
            None => folders.push((input.folder.clone(), 1, input.length)),
        }
    }
    for (name, count, bytes) in &folders {
        println!("DIAG folder name={name} files={count} bytes={bytes}");
    }
    println!(
        "DIAG readthrough mode={} threads={} chunk_kb={} files={} bytes={} lines={} touch={} populate_failures={} ms={:.3}",
        mode.name(),
        threads,
        chunk_bytes / 1024,
        inputs.len(),
        read,
        total_lines.load(Ordering::Relaxed),
        total_touch.load(Ordering::Relaxed),
        populate_failures.load(Ordering::Relaxed),
        elapsed_ms,
    );
    if read != expected {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("read {read} bytes, the files hold {expected}"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// memtouch
// ---------------------------------------------------------------------------

const HUGE_PAGE: usize = 2 << 20;

/// `length` bytes of zeroed anonymous memory. With `huge` the region starts on a 2 MiB boundary
/// and transparent huge pages are requested for it; with `populate` the kernel faults all of it
/// in during the mmap call (MAP_POPULATE). Returns the address and whether the madvise succeeded.
#[cfg(unix)]
fn anonymous(length: usize, huge: bool, populate: bool) -> io::Result<(*mut u8, bool)> {
    let mapped = if huge { length + HUGE_PAGE } else { length };
    let flags = sys::MAP_PRIVATE | sys::MAP_ANONYMOUS | if populate { sys::MAP_POPULATE } else { 0 };
    // SAFETY: a fresh private anonymous mapping.
    let address = unsafe { sys::mmap(std::ptr::null_mut(), mapped, sys::PROT_READ | sys::PROT_WRITE, flags, -1, 0) };
    if address as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    if !huge {
        return Ok((address as *mut u8, false));
    }
    let aligned = (address as usize).next_multiple_of(HUGE_PAGE);
    // SAFETY: `aligned..aligned + length` lies inside the mapping, which is 2 MiB longer.
    let advised = unsafe { sys::madvise(aligned as *mut _, length, sys::MADV_HUGEPAGE) == 0 };
    Ok((aligned as *mut u8, advised))
}

#[cfg(not(unix))]
fn anonymous(length: usize, _huge: bool, _populate: bool) -> io::Result<(*mut u8, bool)> {
    // Not the Linux path: only so the tool runs natively elsewhere.
    Ok((Box::leak(vec![0u8; length].into_boxed_slice()).as_mut_ptr(), false))
}

#[cfg(unix)]
fn unmap(address: *mut u8, length: usize) -> bool {
    // SAFETY: the caller owns this page-aligned part of the mapping and never touches it again.
    unsafe { sys::munmap(address as *mut _, length) == 0 }
}

#[cfg(not(unix))]
fn unmap(_address: *mut u8, _length: usize) -> bool {
    false
}

/// Raw memory handed to several threads, each using its own disjoint part.
#[derive(Clone, Copy)]
struct Region(*mut u8);

// SAFETY: every thread works on a disjoint range of the region.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

fn memtouch(args: &[String], started: Instant) -> io::Result<()> {
    let flag = |name: &str| args.iter().any(|argument| argument == name);
    let megabytes = option_value(args, "--mb").and_then(|text| text.parse::<usize>().ok()).unwrap_or(1024);
    let (huge, populate, unmap_in_threads) = (flag("--thp"), flag("--populate"), flag("--munmap"));
    let threads = option_value(args, "--threads")
        .and_then(|text| text.parse::<usize>().ok())
        .or_else(|| env::var("NCPU").ok().and_then(|text| text.parse::<usize>().ok()))
        .filter(|&count| count > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    let length = megabytes << 20;
    let (address, advised) = anonymous(length, huge, populate)?;
    let region = Region(address);
    let unmapped = AtomicUsize::new(0);
    // Whole 2 MiB blocks per thread, so no two threads fault inside the same huge page.
    let share = (length / threads).next_multiple_of(HUGE_PAGE);
    if !populate || unmap_in_threads {
        std::thread::scope(|scope| {
            for index in 0..threads {
                let unmapped = &unmapped;
                scope.spawn(move || {
                    let region = region;
                    let begin = (index * share).min(length);
                    let end = ((index + 1) * share).min(length);
                    if !populate {
                        let mut at = begin;
                        while at < end {
                            // SAFETY: `at` lies in this thread's part of the mapping.
                            unsafe { std::ptr::write_volatile(region.0.add(at), 1) };
                            at += 4096;
                        }
                    }
                    // SAFETY: this thread's own part, not used after this.
                    if unmap_in_threads && end > begin && unmap(unsafe { region.0.add(begin) }, end - begin) {
                        unmapped.fetch_add(end - begin, Ordering::Relaxed);
                    }
                });
            }
        });
    }
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    // How much the kernel really backed with huge pages. Only read when asked for huge pages:
    // walking a gigabyte of 4 KiB page tables here would count towards the exit cost.
    let huge_kb = if huge {
        fs::read_to_string("/proc/self/smaps_rollup")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("AnonHugePages:"))
                    .and_then(|line| line.split_whitespace().nth(1).map(str::to_string))
            })
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        "unread".to_string()
    };
    println!(
        "DIAG memtouch mb={megabytes} thp_requested={} thp_advised={} populate={} munmap_mb={} anon_huge_kb={huge_kb} threads={threads} ms={elapsed_ms:.3}",
        u8::from(huge),
        u8::from(advised),
        u8::from(populate),
        unmapped.load(Ordering::Relaxed) >> 20,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// sysinfo
// ---------------------------------------------------------------------------

fn show_file(key: &str, path: &str) {
    match fs::read_to_string(path) {
        Ok(text) => println!("SYSINFO {key}: {}", text.trim().replace('\n', " | ")),
        Err(error) => println!("SYSINFO {key}: <{}>", error.kind()),
    }
}

fn show_lines(key: &str, path: &str, wanted: &[&str]) {
    match fs::read_to_string(path) {
        Ok(text) => {
            for line in text.lines() {
                if wanted.iter().any(|prefix| line.starts_with(prefix)) {
                    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
                    println!("SYSINFO {key}: {line}");
                }
            }
        }
        Err(error) => println!("SYSINFO {key}: <{}>", error.kind()),
    }
}

fn sysinfo() {
    println!(
        "SYSINFO available_parallelism: {}",
        std::thread::available_parallelism().map_or(0, usize::from)
    );
    for name in ["DATA_DIR", "OUTPUT_DIR", "SCRATCH_DIR", "HOME", "NCPU", "MEM_GB", "SS_TIMING"] {
        match env::var(name) {
            Ok(value) => println!("SYSINFO env.{name}: {value}"),
            Err(_) => println!("SYSINFO env.{name}: <unset>"),
        }
    }
    if let Ok(text) = fs::read_to_string("/proc/cpuinfo") {
        let processors = text.lines().filter(|line| line.starts_with("processor")).count();
        println!("SYSINFO cpuinfo.processors: {processors}");
        if let Some(model) = text.lines().find(|line| line.starts_with("model name")) {
            println!("SYSINFO cpuinfo.model: {}", model.split(':').nth(1).unwrap_or("").trim());
        }
        if let Some(flags) = text.lines().find(|line| line.starts_with("flags")) {
            let flags: Vec<&str> = flags.split(':').nth(1).unwrap_or("").split_whitespace().collect();
            let interesting = [
                "sse4_2", "popcnt", "avx", "avx2", "bmi1", "bmi2", "fma", "pclmulqdq", "vpclmulqdq", "avx512f",
                "avx512bw", "avx512vl", "avx512dq", "avx512cd", "avx512_vbmi", "avx512_vbmi2", "avx512_vnni",
                "avx512_bitalg", "avx512_vpopcntdq", "gfni", "vaes", "sha_ni", "erms", "fsrm", "hypervisor",
            ];
            let present: Vec<&str> = interesting.iter().copied().filter(|flag| flags.contains(flag)).collect();
            let absent: Vec<&str> = interesting.iter().copied().filter(|flag| !flags.contains(flag)).collect();
            println!("SYSINFO cpuinfo.flags_present: {}", present.join(" "));
            println!("SYSINFO cpuinfo.flags_absent: {}", absent.join(" "));
        }
    }
    show_lines(
        "meminfo",
        "/proc/meminfo",
        &["MemTotal", "MemAvailable", "SwapTotal", "AnonHugePages", "HugePages_Total", "Hugepagesize"],
    );
    for (key, path) in [
        ("cgroup.cpu.max", "/sys/fs/cgroup/cpu.max"),
        ("cgroup.cpu.weight", "/sys/fs/cgroup/cpu.weight"),
        ("cgroup.cpuset.cpus.effective", "/sys/fs/cgroup/cpuset.cpus.effective"),
        ("cgroup.memory.max", "/sys/fs/cgroup/memory.max"),
        ("cgroup.memory.high", "/sys/fs/cgroup/memory.high"),
        ("cgroup.memory.swap.max", "/sys/fs/cgroup/memory.swap.max"),
        ("cgroup.pids.max", "/sys/fs/cgroup/pids.max"),
        ("thp.enabled", "/sys/kernel/mm/transparent_hugepage/enabled"),
        ("thp.defrag", "/sys/kernel/mm/transparent_hugepage/defrag"),
        ("thp.shmem_enabled", "/sys/kernel/mm/transparent_hugepage/shmem_enabled"),
        ("thp.hpage_pmd_size", "/sys/kernel/mm/transparent_hugepage/hpage_pmd_size"),
        ("vm.overcommit_memory", "/proc/sys/vm/overcommit_memory"),
        ("vm.max_map_count", "/proc/sys/vm/max_map_count"),
        ("vm.swappiness", "/proc/sys/vm/swappiness"),
        ("cpu.online", "/sys/devices/system/cpu/online"),
        ("cpu.smt_active", "/sys/devices/system/cpu/smt/active"),
        ("cpu0.thread_siblings", "/sys/devices/system/cpu/cpu0/topology/thread_siblings_list"),
        ("kernel", "/proc/version"),
    ] {
        show_file(key, path);
    }
    for index in 0..4 {
        let base = format!("/sys/devices/system/cpu/cpu0/cache/index{index}");
        let read = |name: &str| fs::read_to_string(format!("{base}/{name}")).map(|text| text.trim().to_string());
        if let (Ok(level), Ok(kind), Ok(size)) = (read("level"), read("type"), read("size")) {
            let shared = read("shared_cpu_list").unwrap_or_default();
            println!("SYSINFO cache.L{level}.{kind}: {size} shared_with_cpus={shared}");
        }
    }
    show_lines(
        "status",
        "/proc/self/status",
        &["Uid", "Gid", "Cpus_allowed_list", "CapEff", "NoNewPrivs", "Seccomp:"],
    );
    show_lines("limits", "/proc/self/limits", &["Max open files", "Max locked memory", "Max processes"]);
    if let Ok(text) = fs::read_to_string("/proc/self/mountinfo") {
        for line in text.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let Some(&point) = fields.get(4) else { continue };
            if !["/", "/data", "/output", "/scratch", "/tmp", "/dev/shm"].contains(&point) {
                continue;
            }
            // mountinfo: id parent major:minor root mount-point options [optional...] - fstype source super-options
            let options = fields.get(5).copied().unwrap_or("");
            let after: Vec<&str> = line.split(" - ").nth(1).unwrap_or("").split(' ').collect();
            let fstype = after.first().copied().unwrap_or("");
            let super_options = after.get(2).copied().unwrap_or("");
            println!("SYSINFO mount {point}: type={fstype} options={options} super={super_options}");
        }
    }
}

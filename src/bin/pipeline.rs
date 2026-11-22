//! The scored entry point: the optimized engine, with the simple reference
//! pipeline as the fallback for inputs the engine declines.

use std::path::Path;
use std::process::ExitCode;

/// The system allocator, asking for transparent huge pages on every large
/// block. On the CI runners a gigabyte of fresh memory in 4 KiB pages costs
/// 100 to 150 ms more container time than in 2 MiB pages (page faults while
/// it is filled, then teardown at exit: about 90 ms per GiB of 4 KiB pages
/// against about 7 ms per GiB of huge pages, measured with a test container).
/// Blocks of 1 MiB or more get a mapping of their own, 2 MiB aligned and a
/// whole number of huge pages long, so that no part of them stays in 4 KiB
/// pages (with the system allocator the unaligned head and tail of every
/// large block did, and blocks under 4 MiB were not advised at all: 76 MB of
/// a 621 MB peak at 20M). `SS_ALIGN=0` keeps the system allocator for them;
/// `SS_THP=0` turns every hint off, for A/B runs.
#[cfg(target_os = "linux")]
mod huge_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU8, Ordering};

    const HUGE_PAGE: usize = 2 << 20;
    /// Smallest block worth a hint: it must contain a whole aligned huge page.
    const SMALLEST: usize = 2 * HUGE_PAGE;
    /// From this size on a block has a mapping of its own.
    const OWN_MAPPING: usize = 1 << 20;
    const MADV_HUGEPAGE: i32 = 14;
    const PROT_READ_WRITE: i32 = 3;
    const MAP_PRIVATE_ANONYMOUS: i32 = 0x22;
    const MREMAP_MAYMOVE: i32 = 1;

    extern "C" {
        fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn mremap(old_address: *mut u8, old_size: usize, new_size: usize, flags: i32, ...) -> *mut u8;
    }

    /// 0 = not read yet; else 0x80 | bit 0 (hints on) | bit 1 (own mappings on).
    static STATE: AtomicU8 = AtomicU8::new(0);

    pub struct HugeAlloc;

    #[cold]
    fn read_state() -> u8 {
        // `var_os` allocates small blocks, and small blocks never reach this
        // (`own` and `hint` look at the size first), so the allocator does
        // not re-enter it.
        let thp = !matches!(std::env::var_os("SS_THP"), Some(value) if value == "0");
        let own = thp && !matches!(std::env::var_os("SS_ALIGN"), Some(value) if value == "0");
        let state = 0x80 | u8::from(thp) | u8::from(own) << 1;
        STATE.store(state, Ordering::Relaxed);
        state
    }

    #[inline]
    fn state() -> u8 {
        match STATE.load(Ordering::Relaxed) {
            0 => read_state(),
            state => state,
        }
    }

    /// Whether a block of this layout lives in a mapping of its own. The
    /// state is read once and never changes, so a block's allocation and its
    /// release always agree.
    #[inline]
    fn own(layout: &Layout) -> bool {
        layout.size() >= OWN_MAPPING && layout.align() <= HUGE_PAGE && state() & 2 != 0
    }

    #[inline]
    fn rounded(size: usize) -> usize {
        (size + HUGE_PAGE - 1) & !(HUGE_PAGE - 1)
    }

    /// The whole huge pages inside `[block, block + size)` advised.
    #[inline]
    unsafe fn advise(block: *mut u8, size: usize) {
        let start = (block as usize + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
        let end = (block as usize + size) & !(HUGE_PAGE - 1);
        if end > start {
            madvise(start as *mut u8, end - start, MADV_HUGEPAGE);
        }
    }

    /// A fresh mapping of `rounded(size)` bytes at a 2 MiB boundary, advised
    /// for huge pages. Fresh anonymous memory reads as zeros.
    unsafe fn map_own(size: usize) -> *mut u8 {
        let length = rounded(size);
        let raw = mmap(std::ptr::null_mut(), length + HUGE_PAGE, PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS, -1, 0);
        if raw as isize == -1 {
            return std::ptr::null_mut();
        }
        let start = (raw as usize + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
        let head = start - raw as usize;
        if head != 0 {
            munmap(raw, head);
        }
        munmap((start + length) as *mut u8, HUGE_PAGE - head);
        madvise(start as *mut u8, length, MADV_HUGEPAGE);
        start as *mut u8
    }

    #[inline]
    unsafe fn hint(block: *mut u8, size: usize) {
        if size < SMALLEST || block.is_null() {
            return;
        }
        if state() & 1 == 0 {
            return;
        }
        advise(block, size);
    }

    // SAFETY: a block of `own` size is a mapping of its own, made by
    // `map_own` (or moved by `mremap`), `rounded(size)` bytes long, released
    // with `munmap` of that length; every other call goes to the system
    // allocator unchanged, and the advice is only a hint about pages that
    // belong to the block.
    unsafe impl GlobalAlloc for HugeAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if own(&layout) {
                return map_own(layout.size());
            }
            let block = System.alloc(layout);
            hint(block, layout.size());
            block
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if own(&layout) {
                return map_own(layout.size());
            }
            let block = System.alloc_zeroed(layout);
            hint(block, layout.size());
            block
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            if own(&layout) {
                munmap(block, rounded(layout.size()));
                return;
            }
            System.dealloc(block, layout);
        }

        unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
            match (own(&layout), own(&new_layout)) {
                (false, false) => {
                    let block = System.realloc(block, layout, new_size);
                    hint(block, new_size);
                    block
                }
                (true, true) => {
                    let (old_length, new_length) = (rounded(layout.size()), rounded(new_size));
                    if new_length == old_length {
                        return block;
                    }
                    // In place when the address space after it is free (and
                    // always when it shrinks); otherwise the kernel moves the
                    // pages without copying them. A moved block may lose its
                    // 2 MiB alignment: its whole huge pages are advised again.
                    let moved = mremap(block, old_length, new_length, MREMAP_MAYMOVE);
                    if moved as isize == -1 {
                        return std::ptr::null_mut();
                    }
                    advise(moved, new_length);
                    moved
                }
                _ => {
                    let fresh = self.alloc(new_layout);
                    if !fresh.is_null() {
                        std::ptr::copy_nonoverlapping(block, fresh, layout.size().min(new_size));
                        self.dealloc(block, layout);
                    }
                    fresh
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: huge_alloc::HugeAlloc = huge_alloc::HugeAlloc;

/// Removes Parquet files a failed attempt may have left behind.
fn clear_outputs(output: &Path) {
    for table in ["user_daily", "campaign_daily"] {
        if let Ok(entries) = std::fs::read_dir(output.join(table)) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Removes the `*.parquet` files already in the two table folders. Every
/// Parquet file in a table's folder counts as part of the table, and the
/// engine's file names differ from run to run (one file per worker that had
/// rows), so a file left by an earlier run into the same folder would be read
/// as rows of this one. A folder that does not exist costs one failed call.
fn remove_earlier_tables(output: &Path) {
    for table in ["user_daily", "campaign_daily"] {
        if let Ok(entries) = std::fs::read_dir(output.join(table)) {
            for entry in entries.flatten() {
                let path = entry.path();
                // Any case: a case-insensitive file system or loader would read PART-9.PARQUET too.
                if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("parquet")) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

fn main() -> ExitCode {
    ss::reference::open_umask();
    let (data, output) = ss::reference::directories_from_environment();
    let threads = ss::reference::thread_count();
    remove_earlier_tables(&output);
    let use_reference = std::env::var_os("SS_ENGINE").is_some_and(|engine| engine == "reference");
    let fast = if use_reference { Ok(false) } else { ss::fast::run(&data, &output, threads) };
    let result = match fast {
        Ok(true) => Ok(()),
        Ok(false) => ss::reference::run(&data, &output, threads),
        Err(error) => {
            eprintln!("pipeline: optimized engine failed ({error}); using the reference pipeline");
            clear_outputs(&output);
            ss::reference::run(&data, &output, threads)
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pipeline: {error}");
            ExitCode::FAILURE
        }
    }
}

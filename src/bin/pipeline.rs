//! The scored entry point: the optimized engine, with the simple reference
//! pipeline as the fallback for inputs the engine declines.

use std::path::Path;
use std::process::ExitCode;

/// The system allocator, asking for transparent huge pages on every large
/// block. On the CI runners a gigabyte of fresh memory in 4 KiB pages costs
/// 100 to 150 ms more container time than in 2 MiB pages (page faults while
/// it is filled, then teardown at exit), and a third of the engine's memory
/// was still in small pages: the big vectors outside the arenas.
/// `SS_THP=0` turns the hint off, for A/B runs.
#[cfg(target_os = "linux")]
mod huge_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU8, Ordering};

    const HUGE_PAGE: usize = 2 << 20;
    /// Smallest block worth a hint: it must contain a whole aligned huge page.
    const SMALLEST: usize = 2 * HUGE_PAGE;
    const MADV_HUGEPAGE: i32 = 14;

    extern "C" {
        fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    }

    /// 0 = not read yet, 1 = on, 2 = off.
    static STATE: AtomicU8 = AtomicU8::new(0);

    pub struct HugeAlloc;

    #[cold]
    fn read_state() -> u8 {
        // `var_os` allocates a small block, and small blocks return from
        // `hint` before reaching this, so the allocator does not re-enter it.
        let off = matches!(std::env::var_os("SS_THP"), Some(value) if value == "0");
        let state = if off { 2 } else { 1 };
        STATE.store(state, Ordering::Relaxed);
        state
    }

    #[inline]
    unsafe fn hint(block: *mut u8, size: usize) {
        if size < SMALLEST || block.is_null() {
            return;
        }
        let state = match STATE.load(Ordering::Relaxed) {
            0 => read_state(),
            state => state,
        };
        if state != 1 {
            return;
        }
        // The whole huge pages inside the block.
        let start = (block as usize + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
        let end = (block as usize + size) & !(HUGE_PAGE - 1);
        if end > start {
            madvise(start as *mut u8, end - start, MADV_HUGEPAGE);
        }
    }

    // SAFETY: every call goes to the system allocator unchanged; the advice
    // is only a hint about pages that belong to the returned block.
    unsafe impl GlobalAlloc for HugeAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let block = System.alloc(layout);
            hint(block, layout.size());
            block
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let block = System.alloc_zeroed(layout);
            hint(block, layout.size());
            block
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            System.dealloc(block, layout);
        }

        unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let block = System.realloc(block, layout, new_size);
            hint(block, new_size);
            block
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

//! The scored entry point: the optimized engine, with the simple reference
//! pipeline as the fallback for inputs the engine declines.

use std::path::Path;
use std::process::ExitCode;

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

fn main() -> ExitCode {
    ss::reference::open_umask();
    let (data, output) = ss::reference::directories_from_environment();
    let threads = ss::reference::thread_count();
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

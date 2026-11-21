//! The simple reference implementation as a standalone binary.

use std::process::ExitCode;

fn main() -> ExitCode {
    ss::reference::open_umask();
    let (data, output) = ss::reference::directories_from_environment();
    match ss::reference::run(&data, &output, ss::reference::thread_count()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reference: {error}");
            ExitCode::FAILURE
        }
    }
}

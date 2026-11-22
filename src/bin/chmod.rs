//! A minimal `chmod -R a+rwX PATH...` for the otherwise empty image.
//!
//! The organizers' `scripts/docker_run.sh` runs `chmod -R a+rwX /output /scratch`
//! inside the image (as uid 1000) so that the host user can delete the run's
//! files afterwards. Only that form is supported; any other mode is an error.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

#[cfg(unix)]
fn open_up(path: &Path, recursive: bool) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = fs::symlink_metadata(path) else { return };
    if metadata.file_type().is_symlink() {
        return;
    }
    let mode = metadata.permissions().mode();
    // `X`: execute only for directories and for files that are already executable.
    let execute = if metadata.is_dir() || mode & 0o111 != 0 { 0o111 } else { 0 };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o666 | execute));
    if recursive && metadata.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                open_up(&entry.path(), true);
            }
        }
    }
}

#[cfg(not(unix))]
fn open_up(_path: &Path, _recursive: bool) {}

fn main() -> ExitCode {
    let mut recursive = false;
    let mut mode: Option<String> = None;
    let mut paths = Vec::new();
    for argument in std::env::args().skip(1) {
        if argument.starts_with('-') && mode.is_none() {
            recursive |= argument.contains('R');
        } else if mode.is_none() {
            mode = Some(argument);
        } else {
            paths.push(argument);
        }
    }
    if mode.as_deref() != Some("a+rwX") {
        eprintln!("chmod: only `chmod [-R] a+rwX PATH...` is supported");
        return ExitCode::from(2);
    }
    for path in &paths {
        open_up(Path::new(path), recursive);
    }
    // Keep the directory listing code from being flagged as unused off unix.
    let _ = fs::metadata(".");
    ExitCode::SUCCESS
}

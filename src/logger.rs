//! Console output plus a small rotating log file.
//!
//! A run that finds nothing to do writes nothing to disk; only changes and
//! real errors are recorded.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::win;

const MAX_LOG_BYTES: u64 = 256 * 1024;

static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

pub fn init(path: PathBuf) {
    let _ = LOG_FILE.set(path);
}

/// Prints to the console (when there is one) without touching the log file.
pub fn say(message: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

/// Prints and records a change.
pub fn info(message: &str) {
    say(message);
    record(message);
}

/// Prints and records a failure.
pub fn error(message: &str) {
    say(&format!("error: {message}"));
    record(&format!("ERROR {message}"));
}

fn record(line: &str) {
    let Some(path) = LOG_FILE.get() else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.old"));
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}  {}", win::local_timestamp(), line);
    }
}

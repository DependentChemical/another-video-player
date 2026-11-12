//! Diagnostics that survive a windowed application.
//!
//! On Windows the player is a graphical program: it has no console window, so
//! everything that is written to the standard error is lost. The notes are
//! therefore also written to a log file next to the program, which is where a
//! user can look when something goes wrong.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The file the notes are written to.
static LOG_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Starts writing the notes to a file.
///
/// The file is created when it does not exist yet. When it cannot be written
/// to, the notes simply go to the standard error and nowhere else.
pub fn init(log_file: &Path) {
    let _ = LOG_FILE.set(Some(log_file.to_path_buf()));
}

/// Writes a note.
///
/// The note always reaches the standard error. It also reaches the log file
/// when one was set up.
pub fn note(message: &str) {
    eprintln!("flatline: {message}");
    let Some(path) = LOG_FILE.get().and_then(|path| path.as_ref()) else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", timestamped(message));
    }
}

/// The note with a time stamp in front of it.
fn timestamped(message: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    format!("[{now}] {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_carries_a_time_stamp() {
        let note = timestamped("hello");
        assert!(note.ends_with("hello"));
        assert!(note.starts_with('['));
    }
}

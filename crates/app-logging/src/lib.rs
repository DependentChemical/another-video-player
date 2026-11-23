//! Flatline's single log queue and asynchronous log file writer.
//!
//! # Layer 0 of the isolation ladder
//!
//! This crate is shared infrastructure, like the media vocabulary. It owns one
//! capability: *turning log calls into appended log lines without blocking the
//! caller*. Every crate may depend on it, and it depends on no other Flatline
//! crate.
//!
//! A call to [`log`] only formats the message and places it on a channel. A
//! single background thread owns the log file, drains that channel and writes
//! the records one at a time. Callers therefore never wait for the filesystem,
//! and two background workers can never interleave their writes.
//!
//! The threshold is set once by the composition root from `settings.json`;
//! [`set_level`] can change it later without restarting the writer.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

/// How important a log record is.
///
/// The order is the filtering order: [`Level::Error`] is always written,
/// [`Level::Debug`] only when the configured threshold is [`Level::Debug`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Detailed tracing that is only useful while investigating a problem.
    Debug,
    /// Normal application events: startup, playback, updates and queue work.
    Notice,
    /// Something recoverable went wrong, or a retry had to be scheduled.
    Warning,
    /// Something failed and the user may not get what was asked for.
    Error,
}

impl Level {
    /// The stable name written into the log file.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Notice => "NOTICE",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }

    /// Parses a level name, ignoring letter case.
    ///
    /// `WARN` is accepted as an alias of [`Level::WARNING`], because it is the
    /// spelling many configuration files use.
    pub fn parse(name: &str) -> Option<Level> {
        match name.trim().to_ascii_uppercase().as_str() {
            "DEBUG" => Some(Level::Debug),
            "NOTICE" | "INFO" => Some(Level::Notice),
            "WARNING" | "WARN" => Some(Level::Warning),
            "ERROR" => Some(Level::Error),
            _ => None,
        }
    }

    /// The numeric rank used by the filter.
    fn rank(self) -> u8 {
        match self {
            Level::Debug => 0,
            Level::Notice => 1,
            Level::Warning => 2,
            Level::Error => 3,
        }
    }

    /// The level that a stored rank describes, when it is known.
    fn from_rank(rank: u8) -> Level {
        match rank {
            0 => Level::Debug,
            2 => Level::Warning,
            3 => Level::Error,
            _ => Level::Notice,
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One queued log line.
#[derive(Debug)]
struct Record {
    /// The moment the caller logged the message.
    at: SystemTime,
    /// The importance of the message.
    level: Level,
    /// The message itself.
    message: String,
}

impl Record {
    /// Builds a record stamped with the current time.
    fn now(level: Level, message: String) -> Record {
        Record { at: SystemTime::now(), level, message }
    }

    /// The complete line, including its time stamp and level.
    fn line(&self) -> String {
        format!("[{}] [{}] {}", timestamp(self.at), self.level.as_str(), self.message)
    }
}

/// The global logger, installed once by [`init`].
struct Global {
    /// The queue the log file thread reads from.
    sender: Sender<Record>,
    /// The threshold, stored as a [`Level::rank`].
    level: AtomicU8,
}

/// The process wide logger.
static LOGGER: OnceLock<Global> = OnceLock::new();

/// The size in bytes past which the log file is cleared on startup.
const MAX_LOG_FILE_BYTES: u64 = 1024 * 1024;

/// Starts the log queue and its background writer.
///
/// The file is created when it is missing and appended to otherwise. A file
/// already holding more than [`MAX_LOG_FILE_BYTES`] is cleared first, so it
/// cannot inflate without bound. Its parent directory is created when
/// needed. Calling `init` again only changes the threshold; the original
/// writer thread keeps owning the file.
///
/// A file that cannot be opened is not fatal: the writer falls back to the
/// standard error stream and logs the reason there, so a graphical build
/// without a console still starts.
pub fn init(path: impl AsRef<Path>, level: Level) {
    if let Some(global) = LOGGER.get() {
        global.level.store(level.rank(), Ordering::Relaxed);
        let _ = global
            .sender
            .send(Record::now(Level::Notice, format!("log level changed to {}", level.as_str())));
        return;
    }

    let path = path.as_ref();
    clear_oversized_log(path);
    let sink = Sink::open(path);
    let (sender, receiver) = channel();
    match spawn_writer(sink, receiver) {
        Ok(_thread) => {
            let _ = LOGGER.set(Global { sender, level: AtomicU8::new(level.rank()) });
        }
        Err(error) => emergency(&format!("the log writer thread could not be started: {error}")),
    }
}

/// Empties the log file when it already holds more than [`MAX_LOG_FILE_BYTES`].
///
/// A log that keeps growing across runs inflates the file without limit, so
/// startup clears it. A missing or small file is left untouched.
fn clear_oversized_log(path: &Path) {
    let Ok(len) = std::fs::metadata(path).map(|metadata| metadata.len()) else {
        return;
    };
    if len > MAX_LOG_FILE_BYTES {
        let _ = OpenOptions::new().write(true).truncate(true).open(path);
    }
}

/// Changes the threshold of the running logger.
///
/// This is a no-op before [`init`]; use the level passed to `init` then.
pub fn set_level(level: Level) {
    if let Some(global) = LOGGER.get() {
        global.level.store(level.rank(), Ordering::Relaxed);
    }
}

/// The threshold the running logger currently uses.
///
/// Before [`init`] this reports [`Level::Notice`], the same default the
/// bundled settings file uses.
pub fn level() -> Level {
    LOGGER
        .get()
        .map_or(Level::Notice, |global| Level::from_rank(global.level.load(Ordering::Relaxed)))
}

/// Whether a message of `level` would be written right now.
///
/// Callers that build an expensive message can use this to skip the work.
pub fn enabled(level: Level) -> bool {
    LOGGER.get().is_some_and(|global| level.rank() >= global.level.load(Ordering::Relaxed))
}

/// Places one log message on the queue.
///
/// The function returns immediately. The message is filtered against the
/// configured threshold, then formatted once and handed to the single writer
/// thread. Records are written in the order they were queued.
pub fn log(level: Level, message: impl AsRef<str>) {
    let Some(global) = LOGGER.get() else {
        return;
    };
    if level.rank() < global.level.load(Ordering::Relaxed) {
        return;
    }
    let _ = global.sender.send(Record::now(level, message.as_ref().to_owned()));
}

/// Queues a [`Level::Debug`] message.
pub fn debug(message: impl AsRef<str>) {
    log(Level::Debug, message);
}

/// Queues a [`Level::Notice`] message.
pub fn notice(message: impl AsRef<str>) {
    log(Level::Notice, message);
}

/// Queues a [`Level::Warning`] message.
pub fn warning(message: impl AsRef<str>) {
    log(Level::Warning, message);
}

/// Queues a [`Level::Error`] message.
pub fn error(message: impl AsRef<str>) {
    log(Level::Error, message);
}

/// Where the writer thread sends its records.
enum Sink {
    /// The configured log file, kept open by the writer thread.
    File(BufWriter<File>),
    /// The fallback used when the log file cannot be opened.
    Stderr,
}

impl Sink {
    /// Opens the log file, falling back to the standard error stream.
    fn open(path: &Path) -> Sink {
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            emergency(&format!(
                "the log directory {} could not be created: {error}",
                parent.display()
            ));
            return Sink::Stderr;
        }
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Sink::File(BufWriter::new(file)),
            Err(error) => {
                emergency(&format!("the log file {} could not be opened: {error}", path.display()));
                Sink::Stderr
            }
        }
    }

    /// Appends one record and flushes it so a crash cannot lose it.
    fn write(&mut self, record: &Record) {
        match self {
            Sink::File(file) => {
                let _ = writeln!(file, "{}", record.line());
                let _ = file.flush();
            }
            Sink::Stderr => {
                let _ = writeln!(std::io::stderr().lock(), "{}", record.line());
            }
        }
    }
}

/// Starts the thread that owns the sink and drains the queue.
fn spawn_writer(mut sink: Sink, receiver: Receiver<Record>) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("flatline-log".to_string()).spawn(move || {
        for record in receiver {
            sink.write(&record);
        }
    })
}

/// Writes a setup problem before a logger exists.
fn emergency(message: &str) {
    let _ = writeln!(std::io::stderr().lock(), "app-logging: {message}");
}

/// Formats a moment as an RFC 3339-style UTC timestamp with milliseconds.
fn timestamp(at: SystemTime) -> String {
    let duration = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs().min(i64::MAX as u64) as i64;
    let millis = duration.subsec_millis();
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = of_day / 3_600;
    let minute = (of_day % 3_600) / 60;
    let second = of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Converts days since 1970-01-01 into a civil date.
///
/// This is Howard Hinnant's `civil_from_days` algorithm, which avoids a
/// calendar dependency for the one date operation the log format needs.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 };
    if month <= 2 {
        year += 1;
    }
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn levels_order_from_most_verbose_to_loudest() {
        assert!(Level::Debug < Level::Notice);
        assert!(Level::Notice < Level::Warning);
        assert!(Level::Warning < Level::Error);
    }

    #[test]
    fn level_names_round_trip_and_accept_warn() {
        for level in [Level::Debug, Level::Notice, Level::Warning, Level::Error] {
            assert_eq!(Level::parse(level.as_str()), Some(level));
            assert_eq!(Level::parse(&level.as_str().to_ascii_lowercase()), Some(level));
        }
        assert_eq!(Level::parse("warn"), Some(Level::Warning));
        assert_eq!(Level::parse("info"), Some(Level::Notice));
        assert_eq!(Level::parse("verbose"), None);
    }

    #[test]
    fn timestamps_are_utc_with_milliseconds() {
        let epoch = UNIX_EPOCH;
        assert_eq!(timestamp(epoch), "1970-01-01T00:00:00.000Z");

        let later = epoch + Duration::from_secs(1_700_000_000);
        assert_eq!(timestamp(later), "2023-11-14T22:13:20.000Z");

        let with_millis = epoch + Duration::from_millis(1_700_000_000_123);
        assert_eq!(timestamp(with_millis), "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn a_record_carries_its_level_and_message() {
        let record = Record {
            at: UNIX_EPOCH,
            level: Level::Warning,
            message: "something to say".to_string(),
        };
        let line = record.line();
        assert!(line.starts_with("[1970-01-01T00:00:00.000Z] [WARNING] "));
        assert!(line.ends_with("something to say"));
    }

    #[test]
    fn the_writer_drains_every_queued_record_in_order() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("flatline-logging-{}-{unique}.log", std::process::id()));
        let sink = Sink::open(&path);
        let (sender, receiver) = channel::<Record>();
        let writer = spawn_writer(sink, receiver).expect("the writer thread starts");

        sender
            .send(Record::now(Level::Notice, "first".to_string()))
            .expect("the queue accepts the first record");
        sender
            .send(Record::now(Level::Error, "second".to_string()))
            .expect("the queue accepts the second record");
        drop(sender);
        writer.join().expect("the writer thread stops after the queue closes");

        let contents = std::fs::read_to_string(&path).expect("the log file can be read");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("[NOTICE] first"));
        assert!(lines[1].contains("[ERROR] second"));
        let _ = std::fs::remove_file(&path);
    }

    fn temp_log_path(tag: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("flatline-logging-{tag}-{}-{unique}.log", std::process::id()))
    }

    #[test]
    fn an_oversized_log_file_is_emptied_on_startup() {
        let path = temp_log_path("oversized");
        std::fs::write(&path, vec![b'x'; MAX_LOG_FILE_BYTES as usize + 1])
            .expect("the oversized log file is written");

        clear_oversized_log(&path);

        assert_eq!(std::fs::metadata(&path).expect("the log file still exists").len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_log_file_at_the_limit_is_kept_on_startup() {
        let path = temp_log_path("at-limit");
        std::fs::write(&path, vec![b'x'; MAX_LOG_FILE_BYTES as usize])
            .expect("the log file is written");

        clear_oversized_log(&path);

        assert_eq!(
            std::fs::metadata(&path).expect("the log file still exists").len(),
            MAX_LOG_FILE_BYTES
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_log_file_is_left_alone_on_startup() {
        let path = temp_log_path("missing");
        clear_oversized_log(&path);
        assert!(!path.exists());
    }
}

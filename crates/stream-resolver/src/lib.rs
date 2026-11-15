//! Resolving a media URL into a catalog of playable streams.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns exactly one capability: *turning a URL into a
//! [`MediaSource`]* and *deciding which streams to play*. It learns the
//! location of `yt-dlp` from the caller, so it never has to know how tools
//! are installed or updated.
//!
//! Nothing in here knows about playback, rendering or the user interface.

pub mod parse;
pub mod plan;

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use media_model::MediaSource;

/// A resolver process must not leave the UI waiting indefinitely.
const RESOLUTION_TIMEOUT: Duration = Duration::from_secs(45);

pub use parse::parse_catalog;
pub use plan::plan;

/// Everything that can go wrong while resolving a URL.
#[derive(Debug)]
pub enum Error {
    /// The resolver could not be started.
    Spawn(std::io::Error),
    /// `yt-dlp` ran but failed.
    Failed {
        /// Exit code, when the process was terminated by one.
        code: Option<i32>,
        /// Whatever `yt-dlp` wrote to stderr.
        stderr: String,
    },
    /// The output of `yt-dlp` could not be understood.
    Json(serde_json::Error),
    /// The document contained no media at all.
    Empty,
    /// The chosen stream identifier does not exist.
    UnknownStream(String),
    /// The selection names neither a video nor an audio stream.
    NoStream,
    /// The resolver process did not finish before its deadline.
    Timeout(u64),
    /// The resolver's captured output could not be read.
    Output(io::Error),
    /// A resolver output reader stopped unexpectedly.
    OutputReaderPanicked,
    /// Captured output was missing from a resolver process.
    MissingOutputPipe,
}

/// How to invoke `yt-dlp`.
///
/// The caller decides *which* executable is used; the resolver only adds the
/// arguments it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    program: PathBuf,
    interpreter: Option<PathBuf>,
    youtube_cookies: Option<PathBuf>,
}

impl CommandSpec {
    /// A spec that runs `program` directly.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), interpreter: None, youtube_cookies: None }
    }

    /// A spec that runs `program` through an interpreter.
    pub fn with_interpreter(program: impl Into<PathBuf>, interpreter: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            interpreter: Some(interpreter.into()),
            youtube_cookies: None,
        }
    }

    /// Supplies a Netscape cookie file for YouTube URLs only.
    pub fn with_youtube_cookies(mut self, path: impl Into<PathBuf>) -> Self {
        self.youtube_cookies = Some(path.into());
        self
    }

    /// Builds the command, without any of the resolver's own arguments.
    pub fn command(&self) -> Command {
        let command = match &self.interpreter {
            Some(interpreter) => {
                let mut command = Command::new(interpreter);
                command.arg(&self.program);
                command
            }
            None => Command::new(&self.program),
        };
        #[cfg(windows)]
        let command = {
            let mut command = command;
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
            command
        };
        command
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Spawn(error) => write!(f, "yt-dlp could not be started: {error}"),
            Error::Failed { code, stderr } => match code {
                Some(code) => write!(f, "yt-dlp failed ({code}): {stderr}"),
                None => write!(f, "yt-dlp failed: {stderr}"),
            },
            Error::Json(error) => write!(f, "yt-dlp returned something unexpected: {error}"),
            Error::Empty => write!(f, "the URL did not lead to any media"),
            Error::UnknownStream(name) => write!(f, "the stream {name} is not available"),
            Error::NoStream => write!(f, "no stream was selected"),
            Error::Timeout(seconds) => {
                write!(f, "yt-dlp did not respond within {seconds} seconds")
            }
            Error::Output(error) => write!(f, "yt-dlp output could not be read: {error}"),
            Error::OutputReaderPanicked => write!(f, "yt-dlp output reader stopped unexpectedly"),
            Error::MissingOutputPipe => write!(f, "yt-dlp output could not be captured"),
        }
    }
}

impl std::error::Error for Error {}

/// Whether a URL targets a YouTube hostname, including its short-link host.
fn is_youtube_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = host.strip_suffix(":443").or_else(|| host.strip_suffix(":80")).unwrap_or(host);

    host.eq_ignore_ascii_case("youtube.com")
        || host.to_ascii_lowercase().ends_with(".youtube.com")
        || host.eq_ignore_ascii_case("youtu.be")
}

/// Resolves URLs with `yt-dlp`.
#[derive(Debug, Clone)]
pub struct Resolver {
    spec: CommandSpec,
}

impl Resolver {
    /// Creates a resolver that uses the given `yt-dlp` invocation.
    pub fn new(spec: CommandSpec) -> Self {
        Self { spec }
    }

    /// Resolves a URL into a [`MediaSource`].
    ///
    /// The raw `yt-dlp` metadata document is returned by
    /// [`parse_catalog`](crate::parse_catalog); this method only adds the
    /// process handling.
    pub fn resolve(&self, url: &str) -> Result<MediaSource, Error> {
        let mut command = self.spec.command();
        command
            .arg("--dump-single-json")
            .arg("--no-warnings")
            .arg("--no-progress")
            .arg("--ignore-config")
            .arg("--socket-timeout")
            .arg("20");
        if let Some(cookies) = self.youtube_cookies_for_url(url) {
            command.arg("--cookies").arg(cookies);
        }
        command.arg(url);
        let output = run_command(command, RESOLUTION_TIMEOUT)?;

        if !output.status.success() {
            return Err(Error::Failed {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        parse_catalog(&String::from_utf8_lossy(&output.stdout))
    }

    /// The cookie file to pass for this URL, if a YouTube file is available.
    fn youtube_cookies_for_url(&self, url: &str) -> Option<&std::path::Path> {
        if is_youtube_url(url) {
            self.spec.youtube_cookies.as_deref().filter(|path| path.is_file())
        } else {
            None
        }
    }
}

/// Captures a resolver process while enforcing a finite deadline.
///
/// The output is drained on two threads while the process runs, so a child
/// that fills its pipe can never deadlock. When the deadline passes the child
/// is killed and the timeout is returned immediately; the readers are left
/// to finish on their own, because a descendant that inherited the pipe may
/// otherwise keep `join` blocked forever.
fn run_command(mut command: Command, timeout: Duration) -> Result<Output, Error> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(Error::Spawn)?;
    let (stdout, stderr) = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => (stdout, stderr),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::MissingOutputPipe);
        }
    };
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Timeout(timeout.as_secs()));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Spawn(error));
            }
        }
    };

    let stdout = join_output(stdout_reader)?;
    let stderr = join_output(stderr_reader)?;
    Ok(Output { status, stdout, stderr })
}

/// Reads one child output stream to completion.
fn read_all(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    Ok(output)
}

/// Collects one output reader after the child exits.
fn join_output(reader: thread::JoinHandle<io::Result<Vec<u8>>>) -> Result<Vec<u8>, Error> {
    reader.join().map_err(|_| Error::OutputReaderPanicked)?.map_err(Error::Output)
}

#[cfg(test)]
mod tests {
    use super::{CommandSpec, Error, Resolver, is_youtube_url, run_command};
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn timeout_test_child() {
        if std::env::var_os("FLATLINE_RESOLVER_TIMEOUT_TEST_CHILD").is_some() {
            thread::sleep(Duration::from_secs(5));
        }
    }

    #[test]
    fn a_hung_resolver_process_is_stopped_at_its_deadline() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let mut command = Command::new(executable);
        command
            .arg("--exact")
            .arg("tests::timeout_test_child")
            .arg("--nocapture")
            .env("FLATLINE_RESOLVER_TIMEOUT_TEST_CHILD", "1");

        let started = Instant::now();
        let result = run_command(command, Duration::from_secs(1));

        assert!(matches!(result, Err(Error::Timeout(1))));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_finished_resolver_process_reports_its_output() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let mut command = Command::new(executable);
        command.arg("--list");

        let output = run_command(command, Duration::from_secs(10)).expect("the output is captured");

        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }

    #[test]
    fn youtube_cookie_matching_is_limited_to_youtube_hosts() {
        for url in [
            "https://youtube.com/watch?v=1",
            "https://www.youtube.com/watch?v=1",
            "https://music.youtube.com/watch?v=1",
            "https://youtu.be/1",
        ] {
            assert!(is_youtube_url(url), "{url}");
        }
        for url in [
            "https://youtube.com.example.org/watch?v=1",
            "https://notyoutube.com/watch?v=1",
            "https://example.org/watch?v=1",
            "file:///video.mp4",
        ] {
            assert!(!is_youtube_url(url), "{url}");
        }
    }

    #[test]
    fn cookies_are_added_only_when_the_youtube_file_exists() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("flatline-cookies-{unique}.txt"));
        let resolver = Resolver::new(CommandSpec::new("yt-dlp").with_youtube_cookies(path.clone()));

        assert_eq!(resolver.youtube_cookies_for_url("https://www.youtube.com/watch?v=1"), None);
        std::fs::write(&path, "# Netscape HTTP Cookie File\n")
            .expect("test cookie file can be created");
        assert_eq!(
            resolver.youtube_cookies_for_url("https://www.youtube.com/watch?v=1"),
            Some(path.as_path())
        );
        assert_eq!(resolver.youtube_cookies_for_url("https://example.org/video"), None);
        std::fs::remove_file(path).expect("test cookie file can be removed");
    }
}

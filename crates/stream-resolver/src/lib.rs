use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use media_model::{MediaSource, Service};

mod parse;
mod plan;

/// A resolver process must not leave the UI waiting indefinitely.
const RESOLUTION_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// The resolver process was cancelled in favour of newer work.
    Cancelled,
}

/// How to invoke `yt-dlp`.
///
/// The caller decides *which* executable is used; the resolver only adds the
/// arguments it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    program: PathBuf,
    interpreter: Option<PathBuf>,
    cookies: Vec<(Service, PathBuf)>,
}

impl CommandSpec {
    /// A spec that runs `program` directly.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), interpreter: None, cookies: Vec::new() }
    }

    /// A spec that runs `program` through an interpreter.
    pub fn with_interpreter(program: impl Into<PathBuf>, interpreter: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), interpreter: Some(interpreter.into()), cookies: Vec::new() }
    }

    /// Supplies a Netscape cookie file for URLs of one service.
    pub fn with_cookies(mut self, service: Service, path: impl Into<PathBuf>) -> Self {
        self.cookies.retain(|(existing, _)| *existing != service);
        self.cookies.push((service, path.into()));
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
            Error::Cancelled => write!(f, "the yt-dlp request was cancelled"),
        }
    }
}

impl std::error::Error for Error {}

/// Whether a URL targets a service whose cookie file the resolver can use.
fn cookie_service(url: &str) -> Option<Service> {
    Service::from_url(url)
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
        self.resolve_with_cancel(url, None)
    }

    /// Resolves a URL into a [`MediaSource`], allowing the caller to cancel the
    /// running `yt-dlp` process and register its process id.
    pub fn resolve_with_cancel(
        &self,
        url: &str,
        cancel: Option<(&AtomicU32, &AtomicBool)>,
    ) -> Result<MediaSource, Error> {
        app_logging::notice(format!("resolving url with yt-dlp: url={url}"));
        let mut command = self.spec.command();
        // yt-dlp on Windows writes stderr in the system ANSI codepage, so
        // non-ASCII characters like the apostrophe in "you're" arrive as
        // invalid UTF-8 and get replaced with U+FFFD. Force UTF-8 output so
        // challenge phrases survive the round-trip intact.
        command.env("PYTHONIOENCODING", "utf-8");
        command
            .arg("--dump-single-json")
            .arg("--no-warnings")
            .arg("--no-progress")
            .arg("--ignore-config")
            .arg("--socket-timeout")
            .arg("20");
        if let Some(cookies) = self.cookies_for_url(url) {
            app_logging::debug(format!(
                "passing cookies to yt-dlp: url={url} cookies={}",
                cookies.display()
            ));
            command.arg("--cookies").arg(cookies);
        }
        command.arg(url);
        let args =
            command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();
        app_logging::debug(format!(
            "running yt-dlp: program={} interpreter={:?} args={args:?}",
            self.spec.program.display(),
            self.spec.interpreter
        ));
        let output = run_command(command, RESOLUTION_TIMEOUT, cancel)?;

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !output.status.success() {
            let code = output.status.code();
            app_logging::error(format!(
                "yt-dlp failed: url={url} status={} code={code:?} stdout_bytes={} stderr={stderr}",
                output.status,
                output.stdout.len()
            ));
            return Err(Error::Failed { code, stderr });
        }

        app_logging::debug(format!(
            "yt-dlp finished: url={url} status={} stdout_bytes={} stderr={}",
            output.status,
            output.stdout.len(),
            if stderr.is_empty() { "<empty>" } else { &stderr }
        ));
        match parse_catalog(&String::from_utf8_lossy(&output.stdout)) {
            Ok(source) => {
                app_logging::notice(format!(
                    "resolved url: url={url} title=\"{}\" video_streams={} audio_streams={} live={}",
                    source.title,
                    source.video_streams.len(),
                    source.audio_streams.len(),
                    source.is_live
                ));
                Ok(source)
            }
            Err(error) => {
                app_logging::error(format!(
                    "yt-dlp output could not be parsed: url={url} error={error}"
                ));
                Err(error)
            }
        }
    }

    /// The cookie file to pass for this URL, when one exists.
    fn cookies_for_url(&self, url: &str) -> Option<&std::path::Path> {
        let service = cookie_service(url)?;
        self.spec
            .cookies
            .iter()
            .find(|(candidate, _)| *candidate == service)
            .map(|(_, path)| path.as_path())
            .filter(|path| path.is_file())
    }
}

/// Captures a resolver process while enforcing a finite deadline.
///
/// The output is drained on two threads while the process runs, so a child
/// that fills its pipe can never deadlock. When cancelled or past its deadline
/// the child is terminated and the call returns immediately; the readers are
/// left to finish on their own, because a descendant that inherited the pipe
/// may otherwise keep `join` blocked forever.
fn run_command(
    mut command: Command,
    timeout: Duration,
    cancel: Option<(&AtomicU32, &AtomicBool)>,
) -> Result<Output, Error> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        app_logging::error(format!("yt-dlp could not be started: {error}"));
        Error::Spawn(error)
    })?;
    app_logging::debug(format!(
        "yt-dlp started: pid={} timeout={}s",
        child.id(),
        timeout.as_secs()
    ));
    let (stdout, stderr) = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => (stdout, stderr),
        _ => {
            app_logging::error("yt-dlp output pipes were missing; stopping the process");
            terminate_child(&mut child, cancel);
            clear_cancel_pid(cancel);
            return Err(Error::MissingOutputPipe);
        }
    };
    if let Some((pid, _)) = cancel {
        pid.store(child.id(), Ordering::SeqCst);
    }
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + timeout;

    let status = loop {
        if cancel.is_some_and(|(_, flag)| flag.load(Ordering::SeqCst)) {
            app_logging::warning(format!(
                "cancelling yt-dlp pid={}: superseded by newer work",
                child.id()
            ));
            terminate_child(&mut child, cancel);
            clear_cancel_pid(cancel);
            return Err(Error::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                app_logging::error(format!(
                    "yt-dlp timed out after {}s; terminating pid={}",
                    timeout.as_secs(),
                    child.id()
                ));
                terminate_child(&mut child, cancel);
                clear_cancel_pid(cancel);
                return Err(Error::Timeout(timeout.as_secs()));
            }
            Err(error) => {
                app_logging::error(format!(
                    "yt-dlp pid={} could not be waited on: {error}",
                    child.id()
                ));
                terminate_child(&mut child, cancel);
                clear_cancel_pid(cancel);
                return Err(Error::Spawn(error));
            }
        }
    };
    app_logging::debug(format!("yt-dlp pid={} exited with {status}", child.id()));

    clear_cancel_pid(cancel);
    let stdout = join_output(stdout_reader).map_err(|error| {
        app_logging::error(format!("yt-dlp stdout could not be read: {error}"));
        error
    })?;
    let stderr = join_output(stderr_reader).map_err(|error| {
        app_logging::error(format!("yt-dlp stderr could not be read: {error}"));
        error
    })?;
    Ok(Output { status, stdout, stderr })
}

fn clear_cancel_pid(cancel: Option<(&AtomicU32, &AtomicBool)>) {
    if let Some((pid, _)) = cancel {
        pid.store(0, Ordering::SeqCst);
    }
}

/// Requests cancellation of the active `yt-dlp` process and terminates it if
/// one is registered.
pub fn cancel_active_process(pid: &AtomicU32, cancelled: &AtomicBool) {
    cancelled.store(true, Ordering::SeqCst);
    let id = pid.load(Ordering::SeqCst);
    if id != 0 {
        app_logging::debug(format!("terminating the active yt-dlp process: pid={id}"));
        terminate_pid(id);
    }
}

/// Stops a resolver process and, on Windows, any process tree it spawned.
///
/// `yt-dlp` may leave helpers behind after a hang, timeout or preemption, and
/// a lingering process can keep the next invocation from running correctly.
fn terminate_child(child: &mut std::process::Child, cancel: Option<(&AtomicU32, &AtomicBool)>) {
    let _ = child.kill();
    let id = cancel
        .and_then(|(pid, _)| {
            let id = pid.load(Ordering::SeqCst);
            (id != 0).then_some(id)
        })
        .unwrap_or_else(|| child.id());
    terminate_pid(id);
    let _ = child.wait();
}

/// Terminates a process tree by id.
pub fn terminate_pid(id: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &id.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill").args(["-TERM", &id.to_string()]).status();
        let _ = std::process::Command::new("kill").args(["-KILL", &id.to_string()]).status();
    }
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
    use super::{CommandSpec, Error, Resolver, run_command};
    use media_model::Service;
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
        let result = run_command(command, Duration::from_secs(1), None);

        assert!(matches!(result, Err(Error::Timeout(1))));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_finished_resolver_process_reports_its_output() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let mut command = Command::new(executable);
        command.arg("--list");

        let output =
            run_command(command, Duration::from_secs(10), None).expect("the output is captured");

        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }

    #[test]
    fn a_cancelled_resolver_process_stops_early() {
        use std::sync::atomic::{AtomicBool, AtomicU32};
        let executable = std::env::current_exe().expect("test executable path is available");
        let mut command = Command::new(executable);
        command
            .arg("--exact")
            .arg("tests::timeout_test_child")
            .arg("--nocapture")
            .env("FLATLINE_RESOLVER_TIMEOUT_TEST_CHILD", "1");

        let pid = AtomicU32::new(0);
        let cancelled = AtomicBool::new(true);
        let started = Instant::now();
        let result = run_command(command, Duration::from_secs(10), Some((&pid, &cancelled)));

        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn cookie_matching_is_limited_to_the_service() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("flatline-cookies-{unique}"));
        std::fs::create_dir_all(&directory).expect("the test directory can be created");
        let youtube = directory.join("cookies-youtube.txt");
        let twitch = directory.join("cookies-twitch.txt");
        let resolver = Resolver::new(
            CommandSpec::new("yt-dlp")
                .with_cookies(Service::YouTube, youtube.clone())
                .with_cookies(Service::Twitch, twitch.clone()),
        );

        assert_eq!(resolver.cookies_for_url("https://www.youtube.com/watch?v=1"), None);
        std::fs::write(&youtube, "# Netscape HTTP Cookie File\n")
            .expect("test cookie file can be created");
        std::fs::write(&twitch, "# Netscape HTTP Cookie File\n")
            .expect("test cookie file can be created");
        assert_eq!(
            resolver.cookies_for_url("https://www.youtube.com/watch?v=1"),
            Some(youtube.as_path())
        );
        assert_eq!(
            resolver.cookies_for_url("https://www.twitch.tv/channel"),
            Some(twitch.as_path())
        );
        assert_eq!(resolver.cookies_for_url("https://example.org/video"), None);
        let _ = std::fs::remove_dir_all(directory);
    }
}

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

use std::path::PathBuf;
use std::process::Command;

use media_model::MediaSource;

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
}

/// How to invoke `yt-dlp`.
///
/// The caller decides *which* executable is used; the resolver only adds the
/// arguments it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    program: PathBuf,
    interpreter: Option<PathBuf>,
}

impl CommandSpec {
    /// A spec that runs `program` directly.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), interpreter: None }
    }

    /// A spec that runs `program` through an interpreter.
    pub fn with_interpreter(program: impl Into<PathBuf>, interpreter: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), interpreter: Some(interpreter.into()) }
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
        }
    }
}

impl std::error::Error for Error {}

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
        let output = self
            .spec
            .command()
            .arg("--dump-single-json")
            .arg("--no-warnings")
            .arg("--no-progress")
            .arg("--ignore-config")
            .arg(url)
            .output()
            .map_err(Error::Spawn)?;

        if !output.status.success() {
            return Err(Error::Failed {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        parse_catalog(&String::from_utf8_lossy(&output.stdout))
    }
}

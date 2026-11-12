//! Discovery, provisioning and self-update of the `yt-dlp` executable.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns exactly one capability: *having a working `yt-dlp` at
//! hand*. It knows nothing about URLs, streams, playback or the user
//! interface. Other crates receive a plain [`YtDlp`] value and use it as a
//! black box.
//!
//! On Unix a `yt-dlp` distribution is a Python zip application, so it needs
//! an interpreter; on Windows the published `yt-dlp.exe` is self contained.
//! [`YtDlp`] hides that difference behind [`YtDlp::command`].

pub mod locate;
pub mod platform;
pub mod update;
pub mod version;

use std::fmt;

pub use locate::{ToolPaths, YtDlp};
pub use platform::Platform;
pub use update::UpdateOutcome;

/// Everything that can go wrong while managing the `yt-dlp` executable.
#[derive(Debug)]
pub enum Error {
    /// The operating system refused an operation.
    Io(std::io::Error),
    /// No `yt-dlp` could be located.
    NotFound,
    /// `yt-dlp` was found but refused to run.
    NotUsable(String),
    /// A network request failed.
    Http(String),
    /// A downloaded payload could not be understood.
    Parse(String),
    /// The current platform has no supported way to run `yt-dlp`.
    UnsupportedPlatform,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::NotFound => write!(f, "yt-dlp was not found"),
            Error::NotUsable(m) => write!(f, "yt-dlp is not usable: {m}"),
            Error::Http(m) => write!(f, "network error: {m}"),
            Error::Parse(m) => write!(f, "could not parse response: {m}"),
            Error::UnsupportedPlatform => write!(f, "yt-dlp is unsupported on this platform"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
}

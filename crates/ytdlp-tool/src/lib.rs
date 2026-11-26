//! Discovery, provisioning and metadata access through the `yt-dlp` executable.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns exactly one capability: *talking to `yt-dlp`*. It knows
//! nothing about playback, the user interface or any other Flatline crate
//! beyond logging. Other crates receive a [`YtDlp`] value and use it as the
//! single access point for `yt-dlp` work.
//!
//! The actual metadata fetch is handled by the `yt-dlp` crate underneath;
//! Flatline keeps control over where the managed executable lives (`tools/`)
//! and how updates are surfaced to the user.

pub mod deno;
pub mod locate;
pub mod platform;
pub mod proxy;
pub mod update;
pub mod version;

mod fetch;

use std::fmt;
use std::path::Path;

pub use fetch::{FetchDocument, FetchRequest, FetchResult, fetch_with_cancel, is_update_required};
pub use locate::{ToolPaths, YtDlp, kill_all};
pub use platform::Platform;
pub use proxy::{ProxyOutcome, ProxyPool};
pub use update::ToolUpdate;

/// Everything that can go wrong while managing or running `yt-dlp`.
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
    /// The request was cancelled before it finished.
    Cancelled,
    /// The `yt-dlp` crate reported an error.
    YtDlp(String),
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
            Error::Cancelled => write!(f, "yt-dlp request was cancelled"),
            Error::YtDlp(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
}

/// Socket timeout passed to `yt-dlp` for network operations.
pub const SOCKET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Resolves a URL into `yt-dlp` video metadata through the singular yt-dlp access point.
pub async fn fetch(request: FetchRequest) -> Result<FetchResult, Error> {
    fetch::fetch(request).await
}

/// Updates the managed yt-dlp executable through the `yt-dlp` crate.
pub async fn update_managed(program: &Path, output_dir: &Path) -> Result<String, Error> {
    fetch::update_managed(program, output_dir).await
}

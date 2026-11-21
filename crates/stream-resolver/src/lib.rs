#![recursion_limit = "256"]
//! Resolving a URL into a catalog of playable streams.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns exactly one capability: *turning a URL into a
//! [`MediaSource`]*. It no longer spawns yt-dlp directly; it uses the
//! singular `ytdlp-tool` access point, which delegates to the `yt-dlp` crate.

use std::collections::BTreeMap;

use media_model::{MediaSource, Service};
use ytdlp_tool::FetchRequest;

pub mod parse;
pub mod plan;

pub use parse::parse_video;
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
    Output(std::io::Error),
    /// A resolver output reader stopped unexpectedly.
    OutputReaderPanicked,
    /// Captured output was missing from a resolver process.
    MissingOutputPipe,
    /// The resolver process was cancelled in favour of newer work.
    Cancelled,
    /// The yt-dlp tool reported an error.
    Tool(String),
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
            Error::Tool(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

/// Resolves URLs with the singular yt-dlp access point.
#[derive(Debug, Clone)]
pub struct Resolver {
    tool: ytdlp_tool::YtDlp,
    cookies: BTreeMap<Service, std::path::PathBuf>,
}

impl Resolver {
    /// Creates a resolver that uses the given `yt-dlp` access point.
    pub fn new(tool: ytdlp_tool::YtDlp) -> Self {
        Self { tool, cookies: BTreeMap::new() }
    }

    /// Supplies a Netscape cookie file for URLs of one service.
    pub fn with_cookies(mut self, service: Service, path: impl Into<std::path::PathBuf>) -> Self {
        self.cookies.insert(service, path.into());
        self
    }

    /// The underlying yt-dlp access point.
    pub fn tool(&self) -> &ytdlp_tool::YtDlp {
        &self.tool
    }

    /// Resolves a URL into a [`MediaSource`].
    pub async fn resolve(&self, url: &str) -> Result<MediaSource, Error> {
        let mut cookies = Vec::new();
        if let Some(service) = Service::from_url(url)
            && let Some(path) = self.cookies.get(&service)
            && path.is_file()
        {
            cookies.push((service.name().to_string(), path.clone()));
        }

        let request = FetchRequest {
            tool: Some(self.tool.clone()),
            url: url.to_string(),
            cookies,
            args: Vec::new(),
        };

        let result = ytdlp_tool::fetch(request).await.map_err(map_tool_error)?;
        parse::parse_video(result.video)
    }
}

fn map_tool_error(error: ytdlp_tool::Error) -> Error {
    let message = error.to_string();
    if ytdlp_tool::is_update_required(&message) {
        return Error::Tool(message);
    }
    Error::Tool(message)
}

/// Whether an error indicates yt-dlp should be updated.
pub fn is_update_required(error: &Error) -> bool {
    match error {
        Error::Tool(message) | Error::Failed { stderr: message, .. } => {
            ytdlp_tool::is_update_required(message)
        }
        _ => false,
    }
}

/// Parses a direct yt-dlp JSON document (compatibility helper).
pub fn parse_catalog(json: &str) -> Result<MediaSource, Error> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(Error::Json)?;
    let video: yt_dlp::model::Video = serde_json::from_value(value).map_err(Error::Json)?;
    parse::parse_video(video)
}

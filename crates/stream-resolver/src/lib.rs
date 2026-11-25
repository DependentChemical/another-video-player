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

pub use parse::{parse_playlist, parse_video};
pub use plan::plan;

/// The resolved document for a URL.
#[derive(Debug, Clone)]
pub enum ResolvedDocument {
    /// A single playable media source.
    Video(MediaSource),
    /// A playlist with entries to be expanded into queue items.
    Playlist(ResolvedPlaylist),
}

/// A playlist resolved from a URL.
#[derive(Debug, Clone)]
pub struct ResolvedPlaylist {
    /// Playlist id.
    pub id: String,
    /// Playlist title.
    pub title: String,
    /// Playlist webpage URL.
    pub url: Option<String>,
    /// Entries in playlist order.
    pub entries: Vec<ResolvedPlaylistEntry>,
}

/// One entry in a resolved playlist.
#[derive(Debug, Clone)]
pub struct ResolvedPlaylistEntry {
    /// Video id.
    pub id: String,
    /// Human readable title.
    pub title: String,
    /// URL that can be resolved/queued.
    pub url: String,
    /// Thumbnail URL.
    pub thumbnail: Option<String>,
    /// Preview thumbnail URLs reported by the extractor, best first when possible.
    pub thumbnails: Vec<String>,
    /// Uploader/channel name.
    pub uploader: Option<String>,
}

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
    /// The resolver process did not answer before its hard deadline.
    ///
    /// The deadline belongs to Flatline: the process was killed because it
    /// ran out of time. A network failure that `yt-dlp` reports itself is
    /// not a timeout and stays an [`Error::Tool`].
    HardTimeout {
        /// The deadline that was reached, in seconds.
        seconds: u64,
        /// What the process had written when it was killed.
        detail: String,
    },
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
            Error::HardTimeout { seconds, detail } => {
                if detail.is_empty() {
                    write!(f, "yt-dlp did not respond within {seconds} seconds")
                } else {
                    write!(f, "yt-dlp did not respond within {seconds} seconds; {detail}")
                }
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
    args: Vec<String>,
    service_args: BTreeMap<Service, Vec<String>>,
    site_args: Vec<(regex::Regex, Vec<String>)>,
}

impl Resolver {
    /// Creates a resolver that uses the given `yt-dlp` access point.
    pub fn new(tool: ytdlp_tool::YtDlp) -> Self {
        Self {
            tool,
            cookies: BTreeMap::new(),
            args: Vec::new(),
            service_args: BTreeMap::new(),
            site_args: Vec::new(),
        }
    }

    /// Supplies a Netscape cookie file for URLs of one service.
    pub fn with_cookies(mut self, service: Service, path: impl Into<std::path::PathBuf>) -> Self {
        self.cookies.insert(service, path.into());
        self
    }

    /// Adds extra yt-dlp arguments used for every resolve request.
    /// Adds extra yt-dlp arguments used for every resolve request.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args.into_iter().filter(|arg| !arg.trim().is_empty()).collect();
        self
    }

    /// Adds extra yt-dlp arguments used for the URLs of one service.
    ///
    /// The arguments are added to the request of every URL that belongs
    /// to the service, so a source specific argument never reaches the
    /// extractor of another source.
    pub fn with_service_args(mut self, service: Service, args: Vec<String>) -> Self {
        let args = args.into_iter().filter(|arg| !arg.trim().is_empty()).collect();
        self.service_args.insert(service, args);
        self
    }

    /// Adds yt-dlp arguments that apply only to URLs matching a regex.
    ///
    /// The rules are evaluated when a request is made, so one resolver can
    /// serve URLs from several sites and each one gets only its own extra
    /// arguments. A pattern that does not compile is skipped instead of
    /// failing every resolve.
    pub fn with_site_args(mut self, rules: Vec<(String, Vec<String>)>) -> Self {
        self.site_args = rules
            .into_iter()
            .filter_map(|(pattern, args)| {
                let args: Vec<String> =
                    args.into_iter().filter(|arg| !arg.trim().is_empty()).collect();
                if args.is_empty() {
                    return None;
                }
                regex::Regex::new(&pattern).ok().map(|regex| (regex, args))
            })
            .collect();
        self
    }

    /// The arguments a request for `url` is made with.
    ///
    /// The general arguments come first, then the site arguments whose
    /// pattern matches, then the ones of the service the URL belongs to.
    fn args_for(&self, url: &str) -> Vec<String> {
        let mut args = self.args.clone();
        for (pattern, extra) in &self.site_args {
            if pattern.is_match(url) {
                args.extend(extra.iter().cloned());
            }
        }
        if let Some(service) = Service::from_url(url)
            && let Some(extra) = self.service_args.get(&service)
        {
            args.extend(extra.iter().cloned());
        }
        args
    }

    /// Returns whether resolves are forced to IPv4.
    pub fn forces_ipv4(&self) -> bool {
        self.args.iter().any(|arg| arg == "-4")
    }

    /// Returns a copy that forces IPv4 for yt-dlp network requests.
    pub fn with_force_ipv4(mut self) -> Self {
        if !self.forces_ipv4() {
            self.args.push("-4".to_string());
        }
        self
    }
    /// The underlying yt-dlp access point.
    pub fn tool(&self) -> &ytdlp_tool::YtDlp {
        &self.tool
    }

    /// Resolves a URL into a [`MediaSource`].
    pub async fn resolve(&self, url: &str) -> Result<MediaSource, Error> {
        let never = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pid = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        self.resolve_with_cancel(url, never, pid).await
    }

    /// Resolves a URL into a [`MediaSource`], stopping when `cancel` is set.
    ///
    /// When the URL resolves to a playlist, the first playable entry is resolved.
    pub async fn resolve_with_cancel(
        &self,
        url: &str,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) -> Result<MediaSource, Error> {
        match self.resolve_document_with_cancel(url, cancel.clone(), pid.clone()).await? {
            ResolvedDocument::Video(source) => Ok(source),
            ResolvedDocument::Playlist(playlist) => {
                for entry in playlist.entries {
                    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(Error::Cancelled);
                    }
                    match self
                        .resolve_document_with_cancel(&entry.url, cancel.clone(), pid.clone())
                        .await
                    {
                        Ok(ResolvedDocument::Video(source)) => return Ok(source),
                        Ok(ResolvedDocument::Playlist(_)) | Err(Error::Empty) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(Error::Empty)
            }
        }
    }

    /// Resolves a URL into a document, stopping when `cancel` is set.
    pub async fn resolve_document_with_cancel(
        &self,
        url: &str,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) -> Result<ResolvedDocument, Error> {
        let mut cookies = Vec::new();
        if let Some(service) = Service::from_url(url)
            && let Some(path) = self.cookies.get(&service)
            && path.is_file()
        {
            cookies.push((service.name().to_string(), path.clone()));
        }

        let mut args = self.args_for(url);
        if is_playlist_url(url) && !args.iter().any(|arg| arg == "--flat-playlist") {
            args.push("--flat-playlist".to_string());
        }
        let request =
            FetchRequest { tool: Some(self.tool.clone()), url: url.to_string(), cookies, args };

        let result =
            ytdlp_tool::fetch_with_cancel(request, cancel, pid).await.map_err(map_tool_error)?;
        match result.document {
            ytdlp_tool::FetchDocument::Video(video) => {
                parse::parse_video(*video).map(ResolvedDocument::Video)
            }
            ytdlp_tool::FetchDocument::Playlist(playlist) => {
                Ok(ResolvedDocument::Playlist(parse::parse_playlist(*playlist, url)))
            }
        }
    }
}

fn is_playlist_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let is_youtube = host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com");
    if !is_youtube {
        return parsed.query_pairs().any(|(k, _)| k.eq_ignore_ascii_case("list"));
    }
    parsed.path().eq_ignore_ascii_case("/playlist")
        || parsed.query_pairs().any(|(k, _)| k.eq_ignore_ascii_case("list"))
}

fn map_tool_error(error: ytdlp_tool::Error) -> Error {
    match error {
        // A cancellation and a hard timeout are Flatline's own decisions, so
        // they keep their own kind instead of hiding in a message.
        ytdlp_tool::Error::Cancelled => Error::Cancelled,
        ytdlp_tool::Error::Timeout { seconds, detail } => Error::HardTimeout { seconds, detail },
        other => Error::Tool(other.to_string()),
    }
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

#[cfg(test)]
mod tests {
    use super::{Error, Resolver, map_tool_error};
    use media_model::Service;

    fn resolver() -> Resolver {
        let tool = ytdlp_tool::YtDlp::new(
            "yt-dlp",
            ytdlp_tool::locate::ToolPaths::new(std::env::temp_dir().join("flatline-tests")),
        );
        Resolver::new(tool)
    }

    #[test]
    fn the_arguments_of_a_service_only_reach_that_service() {
        let resolver = resolver().with_args(vec!["--no-warnings".to_string()]).with_service_args(
            Service::YouTube,
            vec!["--add-header".to_string(), "X-Service: youtube".to_string()],
        );

        let youtube = resolver.args_for("https://www.youtube.com/watch?v=1");
        assert_eq!(youtube[0], "--no-warnings");
        assert_eq!(youtube[1], "--add-header");
        assert_eq!(youtube[2], "X-Service: youtube");

        let twitch = resolver.args_for("https://www.twitch.tv/videos/1");
        assert_eq!(twitch, vec!["--no-warnings".to_string()]);
    }

    #[test]
    fn matching_site_arguments_follow_the_general_arguments() {
        let resolver =
            resolver().with_args(vec!["--no-warnings".to_string()]).with_site_args(vec![
                (
                    "youtube\\.com".to_string(),
                    vec!["--extractor-args".to_string(), "a=b".to_string()],
                ),
                ("twitch\\.tv".to_string(), vec!["--add-header".to_string(), "X=1".to_string()]),
            ]);

        assert_eq!(
            resolver.args_for("https://www.youtube.com/watch?v=1"),
            vec!["--no-warnings", "--extractor-args", "a=b"]
        );
        assert_eq!(
            resolver.args_for("https://www.twitch.tv/videos/1"),
            vec!["--no-warnings", "--add-header", "X=1"]
        );
        assert_eq!(resolver.args_for("https://example.org/video.mp4"), vec!["--no-warnings"]);
    }

    #[test]
    fn a_url_without_a_service_keeps_the_general_arguments_only() {
        let resolver = resolver().with_service_args(
            Service::YouTube,
            vec!["--add-header".to_string(), "X-Service: youtube".to_string()],
        );

        assert!(resolver.args_for("https://example.org/video.mp4").is_empty());
    }

    #[test]
    fn the_hard_timeout_keeps_its_own_kind() {
        let error = map_tool_error(ytdlp_tool::Error::Timeout {
            seconds: 20,
            detail: "last output: stderr=\"\"".to_string(),
        });

        match error {
            Error::HardTimeout { seconds, detail } => {
                assert_eq!(seconds, 20);
                assert!(detail.contains("last output"));
            }
            other => panic!("expected the hard timeout, got {other:?}"),
        }
    }

    #[test]
    fn a_cancellation_keeps_its_own_kind() {
        let error = map_tool_error(ytdlp_tool::Error::Cancelled);

        assert!(matches!(error, Error::Cancelled));
    }

    #[test]
    fn a_network_failure_stays_a_tool_error() {
        let error = map_tool_error(ytdlp_tool::Error::YtDlp(
            "ERROR: Unable to download webpage: The read timed out".to_string(),
        ));

        match error {
            Error::Tool(message) => {
                assert!(message.contains("Unable to download webpage"));
            }
            other => panic!("expected a tool error, got {other:?}"),
        }
    }

    #[test]
    fn playlist_urls_are_detected() {
        assert!(super::is_playlist_url("https://www.youtube.com/playlist?list=PL123"));
        assert!(super::is_playlist_url("https://www.youtube.com/watch?v=abc&list=PL123"));
        assert!(!super::is_playlist_url("https://www.youtube.com/watch?v=abc"));
    }
}

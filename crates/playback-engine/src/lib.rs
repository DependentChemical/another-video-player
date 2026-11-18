//! The libmpv playback engine.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns one capability: *driving an mpv player*. It knows
//! nothing about URLs, streams, toolkits or the user interface. Its whole
//! surface is [`Engine`], a thin, thread safe wrapper around a libmpv
//! handle, plus [`VideoRenderer`] for drawing frames.
//!
//! The crate deliberately does not expose `rsmpv` types: everything the
//! outside world needs is spelled out here, so the rest of Flatline never
//! has to learn the mpv client API.

pub mod render;

use std::fmt;
use std::path::Path;

pub use render::{GlProcAddress, VideoRenderer};

/// The loudest volume the player accepts, in percent.
///
/// mpv's default ceiling is 130; Flatline raises it to 100 so the slider
/// reaches unity gain at 100%.
pub const MAX_VOLUME: f64 = 100.0;

/// How far ahead mpv is allowed to buffer, in seconds.
///
/// The limit is effectively removed so that a paused media keeps downloading
/// to its end instead of stopping at the usual readahead window.
const CACHE_SECONDS: &str = "1000000000";

/// How many seconds of content to buffer before starting playback.
///
/// This controls `demuxer-readahead-secs` and, together with
/// `cache-pause-initial`, makes mpv wait until at least this much content
/// is downloaded before beginning playback.
const INITIAL_BUFFER_SECONDS: &str = "10";

/// How much packet metadata the demuxer may keep.
///
/// With `cache-on-disk` this counts metadata, not the cached payload, so a
/// large value is what lets the whole media stay buffered and the already
/// downloaded ranges stay seekable.
const DEMUXER_MAX_BYTES: &str = "1024MiB";

/// A span of the media that mpv has already downloaded.
///
/// The ranges are in stream timestamps, in seconds, and are not necessarily
/// ordered or disjoint: the demuxer reports them as it fills them in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BufferedRange {
    /// The first timestamp that is buffered.
    pub start: f64,
    /// The last timestamp that is buffered.
    pub end: f64,
}

/// Keeps a volume inside the range the player accepts.
fn clamp_volume(volume: f64) -> f64 {
    volume.clamp(0.0, MAX_VOLUME)
}

/// Why playback of a file ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The end of the media was reached.
    Eof,
    /// Playback was stopped from the outside.
    Stop,
    /// The player is shutting down.
    Quit,
    /// An error aborted playback.
    Error,
    /// A reason this crate does not know about.
    Other,
}

/// Something the player reported.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The player began loading a new file.
    StartFile,
    /// A file finished loading and decoding started.
    FileLoaded,
    /// Playback of a file ended.
    EndFile {
        /// Why it ended.
        reason: EndReason,
        /// The error message, when the reason is [`EndReason::Error`].
        error: Option<String>,
    },
    /// A log line from mpv.
    Log {
        /// Severity, e.g. `error` or `warn`.
        level: String,
        /// The message itself.
        text: String,
    },
    /// The player is going away.
    Shutdown,
}

/// What should be played.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadRequest {
    /// The primary media URL. For a split stream this is the video only URL.
    pub media: String,
    /// The URL of an audio stream that has to be attached separately.
    pub audio: Option<String>,
}

impl LoadRequest {
    /// A request for a single, self contained media URL.
    pub fn new(media: impl Into<String>) -> Self {
        Self { media: media.into(), audio: None }
    }

    /// A request that attaches an extra audio stream.
    pub fn with_audio(media: impl Into<String>, audio: impl Into<String>) -> Self {
        Self { media: media.into(), audio: Some(audio.into()) }
    }
}

/// Everything that can go wrong while talking to mpv.
#[derive(Debug)]
pub enum Error {
    /// mpv rejected a command or a property.
    Mpv(rsmpv::Error),
    /// The on-disk stream cache directory could not be created.
    Cache(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Mpv(error) => write!(f, "mpv error: {error}"),
            Error::Cache(error) => write!(f, "the stream buffer could not be prepared: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Mpv(error) => Some(error),
            Error::Cache(error) => Some(error),
        }
    }
}

impl From<rsmpv::Error> for Error {
    fn from(value: rsmpv::Error) -> Self {
        Error::Mpv(value)
    }
}

/// A running mpv player.
///
/// The handle is shared, so the engine can be used from the thread that
/// drives the user interface and from background threads at the same time.
#[derive(Clone)]
pub struct Engine {
    core: std::sync::Arc<rsmpv::Mpv>,
}

impl Engine {
    /// Creates a player configured for embedding.
    ///
    /// The player renders into an OpenGL context that the embedder provides,
    /// so `vo` is fixed to `libmpv` and no window is ever created.
    ///
    /// Downloaded media is kept in an append-only cache file below
    /// `cache_dir`, so a paused or scrubbed media is buffered to disk instead
    /// of being thrown away. The directory is created when it is missing.
    pub fn new(cache_dir: impl AsRef<Path>) -> Result<Engine, Error> {
        let cache_dir = cache_dir.as_ref();
        std::fs::create_dir_all(cache_dir).map_err(Error::Cache)?;
        let cache_dir = cache_dir.to_string_lossy();

        let mpv = rsmpv::Mpv::builder()?
            .set_property("vo", "libmpv")?
            .set_property("config", "no")?
            .set_property("terminal", "no")?
            .set_property("idle", "yes")?
            .set_property("keep-open", "yes")?
            .set_property("hwdec", "auto-safe")?
            .set_property("audio-display", "no")?
            .set_property("osd-level", "0")?
            .set_property("input-default-bindings", "no")?
            .set_property("input-vo-keyboard", "no")?
            .set_property("title", "Flatline")?
            .set_property("volume-max", MAX_VOLUME)?
            // Keep reading the stream ahead even while playback is paused, and
            // write the packets to Flatline's own folder so the downloaded
            // parts survive a seek instead of being discarded.
            .set_property("cache", "yes")?
            .set_property("cache-on-disk", "yes")?
            .set_property("demuxer-cache-dir", cache_dir.as_ref())?
            .set_property("cache-secs", CACHE_SECONDS)?
            .set_property("demuxer-max-bytes", DEMUXER_MAX_BYTES)?
            .set_property("demuxer-max-back-bytes", DEMUXER_MAX_BYTES)?
            // Wait for at least INITIAL_BUFFER_SECONDS of content before starting
            // playback. `cache-pause-initial` (default yes) pauses initially until
            // the readahead buffer is filled; `demuxer-readahead-secs` sets the
            // target fill level in seconds.
            .set_property("cache-pause-initial", "yes")?
            .set_property("demuxer-readahead-secs", INITIAL_BUFFER_SECONDS)?
            .set_property("cache-pause-wait", INITIAL_BUFFER_SECONDS)?
            .build()?;

        // mpv does not deliver file lifecycle events until they are requested.
        for event in [
            rsmpv::sys::MPV_EVENT_START_FILE,
            rsmpv::sys::MPV_EVENT_FILE_LOADED,
            rsmpv::sys::MPV_EVENT_END_FILE,
        ] {
            mpv.request_event(event, true)?;
        }

        Ok(Engine { core: std::sync::Arc::new(mpv) })
    }

    /// Opens a media, replacing whatever was playing before.
    ///
    /// When the request carries a separate audio stream, it is attached with
    /// mpv's per file `audio-file` option, which is how a video only stream
    /// and an audio only stream are played together.
    pub fn load(&self, request: &LoadRequest) -> Result<(), Error> {
        // `loadfile` takes the per-file options as a map, which keeps an URL
        // that contains a comma from being split into two options. The `-1`
        // is the insertion index mpv requires before the options argument.
        let mut arguments = vec![
            rsmpv::Node::String("loadfile".into()),
            rsmpv::Node::String(request.media.clone()),
            rsmpv::Node::String("replace".into()),
        ];
        if let Some(audio) = &request.audio {
            arguments.push(rsmpv::Node::Int64(-1));
            arguments.push(rsmpv::Node::Map(vec![(
                "audio-file".to_string(),
                rsmpv::Node::String(audio.clone()),
            )]));
        }
        self.core.command_node(&rsmpv::Node::Array(arguments))?;
        Ok(())
    }

    /// Stops playback and unloads the current media.
    pub fn stop(&self) -> Result<(), Error> {
        self.core.command(&["stop"])?;
        Ok(())
    }

    /// Continues playback.
    pub fn play(&self) -> Result<(), Error> {
        self.core.set_property("pause", false)?;
        Ok(())
    }

    /// Pauses playback.
    pub fn pause(&self) -> Result<(), Error> {
        self.core.set_property("pause", true)?;
        Ok(())
    }

    /// Switches between playing and paused.
    pub fn toggle_pause(&self) -> Result<(), Error> {
        let paused = self.core.get_property::<bool>("pause")?;
        self.core.set_property("pause", !paused)?;
        Ok(())
    }

    /// Seeks to an absolute position in seconds.
    pub fn seek(&self, seconds: f64) -> Result<(), Error> {
        self.core.command(&["seek", &seconds.to_string(), "absolute"])?;
        Ok(())
    }

    /// Seeks by an offset in seconds.
    ///
    /// A negative offset goes back in time. This is what a live stream needs
    /// when the user wants to replay a moment that just passed.
    pub fn seek_relative(&self, seconds: f64) -> Result<(), Error> {
        self.core.command(&["seek", &seconds.to_string(), "relative"])?;
        Ok(())
    }

    /// Jumps to the live edge of a live stream.
    ///
    /// mpv has no dedicated command for it. A relative seek far past the end
    /// is clamped to the end of the seekable range, which is exactly the live
    /// edge of a stream that is still being produced.
    pub fn seek_to_live_edge(&self) -> Result<(), Error> {
        if let Some(edge) = self.seekable_end() {
            self.seek(edge)?;
        } else {
            self.core.command(&["seek", "1000000000", "relative"])?;
        }
        Ok(())
    }

    /// The latest timestamp mpv can seek to in its current demuxer cache.
    ///
    /// Live streams keep downloading while paused, so this is a better
    /// estimate of the live edge than the paused playback position.
    pub fn seekable_end(&self) -> Option<f64> {
        self.buffered_ranges().into_iter().map(|range| range.end).max_by(f64::total_cmp)
    }

    /// The spans of the media that are already downloaded into the cache.
    ///
    /// A media that is paused keeps filling this in until its end; a media
    /// that was scrubbed may report several disjoint ranges. The caller can
    /// show them without ever distinguishing a full download from a seek.
    pub fn buffered_ranges(&self) -> Vec<BufferedRange> {
        let Ok(state) = self.core.get_property("demuxer-cache-state") else {
            return Vec::new();
        };
        buffered_ranges_from_cache(&state)
    }

    /// Sets the volume, in mpv's cubic percent.
    ///
    /// The value is clamped to `0..=`[`MAX_VOLUME`], so a stray click on the
    /// slider can never ask mpv for something it refuses. Note that mpv maps
    /// this percent to a linear gain with `(volume / 100)^3`; callers that want
    /// a linear-gain slider have to undo that curve before calling.
    pub fn set_volume(&self, volume: f64) -> Result<(), Error> {
        self.core.set_property("volume", clamp_volume(volume))?;
        Ok(())
    }

    /// Mutes or unmutes the player.
    pub fn set_muted(&self, muted: bool) -> Result<(), Error> {
        self.core.set_property("mute", muted)?;
        Ok(())
    }

    /// The current playback position in seconds.
    pub fn position(&self) -> f64 {
        self.core.get_property::<f64>("time-pos").unwrap_or(0.0)
    }

    /// The duration of the current media in seconds.
    pub fn duration(&self) -> f64 {
        self.core.get_property::<f64>("duration").unwrap_or(0.0)
    }

    /// Whether playback is currently paused.
    pub fn is_paused(&self) -> bool {
        self.core.get_property::<bool>("pause").unwrap_or(false)
    }

    /// Whether the player is idle, i.e. no media is loaded.
    pub fn is_idle(&self) -> bool {
        self.core.get_property::<bool>("idle-active").unwrap_or(true)
    }

    /// Whether the player is decoding and drawing frames right now.
    ///
    /// The value stays false while a file is still being opened, while the
    /// player waits for data and while playback is paused.
    pub fn is_active(&self) -> bool {
        !self.core.get_property::<bool>("core-idle").unwrap_or(true)
    }

    /// Whether playback has to wait for data.
    ///
    /// mpv raises this flag when the network cannot feed the player fast
    /// enough, which is the moment the interface shows its loading dots.
    pub fn is_buffering(&self) -> bool {
        self.core.get_property::<bool>("paused-for-cache").unwrap_or(false)
    }

    /// The current volume.
    pub fn volume(&self) -> f64 {
        self.core.get_property::<f64>("volume").unwrap_or(100.0)
    }

    /// Whether the media has reached its end.
    pub fn is_eof(&self) -> bool {
        self.core.get_property::<bool>("eof-reached").unwrap_or(false)
    }

    /// Takes the next pending event, if there is one.
    pub fn poll_event(&self) -> Option<EngineEvent> {
        let event = self.core.poll_event()?;
        Some(match event {
            rsmpv::Event::StartFile { .. } => EngineEvent::StartFile,
            rsmpv::Event::FileLoaded => EngineEvent::FileLoaded,
            rsmpv::Event::EndFile { reason, error, .. } => EngineEvent::EndFile {
                reason: match reason {
                    rsmpv::EndFileReason::Eof => EndReason::Eof,
                    rsmpv::EndFileReason::Stop => EndReason::Stop,
                    rsmpv::EndFileReason::Quit => EndReason::Quit,
                    rsmpv::EndFileReason::Error => EndReason::Error,
                    _ => EndReason::Other,
                },
                error: error.map(|error| error.to_string()),
            },
            rsmpv::Event::LogMessage(message) => {
                EngineEvent::Log { level: message.level, text: message.text.trim_end().to_string() }
            }
            rsmpv::Event::Shutdown => EngineEvent::Shutdown,
            _ => return None,
        })
    }

    /// Registers a callback that fires whenever mpv has something to say.
    ///
    /// The callback runs on an mpv owned thread, so it must not block.
    pub fn set_wakeup_callback(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.core.set_wakeup_callback(callback);
    }

    /// Creates the renderer that draws the video.
    ///
    /// # Safety
    ///
    /// The call has to happen while an OpenGL context is current, and that
    /// context has to stay current for every later use of the renderer.
    pub unsafe fn create_renderer(&self, gl: GlProcAddress) -> Result<VideoRenderer, Error> {
        // SAFETY: the caller guarantees that an OpenGL context is current,
        // which is the obligation of the constructor below.
        unsafe { VideoRenderer::create(self.core.clone(), gl) }
    }
}

/// The spans mpv reports in its current demuxer cache state.
fn buffered_ranges_from_cache(state: &rsmpv::Node) -> Vec<BufferedRange> {
    let rsmpv::Node::Map(state) = state else {
        return Vec::new();
    };
    let Some(rsmpv::Node::Array(ranges)) =
        state.iter().find_map(|(key, value)| (key == "seekable-ranges").then_some(value))
    else {
        return Vec::new();
    };

    ranges
        .iter()
        .filter_map(|range| {
            let rsmpv::Node::Map(fields) = range else {
                return None;
            };
            let mut start = None;
            let mut end = None;
            for (key, value) in fields {
                let rsmpv::Node::Double(seconds) = value else {
                    continue;
                };
                if !seconds.is_finite() {
                    continue;
                }
                match key.as_str() {
                    "start" => start = Some(*seconds),
                    "end" => end = Some(*seconds),
                    _ => {}
                }
            }
            match (start, end) {
                (Some(start), Some(end)) if end > start => Some(BufferedRange { start, end }),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache directory of its own, so the tests never share cache files.
    fn cache_dir() -> std::path::PathBuf {
        std::env::temp_dir().join("flatline-playback-engine-tests").join("stream_buffer")
    }

    fn cache_state(ranges: &[(f64, f64)]) -> rsmpv::Node {
        rsmpv::Node::Map(vec![(
            "seekable-ranges".to_string(),
            rsmpv::Node::Array(
                ranges
                    .iter()
                    .map(|&(start, end)| {
                        rsmpv::Node::Map(vec![
                            ("start".to_string(), rsmpv::Node::Double(start)),
                            ("end".to_string(), rsmpv::Node::Double(end)),
                        ])
                    })
                    .collect(),
            ),
        )])
    }

    #[test]
    fn the_latest_seekable_range_is_the_live_edge() {
        let cache = cache_state(&[(10.0, 20.0), (30.0, 40.0)]);
        assert_eq!(
            buffered_ranges_from_cache(&cache)
                .into_iter()
                .map(|range| range.end)
                .max_by(f64::total_cmp),
            Some(40.0)
        );
    }

    #[test]
    fn the_cache_reports_every_buffered_span() {
        let cache = cache_state(&[(0.0, 30.0), (60.0, 90.0)]);
        assert_eq!(
            buffered_ranges_from_cache(&cache),
            vec![BufferedRange { start: 0.0, end: 30.0 }, BufferedRange { start: 60.0, end: 90.0 },]
        );
    }

    #[test]
    fn empty_and_malformed_ranges_are_ignored() {
        let cache = rsmpv::Node::Map(vec![(
            "seekable-ranges".to_string(),
            rsmpv::Node::Array(vec![
                rsmpv::Node::Map(vec![
                    ("start".to_string(), rsmpv::Node::Double(20.0)),
                    ("end".to_string(), rsmpv::Node::Double(20.0)),
                ]),
                rsmpv::Node::Map(vec![
                    ("start".to_string(), rsmpv::Node::Double(1.0)),
                    ("end".to_string(), rsmpv::Node::String("broken".into())),
                ]),
            ]),
        )]);
        assert!(buffered_ranges_from_cache(&cache).is_empty());
    }

    #[test]
    fn unavailable_seekable_ranges_have_no_live_edge() {
        assert!(buffered_ranges_from_cache(&rsmpv::Node::None).is_empty());
    }

    #[test]
    fn a_new_engine_is_idle_and_has_no_duration() {
        // Creating an engine needs libmpv; skip the test when it is absent.
        let Ok(engine) = Engine::new(cache_dir()) else {
            return;
        };
        assert!(engine.is_idle());
        assert_eq!(engine.duration(), 0.0);
    }

    #[test]
    fn a_new_engine_is_neither_active_nor_buffering() {
        let Ok(engine) = Engine::new(cache_dir()) else {
            return;
        };
        assert!(!engine.is_active());
        assert!(!engine.is_buffering());
    }

    #[test]
    fn a_new_engine_has_no_seekable_live_edge() {
        let Ok(engine) = Engine::new(cache_dir()) else {
            return;
        };
        assert_eq!(engine.seekable_end(), None);
        assert!(engine.buffered_ranges().is_empty());
    }

    #[test]
    fn load_requests_keep_their_parts() {
        let request =
            LoadRequest::with_audio("https://example.com/v.mp4", "https://example.com/a.m4a");
        assert_eq!(request.media, "https://example.com/v.mp4");
        assert_eq!(request.audio.as_deref(), Some("https://example.com/a.m4a"));
    }

    #[test]
    fn volume_is_clamped_to_the_player_range() {
        assert_eq!(clamp_volume(-10.0), 0.0);
        assert_eq!(clamp_volume(50.0), 50.0);
        assert_eq!(clamp_volume(250.0), MAX_VOLUME);
    }
}

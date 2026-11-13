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

pub use render::{GlProcAddress, VideoRenderer};

/// The loudest volume the player accepts, in percent.
///
/// mpv's default ceiling is 130; Flatline raises it to 100 so the slider
/// reaches unity gain at 100%.
pub const MAX_VOLUME: f64 = 100.0;

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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Mpv(error) => write!(f, "mpv error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Mpv(error) => Some(error),
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
    pub fn new() -> Result<Engine, Error> {
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
        let state = self.core.get_property("demuxer-cache-state").ok()?;
        seekable_end_from_cache(&state)
    }

    /// Sets the volume, in percent.
    ///
    /// The value is clamped to `0..=`[`MAX_VOLUME`], so a stray click on the
    /// slider can never ask mpv for something it refuses.
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

/// The furthest timestamp in mpv's current seekable cache ranges.
fn seekable_end_from_cache(state: &rsmpv::Node) -> Option<f64> {
    let rsmpv::Node::Map(state) = state else {
        return None;
    };
    let ranges =
        state.iter().find_map(|(key, value)| (key == "seekable-ranges").then_some(value))?;
    let rsmpv::Node::Array(ranges) = ranges else {
        return None;
    };

    ranges
        .iter()
        .filter_map(|range| {
            let rsmpv::Node::Map(fields) = range else {
                return None;
            };
            fields.iter().find_map(|(key, value)| {
                if key != "end" {
                    return None;
                }
                match value {
                    rsmpv::Node::Double(end) if end.is_finite() => Some(*end),
                    _ => None,
                }
            })
        })
        .max_by(f64::total_cmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_seekable_range_is_the_live_edge() {
        let cache = rsmpv::Node::Map(vec![(
            "seekable-ranges".to_string(),
            rsmpv::Node::Array(vec![
                rsmpv::Node::Map(vec![
                    ("start".to_string(), rsmpv::Node::Double(10.0)),
                    ("end".to_string(), rsmpv::Node::Double(20.0)),
                ]),
                rsmpv::Node::Map(vec![
                    ("start".to_string(), rsmpv::Node::Double(30.0)),
                    ("end".to_string(), rsmpv::Node::Double(40.0)),
                ]),
            ]),
        )]);

        assert_eq!(seekable_end_from_cache(&cache), Some(40.0));
    }

    #[test]
    fn unavailable_seekable_ranges_have_no_live_edge() {
        assert_eq!(seekable_end_from_cache(&rsmpv::Node::None), None);
    }

    #[test]
    fn a_new_engine_is_idle_and_has_no_duration() {
        // Creating an engine needs libmpv; skip the test when it is absent.
        let Ok(engine) = Engine::new() else {
            return;
        };
        assert!(engine.is_idle());
        assert_eq!(engine.duration(), 0.0);
    }

    #[test]
    fn a_new_engine_is_neither_active_nor_buffering() {
        let Ok(engine) = Engine::new() else {
            return;
        };
        assert!(!engine.is_active());
        assert!(!engine.is_buffering());
    }

    #[test]
    fn a_new_engine_has_no_seekable_live_edge() {
        let Ok(engine) = Engine::new() else {
            return;
        };
        assert_eq!(engine.seekable_end(), None);
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

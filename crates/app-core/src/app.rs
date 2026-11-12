//! The headless part of Flatline.
//!
//! Everything here is free of the user interface: it resolves URLs, decides
//! which streams to play and drives the player. The results travel back to
//! the interface through a channel, so the background work never has to
//! touch the toolkit.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use media_model::{MediaSource, PlaybackPlan, Quality};
use playback_engine::{Engine, LoadRequest};
use stream_resolver::{CommandSpec, Resolver};
use ui_kit::{SourceLists, SourceOption};
use ytdlp_tool::YtDlp;

use crate::paths::AppPaths;
use crate::timeline::{LiveAxis, Timeline};

/// Something the background work wants to tell the user interface.
#[derive(Debug, Clone)]
pub enum Message {
    /// A URL was resolved into a catalog of streams.
    Resolved(Result<MediaSource, String>),
    /// A newer `yt-dlp` was downloaded and is now in use.
    ToolUpdated(String),
    /// Something went wrong that the user should know about.
    Problem(String),
}

/// What the user interface has to show after a media was accepted.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// Title of the media.
    pub title: String,
    /// Duration in seconds, when the extractor knows it.
    pub duration: Option<f64>,
    /// The streams the user can pick from.
    pub sources: SourceLists,
    /// The video-only stream that started playing, if any.
    pub video: Option<String>,
    /// The audio-only stream that started playing, if any.
    pub audio: Option<String>,
    /// The combined stream that started playing, if any.
    pub muxed: Option<String>,
    /// Whether the media is a live stream.
    pub is_live: bool,
}

/// The state of the loaded media.
#[derive(Debug, Default)]
struct State {
    source: Option<MediaSource>,
    selected_video: Option<String>,
    selected_audio: Option<String>,
    selected_muxed: Option<String>,
    is_live: bool,
}

/// The application, without a user interface.
pub struct App {
    engine: Engine,
    resolver: Mutex<Resolver>,
    runtime: tokio::runtime::Runtime,
    outbox: Arc<Mutex<Sender<Box<Message>>>>,
    inbox: Mutex<Receiver<Box<Message>>>,
    state: Mutex<State>,
    /// The time axis of a live stream.
    live: Mutex<LiveAxis>,
}

impl App {
    /// Creates the application around a `yt-dlp` installation.
    pub fn new(ytdlp: &YtDlp) -> Result<App, Error> {
        let engine = Engine::new().map_err(|error| Error::Engine(error.to_string()))?;
        let resolver = Resolver::new(command_spec(ytdlp));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| Error::Runtime(error.to_string()))?;
        let (sender, receiver) = channel();

        Ok(App {
            engine,
            resolver: Mutex::new(resolver),
            runtime,
            outbox: Arc::new(Mutex::new(sender)),
            inbox: Mutex::new(receiver),
            state: Mutex::new(State::default()),
            live: Mutex::new(LiveAxis::new()),
        })
    }

    /// The player.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Resolves a URL and plays it.
    ///
    /// The work happens in the background; the result arrives through
    /// [`App::poll`].
    pub fn load(&self, url: String) {
        let url = url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let resolver = self.resolver.lock().expect("the resolver is not poisoned").clone();
        let outbox = Arc::clone(&self.outbox);
        let runtime = self.runtime.handle().clone();

        runtime.spawn_blocking(move || {
            let result = resolver.resolve(&url).map_err(|error| error.to_string());
            let _ = outbox.lock().map(|sender| sender.send(Box::new(Message::Resolved(result))));
        });
    }

    /// Accepts a resolved source and starts playing it.
    ///
    /// The best streams start playing straight away: the best video with the
    /// best audio next to it, a combined stream, or an audio-only stream for
    /// media that has no picture.
    pub fn accept(&self, source: MediaSource) -> Loaded {
        let sources = source_lists(&source);
        let default = stream_resolver::plan(&source, &Quality::Best).ok();
        let (video, audio, muxed) = default.as_ref().map(selection_of).unwrap_or_default();
        let loaded = Loaded {
            title: source.title.clone(),
            duration: source.duration,
            sources,
            video: video.clone(),
            audio: audio.clone(),
            muxed: muxed.clone(),
            is_live: source.is_live,
        };

        {
            let mut state = self.state.lock().expect("the state is not poisoned");
            state.source = Some(source);
            state.selected_video = video;
            state.selected_audio = audio;
            state.selected_muxed = muxed;
            state.is_live = loaded.is_live;
        }
        self.live.lock().expect("the live axis is not poisoned").reset();

        match default {
            Some(plan) => {
                if let Err(error) = self.start(&plan) {
                    self.report(error);
                }
            }
            None => self.report("this source has no playable stream".to_string()),
        }
        loaded
    }

    /// Plays a source selection.
    ///
    /// A combined stream is exclusive: picking one clears the video-only and
    /// audio-only choices, because it already carries both tracks.
    pub fn select_sources(
        &self,
        video: Option<String>,
        audio: Option<String>,
        muxed: Option<String>,
    ) {
        let (quality, video, audio, muxed) = match muxed {
            Some(id) => {
                (Quality::Streams { video: Some(id.clone()), audio: None }, None, None, Some(id))
            }
            None => (
                Quality::Streams { video: video.clone(), audio: audio.clone() },
                video,
                audio,
                None,
            ),
        };

        let plan = {
            let state = self.state.lock().expect("the state is not poisoned");
            let Some(source) = state.source.as_ref() else {
                self.report("there is nothing to play".to_string());
                return;
            };
            match stream_resolver::plan(source, &quality) {
                Ok(plan) => plan,
                Err(error) => {
                    self.report(error.to_string());
                    return;
                }
            }
        };

        if let Err(error) = self.start(&plan) {
            self.report(error);
            return;
        }
        self.live.lock().expect("the live axis is not poisoned").reset();

        let mut state = self.state.lock().expect("the state is not poisoned");
        state.selected_video = video;
        state.selected_audio = audio;
        state.selected_muxed = muxed;
    }

    /// Switches between playing and paused.
    pub fn toggle_play(&self) {
        if let Err(error) = self.engine.toggle_pause() {
            self.report(error.to_string());
        }
    }

    /// Jumps to a position in seconds.
    ///
    /// The position is the one the interface shows. For a live stream that is
    /// a position inside the shown window, not a position in the stream.
    pub fn seek(&self, seconds: f64) {
        let target = if self.is_live() {
            let mut live = self.live.lock().expect("the live axis is not poisoned");
            live.anchor();
            live.to_stream(seconds)
        } else {
            seconds
        };
        if let Err(error) = self.engine.seek(target) {
            self.report(error.to_string());
        }
    }

    /// Jumps back by a number of seconds.
    ///
    /// This is the fast replay of a live stream.
    pub fn seek_back(&self, seconds: f64) {
        if let Err(error) = self.engine.seek_relative(-seconds.abs()) {
            self.report(error.to_string());
        }
    }

    /// Jumps back to the live edge of a live stream.
    ///
    /// The button must never leave the player paused, and a click while the
    /// playback already sits at the edge must not seek at all: seeking some
    /// backends pause first, which is what made the button look broken.
    pub fn go_live(&self) {
        if !self.is_live() {
            return;
        }

        match self.engine.seek_to_live_edge() {
            Ok(()) => {
                self.live.lock().expect("the live axis is not poisoned").anchor();
                if let Err(error) = self.engine.play() {
                    self.report(error.to_string());
                }
            }
            Err(error) => self.report(error.to_string()),
        }
    }

    /// Whether the media that is playing is a live stream.
    pub fn is_live(&self) -> bool {
        self.state.lock().expect("the state is not poisoned").is_live
    }

    /// The time axis that is shown to the user.
    pub fn timeline(&self) -> Timeline {
        let position = self.engine.position();
        let duration = self.engine.duration();
        if self.is_live() {
            let mut live = self.live.lock().expect("the live axis is not poisoned");
            if let Some(edge) = self.engine.seekable_end() {
                live.observe(edge.max(position));
            }
            if self.engine.is_paused() {
                live.advance_while_paused(std::time::Instant::now());
            }
            live.sync(position);
            live.timeline(position)
        } else {
            Timeline::recorded(position, duration)
        }
    }

    /// Changes the volume.
    pub fn set_volume(&self, volume: f64) {
        if let Err(error) = self.engine.set_volume(volume) {
            self.report(error.to_string());
        }
    }

    /// Uses another `yt-dlp` installation from now on.
    pub fn use_tool(&self, ytdlp: &YtDlp) {
        let mut resolver = self.resolver.lock().expect("the resolver is not poisoned");
        *resolver = Resolver::new(command_spec(ytdlp));
    }

    /// Looks for a newer `yt-dlp` in the background.
    pub fn check_tool_update(&self, ytdlp: &YtDlp, paths: &AppPaths) {
        let ytdlp = ytdlp.clone();
        let paths = paths.clone();
        let outbox = Arc::clone(&self.outbox);
        let runtime = self.runtime.handle().clone();

        runtime.spawn_blocking(move || {
            match ytdlp_tool::update::check_and_update(&ytdlp, &paths.tool_paths()) {
                Ok(outcome) if outcome.updated => {
                    let _ = outbox.lock().map(|sender| {
                        sender.send(Box::new(Message::ToolUpdated(
                            outcome.path.to_string_lossy().into_owned(),
                        )))
                    });
                }
                Ok(outcome) => {
                    crate::diag::note(&format!("yt-dlp {} is up to date", outcome.current));
                }
                Err(error) => {
                    crate::diag::note(&format!("the yt-dlp update check failed: {error}"));
                }
            }
        });
    }

    /// Takes the messages the background work produced.
    pub fn poll(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        if let Ok(receiver) = self.inbox.lock() {
            while let Ok(message) = receiver.try_recv() {
                messages.push(*message);
            }
        }
        messages
    }

    /// Loads a finished playback plan into the player.
    fn start(&self, plan: &PlaybackPlan) -> Result<(), String> {
        let request = match &plan.audio {
            Some(audio) => LoadRequest::with_audio(plan.media_url.clone(), audio.url.clone()),
            None => LoadRequest::new(plan.media_url.clone()),
        };
        self.engine.load(&request).map_err(|error| error.to_string())
    }

    /// Sends a problem to the user interface.
    fn report(&self, text: String) {
        let _ = self.outbox.lock().map(|sender| sender.send(Box::new(Message::Problem(text))));
    }
}

/// The streams of a source, grouped for the source picker.
///
/// A combined stream goes into its own column and is left out of the
/// video-only column, so the three columns never offer the same file twice.
fn source_lists(source: &MediaSource) -> SourceLists {
    let mut video = Vec::new();
    let mut muxed = Vec::new();
    for stream in &source.video_streams {
        // A row without a known size cannot be labelled for the user and is
        // very likely a container the extractor could not inspect.
        if stream.width.is_none() && stream.height.is_none() {
            continue;
        }
        let item =
            SourceOption::new(stream.format_id.clone(), video_label(stream), video_detail(stream));
        if stream.has_audio {
            muxed.push(item);
        } else {
            video.push(item);
        }
    }
    let audio = source
        .audio_streams
        .iter()
        .map(|stream| {
            SourceOption::new(stream.format_id.clone(), audio_label(stream), audio_detail(stream))
        })
        .collect();

    SourceLists { video, audio, muxed }
}

/// The three identifiers a plan is playing.
///
/// A combined plan reports itself in the muxed column; a split plan reports
/// the video and the audio separately.
fn selection_of(plan: &PlaybackPlan) -> (Option<String>, Option<String>, Option<String>) {
    let Some(video) = plan.video.as_ref() else {
        return (None, plan.audio.as_ref().map(|audio| audio.format_id.clone()), None);
    };
    if video.has_audio {
        (None, None, Some(video.format_id.clone()))
    } else {
        (
            Some(video.format_id.clone()),
            plan.audio.as_ref().map(|audio| audio.format_id.clone()),
            None,
        )
    }
}

/// The prominent label of a video stream: its resolution.
fn video_label(stream: &media_model::VideoStream) -> String {
    match (stream.width, stream.height) {
        (Some(_), Some(height)) if height >= 720 => format!("{height}p"),
        (Some(width), Some(height)) => format!("{width}×{height}"),
        (None, Some(height)) => format!("{height}p"),
        _ => stream.format_id.clone(),
    }
}

/// The supporting line of a video stream.
fn video_detail(stream: &media_model::VideoStream) -> String {
    let mut parts = Vec::new();
    if let (Some(width), Some(height)) = (stream.width, stream.height) {
        parts.push(format!("{width}×{height}"));
    }
    if let Some(fps) = stream.fps
        && fps > 0.0
    {
        parts.push(format!("{} fps", fps.round() as u64));
    }
    if stream.has_audio {
        parts.push("muxed".to_string());
    }
    if let Some(bitrate) = stream.total_bitrate {
        parts.push(format_bitrate(bitrate));
    }
    if !stream.ext.is_empty() {
        parts.push(stream.ext.clone());
    }
    parts.join(" · ")
}

/// The prominent label of an audio stream: its bitrate.
fn audio_label(stream: &media_model::AudioStream) -> String {
    if let Some(bitrate) = stream.bitrate {
        return format_bitrate(bitrate);
    }
    if let Some(note) = stream.note.as_ref().filter(|note| !note.is_empty()) {
        return note.clone();
    }
    format!("Audio {}", stream.format_id)
}

/// The supporting line of an audio stream.
fn audio_detail(stream: &media_model::AudioStream) -> String {
    let mut parts = Vec::new();
    if let Some(codec) = stream.audio_codec.as_deref().filter(|codec| *codec != "none") {
        parts.push(codec.to_string());
    }
    if let Some(language) = stream.language.as_ref().filter(|language| !language.is_empty()) {
        parts.push(language.clone());
    }
    if !stream.ext.is_empty() {
        parts.push(stream.ext.clone());
    }
    if let Some(note) = stream.note.as_ref().filter(|note| !note.is_empty()) {
        parts.push(note.clone());
    }
    parts.push(stream.format_id.clone());
    parts.join(" · ")
}

/// Turns a bitrate in kilobits per second into a human line.
fn format_bitrate(kilobits: u64) -> String {
    if kilobits >= 1000 {
        format!("{:.1} Mb/s", kilobits as f64 / 1000.0)
    } else {
        format!("{kilobits} kb/s")
    }
}

/// Describes how `yt-dlp` has to be invoked.
fn command_spec(ytdlp: &YtDlp) -> CommandSpec {
    match ytdlp.interpreter() {
        Some(interpreter) => CommandSpec::with_interpreter(ytdlp.program(), interpreter),
        None => CommandSpec::new(ytdlp.program()),
    }
}

/// Everything that can go wrong while running the application.
#[derive(Debug)]
pub enum Error {
    /// The user interface could not be created.
    Ui(String),
    /// The player could not be created.
    Engine(String),
    /// The background runtime could not be started.
    Runtime(String),
    /// The tools could not be prepared.
    Tools(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Ui(message) => write!(f, "the user interface failed: {message}"),
            Error::Engine(message) => write!(f, "the player failed: {message}"),
            Error::Runtime(message) => write!(f, "the background runtime failed: {message}"),
            Error::Tools(message) => write!(f, "the tools failed: {message}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use media_model::{AudioStream, VideoStream};

    fn video(id: &str, height: Option<u32>, has_audio: bool) -> VideoStream {
        VideoStream {
            format_id: id.to_string(),
            url: format!("https://example.com/{id}.mp4"),
            ext: "mp4".to_string(),
            width: height.map(|h| h * 16 / 9),
            height,
            fps: Some(60.0),
            video_codec: Some("avc1".to_string()),
            total_bitrate: Some(4500),
            file_size: None,
            has_audio,
            protocol: None,
            note: None,
        }
    }

    fn audio(id: &str) -> AudioStream {
        AudioStream {
            format_id: id.to_string(),
            url: format!("https://example.com/{id}.m4a"),
            ext: "m4a".to_string(),
            audio_codec: Some("mp4a".to_string()),
            bitrate: Some(128),
            file_size: None,
            language: None,
            note: None,
            protocol: None,
        }
    }

    fn source() -> MediaSource {
        MediaSource {
            id: "id".to_string(),
            title: "Title".to_string(),
            uploader: None,
            duration: Some(10.0),
            thumbnail: None,
            webpage_url: None,
            extractor: None,
            is_live: false,
            video_streams: vec![video("137", Some(1080), false), video("18", Some(360), true)],
            audio_streams: vec![audio("140")],
        }
    }

    #[test]
    fn combined_streams_move_to_their_own_column() {
        let lists = source_lists(&source());
        assert_eq!(lists.video.len(), 1);
        assert_eq!(lists.video[0].id, "137");
        assert_eq!(lists.muxed.len(), 1);
        assert_eq!(lists.muxed[0].id, "18");
        assert_eq!(lists.audio.len(), 1);
    }

    #[test]
    fn rows_without_a_size_are_not_offered() {
        let mut source = source();
        source.video_streams.push(video("sb0", None, false));
        let lists = source_lists(&source);
        assert!(!lists.video.iter().any(|item| item.id == "sb0"));
        assert!(!lists.muxed.iter().any(|item| item.id == "sb0"));
    }

    #[test]
    fn a_split_plan_reports_both_columns() {
        let plan = PlaybackPlan {
            media_url: "https://example.com/137.mp4".to_string(),
            video: Some(video("137", Some(1080), false)),
            audio: Some(audio("140")),
            title: "Title".to_string(),
        };
        assert_eq!(selection_of(&plan), (Some("137".to_string()), Some("140".to_string()), None));
    }

    #[test]
    fn a_combined_plan_reports_the_muxed_column() {
        let plan = PlaybackPlan {
            media_url: "https://example.com/18.mp4".to_string(),
            video: Some(video("18", Some(360), true)),
            audio: None,
            title: "Title".to_string(),
        };
        assert_eq!(selection_of(&plan), (None, None, Some("18".to_string())));
    }

    #[test]
    fn an_audio_only_plan_reports_the_audio_column() {
        let plan = PlaybackPlan {
            media_url: "https://example.com/140.m4a".to_string(),
            video: None,
            audio: Some(audio("140")),
            title: "Title".to_string(),
        };
        assert_eq!(selection_of(&plan), (None, Some("140".to_string()), None));
    }

    #[test]
    fn bitrates_are_readable() {
        assert_eq!(format_bitrate(128), "128 kb/s");
        assert_eq!(format_bitrate(4500), "4.5 Mb/s");
    }

    #[test]
    fn labels_carry_the_size_the_user_asked_for() {
        let lists = source_lists(&source());
        assert_eq!(lists.video[0].label, "1080p");
        assert!(lists.video[0].detail.contains("1920×1080"));
        assert!(lists.audio[0].label.contains("128"));
    }
}

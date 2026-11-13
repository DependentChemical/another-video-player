//! The headless part of Flatline.
//!
//! Everything here is free of the user interface: it resolves URLs, decides
//! which streams to play and drives the player. The results travel back to
//! the interface through a channel, so the background work never has to
//! touch the toolkit.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use library::{Queue, ThumbnailCache};
use media_model::{MediaSource, PlaybackPlan, Quality};
use playback_engine::{Engine, LoadRequest};
use settings::Settings;
use stream_resolver::{CommandSpec, Resolver};
use ui_kit::{SourceLists, SourceOption};
use ytdlp_tool::YtDlp;

use crate::paths::AppPaths;
use crate::timeline::{LiveAxis, Timeline};

/// Something the background work wants to tell the user interface.
#[derive(Debug, Clone)]
pub enum Message {
    /// A URL was resolved and can start playing.
    Resolved(Result<ResolvedMedia, String>),
    /// A URL was resolved while something else plays, so the user decides.
    Pending(Result<PendingMedia, String>),
    /// A newer `yt-dlp` was downloaded and is now in use.
    ToolUpdated(String),
    /// Something went wrong that the user should know about.
    Problem(String),
}

/// A URL that was resolved and is ready to play.
#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    /// The URL the media was asked for.
    pub url: String,
    /// The thumbnail of the resolved media, for the queue.
    pub thumbnail: Option<String>,
    /// The time the stream URLs were resolved.
    pub resolved_at: i64,
    /// The resolved source.
    pub source: MediaSource,
}

/// A URL that was resolved while another media plays.
///
/// The user chooses what happens next: the media is either appended to the
/// active queue or it replaces the queue and plays right away.
#[derive(Debug, Clone)]
pub struct PendingMedia {
    /// The URL the media was asked for.
    pub url: String,
    /// Human readable title, for the dialog.
    pub title: String,
    /// The cached thumbnail, when one could be downloaded.
    pub thumbnail: Option<PathBuf>,
    /// The time the stream URLs were resolved.
    pub resolved_at: i64,
    /// The resolved source.
    pub source: MediaSource,
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
    /// The thumbnail URL as reported by the extractor.
    pub thumbnail_url: Option<String>,
    /// The uploader / channel name as reported by the extractor.
    pub uploader: Option<String>,
}

/// How long a persisted resolver result may be reused before it is refreshed.
const RESOLVED_SOURCE_MAX_AGE: i64 = 10 * 60;

/// Seconds since the Unix epoch for persisted resolver freshness checks.
fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
enum PendingSeek {
    Position(f64),
    LiveEdge,
}

#[derive(Debug, Default)]
struct PendingRestore {
    seek: Option<PendingSeek>,
    file_started: bool,
}

impl PendingRestore {
    fn begin(&mut self, seek: Option<PendingSeek>) {
        self.seek = seek;
        self.file_started = false;
    }

    fn file_started(&mut self) {
        if self.seek.is_some() {
            self.file_started = true;
        }
    }

    fn file_loaded(&mut self) -> Option<PendingSeek> {
        if self.file_started {
            self.file_started = false;
            self.seek.take()
        } else {
            None
        }
    }
}

/// The state of the loaded media.
#[derive(Debug, Default)]
struct State {
    source: Option<MediaSource>,
    selected_video: Option<String>,
    selected_audio: Option<String>,
    selected_muxed: Option<String>,
    is_live: bool,
    pending_restore: PendingRestore,
    /// The id_hash of the currently playing video, for history tracking.
    current_id_hash: Option<String>,
    /// The url of the currently playing video, for history tracking.
    current_url: Option<String>,
    /// The title of the currently playing video, for history tracking.
    current_title: Option<String>,
    /// The thumbnail_url of the currently playing video, for history tracking.
    current_thumbnail_url: Option<String>,
    /// The uploader of the currently playing video, for history tracking.
    current_uploader: Option<String>,
    /// Last time we saved history position (to throttle saves to ~20s intervals).
    last_history_save: Option<Instant>,
}

/// The application, without a user interface.
pub struct App {
    engine: Engine,
    resolver: Mutex<Resolver>,
    settings: Settings,
    youtube_cookies: std::path::PathBuf,
    runtime: tokio::runtime::Runtime,
    outbox: Arc<Mutex<Sender<Box<Message>>>>,
    inbox: Mutex<Receiver<Box<Message>>>,
    state: Mutex<State>,
    /// The time axis of a live stream.
    live: Mutex<LiveAxis>,
    /// The active queue Flatline keeps between runs.
    queue: Queue,
    /// The thumbnails shown before a media is accepted.
    cache: ThumbnailCache,
    /// The media the user still has to decide about.
    pending: Mutex<Option<PendingMedia>>,
}

impl App {
    /// Creates the application around a `yt-dlp` installation.
    pub fn new(ytdlp: &YtDlp, paths: &AppPaths) -> Result<App, Error> {
        let engine = Engine::new().map_err(|error| Error::Engine(error.to_string()))?;
        let youtube_cookies = paths.youtube_cookies();
        let resolver = Resolver::new(command_spec(ytdlp, &youtube_cookies));
        let settings =
            Settings::load(paths.settings()).map_err(|error| Error::Settings(error.to_string()))?;
        let queue =
            Queue::open(paths.database()).map_err(|error| Error::Library(error.to_string()))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| Error::Runtime(error.to_string()))?;
        let (sender, receiver) = channel();

        Ok(App {
            engine,
            resolver: Mutex::new(resolver),
            settings,
            youtube_cookies,
            runtime,
            outbox: Arc::new(Mutex::new(sender)),
            inbox: Mutex::new(receiver),
            state: Mutex::new(State::default()),
            live: Mutex::new(LiveAxis::new()),
            queue,
            cache: ThumbnailCache::new(paths.cache().join("thumbnails")),
            pending: Mutex::new(None),
        })
    }

    /// The player.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Takes a URL from the user.
    ///
    /// When nothing plays, the URL is resolved and played right away and
    /// becomes the active queue. When a media is already playing, playback is
    /// left alone: the URL is resolved in the background and the user is
    /// asked what to do with it through the pending dialog.
    ///
    /// The work happens in the background; the result arrives through
    /// [`App::poll`].
    pub fn load(&self, url: String) {
        let url = url.trim().to_string();
        if url.is_empty() {
            return;
        }
        if self.has_media() {
            self.resolve_pending(url);
        } else {
            self.prepare_current(&url, "", None, None, None);
            self.consume_current();
        }
    }

    /// Whether a media is loaded in the player.
    pub fn has_media(&self) -> bool {
        self.state.lock().expect("the state is not poisoned").source.is_some()
    }

    /// Resolves the URL read from the active queue in the background.
    fn resolve_play(&self, url: String) {
        let resolver = self.resolver.lock().expect("the resolver is not poisoned").clone();
        let outbox = Arc::clone(&self.outbox);
        let cache = self.cache.clone();
        let runtime = self.runtime.handle().clone();

        runtime.spawn_blocking(move || {
            let result = resolver.resolve(&url).map_err(|error| error.to_string()).map(|source| {
                let thumbnail = source
                    .thumbnail
                    .as_deref()
                    .and_then(|url| cache.fetch(url).ok())
                    .map(|path| path.to_string_lossy().into_owned());
                ResolvedMedia { url: url.clone(), thumbnail, resolved_at: unix_time(), source }
            });
            let _ = outbox.lock().map(|sender| sender.send(Box::new(Message::Resolved(result))));
        });
    }

    /// Resolves a URL in the background, caches its thumbnail and waits for
    /// the user to decide.
    fn resolve_pending(&self, url: String) {
        let resolver = self.resolver.lock().expect("the resolver is not poisoned").clone();
        let outbox = Arc::clone(&self.outbox);
        let cache = self.cache.clone();
        let runtime = self.runtime.handle().clone();

        runtime.spawn_blocking(move || {
            let result = resolver.resolve(&url).map_err(|error| error.to_string()).map(|source| {
                let thumbnail = source.thumbnail.as_deref().and_then(|url| cache.fetch(url).ok());
                PendingMedia {
                    url,
                    title: source.title.clone(),
                    source,
                    thumbnail,
                    resolved_at: unix_time(),
                }
            });
            let _ = outbox.lock().map(|sender| sender.send(Box::new(Message::Pending(result))));
        });
    }

    /// Stores the pending media so the user can decide what to do with it.
    pub fn set_pending(&self, pending: PendingMedia) {
        *self.pending.lock().expect("the pending media is not poisoned") = Some(pending);
    }

    /// Plays the pending media right away, replacing the active queue.
    pub fn play_pending(&self) -> Option<(String, Loaded)> {
        let pending = self.pending.lock().expect("the pending media is not poisoned").take()?;
        let thumbnail = pending.thumbnail.as_ref().map(|path| path.to_string_lossy().into_owned());
        let thumbnail_url = pending.source.thumbnail.clone();
        let uploader = pending.source.uploader.clone();
        self.prepare_current(&pending.url, &pending.title, thumbnail.as_deref(), thumbnail_url.as_deref(), uploader.as_deref());
        let loaded = self.consume_resolved(
            &pending.url,
            pending.source,
            thumbnail.as_deref(),
            pending.resolved_at,
        )?;
        Some((pending.url, loaded))
    }

    /// Appends the pending media to the active queue.
    pub fn queue_pending(&self) -> bool {
        let Some(pending) = self.pending.lock().expect("the pending media is not poisoned").take()
        else {
            return false;
        };
        let thumbnail = pending.thumbnail.as_ref().map(|path| path.to_string_lossy().into_owned());
        let thumbnail_url = pending.source.thumbnail.clone();
        let uploader = pending.source.uploader.clone();
        let entry = match self.queue.push(&pending.url, &pending.title, thumbnail.as_deref(), thumbnail_url.as_deref(), uploader.as_deref()) {
            Ok(entry) => entry,
            Err(error) => {
                crate::diag::note(&format!("the active queue could not be written: {error}"));
                return false;
            }
        };
        if let Ok(source) = serde_json::to_string(&pending.source)
            && let Err(error) = self.queue.set_resolved_source(
                entry.position,
                Some(&source),
                Some(pending.resolved_at),
            )
        {
            crate::diag::note(&format!("the resolved queue source could not be saved: {error}"));
        }
        true
    }

    /// Forgets the pending media without doing anything with it.
    pub fn dismiss_pending(&self) {
        self.pending.lock().expect("the pending media is not poisoned").take();
    }

    /// Puts a freshly resolved media in the active queue before it is loaded.
    pub fn prepare_current(&self, url: &str, title: &str, thumbnail: Option<&str>, thumbnail_url: Option<&str>, uploader: Option<&str>) {
        let thumbnail_path = thumbnail.map(PathBuf::from);
        if let Err(error) = self.cache.retain(thumbnail_path.as_deref()) {
            crate::diag::note(&format!("the thumbnail cache could not be cleared: {error}"));
        }
        if let Err(error) = self.queue.replace_with(url, title, thumbnail, thumbnail_url, uploader) {
            crate::diag::note(&format!("the active queue could not be written: {error}"));
        }
    }

    /// Plays the media that plays next in the queue, if there is one.
    ///
    /// This is what a finished media calls: the current mark moves forward
    /// and the entry it lands on is resolved again, because an extractor may
    /// have changed since the entry was added.
    pub fn advance(&self) -> bool {
        match self.queue.advance() {
            Ok(Some(_)) => {
                self.consume_current();
                true
            }
            Ok(None) => false,
            Err(error) => {
                crate::diag::note(&format!("the active queue could not be read: {error}"));
                false
            }
        }
    }

    /// Starts the media the queue left as current, if there is one.
    ///
    /// This is what the program calls on startup so a queue that was filled
    /// in an earlier run picks up where it left off.
    pub fn resume(&self) -> bool {
        match self.queue.current() {
            Ok(Some(_)) => {
                self.consume_current();
                true
            }
            Ok(None) => false,
            Err(error) => {
                crate::diag::note(&format!("the active queue could not be read: {error}"));
                false
            }
        }
    }

    /// Asks the current queue entry to be consumed.
    ///
    /// The URL is read from the queue, so callers cannot supply a player URL.
    pub fn consume_current(&self) {
        match self.queue.current() {
            Ok(Some(entry)) => {
                if let (Some(json), Some(resolved_at)) =
                    (entry.resolved_source.as_deref(), entry.resolved_at)
                    && unix_time().saturating_sub(resolved_at) <= RESOLVED_SOURCE_MAX_AGE
                    && let Ok(source) = serde_json::from_str::<MediaSource>(json)
                {
                    let message = Message::Resolved(Ok(ResolvedMedia {
                        url: entry.url,
                        thumbnail: entry.thumbnail,
                        resolved_at,
                        source,
                    }));
                    let _ = self.outbox.lock().map(|sender| sender.send(Box::new(message)));
                } else {
                    self.resolve_play(entry.url);
                }
            }
            Ok(None) => {}
            Err(error) => {
                crate::diag::note(&format!("the active queue could not be read: {error}"));
            }
        }
    }

    /// Finishes resolving the queue's current item, ignoring stale results.
    pub fn consume_resolved(
        &self,
        url: &str,
        source: MediaSource,
        thumbnail: Option<&str>,
        resolved_at: i64,
    ) -> Option<Loaded> {
        let current = match self.queue.current() {
            Ok(Some(entry)) if entry.url == url => entry,
            Ok(_) => return None,
            Err(error) => {
                crate::diag::note(&format!("the active queue could not be read: {error}"));
                return None;
            }
        };
        if let Err(error) = self.queue.update_details(
            current.position,
            &source.title,
            thumbnail,
            source.thumbnail.as_deref(),
            source.uploader.as_deref(),
        ) {
            crate::diag::note(&format!("the active queue entry could not be updated: {error}"));
        }
        if let Ok(json) = serde_json::to_string(&source)
            && let Err(error) =
                self.queue.set_resolved_source(current.position, Some(&json), Some(resolved_at))
        {
            crate::diag::note(&format!("the resolved queue source could not be saved: {error}"));
        }
        Some(self.accept_current_source(source, url))
    }

    /// Builds playback state for the source already authorized by the queue.
    fn accept_current_source(&self, source: MediaSource, url: &str) -> Loaded {
        let sources = source_lists(&source);
        let default = self
            .settings
            .select(&source, url)
            .and_then(|quality| {
                stream_resolver::plan(&source, &quality)
                    .map_err(|error| settings::Error::Invalid(error.to_string()))
            })
            .ok();
        let (video, audio, muxed) = default.as_ref().map(selection_of).unwrap_or_default();
        let loaded = Loaded {
            title: source.title.clone(),
            duration: source.duration,
            sources,
            video: video.clone(),
            audio: audio.clone(),
            muxed: muxed.clone(),
            is_live: source.is_live,
            thumbnail_url: source.thumbnail.clone(),
            uploader: source.uploader.clone(),
        };

        // Compute id_hash for history tracking
        let id_hash = media_model::video_id_hash(url);

        // Check history for saved playback position (only for non-live videos)
        let history_position = if !source.is_live {
            self.queue.history_get(&id_hash)
                .ok()
                .flatten()
                .map(|entry| entry.position_seconds)
        } else {
            None
        };

        {
            let mut state = self.state.lock().expect("the state is not poisoned");
            state.source = Some(source);
            state.selected_video = video;
            state.selected_audio = audio;
            state.selected_muxed = muxed;
            state.is_live = loaded.is_live;
            state.pending_restore.begin(history_position.map(PendingSeek::Position));
            // Store metadata for history tracking
            state.current_id_hash = Some(id_hash.clone());
            state.current_url = Some(url.to_string());
            state.current_title = Some(loaded.title.clone());
            state.current_thumbnail_url = loaded.thumbnail_url.clone(); // We'll need to add this to Loaded
            state.current_uploader = loaded.uploader.clone(); // We'll need to add this to Loaded
            state.last_history_save = None;
        }
        self.live.lock().expect("the live axis is not poisoned").reset();

        match default {
            Some(plan) => {
                if let Err(error) = self.start(&plan) {
                    self.report(error);
                } else if let Err(error) = self.engine.play() {
                    self.report(error.to_string());
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

        let pending = {
            let state = self.state.lock().expect("the state is not poisoned");
            if state.is_live {
                Some(PendingSeek::LiveEdge)
            } else {
                let position = self.engine.position().max(0.0);
                (position > 0.0).then_some(PendingSeek::Position(position))
            }
        };

        if let Err(error) = self.start(&plan) {
            self.report(error);
            return;
        }
        self.state.lock().expect("the state is not poisoned").pending_restore.begin(pending);
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
        *resolver = Resolver::new(command_spec(ytdlp, &self.youtube_cookies));
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

    /// Marks the start of a replacement file so an older load event is ignored.
    pub fn on_file_started(&self) {
        let mut state = self.state.lock().expect("the state is not poisoned");
        state.pending_restore.file_started();
    }

    /// Restores playback position after a source change once the file is ready.
    pub fn on_file_loaded(&self) {
        let pending =
            self.state.lock().expect("the state is not poisoned").pending_restore.file_loaded();
        match pending {
            Some(PendingSeek::Position(position)) => {
                if let Err(error) = self.engine.seek(position) {
                    self.report(error.to_string());
                }
            }
            Some(PendingSeek::LiveEdge) => {
                if let Err(error) = self.engine.seek_to_live_edge() {
                    self.report(error.to_string());
                }
                let _ = self.engine.play();
                self.live.lock().expect("the live axis is not poisoned").anchor();
            }
            None => {}
        }
    }

    /// Saves the current playback position to history (throttled to ~20s intervals).
    pub fn save_history_position(&self) {
        let mut state = self.state.lock().expect("the state is not poisoned");
        if state.is_live {
            return; // Don't save history for live streams
        }
        let Some(id_hash) = state.current_id_hash.take() else {
            return;
        };
        // Put it back after checking
        state.current_id_hash = Some(id_hash.clone());

        let now = Instant::now();
        let should_save = state.last_history_save.is_none_or(|last| {
            now.duration_since(last) >= Duration::from_secs(20)
        });

        if !should_save {
            return;
        }

        let position = self.engine.position().max(0.0);
        let Some(url) = state.current_url.clone() else {
            return;
        };
        let Some(title) = state.current_title.clone() else {
            return;
        };
        let thumbnail_url = state.current_thumbnail_url.clone();
        let uploader = state.current_uploader.clone();

        drop(state); // Release lock before DB operation

        if let Err(error) = self.queue.history_upsert(&id_hash, &url, &title, thumbnail_url.as_deref(), uploader.as_deref(), position) {
            crate::diag::note(&format!("failed to save history position: {error}"));
        } else {
            // Update last save time
            if let Ok(mut state) = self.state.lock() {
                state.last_history_save = Some(now);
            }
        }
    }

    /// Resets the history position to 0 (called when video finishes).
    pub fn reset_history_position(&self) {
        let state = self.state.lock().expect("the state is not poisoned");
        let Some(id_hash) = state.current_id_hash.clone() else {
            return;
        };
        drop(state);

        if let Err(error) = self.queue.history_reset_position(&id_hash) {
            crate::diag::note(&format!("failed to reset history position: {error}"));
        }
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
fn command_spec(ytdlp: &YtDlp, youtube_cookies: &std::path::Path) -> CommandSpec {
    let spec = match ytdlp.interpreter() {
        Some(interpreter) => CommandSpec::with_interpreter(ytdlp.program(), interpreter),
        None => CommandSpec::new(ytdlp.program()),
    };
    spec.with_youtube_cookies(youtube_cookies)
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
    /// The playback settings could not be loaded.
    Settings(String),
    /// The tools could not be prepared.
    Tools(String),
    /// The library, queue or cache could not be opened.
    Library(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Ui(message) => write!(f, "the user interface failed: {message}"),
            Error::Engine(message) => write!(f, "the player failed: {message}"),
            Error::Runtime(message) => write!(f, "the background runtime failed: {message}"),
            Error::Settings(message) => write!(f, "{message}"),
            Error::Tools(message) => write!(f, "the tools failed: {message}"),
            Error::Library(message) => write!(f, "the library failed: {message}"),
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
    fn restore_waits_for_the_new_file_start_before_file_loaded() {
        let mut restore = PendingRestore::default();
        restore.begin(Some(PendingSeek::Position(42.0)));

        assert!(restore.file_loaded().is_none());
        restore.file_started();
        assert!(matches!(restore.file_loaded(), Some(PendingSeek::Position(p)) if p == 42.0));
        assert!(restore.file_loaded().is_none());
    }

    #[test]
    fn labels_carry_the_size_the_user_asked_for() {
        let lists = source_lists(&source());
        assert_eq!(lists.video[0].label, "1080p");
        assert!(lists.video[0].detail.contains("1920×1080"));
        assert!(lists.audio[0].label.contains("128"));
    }
}

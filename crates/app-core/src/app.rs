//! The headless part of Flatline.
//!
//! Everything here is free of the user interface: it resolves URLs, decides
//! which streams to play and drives the player. The results travel back to
//! the interface through a channel, so the background work never has to
//! touch the toolkit.

use library::{Queue, ThumbnailCache};
use media_model::{MediaSource, PlaybackPlan, Quality, Service};
use playback_engine::{Engine, LoadRequest};
use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use stream_resolver::Resolver;
use ui_kit::{SourceLists, SourceOption};
use ytdlp_tool::{ProxyPool, YtDlp};

use crate::paths::AppPaths;
use crate::session::Session;
use crate::timeline::{LiveAxis, Timeline};

/// How often Flatline checks GitHub for application and tool updates.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Something the background work wants to tell the user interface.
#[derive(Debug, Clone)]
pub enum Message {
    /// A URL was resolved and can start playing.
    Resolved(Result<ResolvedMedia, ResolveFailure>),
    /// A URL the user submitted was resolved and waits for the user's choice.
    Pending(PendingMedia),
    /// A playlist URL was resolved and waits for the user's choice.
    PendingPlaylist(PendingPlaylist),
    /// A URL the user submitted could not be processed.
    UrlEntryFailed {
        /// The URL that failed.
        url: String,
        /// The error message.
        error: String,
        /// The entry generation the failure belongs to.
        epoch: u64,
    },
    /// A service asks the user to sign in.
    SigningIn(Service),
    /// The cookie file for a service is being regenerated.
    RegeneratingCookies(Service),
    /// A newer application release is available.
    AppUpdateAvailable(updater::Update),
    /// The accepted application update is downloading.
    AppUpdateProgress(f64),
    /// An accepted application update could not be prepared.
    AppUpdateFailed(String),
    /// A newer `yt-dlp` release is available.
    ToolUpdateAvailable(ytdlp_tool::ToolUpdate),
    /// The accepted `yt-dlp` update is downloading.
    ToolUpdateProgress(f64),
    /// The accepted `yt-dlp` update is being applied.
    ToolUpdateStatus(String),
    /// The `yt-dlp` update has been applied.
    ToolUpdateApplied(String),
    /// The `yt-dlp` update could not be downloaded or applied.
    ToolUpdateFailed(String),
    /// Something went wrong that the user should know about.
    Problem(String),
    /// A short informational message shown to the user.
    Toast(String),
    /// A legacy thumbnail migration started; carries how many entries await processing.
    ThumbnailMigrationStarted { count: usize },
    /// A legacy thumbnail migration finished; carries how many entries were processed.
    ThumbnailMigrationFinished { processed: usize },
    /// A background thumbnail refresh for the active queue started; carries how
    /// many entries await processing.
    ThumbnailRefreshStarted { count: usize },
    /// A retry is scheduled.
    RetryScheduled {
        /// The error message.
        error: String,
        /// Seconds until retry.
        delay_seconds: u64,
        /// The kind of error being retried.
        kind: RetryKind,
    },
}

/// The kind of error that triggered a retry, for display purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// yt-dlp resolution failed.
    Resolver,
    /// mpv playback failed.
    Playback,
    /// Source selection failed.
    SourceSelection,
}

/// Why playback of a queue entry was started.
///
/// The reason travels with the resolved media so the log line that reports
/// what is playing can say whether the user asked for it or whether Flatline
/// advanced on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackReason {
    /// The user asked for this media, either from the URL bar or the queue.
    Manual,
    /// The previous media ended and the queue advanced on its own.
    Automatic,
    /// The queue that was left over from an earlier run is being resumed.
    Resume,
    /// A previous attempt failed and this is the scheduled retry.
    Retry,
}

impl PlaybackReason {
    /// The stable label written into the log.
    pub fn as_str(self) -> &'static str {
        match self {
            PlaybackReason::Manual => "manual",
            PlaybackReason::Automatic => "automatic",
            PlaybackReason::Resume => "startup-resume",
            PlaybackReason::Retry => "retry",
        }
    }
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
    /// Why this media is being played.
    pub reason: PlaybackReason,
    /// The resolved source.
    pub source: MediaSource,
    /// SponsorBlock segments fetched for the media, kept only in memory.
    pub sponsorblock_segments: Vec<source_youtube::sponsorblock::Segment>,
}

/// Maximum number of URL entry attempts before showing the final error.
const URL_ENTRY_MAX_ATTEMPTS: u32 = 6;

/// The state of the URL the user submitted.
///
/// Only one URL is processed at a time. The generation tells the results
/// apart: a result of an entry that was replaced belongs to a URL the
/// user no longer waits for, so it is discarded instead of reported.
#[derive(Debug, Default)]
struct UrlEntryState {
    /// The URL that is being processed.
    url: Option<String>,
    /// The generation of the entry that is running.
    epoch: u64,
    /// How many attempts have been made for the current URL.
    attempts: u32,
    /// Whether IPv4 fallback has been enabled for the current URL.
    force_ipv4: bool,
}

impl UrlEntryState {
    /// Starts a new entry for `url`.
    fn begin(&mut self, url: &str, epoch: u64) {
        self.url = Some(url.to_string());
        self.epoch = epoch;
        self.attempts = 1;
        self.force_ipv4 = false;
    }

    /// Whether `epoch` is the generation of the entry that is running.
    fn is_current(&self, epoch: u64) -> bool {
        epoch != 0 && self.epoch == epoch
    }

    /// Whether another attempt may still be made for `url`.
    fn may_retry(&self, url: &str) -> bool {
        self.url.as_deref() == Some(url) && self.attempts < URL_ENTRY_MAX_ATTEMPTS
    }

    /// Notes a retry attempt for `url` and enables IPv4 after the first error.
    fn mark_retry(&mut self, url: &str) -> bool {
        if !self.may_retry(url) {
            return false;
        }
        self.attempts += 1;
        self.force_ipv4 = true;
        true
    }
}

/// A resolve that failed.
#[derive(Debug, Clone)]
pub struct ResolveFailure {
    /// The URL that failed.
    pub url: String,
    /// The message shown to the user.
    pub message: String,
}
/// A URL that was resolved while another media plays.
#[derive(Debug, Clone)]
pub struct PendingMedia {
    /// The URL the media was asked for.
    pub url: String,
    /// Human readable title, for the dialog.
    pub title: String,
    /// The cached mid-size thumbnail, for the pending dialog.
    pub thumbnail: Option<PathBuf>,
    /// The thumbnail cache id the active queue should store.
    pub thumbnail_id: Option<String>,
    /// The time the stream URLs were resolved.
    pub resolved_at: i64,
    /// The resolved source.
    pub source: MediaSource,
    /// The URL entry generation this resolve belongs to.
    pub epoch: u64,
    /// SponsorBlock segments fetched for the media, kept only in memory.
    pub sponsorblock_segments: Vec<source_youtube::sponsorblock::Segment>,
}

/// A playlist that was resolved while another media plays.
#[derive(Debug, Clone)]
pub struct PendingPlaylist {
    /// The URL the playlist was asked for.
    pub url: String,
    /// Playlist title.
    pub title: String,
    /// Thumbnail for the dialog.
    pub thumbnail: Option<PathBuf>,
    /// URL of the video that should be selected after creating/playing playlist.
    pub selected_url: String,
    /// Whether the submitted URL explicitly selected a video inside the playlist.
    pub has_selected_video: bool,
    /// Resolved playlist entries.
    pub entries: Vec<PendingPlaylistEntry>,
    /// URL entry generation.
    pub epoch: u64,
}

/// One entry in a pending playlist.
#[derive(Debug, Clone)]
pub struct PendingPlaylistEntry {
    /// The URL of the entry.
    pub url: String,
    /// Human-readable title.
    pub title: String,
    /// Remote thumbnail URL.
    pub thumbnail_url: Option<String>,
    /// Remote thumbnail URLs in fallback order.
    pub thumbnail_urls: Vec<String>,
    /// Uploader/channel name.
    pub uploader: Option<String>,
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
    /// The full-size cached thumbnail, shown when the media has no video track.
    pub thumbnail: Option<PathBuf>,
    /// SponsorBlock segments fetched for the media, kept only in memory.
    pub sponsorblock_segments: Vec<source_youtube::sponsorblock::Segment>,
}

/// What happened when an entry was removed from the active queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueRemoval {
    /// Whether the entry was found and deleted.
    pub removed: bool,
    /// Whether the entry was playing and the next entry was started.
    pub advanced: bool,
}

/// How long a persisted resolver result may be reused before it is refreshed.
const RESOLVED_SOURCE_MAX_AGE: i64 = 10 * 60;

/// How often a failing resolver, player or selection is retried before the
/// failure is reported to the user instead of silently continuing.
const MAX_RETRY_ATTEMPTS: u32 = 6;

/// Delay before the next retry.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(0);

/// How long the background thumbnail pass waits between downloads or retries.
const THUMBNAIL_REFRESH_DELAY: Duration = Duration::from_secs(5);
/// How long the background thumbnail pass waits when there is no due work.
const THUMBNAIL_REFRESH_IDLE_DELAY: Duration = Duration::from_secs(15);

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
    /// The URL currently being resolved (for retry purposes).
    resolving_url: Option<String>,
    /// Retry state for the current URL.
    retry: RetryState,
    /// SponsorBlock segments for the current media, kept only in memory.
    sponsorblock_segments: Vec<source_youtube::sponsorblock::Segment>,
    /// Segment range suppressed after a manual seek, played once without auto-skip.
    sponsorblock_suppressed: Option<(f64, f64)>,
}

/// Retry state for a URL being resolved or played.
#[derive(Debug, Default)]
struct RetryState {
    /// The URL being retried.
    url: Option<String>,
    /// How many attempts have been made.
    attempt: u32,
    /// When to try again.
    next_retry: Option<Instant>,
    /// The type of error being retried.
    error_kind: Option<RetryErrorKind>,
    /// Last error message, used to keep backoff stable for identical failures.
    last_error: Option<String>,
}

/// The kind of error that triggered a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryErrorKind {
    /// yt-dlp resolution failed (e.g., bot detection).
    Resolver,
    /// mpv playback failed.
    Playback,
    /// Source selection failed (no playable streams).
    SourceSelection,
}

impl RetryState {
    /// Starts a retry for the given URL and error kind.
    ///
    /// Returns `false` when the same retry is already scheduled, so repeated
    /// error events for one failed load neither push the retry back nor report
    /// it again.
    fn start(&mut self, url: String, kind: RetryErrorKind, message: &str) -> bool {
        if self.url.as_deref() == Some(url.as_str()) && self.error_kind == Some(kind) {
            if self.next_retry.is_some() {
                return false;
            }
        } else {
            self.url = Some(url);
            self.error_kind = Some(kind);
            self.attempt = 0;
        }
        self.last_error = Some(message.to_string());
        self.schedule_next(kind, message);
        true
    }

    /// Takes the retry that is due, clearing its timer until the next failure.
    ///
    /// Clearing the timer is what keeps a slow resolve from being retried again
    /// while it is still running.
    fn take_due(&mut self) -> Option<(String, RetryErrorKind)> {
        if !self.is_ready() {
            return None;
        }
        let url = self.url.clone()?;
        let kind = self.error_kind?;
        self.next_retry = None;
        self.attempt += 1;
        Some((url, kind))
    }

    /// Schedules the next retry.
    fn schedule_next(&mut self, kind: RetryErrorKind, message: &str) {
        self.next_retry = Some(Instant::now() + retry_delay(kind, message, self.attempt));
    }

    /// Checks if it's time to retry.
    fn is_ready(&self) -> bool {
        self.next_retry.is_some_and(|t| Instant::now() >= t)
    }

    /// Whether the retry budget is used up.
    fn exhausted(&self) -> bool {
        self.attempt >= MAX_RETRY_ATTEMPTS
    }

    /// Increments the attempt counter and schedules the next retry.
    #[cfg(test)]
    fn next_attempt(&mut self) {
        let kind = self.error_kind.unwrap_or(RetryErrorKind::Resolver);
        let msg = self.last_error.clone().unwrap_or_default();
        self.attempt += 1;
        self.schedule_next(kind, &msg);
    }

    /// Clears the retry state.
    fn clear(&mut self) {
        self.url = None;
        self.error_kind = None;
        self.attempt = 0;
        self.next_retry = None;
    }
}

#[derive(Debug)]
struct YtdlpSlot {
    active: bool,
    cancel: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
}

impl YtdlpSlot {
    fn idle() -> Self {
        Self {
            active: false,
            cancel: Arc::new(AtomicBool::new(false)),
            pid: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Starts a new run and returns its cancel flag and its process id.
    ///
    /// The run that is still going is stopped first and its process is
    /// killed, so a slot never has two processes at the same time.
    fn begin(&mut self) -> (Arc<AtomicBool>, Arc<AtomicU32>) {
        let (_, pid) = self.cancel();
        terminate_pid(pid.load(Ordering::SeqCst));
        self.active = true;
        (self.cancel.clone(), self.pid.clone())
    }

    fn finish(&mut self, cancel: &Arc<AtomicBool>) {
        if Arc::ptr_eq(&self.cancel, cancel) {
            self.active = false;
            self.pid.store(0, Ordering::SeqCst);
        }
    }

    /// Stops the run that is going and returns its cancel flag and process.
    fn cancel(&mut self) -> (Arc<AtomicBool>, Arc<AtomicU32>) {
        self.cancel.store(true, Ordering::SeqCst);
        self.active = false;
        let old = (self.cancel.clone(), self.pid.clone());
        self.cancel = Arc::new(AtomicBool::new(false));
        self.pid = Arc::new(AtomicU32::new(0));
        old
    }
}

/// Coordinates yt-dlp access across playback, URL entry, and thumbnail work.
///
/// Every kind of work has its own slot, so one kind of work can be
/// stopped without touching the others. The URL entry slot holds the one
/// URL that is processed at a time.
#[derive(Debug)]
struct YtdlpControl {
    tool: Mutex<YtDlp>,
    playback: Mutex<YtdlpSlot>,
    url_entry: Mutex<YtdlpSlot>,
    thumbnail: Mutex<YtdlpSlot>,
}

fn terminate_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status();
    }
}

impl YtdlpControl {
    fn new(tool: YtDlp) -> Self {
        Self {
            tool: Mutex::new(tool),
            playback: Mutex::new(YtdlpSlot::idle()),
            url_entry: Mutex::new(YtdlpSlot::idle()),
            thumbnail: Mutex::new(YtdlpSlot::idle()),
        }
    }

    fn tool(&self) -> YtDlp {
        self.tool.lock().expect("the yt-dlp tool is not poisoned").clone()
    }

    fn replace_tool(&self, tool: YtDlp) {
        *self.tool.lock().expect("the yt-dlp tool is not poisoned") = tool;
    }

    /// Starts the playback resolve for `epoch`, when it is still the resolve
    /// the player waits for.
    ///
    /// Returns `None` when the resolve was superseded before it could start.
    /// The slot is then left alone, so a superseded resolve cannot take it
    /// away from the resolve that replaced it.
    fn begin_playback(
        &self,
        epoch_state: &AtomicU64,
        epoch: u64,
    ) -> Option<(Arc<AtomicBool>, Arc<AtomicU32>)> {
        let mut playback = self.playback.lock().expect("yt-dlp playback slot poisoned");
        if epoch_state.load(Ordering::SeqCst) != epoch {
            return None;
        }
        self.thumbnail.lock().expect("yt-dlp thumbnail slot poisoned").cancel();
        Some(playback.begin())
    }

    fn cancel_playback(&self) {
        let (_, pid) = self.playback.lock().expect("yt-dlp playback slot poisoned").cancel();
        terminate_pid(pid.load(Ordering::SeqCst));
    }

    fn finish_playback(&self, cancel: &Arc<AtomicBool>) {
        self.playback.lock().expect("yt-dlp playback slot poisoned").finish(cancel);
    }

    /// Starts the URL entry for `epoch`, when it is still the entry the user
    /// waits for.
    ///
    /// Returns `None` when the entry was replaced before it could start.
    /// The slot is then left alone, so an entry that was replaced cannot
    /// take it away from the entry that replaced it.
    fn begin_url_entry(
        &self,
        epoch_state: &AtomicU64,
        epoch: u64,
    ) -> Option<(Arc<AtomicBool>, Arc<AtomicU32>)> {
        let mut slot = self.url_entry.lock().expect("yt-dlp url entry slot poisoned");
        if epoch_state.load(Ordering::SeqCst) != epoch {
            return None;
        }
        Some(slot.begin())
    }

    fn cancel_url_entry(&self) {
        let (_, pid) = self.url_entry.lock().expect("yt-dlp url entry slot poisoned").cancel();
        terminate_pid(pid.load(Ordering::SeqCst));
    }

    fn finish_url_entry(&self, cancel: &Arc<AtomicBool>) {
        self.url_entry.lock().expect("yt-dlp url entry slot poisoned").finish(cancel);
    }

    fn try_begin_thumbnail(&self) -> Option<(Arc<AtomicBool>, Arc<AtomicU32>)> {
        let playback = self.playback.lock().expect("yt-dlp playback slot poisoned");
        let url = self.url_entry.lock().expect("yt-dlp url entry slot poisoned");
        let mut thumb = self.thumbnail.lock().expect("yt-dlp thumbnail slot poisoned");
        if playback.active || url.active || thumb.active {
            return None;
        }
        Some(thumb.begin())
    }

    fn finish_thumbnail(&self, cancel: &Arc<AtomicBool>) {
        self.thumbnail.lock().expect("yt-dlp thumbnail slot poisoned").finish(cancel);
    }

    fn cancel_thumbnail(&self) {
        let (_, pid) = self.thumbnail.lock().expect("yt-dlp thumbnail slot poisoned").cancel();
        terminate_pid(pid.load(Ordering::SeqCst));
    }

    fn cancel_all(&self) {
        let (_, p1) = self.playback.lock().expect("yt-dlp playback slot poisoned").cancel();
        let (_, p2) = self.url_entry.lock().expect("yt-dlp url entry slot poisoned").cancel();
        let (_, p3) = self.thumbnail.lock().expect("yt-dlp thumbnail slot poisoned").cancel();
        terminate_pid(p1.load(Ordering::SeqCst));
        terminate_pid(p2.load(Ordering::SeqCst));
        terminate_pid(p3.load(Ordering::SeqCst));
    }

    fn kill_all(&self) {
        let tool = self.tool();
        ytdlp_tool::kill_all(&tool);
    }
}

/// The application, without a user interface.
pub struct App {
    engine: Engine,
    resolver: Arc<Mutex<Resolver>>,
    ytdlp_epoch: Arc<AtomicU64>,
    url_entry_epoch: Arc<AtomicU64>,
    ytdlp: Arc<YtdlpControl>,
    settings: Settings,
    session: Session,
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
    /// The directories Flatline uses at runtime.
    paths: AppPaths,
    /// The directory the player buffers downloaded media in.
    stream_buffer: PathBuf,
    /// The media the user still has to decide about.
    pending: Mutex<Option<PendingMedia>>,
    pending_playlist: Mutex<Option<PendingPlaylist>>,
    /// Shared proxy pool for yt-dlp metadata resolution.
    proxy_pool: Arc<ProxyPool>,
    /// The last application release version shown to the user.
    /// Whether -4 was auto-enabled for the current playback retry sequence.
    auto_force_ipv4: AtomicBool,
    /// The URL the user submitted and the generation it belongs to.
    url_entry: Mutex<UrlEntryState>,
    notified_app_update: Mutex<Option<String>>,
    /// The last yt-dlp release version shown to the user.
    notified_tool_update: Mutex<Option<String>>,
}

impl App {
    /// Creates the application around a `yt-dlp` installation.
    pub fn new(ytdlp: &YtDlp, paths: &AppPaths) -> Result<App, Error> {
        app_logging::notice(format!(
            "creating the application around yt-dlp {} with state in {}",
            ytdlp.program().display(),
            paths.root().display()
        ));
        let stream_buffer = paths.stream_buffer();
        // Crash or unclean shutdown may have left cache files behind.
        // Clear them before creating the engine so mpv never sees stale data.
        clear_stream_buffer(&stream_buffer);
        let session = Session::new(paths.session_helper(), paths.cookies(), paths.user_data());
        let settings =
            Settings::load(paths.settings()).map_err(|error| Error::Settings(error.to_string()))?;
        let proxy_pool = Arc::new(ProxyPool::new());
        let resolver = build_resolver(ytdlp, &paths.cookies(), &settings, proxy_pool.clone());
        let pre_buffer_seconds = settings.pre_buffer_seconds().0;
        app_logging::notice(format!(
            "settings loaded from {}; pre-buffer={pre_buffer_seconds}s log_level={}",
            paths.settings().display(),
            app_logging::level()
        ));
        let engine = Engine::new(&stream_buffer, pre_buffer_seconds)
            .map_err(|error| Error::Engine(error.to_string()))?;
        let queue =
            Queue::open(paths.database()).map_err(|error| Error::Library(error.to_string()))?;
        app_logging::notice(format!("the library opened at {}", paths.database().display()));
        let cache = ThumbnailCache::new(queue.database(), paths.thumbnails());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| Error::Runtime(error.to_string()))?;
        let (sender, receiver) = channel();

        let app = App {
            engine,
            resolver: Arc::new(Mutex::new(resolver)),
            ytdlp_epoch: Arc::new(AtomicU64::new(0)),
            url_entry_epoch: Arc::new(AtomicU64::new(0)),
            ytdlp: Arc::new(YtdlpControl::new(ytdlp.clone())),
            settings,
            session,
            runtime,
            outbox: Arc::new(Mutex::new(sender)),
            inbox: Mutex::new(receiver),
            state: Mutex::new(State::default()),
            live: Mutex::new(LiveAxis::new()),
            queue,
            cache,
            paths: paths.clone(),
            stream_buffer,
            pending: Mutex::new(None),
            pending_playlist: Mutex::new(None),
            proxy_pool,
            url_entry: Mutex::new(UrlEntryState::default()),
            auto_force_ipv4: AtomicBool::new(false),
            notified_app_update: Mutex::new(None),
            notified_tool_update: Mutex::new(None),
        };
        // Drop orphaned thumbnail files left by older runs, and migrate any
        // thumbnails that still point at a direct file path, in the
        // background. Start and end are reported to the user as toasts.
        app.maintain_thumbnails();
        // Rebuild the thumbnail cache for the active queue in the background
        // so queued videos show their thumbnails without waiting for playback.
        app.refresh_queue_thumbnails();
        Ok(app)
    }

    /// The player.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Processes a URL the user submitted.
    ///
    /// One URL is processed at a time. A new URL is a hard interrupt for
    /// the one that is still running: the older entry is cancelled, its
    /// process is killed and its result is discarded, so it can neither
    /// report an error nor keep a process alive. Playback and thumbnail
    /// work keep their own rules and are not interrupted.
    ///
    /// The work happens in the background; the result arrives through
    /// [`App::poll`].
    pub fn submit_url(&self, url: String) {
        let mut url = url.trim().to_string();
        if url.is_empty() {
            return;
        }
        if source_youtube::matches_url(&url) {
            url = source_youtube::processing_url(&url);
        }
        app_logging::notice(format!(
            "url entry submitted: url={url} current_media={} playing={}",
            self.has_media(),
            self.is_playing()
        ));
        // The generation moves first: an entry that was replaced can no
        // longer take the slot away from the entry that replaced it.
        let epoch = self.url_entry_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        // The entry that is still running is stopped before the new one
        // starts, so the two never run at the same time.
        self.ytdlp.cancel_url_entry();
        {
            let mut state = self.url_entry.lock().expect("the url entry state is not poisoned");
            state.begin(&url, epoch);
        }
        // The notification follows the cancellation, so it never describes
        // an entry that was already replaced.
        self.toast("Parsing video".to_string());
        let resolver = self.resolver.lock().expect("the resolver is not poisoned").clone();
        self.start_url_entry(url, epoch, resolver);
    }

    /// Whether a media is loaded in the player.
    pub fn has_media(&self) -> bool {
        self.state.lock().expect("the state is not poisoned").source.is_some()
    }

    /// Whether the player holds media that is ready to play or paused.
    ///
    /// A media that is still opening, waiting for data or already failed is not
    /// settled, so a new manual request replaces it instead of becoming pending.
    pub fn is_playing(&self) -> bool {
        if !self.has_media() || self.engine.is_idle() {
            return false;
        }
        self.engine.is_active() || (self.engine.is_paused() && !self.engine.is_buffering())
    }

    /// Whether `url` is the URL of the current active queue entry.
    pub fn is_current_queue_url(&self, url: &str) -> bool {
        matches!(self.queue.current(), Ok(Some(entry)) if entry.url == url)
    }

    /// Whether `epoch` is the generation of the URL entry that is running.
    pub fn is_current_url_entry(&self, epoch: u64) -> bool {
        self.url_entry.lock().expect("the url entry state is not poisoned").is_current(epoch)
    }

    /// Returns the URL currently being resolved, if any.
    pub fn resolving_url(&self) -> Option<String> {
        self.state.lock().expect("the state is not poisoned").resolving_url.clone()
    }

    /// Returns the currently playing URL, if any.
    pub fn current_url(&self) -> Option<String> {
        self.state.lock().expect("the state is not poisoned").current_url.clone()
    }

    /// Resolves a URL the user submitted, in the background.
    ///
    /// The entry owns its `yt-dlp` slot for the whole run. A result that
    /// belongs to an entry that has been replaced is dropped here, before
    /// it can reach the user.
    fn start_url_entry(&self, url: String, epoch: u64, resolver: Resolver) {
        let session = self.session.clone();
        let outbox = Arc::clone(&self.outbox);
        let epoch_state = Arc::clone(&self.url_entry_epoch);
        let ytdlp = Arc::clone(&self.ytdlp);
        let cache = self.cache.clone();
        self.runtime.handle().spawn(async move {
            let Some((cancel, pid)) = ytdlp.begin_url_entry(&epoch_state, epoch) else {
                app_logging::debug(format!(
                    "url entry discarded before it started: url={url} epoch={epoch}"
                ));
                return;
            };
            if cancel.load(Ordering::SeqCst) {
                ytdlp.finish_url_entry(&cancel);
                app_logging::debug(format!(
                    "url entry discarded before it started: url={url} epoch={epoch}"
                ));
                return;
            }
            let sender = outbox.lock().expect("the outbox is not poisoned").clone();
            let result = resolve_document_with_session(
                &resolver,
                &session,
                &epoch_state,
                epoch,
                &cancel,
                &pid,
                &url,
                &sender,
            )
            .await;
            ytdlp.finish_url_entry(&cancel);
            if cancel.load(Ordering::SeqCst)
                || epoch_state.load(Ordering::SeqCst) != epoch
                || matches!(result, Err(stream_resolver::Error::Cancelled))
            {
                app_logging::debug(format!(
                    "url entry discarded because it was replaced: url={url} epoch={epoch}"
                ));
                return;
            }
            let message = match result {
                Ok(stream_resolver::ResolvedDocument::Video(source)) => {
                    let sponsorblock_segments = if source_youtube::matches_url(&url) {
                        source_youtube::sponsorblock::fetch_for_url(&url)
                    } else {
                        Vec::new()
                    };
                    let thumb_candidates = thumbnail_candidates_for_source(&source);
                    let thumbnail_id = cache.fetch(&url, &thumb_candidates).ok().map(|(id, _)| id);
                    let thumbnail = thumbnail_id.as_deref().and_then(|id| cache.mid_path(id));
                    app_logging::notice(format!(
                        "url entry resolved: url={url} epoch={epoch} title=\"{}\" sponsorblock_segments={}",
                        source.title,
                        sponsorblock_segments.len()
                    ));
                    Message::Pending(PendingMedia {
                        url,
                        title: source.title.clone(),
                        source,
                        thumbnail,
                        thumbnail_id,
                        resolved_at: unix_time(),
                        epoch,
                        sponsorblock_segments,
                    })
                }
                Ok(stream_resolver::ResolvedDocument::Playlist(playlist)) => {
                    let (selected_url, has_selected_video) =
                        Self::selected_playlist_url(&url, &playlist);
                    let entries: Vec<PendingPlaylistEntry> = playlist
                        .entries
                        .iter()
                        .map(|entry| PendingPlaylistEntry {
                            url: entry.url.clone(),
                            title: entry.title.clone(),
                            thumbnail_url: entry.thumbnail.clone(),
                            thumbnail_urls: entry.thumbnails.clone(),
                            uploader: entry.uploader.clone(),
                        })
                        .collect();
                    let thumb_candidates = entries
                        .iter()
                        .find(|entry| entry.url == selected_url)
                        .map(|entry| thumbnail_candidates(entry.thumbnail_url.as_deref(), &entry.thumbnail_urls))
                        .or_else(|| entries.first().map(|entry| thumbnail_candidates(entry.thumbnail_url.as_deref(), &entry.thumbnail_urls)))
                        .unwrap_or_default();
                    let thumb_id = cache.fetch(&selected_url, &thumb_candidates).ok().map(|(id, _)| id);
                    let thumbnail = thumb_id.as_deref().and_then(|id| cache.mid_path(id));
                    app_logging::notice(format!(
                        "playlist url entry resolved: url={url} epoch={epoch} title=\"{}\" entries={} has_selected_video={has_selected_video}",
                        playlist.title,
                        entries.len()
                    ));
                    Message::PendingPlaylist(PendingPlaylist {
                        url,
                        title: if playlist.title.trim().is_empty() {
                            "Playlist".to_string()
                        } else {
                            playlist.title
                        },
                        thumbnail,
                        selected_url,
                        has_selected_video,
                        entries,
                        epoch,
                    })
                }
                Err(error) => {
                    let mapped = format!("Failed to process URL: {error}");
                    app_logging::warning(format!(
                        "url entry failed: url={url} epoch={epoch} error={mapped}"
                    ));
                    Message::UrlEntryFailed { url, error: mapped, epoch }
                }
            };
            let _ = outbox.lock().map(|sender| sender.send(Box::new(message)));
        });
    }
}

impl App {
    /// Picks the URL to play from a resolved playlist and reports whether the
    /// submitted URL explicitly selected a video inside that playlist.
    fn selected_playlist_url(
        source_url: &str,
        playlist: &stream_resolver::ResolvedPlaylist,
    ) -> (String, bool) {
        if let Ok(parsed) = url::Url::parse(source_url) {
            let video = parsed
                .query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("v"))
                .map(|(_, value)| value.to_string());
            if let Some(video) = video
                && let Some(entry) = playlist.entries.iter().find(|entry| {
                    url::Url::parse(&entry.url).ok().is_some_and(|u| {
                        u.query_pairs().any(|(k, v)| k.eq_ignore_ascii_case("v") && v == video)
                    }) || entry.id == video
                })
            {
                return (entry.url.clone(), true);
            }
        }
        (
            playlist
                .entries
                .first()
                .map(|entry| entry.url.clone())
                .unwrap_or_else(|| source_url.to_string()),
            false,
        )
    }

    /// Whether a resolved result for `url` still belongs to the active playback.
    ///
    /// A result is relevant while it matches the in-flight resolve or the URL of
    /// the queue entry that is current now.
    pub fn expects_resolved_url(&self, url: &str) -> bool {
        if self.resolving_url().as_deref() == Some(url) {
            return true;
        }
        match self.queue.current() {
            Ok(Some(entry)) => entry.url == url,
            Ok(None) | Err(_) => false,
        }
    }

    fn resolve_play_with_epoch(&self, url: String, epoch: u64, reason: PlaybackReason) {
        {
            let mut state = self.state.lock().expect("the state is not poisoned");
            if self.is_ytdlp_stale(epoch) {
                app_logging::debug(format!(
                    "discarding a playback resolve that was superseded: url={url} reason={} epoch={epoch}",
                    reason.as_str()
                ));
                return;
            }
            state.resolving_url = Some(url.clone());
        }
        app_logging::notice(format!(
            "resolving playback source: url={url} reason={}",
            reason.as_str()
        ));

        let resolver = Arc::clone(&self.resolver);
        let session = self.session.clone();
        let outbox = Arc::clone(&self.outbox);
        let cache = self.cache.clone();
        let epoch_state = Arc::clone(&self.ytdlp_epoch);
        let ytdlp = Arc::clone(&self.ytdlp);

        self.runtime.handle().spawn(async move {
            let Some((cancel, pid)) = ytdlp.begin_playback(&epoch_state, epoch) else {
                app_logging::debug(format!(
                    "discarding a playback resolve that was superseded before it started: url={url} reason={} epoch={epoch}",
                    reason.as_str()
                ));
                return;
            };
            let resolver = resolver.lock().expect("the resolver is not poisoned").clone();
            let sender = outbox.lock().expect("the outbox is not poisoned").clone();
            let result = resolve_with_session(
                &resolver,
                &session,
                &epoch_state,
                epoch,
                &cancel,
                &pid,
                &url,
                &sender,
            )
            .await;
            ytdlp.finish_playback(&cancel);
            let cancelled = cancel.load(Ordering::SeqCst)
                || epoch_state.load(Ordering::SeqCst) != epoch
                || matches!(result, Err(stream_resolver::Error::Cancelled));
            if cancelled {
                app_logging::debug(format!(
                    "discarding a cancelled playback resolve: url={url} reason={}",
                    reason.as_str()
                ));
                return;
            }
            let message = match result {
                Ok(source) => {
                    let sponsorblock_segments = if source_youtube::matches_url(&url) {
                        source_youtube::sponsorblock::fetch_for_url(&url)
                    } else {
                        Vec::new()
                    };
                    let thumb_candidates = thumbnail_candidates_for_source(&source);
                    let thumbnail = cache.fetch(&url, &thumb_candidates).ok().map(|(id, _)| id);
                    app_logging::debug(format!(
                        "playback source resolved: url={url} title=\"{}\" streams={} reason={} sponsorblock_segments={}",
                        source.title,
                        source.video_streams.len() + source.audio_streams.len(),
                        reason.as_str(),
                        sponsorblock_segments.len()
                    ));
                    Message::Resolved(Ok(ResolvedMedia {
                        url: url.clone(),
                        thumbnail,
                        resolved_at: unix_time(),
                        reason,
                        source,
                        sponsorblock_segments,
                    }))
                }
                Err(error) => {
                    app_logging::warning(format!(
                        "playback source could not be resolved: url={url} reason={} error={error}",
                        reason.as_str()
                    ));
                    Message::Resolved(Err(ResolveFailure { url, message: error.to_string() }))
                }
            };
            let _ = outbox.lock().map(|sender| sender.send(Box::new(message)));
        });
    }

    /// Stores the pending media so the user can decide what to do with it.
    pub fn set_pending(&self, pending: PendingMedia) {
        app_logging::debug(format!(
            "pending video is waiting for the user: url={} title=\"{}\"",
            pending.url, pending.title
        ));
        *self.pending.lock().expect("the pending media is not poisoned") = Some(pending);
    }

    /// Stores a pending playlist so the user can decide what to do with it.
    pub fn set_pending_playlist(&self, pending: PendingPlaylist) {
        app_logging::debug(format!(
            "pending playlist is waiting for the user: url={} title=\"{}\" entries={}",
            pending.url,
            pending.title,
            pending.entries.len()
        ));
        *self.pending_playlist.lock().expect("the pending playlist is not poisoned") =
            Some(pending);
    }

    /// Forgets the pending playlist without doing anything with it.
    pub fn dismiss_pending_playlist(&self) {
        if let Some(pending) =
            self.pending_playlist.lock().expect("the pending playlist is not poisoned").take()
        {
            app_logging::debug(format!("the pending playlist was dismissed: url={}", pending.url));
        }
    }

    /// Plays the pending media right away.
    ///
    /// The active queue is replaced, because it is the scratch list. A user
    /// playlist is preserved: the media is appended and becomes its current
    /// entry, so "Play now" never destroys a curated list.
    pub fn play_pending(&self) -> Option<(String, Loaded)> {
        let pending = self.pending.lock().expect("the pending media is not poisoned").take()?;
        app_logging::notice(format!(
            "user chose to play the pending video now: url={} title=\"{}\"",
            pending.url, pending.title
        ));
        self.preempt_ytdlp();
        self.interrupt_playback();
        let thumbnail = pending.thumbnail_id.as_deref();
        let thumbnail_url = pending.source.thumbnail.clone();
        let thumbnail_urls = thumbnail_candidates_for_source(&pending.source);
        let uploader = pending.source.uploader.clone();
        let in_playlist = matches!(self.queue.active_playlist_id(), Ok(Some(_)));
        if in_playlist {
            let entry = match self.queue.push(
                &pending.url,
                &pending.title,
                thumbnail,
                thumbnail_url.as_deref(),
                &thumbnail_urls,
                uploader.as_deref(),
            ) {
                Ok(entry) => entry,
                Err(error) => {
                    app_logging::error(format!(
                        "the playlist could not be written while playing pending media: url={} error={error}",
                        pending.url
                    ));
                    return None;
                }
            };
            if let Err(error) = self.queue.set_current(entry.id) {
                app_logging::error(format!(
                    "the playlist entry could not be made current: url={} error={error}",
                    pending.url
                ));
                return None;
            }
        } else {
            self.prepare_current(
                &pending.url,
                &pending.title,
                thumbnail,
                thumbnail_url.as_deref(),
                &thumbnail_urls,
                uploader.as_deref(),
            );
        }
        let loaded = self.consume_resolved(
            &pending.url,
            pending.source,
            thumbnail,
            pending.resolved_at,
            PlaybackReason::Manual,
            pending.sponsorblock_segments,
        )?;
        Some((pending.url, loaded))
    }

    /// Appends the pending media to the active queue.
    pub fn queue_pending(&self) -> bool {
        self.queue_pending_with(false)
    }

    /// Appends the pending media to the active queue, right after the current
    /// entry when requested.
    pub fn queue_pending_next(&self) -> bool {
        self.queue_pending_with(true)
    }

    fn queue_pending_with(&self, next: bool) -> bool {
        let Some(pending) = self.pending.lock().expect("the pending media is not poisoned").take()
        else {
            return false;
        };
        app_logging::notice(format!(
            "user queued the pending video {}: url={} title=\"{}\"",
            if next { "next" } else { "at the end" },
            pending.url,
            pending.title
        ));
        let thumbnail = pending.thumbnail_id.as_deref();
        let thumbnail_url = pending.source.thumbnail.clone();
        let thumbnail_urls = thumbnail_candidates_for_source(&pending.source);
        let uploader = pending.source.uploader.clone();
        let result = if next {
            self.queue.push_next(
                &pending.url,
                &pending.title,
                thumbnail,
                thumbnail_url.as_deref(),
                &thumbnail_urls,
                uploader.as_deref(),
            )
        } else {
            self.queue.push(
                &pending.url,
                &pending.title,
                thumbnail,
                thumbnail_url.as_deref(),
                &thumbnail_urls,
                uploader.as_deref(),
            )
        };
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                app_logging::error(format!(
                    "the active queue could not be written for {}: {error}",
                    pending.url
                ));
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
            app_logging::warning(format!(
                "the resolved queue source could not be saved for {}: {error}",
                pending.url
            ));
        }
        true
    }

    /// Forgets the pending media without doing anything with it.
    pub fn dismiss_pending(&self) {
        if let Some(pending) =
            self.pending.lock().expect("the pending media is not poisoned").take()
        {
            app_logging::debug(format!("the pending video was dismissed: url={}", pending.url));
        }
    }

    /// Returns the selected video from the pending playlist, if it has one.
    pub fn pending_playlist_selected(&self) -> Option<PendingPlaylistEntry> {
        let slot = self.pending_playlist.lock().expect("the pending playlist is not poisoned");
        let pending = slot.as_ref()?;
        if !pending.has_selected_video {
            return None;
        }
        pending
            .entries
            .iter()
            .find(|entry| entry.url == pending.selected_url)
            .cloned()
            .or_else(|| pending.entries.first().cloned())
    }

    /// Plays the selected video from a pending playlist as a standalone media.
    pub fn play_pending_playlist_selected(&self) -> Option<String> {
        let selected = self.pending_playlist_selected()?;
        self.dismiss_pending_playlist();
        Some(selected.url)
    }

    /// Appends the selected video from a pending playlist to the active queue.
    pub fn queue_pending_playlist_selected(&self) -> bool {
        self.queue_pending_playlist_selected_with(false)
    }

    /// Appends the selected video from a pending playlist next in the active queue.
    pub fn queue_pending_playlist_selected_next(&self) -> bool {
        self.queue_pending_playlist_selected_with(true)
    }

    fn queue_pending_playlist_selected_with(&self, next: bool) -> bool {
        let Some(selected) = self.pending_playlist_selected() else {
            return false;
        };
        let result = if next {
            self.queue.push_next(
                &selected.url,
                &selected.title,
                None,
                selected.thumbnail_url.as_deref(),
                &selected.thumbnail_urls,
                selected.uploader.as_deref(),
            )
        } else {
            self.queue.push(
                &selected.url,
                &selected.title,
                None,
                selected.thumbnail_url.as_deref(),
                &selected.thumbnail_urls,
                selected.uploader.as_deref(),
            )
        };
        match result {
            Ok(_) => {
                self.dismiss_pending_playlist();
                true
            }
            Err(error) => {
                app_logging::error(format!(
                    "the active queue could not be written for {}: {error}",
                    selected.url
                ));
                false
            }
        }
    }

    /// Adds the selected video from a pending playlist to `playlist_id`.
    pub fn add_pending_playlist_selected_to_playlist(&self, playlist_id: &str) -> bool {
        let Some(selected) = self.pending_playlist_selected() else {
            return false;
        };
        let entry = library::QueueEntry {
            id: 0,
            position: 0,
            url: selected.url.clone(),
            title: selected.title.clone(),
            thumbnail: None,
            thumbnail_url: selected.thumbnail_url.clone(),
            thumbnail_urls: selected.thumbnail_urls.clone(),
            uploader: selected.uploader.clone(),
            id_hash: String::new(),
            current: false,
            resolved_source: None,
            resolved_at: None,
            selected_video: None,
            selected_audio: None,
            selected_muxed: None,
        };
        match self.queue.push_entry_to_playlist(playlist_id, &entry) {
            Ok(_) => {
                self.dismiss_pending_playlist();
                true
            }
            Err(error) => {
                app_logging::error(format!(
                    "the pending playlist video could not be added to the playlist: url={} playlist_id={playlist_id} error={error}",
                    selected.url
                ));
                false
            }
        }
    }

    /// Puts a freshly resolved media in the active queue before it is loaded.
    pub fn prepare_current(
        &self,
        url: &str,
        title: &str,
        thumbnail: Option<&str>,
        thumbnail_url: Option<&str>,
        thumbnail_urls: &[String],
        uploader: Option<&str>,
    ) {
        app_logging::debug(format!("replacing the active queue with url={url} title=\"{title}\""));
        self.interrupt_playback();
        if let Err(error) =
            self.queue.replace_with(url, title, thumbnail, thumbnail_url, thumbnail_urls, uploader)
        {
            app_logging::error(format!(
                "the active queue could not be written for url={url}: {error}"
            ));
        }
        // Drop thumbnails no longer referenced by the queue, plus their files.
        if let Err(error) = self.cache.release_unreferenced_thumbnails() {
            app_logging::warning(format!("the thumbnail cache could not be cleared: {error}"));
        }
    }

    /// Plays the media that plays next in the queue, if there is one.
    ///
    /// This is what a finished media calls: the current mark moves forward
    /// and the entry it lands on is resolved again, because an extractor may
    /// have changed since the entry was added.
    pub fn advance(&self) -> bool {
        match self.queue.advance() {
            Ok(Some(entry)) => {
                app_logging::notice(format!(
                    "queue advanced automatically: url={} title=\"{}\" reason=automatic",
                    entry.url, entry.title
                ));
                self.clear_retry();
                self.consume_current_with_epoch(
                    self.ytdlp_epoch.load(Ordering::SeqCst),
                    PlaybackReason::Automatic,
                );
                true
            }
            Ok(None) => {
                app_logging::debug("the active queue ended with no next entry");
                false
            }
            Err(error) => {
                app_logging::error(format!("the active queue could not be advanced: {error}"));
                false
            }
        }
    }

    /// Plays the previous media in the queue, if there is one.
    ///
    /// This moves the current mark backward and resolves the entry it lands on.
    pub fn previous(&self) -> bool {
        let epoch = self.preempt_ytdlp();
        app_logging::notice("manual queue action: previous");
        match self.queue.current() {
            Ok(Some(current)) => match self.queue.previous_before(current.position) {
                Ok(Some(previous)) => {
                    app_logging::notice(format!(
                        "manual queue movement: previous from url={} title=\"{}\" to url={} title=\"{}\"",
                        current.url, current.title, previous.url, previous.title
                    ));
                    if let Err(error) = self.queue.set_current(previous.id) {
                        app_logging::error(format!(
                            "the previous queue entry could not be made current: {error}"
                        ));
                        return false;
                    }
                    self.interrupt_playback();
                    self.consume_current_with_epoch(epoch, PlaybackReason::Manual);
                    true
                }
                Ok(None) => {
                    app_logging::debug("manual previous found no entry before the current one");
                    false
                }
                Err(error) => {
                    app_logging::error(format!("the active queue could not be read: {error}"));
                    false
                }
            },
            Ok(None) => {
                app_logging::debug("manual previous found no current queue entry");
                false
            }
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                false
            }
        }
    }

    /// Plays the next media in the queue, if there is one.
    ///
    /// This moves the current mark forward and resolves the entry it lands on.
    pub fn next(&self) -> bool {
        let epoch = self.preempt_ytdlp();
        app_logging::notice("manual queue action: next");
        match self.queue.current() {
            Ok(Some(current)) => match self.queue.next_after(current.position) {
                Ok(Some(next)) => {
                    app_logging::notice(format!(
                        "manual queue movement: next from url={} title=\"{}\" to url={} title=\"{}\"",
                        current.url, current.title, next.url, next.title
                    ));
                    if let Err(error) = self.queue.set_current(next.id) {
                        app_logging::error(format!(
                            "the next queue entry could not be made current: {error}"
                        ));
                        return false;
                    }
                    self.interrupt_playback();
                    self.consume_current_with_epoch(epoch, PlaybackReason::Manual);
                    true
                }
                Ok(None) => {
                    app_logging::debug("manual next found no entry after the current one");
                    false
                }
                Err(error) => {
                    app_logging::error(format!("the active queue could not be read: {error}"));
                    false
                }
            },
            Ok(None) => {
                app_logging::debug("manual next found no current queue entry");
                false
            }
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                false
            }
        }
    }

    /// Starts the media the queue left as current, if there is one.
    ///
    /// This is what the program calls on startup so a queue that was filled
    /// in an earlier run picks up where it left off.
    pub fn resume(&self) -> bool {
        let epoch = self.preempt_ytdlp();
        self.clear_retry();
        match self.queue.current() {
            Ok(Some(entry)) => {
                app_logging::notice(format!(
                    "resuming the queue left by an earlier run: url={} title=\"{}\" reason=startup-resume",
                    entry.url, entry.title
                ));
                self.consume_current_with_epoch(epoch, PlaybackReason::Resume);
                true
            }
            Ok(None) => {
                app_logging::debug("there is no queue to resume");
                false
            }
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                false
            }
        }
    }

    /// Returns every entry of the active playlist, in play order.
    pub fn queue_entries(&self) -> Vec<library::QueueEntry> {
        match self.queue.entries() {
            Ok(entries) => entries,
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                Vec::new()
            }
        }
    }

    /// Every playlist, with the built-in active queue first.
    pub fn playlists(&self) -> Vec<library::PlaylistInfo> {
        match self.queue.playlists() {
            Ok(playlists) => playlists,
            Err(error) => {
                app_logging::error(format!("the playlists could not be read: {error}"));
                Vec::new()
            }
        }
    }

    /// The playlist the player is on.
    pub fn active_playlist(&self) -> Option<library::PlaylistInfo> {
        self.playlists().into_iter().find(|playlist| playlist.active)
    }

    /// The name of the user playlist that is active, or `None` when the
    /// built-in active queue is active.
    pub fn active_playlist_name(&self) -> Option<String> {
        self.active_playlist()
            .filter(|playlist| !playlist.is_active_queue)
            .map(|playlist| playlist.name)
    }

    /// Switches every queue operation to `playlist_id` and resumes the entry
    /// that playlist left as current.
    ///
    /// Returns whether a media was started. An empty playlist is still
    /// selected, it just has nothing to play yet.
    pub fn select_playlist(&self, playlist_id: &str) -> bool {
        let epoch = self.preempt_ytdlp();
        app_logging::notice(format!("user selected playlist id={playlist_id}"));
        if let Err(error) = self.queue.select_playlist(playlist_id) {
            app_logging::error(format!(
                "the playlist could not be selected: id={playlist_id} error={error}"
            ));
            return false;
        }
        self.interrupt_playback();
        let resumed = match self.queue.current() {
            Ok(Some(entry)) => {
                app_logging::notice(format!(
                    "playing the current entry of the selected playlist: url={} title=\"{}\" reason=manual",
                    entry.url, entry.title
                ));
                self.consume_current_with_epoch(epoch, PlaybackReason::Manual);
                true
            }
            Ok(None) => {
                app_logging::debug(format!(
                    "the selected playlist has no current entry: id={playlist_id}"
                ));
                false
            }
            Err(error) => {
                app_logging::error(format!(
                    "the selected playlist could not be read: id={playlist_id} error={error}"
                ));
                false
            }
        };
        // Playlist entries collect their thumbnails in the background just
        // like the active queue does.
        self.refresh_queue_thumbnails();
        resumed
    }

    /// Whether a user playlist already uses `name`, ignoring case.
    ///
    /// The built-in active queue is not considered a duplicate, because its
    /// name is reserved and rejected outright.
    pub fn playlist_name_exists(&self, name: &str) -> bool {
        let name = name.trim();
        self.playlists()
            .iter()
            .any(|playlist| !playlist.is_active_queue && playlist.name.eq_ignore_ascii_case(name))
    }

    /// Creates a user playlist.
    pub fn create_playlist(&self, name: &str) -> Result<library::PlaylistInfo, String> {
        app_logging::notice(format!("user created playlist \"{name}\""));
        self.queue.create_playlist(name).map_err(|error| {
            app_logging::error(format!("the playlist \"{name}\" could not be created: {error}"));
            error.to_string()
        })
    }

    /// Copies an entry of the active playlist into `playlist_id`.
    ///
    /// The copy keeps the URL, the details and the cached thumbnail, so adding
    /// an item to another playlist never resolves it again.
    pub fn add_entry_to_playlist(&self, entry_id: i64, playlist_id: &str) -> bool {
        let entry = match self.queue.entries() {
            Ok(entries) => entries.into_iter().find(|entry| entry.id == entry_id),
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                None
            }
        };
        let Some(entry) = entry else {
            app_logging::warning(format!(
                "the queue entry to copy was not found: entry_id={entry_id} playlist_id={playlist_id}"
            ));
            return false;
        };
        app_logging::notice(format!(
            "user added a video to playlist: url={} title=\"{}\" playlist_id={playlist_id} action=copy",
            entry.url, entry.title
        ));
        match self.queue.push_entry_to_playlist(playlist_id, &entry) {
            Ok(_) => true,
            Err(error) => {
                app_logging::error(format!(
                    "the video could not be added to the playlist: url={} playlist_id={playlist_id} error={error}",
                    entry.url
                ));
                false
            }
        }
    }

    /// Adds the pending media to `playlist_id` and forgets it.
    pub fn add_pending_to_playlist(&self, playlist_id: &str) -> bool {
        let mut slot = self.pending.lock().expect("the pending media is not poisoned");
        let Some(pending) = slot.as_ref() else {
            return false;
        };
        let entry = library::QueueEntry {
            id: 0,
            position: 0,
            url: pending.url.clone(),
            title: pending.title.clone(),
            thumbnail: pending.thumbnail_id.clone(),
            thumbnail_url: pending.source.thumbnail.clone(),
            thumbnail_urls: thumbnail_candidates_for_source(&pending.source),
            uploader: pending.source.uploader.clone(),
            id_hash: String::new(),
            current: false,
            resolved_source: serde_json::to_string(&pending.source).ok(),
            resolved_at: Some(pending.resolved_at),
            selected_video: None,
            selected_audio: None,
            selected_muxed: None,
        };
        match self.queue.push_entry_to_playlist(playlist_id, &entry) {
            Ok(_) => {
                app_logging::notice(format!(
                    "user added the pending video to playlist: url={} title=\"{}\" playlist_id={playlist_id}",
                    pending.url, pending.title
                ));
                slot.take();
                true
            }
            Err(error) => {
                app_logging::error(format!(
                    "the pending video could not be added to the playlist: url={} playlist_id={playlist_id} error={error}",
                    pending.url
                ));
                false
            }
        }
    }

    /// Plays a pending playlist by creating a new playlist and making it active.
    pub fn play_pending_playlist_as_new(&self, name: Option<&str>) -> Result<(), String> {
        let mut slot = self.pending_playlist.lock().expect("the pending playlist is not poisoned");
        let Some(pending) = slot.as_ref() else {
            return Err("no pending playlist".to_string());
        };
        let playlist_name = name
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| pending.title.clone());
        let info = self.create_playlist(&playlist_name)?;
        let selected = pending.selected_url.clone();
        let entries = pending.entries.clone();
        slot.take();
        drop(slot);
        self.populate_playlist_entries(&info.id, &entries, &selected)?;
        let _ = self.select_playlist(&info.id);
        self.play_url_in_active_playlist(&selected);
        Ok(())
    }

    /// Plays a pending playlist into an already created playlist.
    pub fn play_pending_playlist_into(&self, playlist_id: &str) -> Result<(), String> {
        let mut slot = self.pending_playlist.lock().expect("the pending playlist is not poisoned");
        let Some(pending) = slot.as_ref() else {
            return Err("no pending playlist".to_string());
        };
        let selected = pending.selected_url.clone();
        let entries = pending.entries.clone();
        slot.take();
        drop(slot);
        self.populate_playlist_entries(playlist_id, &entries, &selected)?;
        let _ = self.select_playlist(playlist_id);
        self.play_url_in_active_playlist(&selected);
        Ok(())
    }

    fn populate_playlist_entries(
        &self,
        playlist_id: &str,
        entries: &[PendingPlaylistEntry],
        selected_url: &str,
    ) -> Result<(), String> {
        let has_selected = entries.iter().any(|entry| entry.url == selected_url);
        for (index, entry) in entries.iter().enumerate() {
            let current = if has_selected { entry.url == selected_url } else { index == 0 };
            let row = library::QueueEntry {
                id: 0,
                position: 0,
                url: entry.url.clone(),
                title: entry.title.clone(),
                thumbnail: None,
                thumbnail_url: entry.thumbnail_url.clone(),
                thumbnail_urls: entry.thumbnail_urls.clone(),
                uploader: entry.uploader.clone(),
                id_hash: String::new(),
                current,
                resolved_source: None,
                resolved_at: None,
                selected_video: None,
                selected_audio: None,
                selected_muxed: None,
            };
            self.queue
                .push_entry_to_playlist(playlist_id, &row)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Appends a pending playlist to an existing playlist. Does not change active playlist.
    pub fn append_pending_playlist_to(&self, playlist_id: &str) -> Result<usize, String> {
        let slot = self.pending_playlist.lock().expect("the pending playlist is not poisoned");
        let Some(pending) = slot.as_ref() else {
            return Err("no pending playlist".to_string());
        };
        let mut count = 0usize;
        for entry in &pending.entries {
            let row = library::QueueEntry {
                id: 0,
                position: 0,
                url: entry.url.clone(),
                title: entry.title.clone(),
                thumbnail: None,
                thumbnail_url: entry.thumbnail_url.clone(),
                thumbnail_urls: entry.thumbnail_urls.clone(),
                uploader: entry.uploader.clone(),
                id_hash: String::new(),
                current: false,
                resolved_source: None,
                resolved_at: None,
                selected_video: None,
                selected_audio: None,
                selected_muxed: None,
            };
            self.queue
                .push_entry_to_playlist(playlist_id, &row)
                .map_err(|error| error.to_string())?;
            count += 1;
        }
        Ok(count)
    }

    /// Clears pending playlist after successful append.
    pub fn take_pending_playlist(&self) -> Option<PendingPlaylist> {
        self.pending_playlist.lock().expect("the pending playlist is not poisoned").take()
    }

    /// Plays a URL from the active playlist if present, else submits it.
    fn play_url_in_active_playlist(&self, url: &str) {
        if let Ok(entries) = self.queue.entries()
            && let Some(entry) = entries.into_iter().find(|entry| entry.url == url)
        {
            let _ = self.play_queue_entry(entry.id);
            return;
        }
        self.submit_url(url.to_string());
    }

    /// Returns the small thumbnail of a cached thumbnail for the queue list.
    ///
    /// Only a thumbnail that is already on disk is returned; the image work
    /// happens when the thumbnail is fetched, never on the event loop. When a
    /// queue row has no thumbnail ref yet, the deterministic cache id derived
    /// from the entry URL is used as a read-only fallback.
    pub fn queue_thumbnail(&self, thumbnail: Option<&str>, url: &str) -> Option<PathBuf> {
        let id = thumbnail
            .filter(|value| library::ThumbnailCache::is_thumbnail_id(value))
            .map(str::to_string)
            .or_else(|| {
                (!url.trim().is_empty()).then(|| library::ThumbnailCache::thumbnail_id_for(url))
            });
        id.as_deref().and_then(|id| self.cache.small_path(id))
    }

    /// Maintains thumbnail cache files without mutating queue rows.
    ///
    /// The background worker may read queue entries to discover referenced
    /// thumbnail IDs and candidate URLs, but it must never edit queue content.
    /// Queue rows hold deterministic thumbnail references derived from the
    /// video URL; thumbnail files and thumbnail URL metadata live in the cache.
    pub fn maintain_thumbnails(&self) {
        let cache = self.cache.clone();
        let queue = self.queue.clone();
        let runtime = self.runtime.handle().clone();

        runtime.spawn_blocking(move || {
            app_logging::notice(
                "thumbnail maintenance started: action=cache-only mode=background queue_write=false",
            );
            if let Err(error) = cache.prune_orphans() {
                app_logging::warning(format!("thumbnail cache orphan prune failed: {error}"));
            }

            let entries = match queue.entries() {
                Ok(entries) => entries,
                Err(error) => {
                    app_logging::error(format!(
                        "thumbnail maintenance could not read the active queue: {error}"
                    ));
                    return;
                }
            };

            let mut repaired = 0usize;
            let mut remembered = 0usize;
            for entry in &entries {
                let id = entry
                    .thumbnail_id()
                    .map(str::to_string)
                    .unwrap_or_else(|| ThumbnailCache::thumbnail_id_for(&entry.url));
                match cache.repair_row(&id) {
                    Ok(true) => repaired += 1,
                    Ok(false) => {}
                    Err(error) => app_logging::warning(format!(
                        "thumbnail maintenance could not repair cache row: id={id} url={} error={error}",
                        entry.url
                    )),
                }
                let candidates = entry_thumbnail_candidates(entry);
                if !candidates.is_empty() {
                    match cache.remember_thumbnail_urls(&entry.url, &candidates) {
                        Ok(cache_id) => {
                            remembered += 1;
                            app_logging::debug(format!(
                                "thumbnail maintenance remembered candidates: queue_write=false id={cache_id} url={} candidates={} entry_id={} position={}",
                                entry.url,
                                candidates.len(),
                                entry.id,
                                entry.position
                            ));
                        }
                        Err(error) => app_logging::warning(format!(
                            "thumbnail maintenance could not store thumbnail candidates: url={} error={error}",
                            entry.url
                        )),
                    }
                }
            }

            if let Err(error) = cache.release_unreferenced_thumbnails() {
                app_logging::warning(format!(
                    "thumbnail maintenance could not release unreferenced thumbnails: {error}"
                ));
            }
            app_logging::notice(format!(
                "thumbnail maintenance finished: queue_write=false entries={} repaired_rows={} remembered_candidate_sets={}",
                entries.len(),
                repaired,
                remembered
            ));
        });
    }

    /// Refreshes thumbnail cache files in the background without mutating queue rows.
    ///
    /// The worker reads queue/playlist entries and writes only thumbnail cache
    /// files/metadata. Queue entries are never removed, reordered, or edited by
    /// this process.
    pub fn refresh_queue_thumbnails(&self) {
        let cache = self.cache.clone();
        let queue = self.queue.clone();
        let outbox = Arc::clone(&self.outbox);
        let resolver = Arc::clone(&self.resolver);
        let session = self.session.clone();
        let ytdlp_epoch = Arc::clone(&self.ytdlp_epoch);
        let ytdlp = Arc::clone(&self.ytdlp);
        let handle = self.runtime.handle().clone();
        let task_handle = handle.clone();

        handle.spawn_blocking(move || {
            let mut announced = false;
            let mut active_playlist_id = queue.active_playlist_id().ok().flatten();
            loop {
                let current_playlist = queue.active_playlist_id().ok().flatten();
                if current_playlist != active_playlist_id {
                    active_playlist_id = current_playlist;
                    announced = false;
                }
                let entries = match queue.entries() {
                    Ok(entries) => entries,
                    Err(error) => {
                        app_logging::error(format!(
                            "thumbnail refresh could not read the active queue: {error}"
                        ));
                        std::thread::sleep(THUMBNAIL_REFRESH_DELAY);
                        continue;
                    }
                };

                for entry in &entries {
                    let candidates = entry_thumbnail_candidates(entry);
                    if !candidates.is_empty() {
                        let _ = cache.remember_thumbnail_urls(&entry.url, &candidates);
                    }
                }

                let due = entries_needing_thumbnails(&entries, &cache);
                if due.is_empty() {
                    let repaired = scan_playlist_thumbnails(&queue, &cache);
                    if repaired > 0 {
                        app_logging::debug(format!(
                            "thumbnail refresh repaired playlist cache files: count={repaired} queue_write=false"
                        ));
                        announced = false;
                        continue;
                    }
                    if announced {
                        app_logging::notice("thumbnail refresh idle: no queue thumbnails pending");
                        announced = false;
                    }
                    std::thread::sleep(THUMBNAIL_REFRESH_IDLE_DELAY);
                    continue;
                }

                if !announced {
                    let count = due.len();
                    app_logging::notice(format!(
                        "refreshing {count} queue thumbnails in the background: queue_write=false"
                    ));
                    let _ = outbox.lock().map(|sender| {
                        sender.send(Box::new(Message::ThumbnailRefreshStarted { count }))
                    });
                    announced = true;
                }

                let mut progressed = false;
                for item in due {
                    let pass_epoch = ytdlp_epoch.load(Ordering::SeqCst);
                    let outcome = refresh_thumbnail_entry(
                        &cache,
                        &queue,
                        &resolver,
                        &session,
                        &ytdlp_epoch,
                        pass_epoch,
                        &ytdlp,
                        &task_handle,
                        &outbox,
                        &item,
                    );
                    match outcome {
                        ThumbnailRefreshOutcome::Completed => {
                            progressed = true;
                            std::thread::sleep(THUMBNAIL_REFRESH_DELAY);
                        }
                        ThumbnailRefreshOutcome::Preempted => {
                            app_logging::debug(
                                "thumbnail refresh yielded to higher-priority yt-dlp work; retrying in 5s",
                            );
                            std::thread::sleep(THUMBNAIL_REFRESH_DELAY);
                            break;
                        }
                        ThumbnailRefreshOutcome::RetryLater(error) => {
                            app_logging::warning(format!(
                                "thumbnail refresh failed; retrying in 5s: queue_write=false id={} url={} position={} error={error}",
                                item.id, item.url, item.position
                            ));
                            std::thread::sleep(THUMBNAIL_REFRESH_DELAY);
                        }
                    }
                }

                if !progressed {
                    std::thread::sleep(THUMBNAIL_REFRESH_DELAY);
                }
            }
        });
    }

    /// Returns every history entry, most recently played first.
    pub fn history_entries(&self) -> Vec<library::HistoryEntry> {
        match self.queue.history_all() {
            Ok(entries) => entries,
            Err(error) => {
                app_logging::error(format!("the history could not be read: {error}"));
                Vec::new()
            }
        }
    }

    /// Starts the entry with `id` and marks it as the current one.
    ///
    /// Any playback, resolve, or retry tied to the previously active media is
    /// abandoned first so the selected entry plays immediately and a stale end
    /// of the previous media cannot advance the queue past it.
    pub fn play_queue_entry(&self, id: i64) -> bool {
        let epoch = self.preempt_ytdlp();
        app_logging::notice(format!("manual queue action: play entry id={id}"));
        match self.queue.set_current(id) {
            Ok(()) => {
                if let Ok(Some(entry)) = self.queue.current() {
                    app_logging::notice(format!(
                        "manual playback of a queue entry: url={} title=\"{}\" reason=manual",
                        entry.url, entry.title
                    ));
                }
                self.interrupt_playback();
                self.consume_current_with_epoch(epoch, PlaybackReason::Manual);
                true
            }
            Err(error) => {
                app_logging::error(format!(
                    "the active queue entry id={id} could not be made current: {error}"
                ));
                false
            }
        }
    }

    /// Stops the current playback and clears transient state tied to it.
    fn interrupt_playback(&self) {
        app_logging::debug("interrupting the current playback and clearing transient state");
        let _ = self.engine.stop();
        self.clear_retry();
        self.auto_force_ipv4.store(false, Ordering::SeqCst);
        *self.pending.lock().expect("the pending media is not poisoned") = None;
        let mut state = self.state.lock().expect("the state is not poisoned");
        state.pending_restore = PendingRestore::default();
        state.resolving_url = None;
        state.current_id_hash = None;
        state.current_url = None;
        state.current_title = None;
        state.current_thumbnail_url = None;
        state.current_uploader = None;
        state.last_history_save = None;
        state.sponsorblock_segments.clear();
        state.sponsorblock_suppressed = None;
    }

    /// Performs a clean shutdown: stops playback and kills child processes.
    /// The tokio runtime is shut down when the App is dropped.
    pub fn shutdown(&self) {
        app_logging::notice("shutting down Flatline");

        self.interrupt_playback();
        self.ytdlp.cancel_all();

        app_logging::notice("Flatline shutdown complete");
    }

    /// Removes an entry from the active queue.
    ///
    /// The entry is identified by its stable database id, so exactly one row
    /// is removed. When the removed entry was the one playing, playback moves
    /// to the entry that follows it; when no entry follows it, the player is
    /// stopped.
    pub fn remove_queue_entry(&self, id: i64) -> QueueRemoval {
        match self.queue.remove_entry(id) {
            Ok(Some(entry)) => {
                app_logging::notice(format!(
                    "user removed a queue entry: entry_id={id} url={} title=\"{}\" was_current={}",
                    entry.url, entry.title, entry.current
                ));
                self.finish_removal(entry)
            }
            Ok(None) => {
                app_logging::warning(format!("the queue entry to remove was not found: id={id}"));
                QueueRemoval::default()
            }
            Err(error) => {
                app_logging::error(format!(
                    "the active queue entry id={id} could not be removed: {error}"
                ));
                QueueRemoval::default()
            }
        }
    }

    /// Moves an entry from the active playlist into `playlist_id`.
    ///
    /// Returns the removal details so the caller can start whatever follows
    /// when the moved entry was the one playing.
    pub fn move_entry_to_playlist(&self, entry_id: i64, playlist_id: &str) -> Option<QueueRemoval> {
        match self.queue.move_entry_to_playlist(entry_id, playlist_id) {
            Ok(Some(entry)) => {
                app_logging::notice(format!(
                    "user moved a video to another playlist: entry_id={entry_id} url={} title=\"{}\" playlist_id={playlist_id} was_current={}",
                    entry.url, entry.title, entry.current
                ));
                Some(self.finish_removal(entry))
            }
            Ok(None) => {
                app_logging::warning(format!(
                    "the queue entry to move was not found: entry_id={entry_id} playlist_id={playlist_id}"
                ));
                None
            }
            Err(error) => {
                app_logging::error(format!(
                    "the playlist entry could not be moved: entry_id={entry_id} playlist_id={playlist_id} error={error}"
                ));
                None
            }
        }
    }

    /// Handles the shared work after an entry left the active playlist.
    fn finish_removal(&self, removed: library::QueueEntry) -> QueueRemoval {
        // Deleting an entry drops its thumbnail when nothing else references it.
        if let Some(thumbnail_id) = removed.thumbnail_id()
            && let Err(error) = self.cache.release_thumbnail(thumbnail_id)
        {
            app_logging::warning(format!("the thumbnail cache could not be cleared: {error}"));
        }

        if !removed.current {
            app_logging::debug(format!(
                "removed a non-current queue entry: url={} title=\"{}\"",
                removed.url, removed.title
            ));
            return QueueRemoval { removed: true, advanced: false };
        }

        match self.queue.next_after(removed.position) {
            Ok(Some(next)) => {
                if let Err(error) = self.queue.set_current(next.id) {
                    app_logging::error(format!(
                        "the next queue entry could not be made current: {error}"
                    ));
                    return QueueRemoval { removed: true, advanced: false };
                }
                app_logging::notice(format!(
                    "the removed entry was playing; advancing automatically to url={} title=\"{}\" reason=automatic",
                    next.url, next.title
                ));
                self.clear_retry();
                self.consume_current_with_epoch(
                    self.ytdlp_epoch.load(Ordering::SeqCst),
                    PlaybackReason::Automatic,
                );
                QueueRemoval { removed: true, advanced: true }
            }
            Ok(None) => {
                app_logging::notice(
                    "the removed entry was playing and nothing follows it; stopping playback",
                );
                self.preempt_ytdlp();
                let _ = self.engine.stop();
                self.clear_retry();
                self.state.lock().expect("the state is not poisoned").resolving_url = None;
                QueueRemoval { removed: true, advanced: false }
            }
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                QueueRemoval { removed: true, advanced: false }
            }
        }
    }

    /// Reorders the active queue to the given id order and refreshes the view.
    ///
    /// This is what the queue dialog uses after the user drags items around.
    /// Positions are reassigned 0..n-1 in the new order; the current mark
    /// follows its entry because it is tracked by id.
    pub fn reorder_queue(&self, ordered_ids: &[i64]) -> bool {
        app_logging::notice(format!("user reordered the active queue: entry_ids={ordered_ids:?}"));
        match self.queue.reorder(ordered_ids) {
            Ok(()) => true,
            Err(error) => {
                app_logging::error(format!("the active queue could not be reordered: {error}"));
                false
            }
        }
    }

    /// Asks the current queue entry to be consumed.
    ///
    /// The URL is read from the queue, so callers cannot supply a player URL.
    pub fn consume_current(&self) {
        self.consume_current_with_epoch(
            self.ytdlp_epoch.load(Ordering::SeqCst),
            PlaybackReason::Manual,
        );
    }

    fn consume_current_with_epoch(&self, epoch: u64, reason: PlaybackReason) {
        if self.is_ytdlp_stale(epoch) {
            app_logging::debug(format!(
                "discarding a queue consume that was superseded: reason={} epoch={epoch}",
                reason.as_str()
            ));
            return;
        }
        match self.queue.current() {
            Ok(Some(entry)) => {
                if let (Some(json), Some(resolved_at)) =
                    (entry.resolved_source.as_deref(), entry.resolved_at)
                    && unix_time().saturating_sub(resolved_at) <= RESOLVED_SOURCE_MAX_AGE
                    && !source_youtube::matches_url(&entry.url)
                    && let Ok(source) = serde_json::from_str::<MediaSource>(json)
                {
                    app_logging::debug(format!(
                        "using the cached resolved source for url={} age={}s reason={}",
                        entry.url,
                        unix_time().saturating_sub(resolved_at),
                        reason.as_str()
                    ));
                    let message = Message::Resolved(Ok(ResolvedMedia {
                        url: entry.url,
                        thumbnail: entry.thumbnail,
                        resolved_at,
                        reason,
                        source,
                        sponsorblock_segments: Vec::new(),
                    }));
                    let _ = self.outbox.lock().map(|sender| sender.send(Box::new(message)));
                } else {
                    self.resolve_play_with_epoch(entry.url, epoch, reason);
                }
            }
            Ok(None) => {
                app_logging::warning("the active queue has nothing to play");
                self.report("the active queue has nothing to play".to_string());
            }
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                self.report(format!("the active queue could not be read: {error}"));
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
        reason: PlaybackReason,
        sponsorblock_segments: Vec<source_youtube::sponsorblock::Segment>,
    ) -> Option<Loaded> {
        let current = match self.queue.current() {
            Ok(Some(entry)) if entry.url == url => entry,
            Ok(_) => {
                app_logging::debug(format!(
                    "discarding a resolved source that is no longer current: url={url}"
                ));
                return None;
            }
            Err(error) => {
                app_logging::error(format!("the active queue could not be read: {error}"));
                return None;
            }
        };
        // A late retry resolve must not interrupt playback that is already healthy.
        if reason == PlaybackReason::Retry {
            let state = self.state.lock().expect("the state is not poisoned");
            let same_url = state.current_url.as_deref() == Some(url);
            let playback_healthy = same_url && !self.engine.is_idle();
            let has_pending_restore = state.pending_restore.seek.is_some();
            drop(state);
            if playback_healthy || has_pending_restore {
                app_logging::debug(format!(
                    "discarding a stale retry resolve because playback is already active: url={url}"
                ));
                return None;
            }
        }
        if let Err(error) = self.queue.update_details(
            current.position,
            &source.title,
            thumbnail,
            source.thumbnail.as_deref(),
            &thumbnail_candidates_for_source(&source),
            source.uploader.as_deref(),
        ) {
            app_logging::warning(format!("the active queue entry could not be updated: {error}"));
        }
        if let Ok(json) = serde_json::to_string(&source)
            && let Err(error) =
                self.queue.set_resolved_source(current.position, Some(&json), Some(resolved_at))
        {
            app_logging::warning(format!("the resolved queue source could not be saved: {error}"));
        }
        let thumbnail_path = current
            .thumbnail_id()
            .and_then(|id| self.cache.full_path(id))
            .and_then(|path| path.to_str().map(|s| s.to_string()));
        Some(self.accept_current_source(
            source,
            url,
            &current,
            thumbnail_path.as_deref(),
            reason,
            sponsorblock_segments,
        ))
    }

    /// Builds playback state for the source already authorized by the queue.
    fn accept_current_source(
        &self,
        source: MediaSource,
        url: &str,
        current: &library::QueueEntry,
        thumbnail: Option<&str>,
        reason: PlaybackReason,
        sponsorblock_segments: Vec<source_youtube::sponsorblock::Segment>,
    ) -> Loaded {
        let sources = source_lists(&source);
        let default_plan = self
            .settings
            .select(&source, url)
            .and_then(|quality| {
                stream_resolver::plan(&source, &quality)
                    .map_err(|error| settings::Error::Invalid(error.to_string()))
            })
            .ok();
        let default_selection = default_plan.as_ref().map(selection_of).unwrap_or_default();
        let has_saved_selection = current.selected_video.is_some()
            || current.selected_audio.is_some()
            || current.selected_muxed.is_some();
        let saved_selection = if has_saved_selection {
            let video = current
                .selected_video
                .as_deref()
                .and_then(|s| parse_saved_selection(s, &source, false));
            let audio = current
                .selected_audio
                .as_deref()
                .and_then(|s| parse_saved_selection(s, &source, true));
            let muxed = current
                .selected_muxed
                .as_deref()
                .and_then(|s| parse_saved_selection(s, &source, false));
            let all_saved_streams_exist = (current.selected_video.is_none() || video.is_some())
                && (current.selected_audio.is_none() || audio.is_some())
                && (current.selected_muxed.is_none() || muxed.is_some());
            (all_saved_streams_exist && !(muxed.is_some() && (video.is_some() || audio.is_some())))
                .then_some((video, audio, muxed))
        } else {
            None
        };
        let (video, audio, muxed) = saved_selection.unwrap_or(default_selection);
        let plan = plan_from_selection(&source, &video, &audio, &muxed).or(default_plan);
        let (video, audio, muxed) = plan.as_ref().map(selection_of).unwrap_or_default();

        app_logging::notice(format!(
            "starting playback: url={url} title=\"{}\" reason={} live={} video={:?} audio={:?} muxed={:?}",
            source.title,
            reason.as_str(),
            source.is_live,
            video,
            audio,
            muxed
        ));
        app_logging::debug(format!(
            "source catalog: url={url} video_streams={} audio_streams={} uploader={:?}",
            source.video_streams.len(),
            source.audio_streams.len(),
            source.uploader
        ));

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
            thumbnail: thumbnail.map(PathBuf::from),
            sponsorblock_segments: sponsorblock_segments.clone(),
        };

        // Compute id_hash for history tracking
        let id_hash = media_model::video_id_hash(url);

        // Check history for saved playback position (only for non-live videos).
        // A saved position that would land within 5 seconds of the end is treated
        // as fully watched and reset so playback starts from the beginning.
        let mut history_position = if !source.is_live {
            self.queue.history_get(&id_hash).ok().flatten().map(|entry| entry.position_seconds)
        } else {
            None
        };
        if let (Some(position), Some(duration)) = (history_position, source.duration)
            && duration.is_finite()
            && duration > 0.0
            && position.is_finite()
            && position >= (duration - 5.0).max(0.0)
        {
            history_position = None;
            if let Err(error) = self.queue.history_reset_position(&id_hash) {
                app_logging::warning(format!("failed to reset history position: {error}"));
            }
        }

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
            state.current_thumbnail_url = loaded.thumbnail_url.clone();
            state.current_uploader = loaded.uploader.clone();
            state.last_history_save = None;
            state.sponsorblock_segments = sponsorblock_segments;
            state.sponsorblock_suppressed = None;
            // A successful resolution ends any prior retry cycle.
            state.resolving_url = None;
            if plan.is_some() && state.retry.error_kind != Some(RetryErrorKind::Playback) {
                state.retry.clear();
            }
        }
        self.live.lock().expect("the live axis is not poisoned").reset();

        match plan {
            Some(plan) => {
                if let Err(error) = self.start(&plan, loaded.is_live) {
                    app_logging::error(format!(
                        "libmpv could not load the playback plan: url={url} error={error}"
                    ));
                    self.handle_playback_error(url.to_string(), error);
                } else if let Err(error) = self.engine.play() {
                    app_logging::error(format!(
                        "libmpv could not start playback: url={url} error={error}"
                    ));
                    self.handle_playback_error(url.to_string(), error.to_string());
                } else {
                    app_logging::debug(format!("libmpv accepted the playback plan: url={url}"));
                }
            }
            None => {
                app_logging::warning(format!("no playable stream matched the settings: url={url}"));
                self.handle_source_selection_error(url.to_string());
            }
        }
        loaded
    }

    /// Handles a playback error by scheduling a retry.
    pub fn handle_playback_error(&self, url: String, error: String) {
        self.notify_tool_update_if_needed(&error);
        self.schedule_retry(url, RetryErrorKind::Playback, RetryKind::Playback, error);
    }

    /// Handles a source selection error by scheduling a retry with fresh resolution.
    fn handle_source_selection_error(&self, url: String) {
        self.schedule_retry(
            url,
            RetryErrorKind::SourceSelection,
            RetryKind::SourceSelection,
            "this source has no playable stream".to_string(),
        );
    }

    /// Handles a resolver error by scheduling a retry.
    pub fn handle_resolver_error(&self, failure: ResolveFailure) {
        let ResolveFailure { url, message } = failure;
        if !self.expects_resolved_url(&url) {
            return;
        }
        {
            let mut state = self.state.lock().expect("the state is not poisoned");
            if state.resolving_url.as_deref() == Some(url.as_str()) {
                state.resolving_url = None;
            }
        }
        self.notify_tool_update_if_needed(&message);
        self.enable_resolver_ipv4_fallback(&url);
        self.schedule_retry(url, RetryErrorKind::Resolver, RetryKind::Resolver, message);
    }

    /// Enables `-4` for subsequent playback resolves in the current sequence.
    fn enable_resolver_ipv4_fallback(&self, url: &str) {
        let mut resolver = self.resolver.lock().expect("the resolver is not poisoned");
        if resolver.forces_ipv4() {
            return;
        }
        *resolver = resolver.clone().with_force_ipv4();
        self.auto_force_ipv4.store(true, Ordering::SeqCst);
        app_logging::warning(format!(
            "playback resolver error; enabling -4 for subsequent retries: url={url}"
        ));
        let _ = self.outbox.lock().map(|sender| {
            sender.send(Box::new(Message::Toast("Retrying with IPv4...".to_string())))
        });
    }

    /// Schedules another attempt immediately, or reports failure when exhausted.
    fn schedule_retry(
        &self,
        url: String,
        kind: RetryErrorKind,
        report_kind: RetryKind,
        message: String,
    ) {
        if !self.expects_resolved_url(&url) {
            return;
        }
        let mut state = self.state.lock().expect("the state is not poisoned");
        let scheduled = state.retry.start(url, kind, &message);
        let attempt = state.retry.attempt;
        if state.retry.exhausted() {
            state.retry.clear();
            drop(state);
            app_logging::error(format!(
                "giving up after {attempt} retries: kind={kind:?} error={message}"
            ));
            self.report(format!("{message} (gave up after {attempt} retries)"));
            return;
        }
        if !scheduled {
            return;
        }
        let delay = retry_delay(kind, &message, attempt.saturating_sub(1));
        let delay_seconds = delay.as_secs();
        let error = message.clone();
        drop(state);
        app_logging::warning(format!(
            "retry scheduled: kind={kind:?} attempt={attempt} delay={}s error={message}",
            delay_seconds
        ));
        let _ = self.outbox.lock().map(|sender| {
            sender.send(Box::new(Message::RetryScheduled {
                error,
                delay_seconds,
                kind: report_kind,
            }))
        });
    }

    /// Checks if any retry is due and executes it.
    pub fn check_retries(&self) -> Option<RetryKind> {
        let (url, kind) = {
            let mut state = self.state.lock().expect("the state is not poisoned");
            state.retry.take_due()?
        };
        let epoch = self.ytdlp_epoch.load(Ordering::SeqCst);
        app_logging::notice(format!("retrying now: kind={kind:?} url={url} reason=retry"));
        Some(match kind {
            RetryErrorKind::Resolver | RetryErrorKind::SourceSelection => {
                self.resolve_play_with_epoch(url, epoch, PlaybackReason::Retry);
                RetryKind::Resolver
            }
            RetryErrorKind::Playback => {
                self.consume_current_with_epoch(epoch, PlaybackReason::Retry);
                RetryKind::Playback
            }
        })
    }

    fn notify_tool_update_if_needed(&self, error: &str) {
        if !ytdlp_tool::is_update_required(error) {
            return;
        }
        let tools = self.paths.tool_paths();
        let current = ytdlp_tool::locate::managed(&tools)
            .and_then(|tool| tool.version().ok())
            .unwrap_or_default();
        match ytdlp_tool::update::check(&current) {
            Ok(Some(update)) => {
                let should_notify = {
                    let mut guard = self.notified_tool_update.lock().expect("poisoned");
                    if guard.as_deref() == Some(update.latest.as_str()) {
                        false
                    } else {
                        *guard = Some(update.latest.clone());
                        true
                    }
                };
                if should_notify {
                    let _ = self
                        .outbox
                        .lock()
                        .map(|sender| sender.send(Box::new(Message::ToolUpdateAvailable(update))));
                }
            }
            Ok(None) => {}
            Err(err) => app_logging::warning(format!(
                "yt-dlp update check after update-required error failed: {err}"
            )),
        }
    }

    /// Clears any pending retry state.
    fn clear_retry(&self) {
        let mut state = self.state.lock().expect("the state is not poisoned");
        state.retry.clear();
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

        let is_live = {
            let state = self.state.lock().expect("the state is not poisoned");
            state.is_live
        };
        let pending = {
            let _state = self.state.lock().expect("the state is not poisoned");
            if is_live {
                Some(PendingSeek::LiveEdge)
            } else {
                let position = self.engine.position().max(0.0);
                (position > 0.0).then_some(PendingSeek::Position(position))
            }
        };

        if let Err(error) = self.start(&plan, is_live) {
            self.report(error);
            return;
        }
        self.clear_retry();
        self.state.lock().expect("the state is not poisoned").pending_restore.begin(pending);
        self.live.lock().expect("the live axis is not poisoned").reset();

        let mut state = self.state.lock().expect("the state is not poisoned");
        state.selected_video = video.clone();
        state.selected_audio = audio.clone();
        state.selected_muxed = muxed.clone();

        // Save the selection to the queue
        if let Ok(Some(current)) = self.queue.current() {
            let source = state.source.as_ref().expect("source should exist");
            let video_formatted =
                video.as_ref().and_then(|id| format_saved_selection(id, source, false));
            let audio_formatted =
                audio.as_ref().and_then(|id| format_saved_selection(id, source, true));
            let muxed_formatted =
                muxed.as_ref().and_then(|id| format_saved_selection(id, source, false));
            let _ = self.queue.set_selected_sources(
                current.position,
                video_formatted.as_deref(),
                audio_formatted.as_deref(),
                muxed_formatted.as_deref(),
            );
        }
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
        self.suppress_sponsorblock_at(target);
        if let Err(error) = self.engine.seek(target) {
            self.report(error.to_string());
        }
    }

    /// Jumps back by a number of seconds.
    ///
    /// This is the fast replay of a live stream.
    pub fn seek_back(&self, seconds: f64) {
        let target = (self.engine.position() - seconds.abs()).max(0.0);
        self.suppress_sponsorblock_at(target);
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

    /// The downloaded spans of the current media, as fractions of the axis.
    ///
    /// The interface draws them as the secondary bar of the scrubber, so the
    /// user can see where a paused media has already buffered to and which
    /// parts a seek would have to download again.
    pub fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        let ranges: Vec<(f64, f64)> = self
            .engine
            .buffered_ranges()
            .into_iter()
            .map(|range| (range.start, range.end))
            .collect();
        if ranges.is_empty() {
            return Vec::new();
        }

        if self.is_live() {
            let origin = self.live.lock().expect("the live axis is not poisoned").window_start();
            crate::timeline::buffered_fractions(crate::timeline::LIVE_WINDOW, origin, &ranges)
        } else {
            crate::timeline::buffered_fractions(self.engine.duration(), 0.0, &ranges)
        }
    }

    /// The SponsorBlock segments that will be skipped, as fractions of the axis.
    ///
    /// Each span is paired with the colour of its category so the interface can
    /// draw the sections on the scrubber without knowing the category names.
    pub fn sponsorblock_ranges(&self) -> Vec<(f64, f64, [u8; 3])> {
        let (segments, is_live) = {
            let state = self.state.lock().expect("the state is not poisoned");
            (state.sponsorblock_segments.clone(), state.is_live)
        };
        if segments.is_empty() {
            return Vec::new();
        }

        let (span, origin) = if is_live {
            let origin = self.live.lock().expect("the live axis is not poisoned").window_start();
            (crate::timeline::LIVE_WINDOW, origin)
        } else {
            (self.engine.duration(), 0.0)
        };
        if span <= 0.0 {
            return Vec::new();
        }

        let mut marked = Vec::new();
        for segment in &segments {
            if !self.settings.sponsorblock_skip(&segment.category) {
                continue;
            }
            let start = ((segment.start - origin) / span).clamp(0.0, 1.0);
            let end = ((segment.end - origin) / span).clamp(0.0, 1.0);
            if end > start {
                marked.push((start, end, sponsorblock_color(&segment.category)));
            }
        }
        marked.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.total_cmp(&right.1)));
        marked
    }

    /// Changes the volume.
    pub fn set_volume(&self, volume: f64) {
        if let Err(error) = self.engine.set_volume(volume) {
            self.report(error.to_string());
        }
    }

    /// Changes the playback speed.
    pub fn set_speed(&self, speed: f64) {
        if let Err(error) = self.engine.set_speed(speed) {
            self.report(error.to_string());
        }
    }

    /// The current playback speed.
    pub fn speed(&self) -> f64 {
        self.engine.speed()
    }

    /// Uses another `yt-dlp` installation from now on.
    pub fn use_tool(&self, ytdlp: &YtDlp) {
        self.ytdlp.cancel_all();
        self.ytdlp.replace_tool(ytdlp.clone());
        let mut resolver = self.resolver.lock().expect("the resolver is not poisoned");
        *resolver = build_resolver(
            ytdlp,
            self.session.cookies_dir(),
            &self.settings,
            self.proxy_pool.clone(),
        );
    }

    /// Starts background checks for application and `yt-dlp` updates at startup and then periodically.
    pub fn start_update_checks(self: &Arc<Self>, paths: AppPaths) {
        let app = Arc::clone(self);
        self.runtime.handle().spawn(async move {
            loop {
                app.clone().poll_app_update_background();
                app.clone().poll_tool_update_background(paths.clone());
                tokio::time::sleep(UPDATE_CHECK_INTERVAL).await;
            }
        });
    }

    /// Invalidates queued/active playback resolve work started before this point.
    fn preempt_ytdlp(&self) -> u64 {
        // The generation moves first: a resolve that was superseded can no
        // longer take the slot away from the resolve that replaced it.
        let epoch = self.ytdlp_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.ytdlp.cancel_playback();
        self.ytdlp.cancel_url_entry();
        self.ytdlp.cancel_thumbnail();
        self.ytdlp.kill_all();
        epoch
    }

    /// Whether a `yt-dlp` run started for `epoch` has been superseded.
    fn is_ytdlp_stale(&self, epoch: u64) -> bool {
        self.ytdlp_epoch.load(Ordering::SeqCst) != epoch
    }

    fn poll_app_update_background(self: Arc<Self>) {
        let outbox = Arc::clone(&self.outbox);
        let app = Arc::clone(&self);
        let handle = self.runtime.handle().clone();
        handle.spawn_blocking(move || match updater::check(crate::VERSION) {
            Ok(Some(update)) => {
                app_logging::notice(format!(
                    "Flatline update available: current={} latest={}",
                    crate::VERSION,
                    update.version
                ));
                let should_notify = {
                    let mut guard = app.notified_app_update.lock().expect("poisoned");
                    if guard.as_deref() == Some(update.version.as_str()) {
                        false
                    } else {
                        *guard = Some(update.version.clone());
                        true
                    }
                };
                if should_notify {
                    let _ = outbox
                        .lock()
                        .map(|sender| sender.send(Box::new(Message::AppUpdateAvailable(update))));
                }
            }
            Ok(None) => app_logging::debug(format!(
                "Flatline is up to date: current={} latest=none",
                crate::VERSION
            )),
            Err(error) => app_logging::warning(format!(
                "the Flatline update check failed: current={} error={error}",
                crate::VERSION
            )),
        });
    }

    fn poll_tool_update_background(self: Arc<Self>, paths: AppPaths) {
        let outbox = Arc::clone(&self.outbox);
        let app = Arc::clone(&self);
        let handle = self.runtime.handle().clone();
        handle.spawn_blocking(move || {
            let tools = paths.tool_paths();
            let current = ytdlp_tool::locate::managed(&tools)
                .and_then(|tool| tool.version().ok())
                .unwrap_or_default();
            if current.is_empty() {
                return;
            }
            match ytdlp_tool::update::check(&current) {
                Ok(Some(update)) => {
                    app_logging::notice(format!(
                        "yt-dlp update available: current={current} latest={}",
                        update.latest
                    ));
                    let should_notify = {
                        let mut guard = app.notified_tool_update.lock().expect("poisoned");
                        if guard.as_deref() == Some(update.latest.as_str()) {
                            false
                        } else {
                            *guard = Some(update.latest.clone());
                            true
                        }
                    };
                    if should_notify {
                        let _ = outbox.lock().map(|sender| {
                            sender.send(Box::new(Message::ToolUpdateAvailable(update)))
                        });
                    }
                }
                Ok(None) => app_logging::debug(format!("yt-dlp {current} is up to date")),
                Err(error) => app_logging::warning(format!(
                    "the yt-dlp update check failed: current={current} error={error}"
                )),
            }
        });
    }

    /// Prompts the user to download `yt-dlp` when it is missing.
    pub fn prompt_missing_tool_install(&self) {
        let outbox = Arc::clone(&self.outbox);
        self.runtime.handle().spawn_blocking(move || {
            match ytdlp_tool::update::latest_available() {
                Ok(update) => {
                    app_logging::notice(format!(
                        "yt-dlp is missing; offering install of latest={}",
                        update.latest
                    ));
                    let _ = outbox
                        .lock()
                        .map(|sender| sender.send(Box::new(Message::ToolUpdateAvailable(update))));
                }
                Err(error) => {
                    app_logging::error(format!(
                        "the yt-dlp release could not be fetched while installing a missing tool: {error}"
                    ));
                }
            }
        });
    }

    /// Downloads and prepares an accepted application update in the background.
    pub fn install_app_update(
        &self,
        update: updater::Update,
        directory: PathBuf,
        executable: PathBuf,
    ) {
        let outbox = Arc::clone(&self.outbox);
        self.runtime.handle().spawn_blocking(move || {
            app_logging::notice(format!(
                "downloading Flatline update: version={} directory={}",
                update.version,
                directory.display()
            ));
            let progress_outbox = Arc::clone(&outbox);
            let result = updater::install(&update, &directory, &executable, move |progress| {
                let _ = progress_outbox
                    .lock()
                    .map(|sender| sender.send(Box::new(Message::AppUpdateProgress(progress))));
            });
            match result {
                Ok(()) => {
                    app_logging::notice(format!(
                        "Flatline update prepared: version={} executable={}",
                        update.version,
                        executable.display()
                    ));
                }
                Err(error) => {
                    let message = error.to_string();
                    app_logging::error(format!(
                        "the Flatline update failed: version={} error={message}",
                        update.version
                    ));
                    let _ = outbox
                        .lock()
                        .map(|sender| sender.send(Box::new(Message::AppUpdateFailed(message))));
                }
            }
        });
    }

    /// Downloads and applies an accepted `yt-dlp` update in the background.
    pub fn install_tool_update(&self, update: ytdlp_tool::ToolUpdate) {
        let outbox = Arc::clone(&self.outbox);
        let resolver = Arc::clone(&self.resolver);
        let ytdlp = Arc::clone(&self.ytdlp);
        let cookies = self.session.cookies_dir().to_path_buf();
        let settings = self.settings.clone();
        let paths = self.paths.clone();
        let proxy_pool = Arc::clone(&self.proxy_pool);
        self.runtime.handle().spawn(async move {
            app_logging::notice(format!(
                "downloading yt-dlp update via yt-dlp crate: latest={} url={}",
                update.latest, update.download_url
            ));
            let _ = outbox.lock().map(|sender| {
                sender.send(Box::new(Message::ToolUpdateStatus("Applying update...".to_string())))
            });

            let tools = paths.tool_paths();
            let managed = tools.managed_binary();
            match ytdlp_tool::update_managed(&managed, tools.root()).await {
                Ok(version) => {
                    let tool = ytdlp_tool::locate::at(&managed, tools.clone());
                    ytdlp.replace_tool(tool.clone());
                    if let Ok(mut resolver) = resolver.lock() {
                        *resolver = build_resolver(&tool, &cookies, &settings, proxy_pool.clone());
                    }
                    app_logging::notice(format!("yt-dlp update applied: latest={version}"));
                    let _ = outbox
                        .lock()
                        .map(|sender| sender.send(Box::new(Message::ToolUpdateApplied(version))));
                }
                Err(error) => {
                    let message = error.to_string();
                    app_logging::error(format!(
                        "the yt-dlp update could not be applied: latest={} error={message}",
                        update.latest
                    ));
                    let _ = outbox
                        .lock()
                        .map(|sender| sender.send(Box::new(Message::ToolUpdateFailed(message))));
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
        self.clear_retry();
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
        let should_save = state
            .last_history_save
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(20));

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

        if let Err(error) = self.queue.history_upsert(
            &id_hash,
            &url,
            &title,
            thumbnail_url.as_deref(),
            uploader.as_deref(),
            position,
        ) {
            app_logging::warning(format!("failed to save history position: {error}"));
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
            app_logging::warning(format!("failed to reset history position: {error}"));
        }
    }

    /// Applies SponsorBlock skips for the current playback position.
    ///
    /// Manual seeks can land inside a skip range to let the user watch it once;
    /// normal playback skips enabled ranges automatically. Overlapping enabled
    /// segments are merged so the range is skipped in a single seek.
    pub fn apply_sponsorblock_skips(&self) {
        if self.engine.is_idle() {
            return;
        }
        let position = self.engine.position();
        if !position.is_finite() || position < 0.0 {
            return;
        }
        let duration = self.engine.duration();
        let mut state = self.state.lock().expect("the state is not poisoned");
        if state.is_live || state.sponsorblock_segments.is_empty() {
            state.sponsorblock_suppressed = None;
            return;
        }
        // Wait until the media duration is known and the player is seekable.
        if !duration.is_finite() || duration <= 0.0 {
            return;
        }

        let segments = state.sponsorblock_segments.clone();
        let ranges = source_youtube::sponsorblock::merged_skip_ranges(&segments, |category| {
            self.settings.sponsorblock_skip(category)
        });
        if ranges.is_empty() {
            state.sponsorblock_suppressed = None;
            return;
        }

        if let Some((start, end)) = state.sponsorblock_suppressed {
            // While the user-selected (or in-progress) range is playing, do not
            // auto-skip. Once playback leaves it, normal skip rules resume so a
            // later scrub back before the range will skip it again.
            if position + 0.05 >= start && position < end - 0.05 {
                return;
            }
            state.sponsorblock_suppressed = None;
        }

        let Some(range) = ranges
            .iter()
            .find(|range| position + 0.05 >= range.start && position < range.end - 0.05)
            .cloned()
        else {
            return;
        };

        // Skip to the merged range end in one seek. Wait until the target is
        // within mpv's reported seekable range to avoid ignored/failed seeks
        // (e.g. during startup or for segments starting at 0:00).
        let desired = range.end.min((duration - 0.05).max(0.0));
        let Some(edge) = self.engine.seekable_end() else {
            return;
        };
        if !edge.is_finite() || edge < desired - 0.05 {
            return;
        }
        let target = desired.min(edge).max(0.0);
        if target <= position + 0.05 {
            return;
        }

        let fully_skippable = target >= range.end - 0.05;
        // Suppress while the seek is in flight; only keep suppression if we
        // could not jump past the whole merged range (e.g. near EOF).
        state.sponsorblock_suppressed =
            if fully_skippable { None } else { Some((range.start, range.end)) };
        drop(state);

        app_logging::debug(format!(
            "skipping SponsorBlock segment during playback: category={} from={position:.3}s to={target:.3}s",
            range.category
        ));
        if let Err(error) = self.engine.seek(target) {
            app_logging::warning(format!(
                "failed to auto-skip SponsorBlock segment: category={} target={target:.3}s error={error}",
                range.category
            ));
            let mut state = self.state.lock().expect("the state is not poisoned");
            if state.sponsorblock_suppressed == Some((range.start, range.end)) {
                state.sponsorblock_suppressed = None;
            }
        } else if fully_skippable {
            self.toast(format!("Skipped {}", sponsorblock_skip_label(&range.category)));
        }
    }

    /// Remembers a manually selected skip range so it can play once.
    fn suppress_sponsorblock_at(&self, target: f64) {
        let mut state = self.state.lock().expect("the state is not poisoned");
        if state.is_live || state.sponsorblock_segments.is_empty() || !target.is_finite() {
            state.sponsorblock_suppressed = None;
            return;
        }
        let segments = state.sponsorblock_segments.clone();
        let ranges = source_youtube::sponsorblock::merged_skip_ranges(&segments, |category| {
            self.settings.sponsorblock_skip(category)
        });
        state.sponsorblock_suppressed = ranges
            .iter()
            .find(|range| target + 0.05 >= range.start && target < range.end - 0.05)
            .map(|range| (range.start, range.end));
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
    fn start(&self, plan: &PlaybackPlan, is_live: bool) -> Result<(), String> {
        // Every load belongs to a different media, and a cached packet from
        // the old source would be plain garbage to the new demuxer.
        clear_stream_buffer(&self.stream_buffer);
        let pre_buffer_seconds = self.settings.pre_buffer_seconds().0;
        self.engine
            .set_pre_buffer(pre_buffer_seconds, is_live)
            .map_err(|error| error.to_string())?;

        let (http_headers, cookies) = if let Some(video) = &plan.video {
            (video.http_headers.as_ref(), video.cookies.as_deref())
        } else if let Some(audio) = &plan.audio {
            (audio.http_headers.as_ref(), audio.cookies.as_deref())
        } else {
            (None, None)
        };
        self.engine
            .apply_stream_request(http_headers, cookies)
            .map_err(|error| error.to_string())?;

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

    /// Retries a failed URL entry with bounded attempts and IPv4 fallback.
    ///
    /// Returns true when another attempt was scheduled.
    pub fn retry_url_entry(&self, url: &str, epoch: u64, error: &str) -> bool {
        let normalized = url.trim().to_string();
        if normalized.is_empty() {
            return false;
        }
        let (attempt, force_ipv4) = {
            let mut state = self.url_entry.lock().expect("the url entry state is not poisoned");
            if !state.is_current(epoch) || !state.mark_retry(&normalized) {
                return false;
            }
            (state.attempts, state.force_ipv4)
        };

        let resolver = {
            let resolver = self.resolver.lock().expect("the resolver is not poisoned").clone();
            if force_ipv4 && !resolver.forces_ipv4() {
                resolver.with_force_ipv4()
            } else {
                resolver
            }
        };

        app_logging::warning(format!(
            "url entry failed; retrying: url={normalized} epoch={epoch} attempt={attempt}/{URL_ENTRY_MAX_ATTEMPTS} error={error}"
        ));
        self.toast(format!("Could not process the URL: {error}"));
        if attempt == 2 {
            self.toast("Retrying with IPv4...".to_string());
        } else {
            self.toast(format!("Retrying ({}/{})...", attempt - 1, URL_ENTRY_MAX_ATTEMPTS - 1));
        }
        self.start_url_entry(normalized, epoch, resolver);
        true
    }

    /// Ends the URL entry that is running.
    ///
    /// The process it started is killed and its result is discarded, so
    /// nothing of it can reach the user afterwards.
    pub fn cancel_url_entry(&self) {
        self.url_entry_epoch.fetch_add(1, Ordering::SeqCst);
        self.ytdlp.cancel_url_entry();
        app_logging::notice("the url entry was cancelled");
    }

    /// Clears auto-enabled playback IPv4 fallback after a successful resolve.
    pub fn clear_playback_ipv4_fallback(&self) {
        self.auto_force_ipv4.store(false, Ordering::SeqCst);
    }

    /// Sends an informational toast to the user interface.
    fn toast(&self, text: String) {
        let _ = self.outbox.lock().map(|sender| sender.send(Box::new(Message::Toast(text))));
    }
}
#[derive(Debug, Clone)]
struct ThumbnailRefreshItem {
    id: i64,
    position: i64,
    url: String,
    candidates: Vec<String>,
}

/// The queue entries that lack a valid cached thumbnail but can still be
/// downloaded. Queue rows are read only; candidates come from queue metadata
/// and/or thumbnail cache metadata.
fn entries_needing_thumbnails(
    entries: &[library::QueueEntry],
    cache: &ThumbnailCache,
) -> Vec<ThumbnailRefreshItem> {
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry
                .thumbnail_id()
                .map(str::to_string)
                .unwrap_or_else(|| ThumbnailCache::thumbnail_id_for(&entry.url));
            if cache.full_path(&id).is_some() {
                return None;
            }
            let mut candidates = entry_thumbnail_candidates(entry);
            if candidates.is_empty()
                && let Ok(cached) = cache.thumbnail_urls(&id)
            {
                candidates = cached;
            }
            if candidates.is_empty() {
                app_logging::debug(format!(
                    "thumbnail refresh skipping entry with no candidates: queue_write=false entry_id={} position={} url={}",
                    entry.id, entry.position, entry.url
                ));
                return None;
            }
            Some(ThumbnailRefreshItem {
                id: entry.id,
                position: entry.position,
                url: entry.url.clone(),
                candidates,
            })
        })
        .collect()
}

fn entry_thumbnail_candidates(entry: &library::QueueEntry) -> Vec<String> {
    thumbnail_candidates(entry.thumbnail_url.as_deref(), &entry.thumbnail_urls)
}

fn thumbnail_candidates_for_source(source: &MediaSource) -> Vec<String> {
    thumbnail_candidates(source.thumbnail.as_deref(), &source.thumbnails)
}

fn thumbnail_candidates(primary: Option<&str>, urls: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for url in primary.into_iter().chain(urls.iter().map(String::as_str)) {
        let trimmed = url.trim();
        if trimmed.is_empty() || out.iter().any(|existing: &String| existing == trimmed) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

/// Scans playlists and repairs missing/broken thumbnail cache files.
/// This reads queue rows and writes only thumbnail cache data.
fn scan_playlist_thumbnails(queue: &Queue, cache: &ThumbnailCache) -> usize {
    let playlists = match queue.playlists() {
        Ok(playlists) => playlists,
        Err(error) => {
            app_logging::warning(format!(
                "thumbnail refresh could not read playlists for cache scan: {error}"
            ));
            return 0;
        }
    };
    let mut repaired = 0usize;
    for playlist in playlists {
        let entries = match queue.entries_for_playlist(&playlist.id) {
            Ok(entries) => entries,
            Err(error) => {
                app_logging::warning(format!(
                    "thumbnail refresh could not read playlist for cache scan: playlist_id={} error={error}",
                    playlist.id
                ));
                continue;
            }
        };
        for entry in entries {
            let id = entry
                .thumbnail_id()
                .map(str::to_string)
                .unwrap_or_else(|| ThumbnailCache::thumbnail_id_for(&entry.url));
            if cache.full_path(&id).is_some() {
                continue;
            }
            let mut candidates = entry_thumbnail_candidates(&entry);
            if candidates.is_empty() {
                candidates = cache.thumbnail_urls(&id).unwrap_or_default();
            }
            if candidates.is_empty() {
                continue;
            }
            let _ = cache.remember_thumbnail_urls(&entry.url, &candidates);
            match cache.fetch(&entry.url, &candidates) {
                Ok((thumb_id, used_url)) => {
                    app_logging::notice(format!(
                        "thumbnail cache repaired: queue_write=false playlist_id={} entry_id={} position={} url={} thumb_id={} source_url={}",
                        playlist.id, entry.id, entry.position, entry.url, thumb_id, used_url
                    ));
                    repaired += 1;
                    if repaired >= 8 {
                        return repaired;
                    }
                }
                Err(error) => {
                    app_logging::warning(format!(
                        "thumbnail cache repair failed: queue_write=false playlist_id={} entry_id={} position={} url={} error={error}",
                        playlist.id, entry.id, entry.position, entry.url
                    ));
                }
            }
        }
    }
    repaired
}

/// Removes the media a previous source buffered.
fn clear_stream_buffer(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(error) = result {
            app_logging::warning(format!("a stream buffer file could not be removed: {error}"));
        }
    }
}

#[derive(Debug)]
enum ThumbnailRefreshOutcome {
    Completed,
    Preempted,
    RetryLater(String),
}

#[allow(clippy::too_many_arguments)]
fn refresh_thumbnail_entry(
    cache: &ThumbnailCache,
    _queue: &Queue,
    resolver: &Arc<Mutex<Resolver>>,
    session: &Session,
    ytdlp_epoch: &Arc<AtomicU64>,
    pass_epoch: u64,
    ytdlp: &Arc<YtdlpControl>,
    task_handle: &tokio::runtime::Handle,
    outbox: &Arc<Mutex<Sender<Box<Message>>>>,
    item: &ThumbnailRefreshItem,
) -> ThumbnailRefreshOutcome {
    loop {
        if ytdlp_epoch.load(Ordering::SeqCst) != pass_epoch {
            return ThumbnailRefreshOutcome::Preempted;
        }
        let Some((cancel, _pid)) = ytdlp.try_begin_thumbnail() else {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };

        app_logging::debug(format!(
            "thumbnail refresh: downloading cache only: queue_write=false entry_id={} position={} url={} candidates={}",
            item.id,
            item.position,
            item.url,
            item.candidates.len()
        ));
        let mut candidates = item.candidates.clone();
        let _ = cache.remember_thumbnail_urls(&item.url, &candidates);

        let direct_result = cache.fetch(&item.url, &candidates);
        match direct_result {
            Ok((thumb_id, used_url)) => {
                ytdlp.finish_thumbnail(&cancel);
                app_logging::notice(format!(
                    "thumbnail refresh cached: queue_write=false entry_id={} position={} url={} thumb_id={} source_url={}",
                    item.id, item.position, item.url, thumb_id, used_url
                ));
                return ThumbnailRefreshOutcome::Completed;
            }
            Err(error) => {
                app_logging::warning(format!(
                    "thumbnail refresh direct download failed: queue_write=false entry_id={} url={} error={error}",
                    item.id, item.url
                ));
            }
        }
        ytdlp.finish_thumbnail(&cancel);

        match refresh_thumbnail_entry_via_ytdlp(
            cache,
            resolver,
            session,
            ytdlp_epoch,
            pass_epoch,
            ytdlp,
            task_handle,
            outbox,
            item,
            &mut candidates,
            "thumbnail refresh: resolving candidates via yt-dlp",
        ) {
            ThumbnailRefreshViaYtdlp::Completed => return ThumbnailRefreshOutcome::Completed,
            ThumbnailRefreshViaYtdlp::Preempted => return ThumbnailRefreshOutcome::Preempted,
            ThumbnailRefreshViaYtdlp::RetryLater(message) => {
                return ThumbnailRefreshOutcome::RetryLater(message);
            }
        }
    }
}

enum ThumbnailRefreshViaYtdlp {
    Completed,
    Preempted,
    RetryLater(String),
}

#[allow(clippy::too_many_arguments)]
fn refresh_thumbnail_entry_via_ytdlp(
    cache: &ThumbnailCache,
    resolver: &Arc<Mutex<Resolver>>,
    session: &Session,
    ytdlp_epoch: &Arc<AtomicU64>,
    pass_epoch: u64,
    ytdlp: &Arc<YtdlpControl>,
    task_handle: &tokio::runtime::Handle,
    outbox: &Arc<Mutex<Sender<Box<Message>>>>,
    item: &ThumbnailRefreshItem,
    candidates: &mut Vec<String>,
    reason: &str,
) -> ThumbnailRefreshViaYtdlp {
    if ytdlp_epoch.load(Ordering::SeqCst) != pass_epoch {
        return ThumbnailRefreshViaYtdlp::Preempted;
    }
    let Some((cancel, pid)) = ytdlp.try_begin_thumbnail() else {
        return ThumbnailRefreshViaYtdlp::Preempted;
    };
    app_logging::debug(format!(
        "{reason}: queue_write=false entry_id={} position={} url={}",
        item.id, item.position, item.url
    ));

    let sender = outbox.lock().expect("outbox poisoned").clone();
    let resolver = resolver.lock().expect("the resolver is not poisoned").clone();
    let resolved = task_handle.block_on(resolve_with_session(
        &resolver,
        session,
        ytdlp_epoch,
        pass_epoch,
        &cancel,
        &pid,
        &item.url,
        &sender,
    ));
    ytdlp.finish_thumbnail(&cancel);

    match resolved {
        Ok(source) => {
            if ytdlp_epoch.load(Ordering::SeqCst) != pass_epoch || cancel.load(Ordering::SeqCst) {
                return ThumbnailRefreshViaYtdlp::Preempted;
            }
            let resolved_candidates = thumbnail_candidates_for_source(&source);
            if resolved_candidates.is_empty() {
                let message = "yt-dlp refresh returned no thumbnail URL".to_string();
                app_logging::warning(format!(
                    "thumbnail refresh: queue_write=false entry_id={} url={} {message}",
                    item.id, item.url
                ));
                return ThumbnailRefreshViaYtdlp::RetryLater(message);
            }
            *candidates = resolved_candidates;
            let _ = cache.remember_thumbnail_urls(&item.url, candidates);
            match cache.fetch(&item.url, candidates) {
                Ok((thumb_id, used_url)) => {
                    app_logging::notice(format!(
                        "thumbnail refresh recovered via yt-dlp: queue_write=false entry_id={} position={} url={} thumb_id={} source_url={}",
                        item.id, item.position, item.url, thumb_id, used_url
                    ));
                    ThumbnailRefreshViaYtdlp::Completed
                }
                Err(error) => {
                    let message =
                        format!("thumbnail download after yt-dlp refresh failed: {error}");
                    app_logging::warning(format!(
                        "thumbnail refresh failed: queue_write=false entry_id={} url={} {message}",
                        item.id, item.url
                    ));
                    ThumbnailRefreshViaYtdlp::RetryLater(message)
                }
            }
        }
        Err(stream_resolver::Error::Cancelled) => ThumbnailRefreshViaYtdlp::Preempted,
        Err(error) => {
            let message = format!("yt-dlp re-resolve failed: {error}");
            app_logging::warning(format!(
                "thumbnail refresh failed: queue_write=false entry_id={} url={} {message}",
                item.id, item.url
            ));
            ThumbnailRefreshViaYtdlp::RetryLater(message)
        }
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

/// Builds the playback plan represented by a saved picker selection.
fn plan_from_selection(
    source: &MediaSource,
    video: &Option<String>,
    audio: &Option<String>,
    muxed: &Option<String>,
) -> Option<PlaybackPlan> {
    let quality = if let Some(id) = muxed {
        Quality::Streams { video: Some(id.clone()), audio: None }
    } else {
        Quality::Streams { video: video.clone(), audio: audio.clone() }
    };
    stream_resolver::plan(source, &quality).ok()
}

/// Parses a saved selection string (format "ID|value") and verifies the stream
/// still exists in the source. Returns the format_id if valid.
fn parse_saved_selection(saved: &str, source: &MediaSource, is_audio: bool) -> Option<String> {
    let parts: Vec<&str> = saved.split('|').collect();
    if parts.len() != 2 {
        return None;
    }
    let format_id = parts[0];
    let _value = parts[1]; // We could verify this matches, but just check ID exists

    if is_audio {
        source.audio_streams.iter().find(|s| s.format_id == format_id).map(|s| s.format_id.clone())
    } else {
        source.video_streams.iter().find(|s| s.format_id == format_id).map(|s| s.format_id.clone())
    }
}

/// Formats a selection as "ID|value" for storage. Value is height for video/muxed,
/// bitrate for audio.
fn format_saved_selection(format_id: &str, source: &MediaSource, is_audio: bool) -> Option<String> {
    if is_audio {
        source.audio_streams.iter().find(|s| s.format_id == format_id).map(|s| {
            let bitrate = s.bitrate.unwrap_or(0);
            format!("{}|{}", format_id, bitrate)
        })
    } else {
        source.video_streams.iter().find(|s| s.format_id == format_id).map(|s| {
            let height = s.height.unwrap_or(0);
            format!("{}|{}", format_id, height)
        })
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

/// The label shown when a SponsorBlock category is skipped automatically.
fn sponsorblock_skip_label(category: &str) -> &'static str {
    match category {
        "sponsor" => "Sponsor",
        "selfpromo" => "Unpaid/Self Promotion",
        "interaction" => "Interaction Reminder",
        "intro" => "Intermission/Intro",
        "outro" => "Endcards/Credits",
        "preview" => "Preview/Recap",
        "hook" => "Hook",
        "filler" => "Filler Tangent",
        _ => "Segment",
    }
}

/// The scrubber colour of a SponsorBlock category.
///
/// The colours follow the ones the SponsorBlock web extension uses, so the
/// segments are recognisable to people who already know them.
fn sponsorblock_color(category: &str) -> [u8; 3] {
    match category {
        "sponsor" => [0, 212, 0],
        "selfpromo" => [255, 255, 0],
        "interaction" => [204, 0, 255],
        "intro" => [0, 255, 255],
        "outro" => [2, 2, 237],
        "preview" => [0, 143, 214],
        "hook" => [57, 86, 153],
        "filler" => [115, 0, 255],
        _ => [180, 180, 180],
    }
}

/// Retry delay for a failure class.
fn retry_delay(_kind: RetryErrorKind, _message: &str, _attempt: u32) -> Duration {
    RETRY_BASE_DELAY
}

/// Builds the resolver around the managed yt-dlp tool and known cookie files.
fn build_resolver(
    ytdlp: &YtDlp,
    cookies: &std::path::Path,
    settings: &Settings,
    proxy_pool: Arc<ProxyPool>,
) -> Resolver {
    let deno_path = ensure_deno(ytdlp, settings);
    let extra_args = settings.ytdlp_extra_args(deno_path.as_deref());
    if !extra_args.is_empty() {
        app_logging::notice(format!("yt-dlp extra args enabled: {}", extra_args.join(" ")));
    }
    // Site arguments are regex matched when a URL is resolved, so they are
    // applied only to the sites whose entry actually names them.
    let site_args = settings.ytdlp_site_arg_rules();
    if !site_args.is_empty() {
        app_logging::notice(format!(
            "yt-dlp site args enabled: {} rule(s) from settings",
            site_args.len()
        ));
    }
    let mut resolver = Resolver::new(ytdlp.clone())
        .with_args(extra_args)
        .with_site_args(site_args)
        .with_proxy_pool(proxy_pool);
    let services: Vec<Service> = web_session::Sources::installed()
        .map(|sources| sources.all().iter().map(|source| source.service()).collect())
        .unwrap_or_else(|| vec![Service::YouTube, Service::Twitch]);
    for service in services {
        let path = web_session::cookie_file(cookies, service);
        if web_session::cookie::signed_in_file(service, &path) {
            resolver = resolver.with_cookies(service, path);
        }
    }
    resolver
}

/// Ensures the Deno runtime is installed when the settings request it.
///
/// The Deno binary is downloaded next to `yt-dlp` inside the tool directory
/// and never writes outside of it: `DENO_DIR` points at `tools/deno-files`
/// when the runtime is spawned, so all caches and configuration stay inside
/// the portable package.
fn ensure_deno(ytdlp: &YtDlp, settings: &Settings) -> Option<std::path::PathBuf> {
    if settings.js_runtime() != settings::BUNDLED_JS_RUNTIME {
        return None;
    }
    let platform = ytdlp_tool::platform::Platform::current();
    let binary = ytdlp.paths().root().join(platform.deno_binary_name());
    if binary.is_file() {
        // The directory the runtime writes its files to is part of the
        // portable folder, so it is created next to the runtime.
        let files = ytdlp.paths().deno_files_dir();
        if let Err(error) = std::fs::create_dir_all(&files) {
            app_logging::warning(format!(
                "the deno files directory could not be created: dir={} error={error}",
                files.display()
            ));
        }
        return Some(binary);
    }
    match ytdlp_tool::deno::install(ytdlp.paths()) {
        Ok(path) => Some(path),
        Err(error) => {
            app_logging::warning(format!("the deno runtime could not be installed: {error}"));
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sign_in_and_resolve_document(
    resolver: &Resolver,
    session: &Session,
    epoch_state: &AtomicU64,
    epoch: u64,
    cancel: &Arc<AtomicBool>,
    pid: &Arc<AtomicU32>,
    url: &str,
    notice: &Sender<Box<Message>>,
    service: media_model::Service,
    original_error: stream_resolver::Error,
) -> Result<stream_resolver::ResolvedDocument, stream_resolver::Error> {
    use stream_resolver::Error as ResolveError;
    match session.sign_in(service, url, notice) {
        Ok(()) => {
            if cancel.load(Ordering::SeqCst) || epoch_state.load(Ordering::SeqCst) != epoch {
                return Err(ResolveError::Cancelled);
            }
            resolver.resolve_document_with_cancel(url, cancel.clone(), pid.clone()).await
        }
        Err(crate::session::Error::Declined) => {
            if cancel.load(Ordering::SeqCst) || epoch_state.load(Ordering::SeqCst) != epoch {
                return Err(ResolveError::Cancelled);
            }
            let _ = notice.send(Box::new(Message::Problem("User did not sign-in".to_string())));
            Err(original_error)
        }
        Err(problem) => {
            if cancel.load(Ordering::SeqCst) || epoch_state.load(Ordering::SeqCst) != epoch {
                return Err(ResolveError::Cancelled);
            }
            let message = original_error.to_string();
            Err(ResolveError::Tool(format!("{message} ({problem})")))
        }
    }
}

/// Resolves a URL, offering sign-in when the service requires it.
#[allow(clippy::too_many_arguments)]
async fn resolve_with_session(
    resolver: &Resolver,
    session: &Session,
    epoch_state: &AtomicU64,
    epoch: u64,
    cancel: &Arc<AtomicBool>,
    pid: &Arc<AtomicU32>,
    url: &str,
    notice: &Sender<Box<Message>>,
) -> Result<MediaSource, stream_resolver::Error> {
    match resolve_document_with_session(
        resolver,
        session,
        epoch_state,
        epoch,
        cancel,
        pid,
        url,
        notice,
    )
    .await?
    {
        stream_resolver::ResolvedDocument::Video(source) => Ok(source),
        stream_resolver::ResolvedDocument::Playlist(playlist) => {
            for entry in playlist.entries {
                if cancel.load(Ordering::SeqCst) || epoch_state.load(Ordering::SeqCst) != epoch {
                    return Err(stream_resolver::Error::Cancelled);
                }
                match resolve_document_with_session(
                    resolver,
                    session,
                    epoch_state,
                    epoch,
                    cancel,
                    pid,
                    &entry.url,
                    notice,
                )
                .await?
                {
                    stream_resolver::ResolvedDocument::Video(source) => return Ok(source),
                    stream_resolver::ResolvedDocument::Playlist(_) => continue,
                }
            }
            Err(stream_resolver::Error::Empty)
        }
    }
}

/// Resolves a URL into a document, offering sign-in when the service requires it.
#[allow(clippy::too_many_arguments)]
async fn resolve_document_with_session(
    resolver: &Resolver,
    session: &Session,
    epoch_state: &AtomicU64,
    epoch: u64,
    cancel: &Arc<AtomicBool>,
    pid: &Arc<AtomicU32>,
    url: &str,
    notice: &Sender<Box<Message>>,
) -> Result<stream_resolver::ResolvedDocument, stream_resolver::Error> {
    use stream_resolver::Error as ResolveError;

    if cancel.load(Ordering::SeqCst) || epoch_state.load(Ordering::SeqCst) != epoch {
        return Err(ResolveError::Cancelled);
    }
    let first = resolver.resolve_document_with_cancel(url, cancel.clone(), pid.clone()).await;
    let error = match first {
        Ok(document) => return Ok(document),
        Err(error) => error,
    };
    if matches!(error, ResolveError::Cancelled)
        || cancel.load(Ordering::SeqCst)
        || epoch_state.load(Ordering::SeqCst) != epoch
    {
        return Err(ResolveError::Cancelled);
    }

    let message = error.to_string();
    let Some(service) = web_session::challenge_service(url, &message) else {
        return Err(error);
    };
    sign_in_and_resolve_document(
        resolver,
        session,
        epoch_state,
        epoch,
        cancel,
        pid,
        url,
        notice,
        service,
        error,
    )
    .await
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
            http_headers: None,
            cookies: None,
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
            http_headers: None,
            cookies: None,
        }
    }

    fn source() -> MediaSource {
        MediaSource {
            id: "id".to_string(),
            title: "Title".to_string(),
            uploader: None,
            duration: Some(10.0),
            thumbnail: None,
            thumbnails: Vec::new(),
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
    fn a_new_url_replaces_the_entry_that_is_running() {
        let mut state = UrlEntryState::default();
        state.begin("https://example.com/one", 1);
        assert!(state.is_current(1));

        state.begin("https://example.com/two", 2);

        assert!(!state.is_current(1));
        assert!(state.is_current(2));
    }

    #[test]
    fn an_entry_that_never_started_is_not_the_current_one() {
        let state = UrlEntryState::default();

        assert!(!state.is_current(0));
        assert!(!state.is_current(1));
    }

    #[test]
    fn url_entry_retries_enable_ipv4_and_stop_after_budget() {
        let mut state = UrlEntryState::default();
        state.begin("https://example.com/one", 1);

        for attempt in 2..=URL_ENTRY_MAX_ATTEMPTS {
            assert!(state.mark_retry("https://example.com/one"));
            assert_eq!(state.attempts, attempt);
            assert!(state.force_ipv4);
        }
        assert!(!state.mark_retry("https://example.com/one"));
    }

    #[test]
    fn a_new_url_entry_resets_retry_state() {
        let mut state = UrlEntryState::default();
        state.begin("https://example.com/one", 1);
        assert!(state.mark_retry("https://example.com/one"));

        state.begin("https://example.com/two", 2);

        assert_eq!(state.attempts, 1);
        assert!(!state.force_ipv4);
        assert!(state.may_retry("https://example.com/two"));
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
    fn retry_attempts_keep_backoff_for_the_same_failure() {
        let mut retry = RetryState::default();
        retry.start("https://example.com/video".into(), RetryErrorKind::Resolver, "err");
        retry.next_attempt();
        retry.start("https://example.com/video".into(), RetryErrorKind::Resolver, "err");

        assert_eq!(retry.attempt, 1);
        assert!(retry.next_retry.is_some());

        retry.start("https://example.com/other".into(), RetryErrorKind::Resolver, "err");
        assert_eq!(retry.attempt, 0);
    }

    #[test]
    fn a_due_retry_is_taken_once_and_its_timer_is_cleared() {
        let mut retry = RetryState::default();
        assert!(retry.start("https://example.com/video".into(), RetryErrorKind::Resolver, "err"));
        assert!(!retry.start("https://example.com/video".into(), RetryErrorKind::Resolver, "err"));

        let due = retry.take_due().expect("the retry is due");
        assert_eq!(due.0, "https://example.com/video");
        assert_eq!(due.1, RetryErrorKind::Resolver);
        assert!(retry.next_retry.is_none());
        assert_eq!(retry.attempt, 1);
        assert!(retry.take_due().is_none());
    }

    #[test]
    fn retry_delay_stays_immediate() {
        assert_eq!(retry_delay(RetryErrorKind::Resolver, "any error", 0), Duration::from_secs(0));
        assert_eq!(retry_delay(RetryErrorKind::Playback, "timed out", 2), Duration::from_secs(0));
    }

    #[test]
    fn a_retry_stops_after_the_attempt_budget_is_used_up() {
        let mut retry = RetryState::default();
        retry.start("https://example.com/video".into(), RetryErrorKind::Resolver, "some error");

        for _ in 0..MAX_RETRY_ATTEMPTS {
            assert!(!retry.exhausted());
            retry.next_attempt();
        }

        assert!(retry.exhausted());
    }

    #[test]
    fn saved_split_selection_builds_the_selected_playback_plan() {
        let source = source();
        let video = Some("137".to_string());
        let audio = Some("140".to_string());
        let plan = plan_from_selection(&source, &video, &audio, &None).expect("selection is valid");

        assert_eq!(selection_of(&plan), (video, audio, None));
    }

    #[test]
    fn saved_video_only_selection_does_not_attach_default_audio() {
        let source = source();
        let video = Some("137".to_string());
        let plan = plan_from_selection(&source, &video, &None, &None).expect("selection is valid");

        assert_eq!(selection_of(&plan), (video, None, None));
    }

    #[test]
    fn saved_audio_only_selection_builds_an_audio_plan() {
        let source = source();
        let audio = Some("140".to_string());
        let plan = plan_from_selection(&source, &None, &audio, &None).expect("selection is valid");

        assert_eq!(selection_of(&plan), (None, audio, None));
    }

    #[test]
    fn labels_carry_the_size_the_user_asked_for() {
        let lists = source_lists(&source());
        assert_eq!(lists.video[0].label, "1080p");
        assert!(lists.video[0].detail.contains("1920×1080"));
        assert!(lists.audio[0].label.contains("128"));
    }

    #[test]
    fn clearing_the_stream_buffer_removes_every_downloaded_file() {
        let directory = std::env::temp_dir().join("flatline-stream-buffer-test");
        let nested = directory.join("nested");
        std::fs::create_dir_all(&nested).expect("the test directory can be created");
        std::fs::write(directory.join("cache.bin"), b"media").expect("a cache file can be written");
        std::fs::write(nested.join("part.bin"), b"more").expect("a nested file can be written");

        clear_stream_buffer(&directory);

        assert!(
            std::fs::read_dir(&directory)
                .expect("the buffer directory stays in place")
                .next()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn entries_needing_thumbnails_skips_those_that_are_cached() {
        let temp = tempfile::tempdir().expect("a temp directory can be created");
        let queue = Queue::in_memory().expect("an in-memory queue opens");
        let cache = ThumbnailCache::new(queue.database(), temp.path());

        let image = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgb([255u8, 0, 0]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .expect("a valid JPEG encodes");
        let jpeg = temp.path().join("source.jpg");
        std::fs::write(&jpeg, &bytes).unwrap();

        let cached_id = cache.register_legacy("https://example.com/cached", &jpeg).unwrap();

        let link = |s: &str| s.to_string();
        let entries = vec![
            library::QueueEntry {
                id: 1,
                position: 0,
                url: link("https://example.com/cached"),
                title: "cached".into(),
                thumbnail: Some(cached_id.clone()),
                thumbnail_url: Some(link("https://example.com/thumb.jpg")),
                thumbnail_urls: vec![link("https://example.com/thumb.jpg")],
                uploader: None,
                id_hash: "h".into(),
                current: false,
                resolved_source: None,
                resolved_at: None,
                selected_video: None,
                selected_audio: None,
                selected_muxed: None,
            },
            library::QueueEntry {
                id: 2,
                position: 1,
                url: link("https://example.com/missing"),
                title: "missing".into(),
                thumbnail: None,
                thumbnail_url: Some(link("https://example.com/thumb.jpg")),
                thumbnail_urls: vec![link("https://example.com/thumb.jpg")],
                uploader: None,
                id_hash: "h".into(),
                current: false,
                resolved_source: None,
                resolved_at: None,
                selected_video: None,
                selected_audio: None,
                selected_muxed: None,
            },
            library::QueueEntry {
                id: 3,
                position: 2,
                url: link("https://example.com/stale"),
                title: "stale".into(),
                thumbnail: Some("thumb_deadbeef".into()),
                thumbnail_url: Some(link("https://example.com/thumb.jpg")),
                thumbnail_urls: vec![link("https://example.com/thumb.jpg")],
                uploader: None,
                id_hash: "h".into(),
                current: false,
                resolved_source: None,
                resolved_at: None,
                selected_video: None,
                selected_audio: None,
                selected_muxed: None,
            },
            library::QueueEntry {
                id: 4,
                position: 3,
                url: link("https://example.com/nourl"),
                title: "nourl".into(),
                thumbnail: None,
                thumbnail_url: None,
                thumbnail_urls: Vec::new(),
                uploader: None,
                id_hash: "h".into(),
                current: false,
                resolved_source: None,
                resolved_at: None,
                selected_video: None,
                selected_audio: None,
                selected_muxed: None,
            },
        ];

        let due = entries_needing_thumbnails(&entries, &cache);
        let due_urls: Vec<_> = due.iter().map(|item| item.url.as_str()).collect();
        assert_eq!(due_urls, vec!["https://example.com/missing", "https://example.com/stale"]);
    }

    #[test]
    fn thumbnail_refresh_failure_does_not_remove_queue_entry() {
        let queue = Queue::in_memory().expect("an in-memory queue opens");
        let entry = queue
            .push(
                "https://www.eporner.com/video-i4Gl3h2QyRE/",
                "error video",
                None,
                Some("https://thumbnails.example.com/broken.jpg"),
                &[],
                None,
            )
            .expect("an entry is added");

        let before = queue.entries().expect("the entries are read");
        let outcome = ThumbnailRefreshOutcome::RetryLater("404 not found".to_string());
        assert!(matches!(outcome, ThumbnailRefreshOutcome::RetryLater(_)));
        let after = queue.entries().expect("the entries are read");
        assert_eq!(before, after);
        assert!(after.iter().any(|other| other.id == entry.id));
    }

    #[test]
    fn a_replaced_entry_cannot_take_the_slot_from_the_entry_that_replaced_it() {
        let ytdlp =
            Arc::new(YtdlpControl::new(YtDlp::new("yt-dlp", ytdlp_tool::ToolPaths::new("tools"))));
        let epoch_state = AtomicU64::new(2);

        // The entry for generation 1 was replaced, so it never starts and
        // leaves the slot alone.
        assert!(ytdlp.begin_url_entry(&epoch_state, 1).is_none());

        // The entry for generation 2 takes the slot.
        assert!(ytdlp.begin_url_entry(&epoch_state, 2).is_some());
    }

    #[test]
    fn a_superseded_playback_resolve_cannot_take_the_slot() {
        let ytdlp =
            Arc::new(YtdlpControl::new(YtDlp::new("yt-dlp", ytdlp_tool::ToolPaths::new("tools"))));
        let epoch_state = AtomicU64::new(5);

        assert!(ytdlp.begin_playback(&epoch_state, 4).is_none());
        assert!(ytdlp.begin_playback(&epoch_state, 5).is_some());
    }
}

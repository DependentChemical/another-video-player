//! The Slint component library of Flatline.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns the *look* of the player: the window, the URL bar, the
//! control bar and the quality picker. It knows nothing about mpv, `yt-dlp`
//! or the network.
//!
//! Everything the outside world can do is expressed as a callback the host
//! installs, and everything the host wants to show is a setter. That keeps
//! the crate free of any dependency on the other capabilities of Flatline.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

slint::include_modules!();

/// How long a toast stays on screen before it is removed on its own.
const TOAST_TIMEOUT: Duration = Duration::from_secs(10);

/// How many toasts are kept at once; newer ones push out the oldest.
const TOAST_LIMIT: usize = 5;

/// The number of lines a notification shows before the rest is cut off.
///
/// The component that draws the toast elides the text that does not fit,
/// so the user sees four lines and an ellipsis.
const TOAST_MAX_LINES: usize = 4;

/// The characters one line of a notification holds.
///
/// The text of a toast is drawn 334px wide in a 13px font, which fits about
/// this many characters. The budget only keeps the text a toast carries
/// from growing without bound; the number of lines the user sees is the
/// hard limit of the component that draws it.
const TOAST_LINE_CHARS: usize = 64;

/// The mark that says the text was cut off.
const TOAST_ELLIPSIS: char = '\u{2026}';

/// How often expired toasts are swept away.
const TOAST_SWEEP: Duration = Duration::from_millis(500);

/// The vertical distance between the tops of two queue rows.
///
/// It mirrors `row-step` in `QueueDialog.slint` (a 76px row plus the 4px
/// spacing between rows) and is used to scroll the current entry to the top
/// when the queue opens.
const QUEUE_ROW_STEP: f32 = 80.0;

/// Every script fontique knows, so a fallback can be reached from any run.
///
/// Parley labels a run of characters whose script is Common with the run's
/// dominant script. A title made only of mathematical letters and digits has
/// no dominant script, so it is shaped as Latin; a math run inside a title is
/// shaped as whatever script surrounds it. Registering the math font for every
/// script covers both cases.
fn fallback_scripts() -> Vec<slint::fontique_011::fontique::Script> {
    use slint::fontique_011::fontique::ScriptExt;

    slint::fontique_011::fontique::Script::all_samples().iter().map(|(script, _)| *script).collect()
}

/// Makes the embedded math and CJK fonts reachable from the fallback chain.
///
/// Slint shapes a character with the first family that covers it. The font
/// embedded for mathematical alphanumerics is not the requested UI family,
/// and DirectWrite's fallback for the Common script does not cover the
/// Mathematical Alphanumeric Symbols block, so STIX has to be appended for
/// every script explicitly. Noto Sans CJK SC is appended for the CJK scripts
/// for the same reason, and so that the shipped font is used instead of
/// whatever the system happens to have.
fn configure_font_fallbacks() {
    use slint::fontique_011::fontique;

    let mut collection = slint::fontique_011::shared_collection();

    if let Some(cjk) = collection.family_id("Noto Sans CJK SC") {
        for script in [b"Hani", b"Hira", b"Kana"] {
            let key = fontique::FallbackKey::new(fontique::Script::from_bytes(*script), None);
            collection.append_fallbacks(key, std::iter::once(cjk));
        }
    }

    if let Some(stix) = collection.family_id("STIX Two Math") {
        for script in fallback_scripts() {
            let key = fontique::FallbackKey::new(script, None);
            collection.append_fallbacks(key, std::iter::once(stix));
        }
    }
}

/// Replaces the unassigned code point that some sources use for an italic h.
///
/// U+1D455 is a hole in the Mathematical Alphanumeric Symbols block; the
/// character meant by it is the Planck constant, U+210E. Without this the
/// title shows a blank where the h should be.
fn display_title(title: &str) -> Cow<'_, str> {
    if title.contains('\u{1D455}') {
        Cow::Owned(title.replace('\u{1D455}', "\u{210E}"))
    } else {
        Cow::Borrowed(title)
    }
}

/// One toast that is currently on screen and the moment it was shown.
struct ActiveToast {
    item: ToastItem,
    shown_at: Instant,
}

/// The toasts the window is showing.
///
/// The store owns their lifetime: callers only add to it, and the sweep
/// removes a toast once it is older than [`TOAST_TIMEOUT`] or has been
/// pushed past [`TOAST_LIMIT`].
#[derive(Default)]
struct ToastStore {
    toasts: Vec<ActiveToast>,
    next_id: i32,
}

impl ToastStore {
    /// Adds a toast, evicting the oldest when the limit is exceeded.
    fn show(&mut self, text: &str, now: Instant) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let text = truncate_toast(text);
        let text = slint::SharedString::from(&*text);
        self.toasts.push(ActiveToast { item: ToastItem { text, id }, shown_at: now });
        if self.toasts.len() > TOAST_LIMIT {
            let excess = self.toasts.len() - TOAST_LIMIT;
            self.toasts.drain(..excess);
        }
    }

    /// Removes expired toasts and reports whether the set changed.
    fn prune(&mut self, now: Instant) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|toast| now.duration_since(toast.shown_at) < TOAST_TIMEOUT);
        self.toasts.len() != before
    }

    /// Whether no toast is left.
    fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    /// The items the Slint model expects.
    fn items(&self) -> Vec<ToastItem> {
        self.toasts.iter().map(|toast| toast.item.clone()).collect()
    }
}

/// Shortens a notification to the lines a toast shows.
///
/// A line is a piece of the text between two line breaks; a piece that is
/// too long for one line is cut at the last word that fits. The text is cut
/// after the last line that fits and marked with an ellipsis, so the user
/// can see that there was more. A text that fits is left alone.
fn truncate_toast(text: &str) -> Cow<'_, str> {
    let mut lines: Vec<&str> = Vec::new();

    for piece in text.split('\n') {
        if lines.len() == TOAST_MAX_LINES {
            return Cow::Owned(cut_toast(&lines));
        }
        if piece.chars().count() > TOAST_LINE_CHARS {
            lines.push(cut_at_word_boundary(piece));
            return Cow::Owned(cut_toast(&lines));
        }
        lines.push(piece);
    }

    Cow::Borrowed(text)
}

/// Builds the shortened text of a toast from the lines that are kept.
///
/// The ellipsis marks the cut, so the user knows the text was longer.
fn cut_toast(lines: &[&str]) -> String {
    let mut cut = lines.join("\n");
    cut.push(TOAST_ELLIPSIS);
    cut
}

/// Cuts a piece of text at the last word that fits on one line.
fn cut_at_word_boundary(piece: &str) -> &str {
    let cut =
        piece.char_indices().nth(TOAST_LINE_CHARS).map(|(index, _)| index).unwrap_or(piece.len());
    let head = &piece[..cut];
    match head.rfind(char::is_whitespace) {
        Some(index) => head[..index].trim_end(),
        None => head.trim_end(),
    }
}

/// How prominent a status message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// A neutral hint.
    Info,
    /// Something is happening.
    Busy,
    /// Something went wrong.
    Error,
}

impl StatusKind {
    /// The value the user interface expects.
    pub fn as_str(self) -> &'static str {
        match self {
            StatusKind::Info => "info",
            StatusKind::Busy => "busy",
            StatusKind::Error => "error",
        }
    }
}

/// One selectable stream in the source picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOption {
    /// Identifier of the stream, handed back to the host when picked.
    pub id: String,
    /// The prominent label, e.g. `1080p` or `128 kb/s`.
    pub label: String,
    /// The supporting line, e.g. `1920×1080 · 60 fps · mp4`.
    pub detail: String,
}

impl SourceOption {
    /// Creates an option.
    pub fn new(id: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), detail: detail.into() }
    }
}

/// The three kinds of stream a source can offer.
///
/// A source may have video-only streams, audio-only streams and streams that
/// already carry both. The picker shows them in three columns; the host
/// decides what to do with the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceLists {
    /// Streams that carry only a video track.
    pub video: Vec<SourceOption>,
    /// Streams that carry only an audio track.
    pub audio: Vec<SourceOption>,
    /// Streams that carry both tracks in one file.
    pub muxed: Vec<SourceOption>,
}

/// One entry of the active queue shown in the queue dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueListEntry {
    /// The stable database id of the entry, used to remove exactly one row.
    pub id: i32,
    /// The position of the entry in the active queue.
    pub position: i32,
    /// Human readable title of the media.
    pub title: String,
    /// The channel or uploader of the media.
    pub uploader: String,
    /// A cached thumbnail on disk, when one is available.
    pub thumbnail: Option<PathBuf>,
    /// Whether this is the entry that plays now.
    pub current: bool,
    /// The URL of the media, used for copying.
    pub url: String,
}

/// One entry of the playlist list shown in the playlist dialog.
///
/// The built-in active queue is included with [`PlaylistListEntry::is_active_queue`]
/// set, so the interface can list and pick it exactly like a user playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistListEntry {
    /// The stable id handed back when the playlist is picked.
    pub id: String,
    /// The name shown to the user.
    pub name: String,
    /// Whether this is the playlist the player is on.
    pub active: bool,
    /// Whether this is the built-in active queue.
    pub is_active_queue: bool,
}

/// One entry of the playback history shown in the history dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryListEntry {
    /// Human readable title of the media.
    pub title: String,
    /// The channel or uploader of the media, or its URL as a fallback.
    pub uploader: String,
    /// The URL the media can be resolved from again.
    pub url: String,
}

/// The player window.
pub struct PlayerUi {
    window: MainWindow,
    fullscreen: Cell<bool>,
    toasts: Rc<RefCell<ToastStore>>,
    toast_timer: Rc<Timer>,
    // While a queue row is pressed this holds its index; when the long-press
    // timer fires the window's `queue-long-press-index` is set so the queue
    // dialog knows which row is being dragged.
    long_press_pending: Cell<Option<i32>>,
    long_press_timer: Rc<Timer>,
    // Fires every 16ms while a queue row is being dragged; auto-scrolls the
    // queue list when the dragged row is near the top or bottom viewport edge.
    drag_scroll_timer: Rc<Timer>,
}

impl Drop for PlayerUi {
    fn drop(&mut self) {
        self.toast_timer.stop();
        self.long_press_timer.stop();
        self.drag_scroll_timer.stop();
    }
}

/// A handle that asks the toolkit to draw another frame.
///
/// The handle is `Send` and `Sync`, so it can be used from the thread a
/// player runs on.
#[derive(Clone)]
pub struct RedrawHandle {
    weak: slint::Weak<MainWindow>,
}

impl RedrawHandle {
    /// Asks for another frame.
    pub fn request(&self) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.window().request_redraw();
            }
        });
    }
}

impl Default for PlayerUi {
    fn default() -> Self {
        Self::new().expect("the player window could not be created")
    }
}

impl PlayerUi {
    /// Creates the window.
    pub fn new() -> Result<PlayerUi, slint::PlatformError> {
        let window = MainWindow::new()?;
        // The component's fonts are registered by now, so the fallback
        // chain can be extended before the first layout runs.
        configure_font_fallbacks();
        let window_weak = window.as_weak();
        let long_press_pending = Cell::new(None::<i32>);
        let long_press_timer = Rc::new(Timer::default());
        // The long-press timer is (re)armed on every row press in
        // [`on_queue_row_pressed`]; when it fires the window's
        // `queue-long-press-index` is set so the queue dialog can drag.
        let drag_scroll_timer = Rc::new(Timer::default());
        // Auto-scroll while dragging: every 16ms, if a row is being dragged
        // (`queue-long-press-index >= 0`), toggle a tick that makes the queue
        // dialog scroll the list when the pointer is near a viewport edge.
        // The dialog owns the geometry and the acceleration; this timer only
        // provides the clock, so no layout values have to cross the FFI.
        let scroll_window = window_weak.clone();
        drag_scroll_timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            let Some(window) = scroll_window.upgrade() else { return };
            if window.get_queue_long_press_index() < 0 {
                return;
            }
            window.set_queue_drag_scroll_tick(!window.get_queue_drag_scroll_tick());
        });

        Ok(PlayerUi {
            window,
            fullscreen: Cell::new(false),
            toasts: Rc::new(RefCell::new(ToastStore::default())),
            toast_timer: Rc::new(Timer::default()),
            long_press_pending,
            long_press_timer,
            drag_scroll_timer,
        })
    }

    /// The underlying component.
    pub fn window(&self) -> &MainWindow {
        &self.window
    }

    /// Shows the window.
    pub fn show(&self) {
        let _ = self.window.show();
    }

    /// Runs the event loop until the last window is closed.
    pub fn run(&self) {
        let _ = slint::run_event_loop();
    }

    /// Asks the toolkit to draw another frame.
    ///
    /// This is safe to call from any thread.
    pub fn request_redraw(&self) {
        let weak = self.window.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.window().request_redraw();
            }
        });
    }

    /// A handle that asks for another frame, usable from any thread.
    pub fn redraw_handle(&self) -> RedrawHandle {
        RedrawHandle { weak: self.window.as_weak() }
    }

    /// The height of the top bar, in logical pixels.
    pub fn top_bar_height(&self) -> f32 {
        self.window.get_top_bar_height()
    }

    /// Switches between windowed and full screen.
    pub fn toggle_fullscreen(&self) {
        let fullscreen = !self.fullscreen.get();
        self.fullscreen.set(fullscreen);
        self.window.window().set_fullscreen(fullscreen);
        self.window.set_fullscreen(fullscreen);
    }

    // ------------------------------------------------------------- state --

    /// The URL in the address bar.
    pub fn set_url(&self, url: &str) {
        self.window.set_url(url.into());
    }

    /// The URL that is currently in the address bar.
    pub fn get_url(&self) -> String {
        self.window.get_url().to_string()
    }

    /// Removes focus from the URL bar so it shows its placeholder when empty.
    pub fn blur_url(&self) {
        self.window.invoke_blur_url();
    }

    /// The title of the media that is playing.
    pub fn set_media_title(&self, title: &str) {
        self.window.set_media_title(display_title(title).as_ref().into());
    }

    /// The application version to show in the window title.
    pub fn set_app_version(&self, version: &str) {
        self.window.set_app_version(version.into());
    }

    /// The channel or uploader of the media that is playing.
    pub fn set_media_uploader(&self, uploader: &str) {
        self.window.set_media_uploader(uploader.into());
    }

    /// Shows a message.
    pub fn set_status(&self, status: &str, kind: StatusKind) {
        self.window.set_status(status.into());
        self.window.set_status_kind(kind.as_str().into());
    }

    /// Offers the streams of a source to the source picker.
    ///
    /// Replacing the lists also closes the picker and forgets the previous
    /// selection; the host calls [`PlayerUi::set_selected_sources`] right
    /// after this to show what is playing.
    pub fn set_sources(&self, sources: &SourceLists) {
        self.window.set_video_sources(to_model(&sources.video));
        self.window.set_audio_sources(to_model(&sources.audio));
        self.window.set_muxed_sources(to_model(&sources.muxed));
        self.window.set_sources_open(false);
    }

    /// Marks the streams that are currently playing.
    pub fn set_selected_sources(
        &self,
        video: Option<&str>,
        audio: Option<&str>,
        muxed: Option<&str>,
    ) {
        let video = video.unwrap_or_default();
        let audio = audio.unwrap_or_default();
        let muxed = muxed.unwrap_or_default();
        self.window.set_selected_video_id(video.into());
        self.window.set_selected_audio_id(audio.into());
        self.window.set_selected_muxed_id(muxed.into());
    }

    /// Whether a media is loaded.
    pub fn set_has_media(&self, has_media: bool) {
        self.window.set_has_media(has_media);
    }

    /// Whether the media that is playing has no video track.
    pub fn set_audio_only(&self, audio_only: bool) {
        self.window.set_audio_only(audio_only);
    }

    /// The full-size thumbnail to show when the media is audio only.
    pub fn set_video_thumbnail(&self, thumbnail: Option<&Path>) {
        let image =
            thumbnail.and_then(|path| slint::Image::load_from_path(path).ok()).unwrap_or_default();
        self.window.set_video_thumbnail(image);
    }

    /// Whether the window has loaded media.
    pub fn has_media(&self) -> bool {
        self.window.get_has_media()
    }

    /// Whether playback is running.
    pub fn set_playing(&self, playing: bool) {
        self.window.set_playing(playing);
    }

    /// The playback position in seconds.
    pub fn set_position(&self, seconds: f64) {
        self.window.set_position(seconds as f32);
    }

    /// The duration of the media in seconds.
    pub fn set_duration(&self, seconds: f64) {
        self.window.set_duration(seconds as f32);
    }

    /// The spans of the media that are already downloaded.
    ///
    /// Each span is a fraction between zero and one of the shown axis, so the
    /// scrubber can draw them without knowing which time axis it is on.
    pub fn set_buffered_ranges(&self, ranges: &[(f64, f64)]) {
        let ranges: Vec<BufferedRange> = ranges
            .iter()
            .filter_map(|&(start, end)| {
                (end > start).then_some(BufferedRange { start: start as f32, end: end as f32 })
            })
            .collect();
        self.window.set_buffered_ranges(slint::ModelRc::new(slint::VecModel::from(ranges)));
    }

    /// The marked spans on the scrubber, such as SponsorBlock segments.
    ///
    /// Each span is a fraction between zero and one of the shown axis, and the
    /// colour identifies the kind of segment it represents.
    pub fn set_sponsor_segments(&self, segments: &[(f64, f64, [u8; 3])]) {
        let segments: Vec<SponsorSegment> = segments
            .iter()
            .filter_map(|&(start, end, [red, green, blue])| {
                (end > start).then_some(SponsorSegment {
                    start: start as f32,
                    end: end as f32,
                    color: slint::Color::from_rgb_u8(red, green, blue),
                })
            })
            .collect();
        self.window.set_sponsor_segments(slint::ModelRc::new(slint::VecModel::from(segments)));
    }

    /// The volume slider position, from 0 to 100.
    pub fn set_volume(&self, volume: f64) {
        self.window.set_volume(volume as f32);
    }

    /// The playback position, formatted for the user.
    pub fn set_time_text(&self, text: &str) {
        self.window.set_time_text(text.into());
    }

    /// Whether the media that plays is a live stream.
    ///
    /// The live button is only shown while this is true.
    pub fn set_live(&self, live: bool) {
        self.window.set_live(live);
    }

    /// Whether the playback is at the live edge of a live stream.
    pub fn set_at_live_edge(&self, at_edge: bool) {
        self.window.set_at_live_edge(at_edge);
    }

    /// Shows the playback controls while the pointer is active.
    pub fn set_controls_visible(&self, visible: bool) {
        self.window.set_controls_visible(visible);
    }

    /// Advances the loading indicator animation.
    ///
    /// The step selects which of the three dots is lit up.
    pub fn set_spinner_step(&self, step: u32) {
        self.window.set_spinner_step(step as i32);
    }

    /// Whether the source picker is open.
    pub fn sources_open(&self) -> bool {
        self.window.get_sources_open()
    }

    /// Whether the settings menu is open.
    pub fn settings_open(&self) -> bool {
        self.window.get_settings_open()
    }

    /// Whether a popup above the control bar is open.
    pub fn controls_popup_open(&self) -> bool {
        self.sources_open() || self.settings_open()
    }

    /// Closes the settings menu.
    pub fn close_settings(&self) {
        self.window.set_settings_open(false);
        self.window.set_settings_speed_mode(false);
    }

    /// The playback speed shown in the settings menu.
    pub fn set_playback_speed(&self, speed: f64) {
        self.window.set_playback_speed(speed as f32);
    }

    /// The selected source quality shown in the settings menu.
    pub fn set_source_quality_label(&self, label: &str) {
        self.window.set_source_quality_label(label.into());
    }

    /// Shows a short note in the corner of the window.
    ///
    /// This is fire-and-forget: the caller never has to remove it. The toast
    /// disappears on its own after ten seconds, and when more than five are
    /// shown at once the oldest are dropped to make room.
    pub fn show_toast(&self, text: &str) {
        let items = {
            let mut toasts = self.toasts.borrow_mut();
            toasts.show(text, Instant::now());
            toasts.items()
        };
        self.window.set_toasts(slint::ModelRc::new(slint::VecModel::from(items)));
        self.sweep_toasts();
    }

    /// Puts text on the system clipboard.
    ///
    /// Returns whether the platform accepted the text, so the caller can tell
    /// the user that copying failed instead of pretending it worked.
    pub fn copy_to_clipboard(&self, text: &str) -> bool {
        arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)).is_ok()
    }

    /// Makes sure the sweep timer runs while any toast is on screen.
    fn sweep_toasts(&self) {
        if self.toast_timer.running() {
            return;
        }
        let toasts = Rc::clone(&self.toasts);
        let weak_window = self.window.as_weak();
        let weak_timer = Rc::downgrade(&self.toast_timer);
        self.toast_timer.start(TimerMode::Repeated, TOAST_SWEEP, move || {
            let items = {
                let mut toasts = toasts.borrow_mut();
                if !toasts.prune(Instant::now()) {
                    return;
                }
                if toasts.is_empty()
                    && let Some(timer) = weak_timer.upgrade()
                {
                    timer.stop();
                }
                toasts.items()
            };
            if let Some(window) = weak_window.upgrade() {
                window.set_toasts(slint::ModelRc::new(slint::VecModel::from(items)));
            }
        });
    }

    /// Opens the dialog that asks what to do with a resolved media.
    pub fn show_pending(
        &self,
        title: &str,
        thumbnail: Option<&Path>,
        in_playlist: bool,
        playlists: &[PlaylistListEntry],
    ) {
        self.set_playlists(playlists);
        self.window.set_pending_title(display_title(title).as_ref().into());
        self.window.set_pending_in_playlist(in_playlist);
        self.window.set_pending_is_playlist(false);
        self.window.set_pending_has_selected_video(false);
        self.window.set_pending_playlist_count(0);
        let image =
            thumbnail.and_then(|path| slint::Image::load_from_path(path).ok()).unwrap_or_default();
        self.window.set_pending_thumbnail(image);
        self.window.set_pending_open(true);
    }

    /// Opens the dialog that asks what to do with a resolved playlist.
    pub fn show_pending_playlist(
        &self,
        title: &str,
        thumbnail: Option<&Path>,
        in_playlist: bool,
        has_selected_video: bool,
        count: usize,
        playlists: &[PlaylistListEntry],
    ) {
        self.set_playlists(playlists);
        self.window.set_pending_title(display_title(title).as_ref().into());
        self.window.set_pending_in_playlist(in_playlist);
        self.window.set_pending_is_playlist(true);
        self.window.set_pending_has_selected_video(has_selected_video);
        self.window.set_pending_playlist_count(count as i32);
        let image =
            thumbnail.and_then(|path| slint::Image::load_from_path(path).ok()).unwrap_or_default();
        self.window.set_pending_thumbnail(image);
        self.window.set_pending_open(true);
    }

    /// Offers the playlists to the pending dialog and every playlist picker.
    pub fn set_playlists(&self, playlists: &[PlaylistListEntry]) {
        let items: Vec<PlaylistItem> = playlists
            .iter()
            .map(|playlist| PlaylistItem {
                id: playlist.id.as_str().into(),
                name: display_title(&playlist.name).as_ref().into(),
                active: playlist.active,
                is_active_queue: playlist.is_active_queue,
            })
            .collect();
        self.window.set_playlist_items(slint::ModelRc::new(slint::VecModel::from(items)));
    }

    /// Opens the list of playlists.
    pub fn show_playlists(&self, playlists: &[PlaylistListEntry]) {
        self.set_playlists(playlists);
        self.window.set_create_playlist_open(false);
        self.window.set_create_playlist_duplicate("".into());
        self.window.set_playlists_open(true);
    }

    /// Closes the list of playlists.
    pub fn close_playlists(&self) {
        self.window.set_playlists_open(false);
        self.window.set_create_playlist_open(false);
        self.window.set_create_playlist_duplicate("".into());
    }

    /// Opens the dialog that asks for a new playlist name.
    pub fn show_create_playlist(&self) {
        self.window.set_create_playlist_duplicate("".into());
        self.window.set_create_playlist_open(true);
    }

    /// Closes the dialog that asks for a new playlist name.
    pub fn close_create_playlist(&self) {
        self.window.set_create_playlist_open(false);
        self.window.set_create_playlist_duplicate("".into());
    }

    /// Keeps the creation dialog open and asks whether a duplicate name is
    /// really wanted.
    pub fn show_duplicate_playlist(&self, name: &str) {
        self.window.set_create_playlist_open(true);
        self.window.set_create_playlist_duplicate(name.into());
    }

    /// Closes the pending dialog.
    pub fn hide_pending(&self) {
        self.window.set_pending_open(false);
    }

    /// Shows the active playlist in a list over the video.
    ///
    /// `title` is "Queue" for the built-in active queue and "Queue [name]" for
    /// a user playlist. Only thumbnails that exist on disk are decoded; a
    /// missing file simply leaves the placeholder in the row.
    pub fn show_queue(
        &self,
        entries: &[QueueListEntry],
        title: &str,
        playlists: &[PlaylistListEntry],
    ) {
        self.set_playlists(playlists);
        self.window.set_queue_title(title.into());
        self.window.set_queue_in_playlist(
            playlists.iter().any(|playlist| playlist.active && !playlist.is_active_queue),
        );
        let already_open = self.window.get_queue_open();
        let items: Vec<QueueItem> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| QueueItem {
                id: entry.id,
                index: index as i32,
                position: entry.position,
                title: display_title(&entry.title).as_ref().into(),
                uploader: entry.uploader.as_str().into(),
                thumbnail: entry
                    .thumbnail
                    .as_deref()
                    .and_then(|path| slint::Image::load_from_path(path).ok())
                    .unwrap_or_default(),
                current: entry.current,
                url: entry.url.as_str().into(),
            })
            .collect();
        self.window.set_queue_items(slint::ModelRc::new(slint::VecModel::from(items)));
        let current_id = entries.iter().find(|entry| entry.current).map_or(-1, |entry| entry.id);
        let current_index =
            entries.iter().position(|entry| entry.current).map_or(-1, |index| index as i32);
        self.window.set_queue_current_id(current_id);
        self.window.set_queue_current_index(current_index);
        // Scroll the current entry into view the first time the queue opens,
        // but keep the scroll position on the refreshes that follow a drag.
        if !already_open {
            self.window.set_queue_long_press_index(-1);
            self.window.set_queue_drag_pointer_y(0.0);
            if current_index >= 0 {
                self.window.set_queue_content_y(-(current_index as f32 * QUEUE_ROW_STEP));
            }
        }
        self.window.set_queue_open(true);
    }

    /// Shows the playback history in a list over the video.
    pub fn show_history(&self, entries: &[HistoryListEntry]) {
        let items: Vec<HistoryItem> = entries
            .iter()
            .map(|entry| HistoryItem {
                title: display_title(&entry.title).as_ref().into(),
                uploader: entry.uploader.as_str().into(),
                url: entry.url.as_str().into(),
            })
            .collect();
        self.window.set_history_items(slint::ModelRc::new(slint::VecModel::from(items)));
        self.window.set_history_open(true);
    }

    /// Shows the release notes and available application version.
    pub fn show_app_update(&self, version: &str, notes: &str) {
        self.show_update("Flatline update available", version, notes, "Yes, download and install");
    }

    /// Shows the release notes and available yt-dlp version.
    pub fn show_tool_update(&self, version: &str, notes: &str) {
        self.show_update("yt-dlp update available", version, notes, "Yes, download update");
    }

    /// Shows an update dialog with a custom title and action label.
    pub fn show_update(&self, title: &str, version: &str, notes: &str, action_label: &str) {
        self.window.set_update_title(title.into());
        self.window.set_update_version(version.into());
        self.window.set_update_notes(notes.into());
        self.window.set_update_action_label(action_label.into());
        self.window.set_update_installing(false);
        self.window.set_update_status_text("Downloading update…".into());
        self.window.set_update_error("".into());
        self.window.set_update_download_progress(0.0);
        self.window.set_update_open(true);
    }

    /// Shows an update preparation error in the open update dialog.
    pub fn show_update_error(&self, message: &str) {
        self.window.set_update_installing(false);
        self.window.set_update_error(message.into());
    }

    /// Shows that the package is being downloaded and installed.
    pub fn set_update_installing(&self, installing: bool) {
        self.window.set_update_installing(installing);
    }

    /// Sets the status text shown while an update is being applied.
    pub fn set_update_status_text(&self, text: &str) {
        self.window.set_update_status_text(text.into());
    }

    /// Sets the download progress of the update package, from zero to one.
    pub fn set_update_download_progress(&self, progress: f64) {
        self.window.set_update_download_progress(progress as f32);
    }

    /// Closes the update dialog.
    pub fn close_update(&self) {
        self.window.set_update_open(false);
        self.window.set_update_installing(false);
        self.window.set_update_error("".into());
        self.window.set_update_download_progress(0.0);
    }

    /// Shows a simple URL processing error modal.
    pub fn show_url_error(&self, message: &str) {
        self.window.set_error_title("URL could not be processed".into());
        self.window.set_error_message(message.into());
        self.window.set_error_open(true);
    }

    /// Closes the URL processing error modal.
    pub fn close_url_error(&self) {
        self.window.set_error_open(false);
        self.window.set_error_message("".into());
    }

    // ------------------------------------------------------------ events --

    /// Called when the user asks for the URL to be loaded.
    pub fn on_load(&self, handler: impl Fn(String) + 'static) {
        let weak = self.window.as_weak();
        self.window.on_load_requested(move || {
            if let Some(window) = weak.upgrade() {
                handler(window.get_url().to_string());
            }
        });
    }

    /// Called when the user clicks outside the URL bar.
    ///
    /// The URL bar uses this to stop looking active when it is empty.
    pub fn on_outside_clicked(&self, handler: impl Fn() + 'static) {
        self.window.on_outside_clicked(handler);
    }

    /// Called when the user applies a source selection.
    ///
    /// The arguments are the chosen video-only stream, audio-only stream and
    /// combined stream. An empty value means "nothing selected" in that
    /// column.
    pub fn on_sources_applied(
        &self,
        handler: impl Fn(Option<String>, Option<String>, Option<String>) + 'static,
    ) {
        self.window.on_sources_applied(move |video, audio, muxed| {
            handler(non_empty(video), non_empty(audio), non_empty(muxed));
        });
    }

    /// Called when the user presses play or pause.
    pub fn on_play_toggled(&self, handler: impl Fn() + 'static) {
        self.window.on_play_toggled(handler);
    }

    /// Called when the user scrubs through the media.
    pub fn on_seek(&self, handler: impl Fn(f64) + 'static) {
        self.window.on_seek_requested(move |seconds| handler(seconds as f64));
    }

    /// Called when the user changes the volume.
    pub fn on_volume(&self, handler: impl Fn(f64) + 'static) {
        self.window.on_volume_requested(move |volume| handler(volume as f64));
    }

    /// Called when the user toggles mute from the volume icon.
    pub fn on_volume_toggled(&self, handler: impl Fn() + 'static) {
        self.window.on_volume_toggled(handler);
    }

    /// Called when the user toggles full screen.
    pub fn on_fullscreen_toggled(&self, handler: impl Fn() + 'static) {
        self.window.on_fullscreen_toggled(handler);
    }

    /// Called when the user presses the previous button.
    pub fn on_previous_requested(&self, handler: impl Fn() + 'static) {
        self.window.on_previous_requested(handler);
    }

    /// Called when the user presses the next button.
    pub fn on_next_requested(&self, handler: impl Fn() + 'static) {
        self.window.on_next_requested(handler);
    }

    /// Called when the user asks to go back to the live edge.
    pub fn on_live_pressed(&self, handler: impl Fn() + 'static) {
        self.window.on_live_requested(handler);
    }

    /// Called when the pointer moves over the video area.
    pub fn on_pointer_activity(&self, handler: impl Fn() + 'static) {
        self.window.on_pointer_activity(handler);
    }

    /// Called when the pointer enters or leaves the control bar.
    pub fn on_controls_hovered(&self, handler: impl Fn(bool) + 'static) {
        self.window.on_controls_hovered(handler);
    }

    /// Called when an interaction with the control bar starts or ends.
    pub fn on_interacting_changed(&self, handler: impl Fn(bool) + 'static) {
        self.window.on_interacting_changed(handler);
    }

    /// Called when the user changes the playback speed.
    pub fn on_playback_speed_changed(&self, handler: impl Fn(f64) + 'static) {
        self.window.on_playback_speed_changed(move |speed| handler(speed as f64));
    }

    /// Called when the user asks to play the pending media right away.
    pub fn on_pending_play(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_play_requested(handler);
    }

    /// Called when the user asks to append the pending media to the queue.
    pub fn on_pending_queue(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_queue_requested(handler);
    }

    /// Called when the user asks to play the pending media next in the queue.
    pub fn on_pending_queue_next(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_queue_next_requested(handler);
    }

    /// Called when the user asks to play the selected video from a playlist.
    pub fn on_pending_play_selected(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_play_selected_requested(handler);
    }

    /// Called when the user dismisses the pending dialog.
    pub fn on_pending_dismissed(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_dismissed(handler);
    }

    /// Called when the user adds the pending media to another playlist.
    pub fn on_pending_add_to_playlist(&self, handler: impl Fn(String) + 'static) {
        self.window.on_pending_add_to_playlist(move |playlist| handler(playlist.to_string()));
    }

    /// Called when the user adds the pending media to a playlist they create.
    pub fn on_pending_add_to_new_playlist(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_add_to_new_playlist(handler);
    }

    /// Called when the user asks to play a pending playlist.
    pub fn on_pending_play_playlist(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_play_playlist_requested(handler);
    }

    /// Called when the user asks to create a new playlist and play it.
    pub fn on_pending_play_playlist_new(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_play_playlist_new_requested(handler);
    }

    /// Called when the user appends a pending playlist to an existing playlist.
    pub fn on_pending_append_playlist(&self, handler: impl Fn(String) + 'static) {
        self.window.on_pending_append_playlist(move |playlist| handler(playlist.to_string()));
    }

    /// Called when the user appends a pending playlist to a new playlist.
    pub fn on_pending_append_playlist_new(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_append_playlist_new(handler);
    }

    /// Called when the user accepts the update.
    pub fn on_update_accepted(&self, handler: impl Fn() + 'static) {
        self.window.on_update_accepted(handler);
    }

    /// Called when the user declines the update.
    pub fn on_update_dismissed(&self, handler: impl Fn() + 'static) {
        self.window.on_update_dismissed(handler);
    }

    /// Called when the URL error modal is dismissed.
    pub fn on_error_dismissed(&self, handler: impl Fn() + 'static) {
        self.window.on_error_dismissed(handler);
    }

    /// Called when the user asks for the active queue.
    pub fn on_queue_requested(&self, handler: impl Fn() + 'static) {
        self.window.on_queue_requested(handler);
    }

    /// Called when the user asks for the playback history.
    pub fn on_history_requested(&self, handler: impl Fn() + 'static) {
        self.window.on_history_requested(handler);
    }

    /// Called when the user asks for the playlist list.
    pub fn on_playlists_requested(&self, handler: impl Fn() + 'static) {
        self.window.on_playlists_requested(handler);
    }

    /// Called when the user picks a playlist to play from.
    ///
    /// The argument is the stable playlist id; the built-in active queue uses
    /// a reserved id.
    pub fn on_playlist_selected(&self, handler: impl Fn(String) + 'static) {
        self.window.on_playlist_selected(move |id| handler(id.to_string()));
    }

    /// Called when the user confirms the name of a new playlist.
    pub fn on_create_playlist(&self, handler: impl Fn(String) + 'static) {
        self.window.on_create_playlist(move |name| handler(name.to_string()));
    }

    /// Called when the user confirms a duplicate playlist name.
    pub fn on_confirm_create_playlist(&self, handler: impl Fn(String) + 'static) {
        self.window.on_confirm_create_playlist(move |name| handler(name.to_string()));
    }

    /// Called when the user closes the new playlist dialog without creating.
    pub fn on_create_playlist_dismissed(&self, handler: impl Fn() + 'static) {
        self.window.on_create_playlist_dismissed(handler);
    }

    /// Called when the user adds a queue entry to another playlist.
    ///
    /// The arguments are the entry id and the target playlist id.
    pub fn on_queue_add_to_playlist(&self, handler: impl Fn(i32, String) + 'static) {
        self.window
            .on_queue_add_to_playlist(move |entry, playlist| handler(entry, playlist.to_string()));
    }

    /// Called when the user moves a queue entry to another playlist.
    ///
    /// The arguments are the entry id and the target playlist id.
    pub fn on_queue_move_to_playlist(&self, handler: impl Fn(i32, String) + 'static) {
        self.window
            .on_queue_move_to_playlist(move |entry, playlist| handler(entry, playlist.to_string()));
    }

    /// Called when the user adds a queue entry to a playlist they create now.
    pub fn on_queue_add_to_new_playlist(&self, handler: impl Fn(i32) + 'static) {
        self.window.on_queue_add_to_new_playlist(handler);
    }

    /// Called when the user moves a queue entry to a playlist they create now.
    pub fn on_queue_move_to_new_playlist(&self, handler: impl Fn(i32) + 'static) {
        self.window.on_queue_move_to_new_playlist(handler);
    }

    /// Called when the user picks an entry of the active queue.
    ///
    /// The argument is the position of the entry in the queue.
    pub fn on_queue_item_selected(&self, handler: impl Fn(i32) + 'static) {
        self.window.on_queue_item_selected(handler);
    }

    /// Called when the user asks to copy the URL of a queue entry.
    pub fn on_queue_copy_url(&self, handler: impl Fn(String) + 'static) {
        self.window.on_queue_copy_url(move |url| handler(url.to_string()));
    }

    /// Called when the user asks to refresh the sources of a queue entry.
    pub fn on_queue_refresh_sources(&self, handler: impl Fn(i32) + 'static) {
        self.window.on_queue_refresh_sources(handler);
    }

    /// Called when the user asks to remove an entry from the active queue.
    ///
    /// The argument is the stable id of the entry, so exactly one row is
    /// removed even if positions ever collide.
    pub fn on_queue_remove_entry(&self, handler: impl Fn(i32) + 'static) {
        self.window.on_queue_remove_entry(handler);
    }

    /// Called when a queue row is pressed.
    ///
    /// PlayerUi uses this to arm a 750ms long-press timer; the window sets
    /// `queue-long-press-index` when it fires so the queue dialog can drag.
    pub fn on_queue_row_pressed(&self, handler: impl Fn(i32) + 'static) {
        let pending = self.long_press_pending.clone();
        let timer = Rc::clone(&self.long_press_timer);
        let window_weak = self.window.as_weak();
        self.window.on_queue_row_pressed(move |index| {
            pending.set(Some(index));
            if let Some(window) = window_weak.upgrade() {
                window.set_queue_long_press_index(-1);
            }
            let pending = pending.clone();
            let window_weak = window_weak.clone();
            timer.start(TimerMode::SingleShot, Duration::from_millis(750), move || {
                let Some(index) = pending.get() else { return };
                let Some(window) = window_weak.upgrade() else { return };
                window.set_queue_long_press_index(index);
            });
            handler(index);
        });
    }

    /// Called when a queue row is released.
    ///
    /// Disarms the long-press timer for the released row.
    pub fn on_queue_row_released(&self, handler: impl Fn(i32) + 'static) {
        let pending = self.long_press_pending.clone();
        let timer = Rc::clone(&self.long_press_timer);
        let window_weak = self.window.as_weak();
        self.window.on_queue_row_released(move |index| {
            if pending.get() == Some(index) {
                timer.stop();
                pending.set(None);
            }
            // The `.slint` code has already read `queue-long-press-index` to
            // decide between a drop and a play, so the drag is over by now.
            if let Some(window) = window_weak.upgrade() {
                window.set_queue_long_press_index(-1);
                window.set_queue_drag_pointer_y(0.0);
            }
            handler(index);
        });
    }

    /// Called when the user reorders the active queue by dragging items.
    ///
    /// The arguments are the source index and the target index of the move.
    pub fn on_queue_move_requested(&self, handler: impl Fn(i32, i32) + 'static) {
        self.window.on_queue_move_requested(handler);
    }

    /// Called when the user picks a history entry.
    ///
    /// The argument is the URL of the entry, ready to be resolved again.
    pub fn on_history_item_selected(&self, handler: impl Fn(String) + 'static) {
        self.window.on_history_item_selected(move |url| handler(url.to_string()));
    }
}

/// Converts host options into the model Slint understands.
fn to_model(options: &[SourceOption]) -> slint::ModelRc<StreamItem> {
    let items: Vec<StreamItem> = options
        .iter()
        .map(|option| StreamItem {
            id: option.id.as_str().into(),
            label: option.label.as_str().into(),
            detail: option.detail.as_str().into(),
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(items))
}

/// Turns the empty string of an unselected column into `None`.
fn non_empty(value: slint::SharedString) -> Option<String> {
    let value = value.to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use slint::Model;

    use super::*;

    #[test]
    fn test_media_metadata_setters() {
        let ui = PlayerUi::new().expect("UI window should initialize");
        ui.set_media_title("Test Video Title");
        ui.set_media_uploader("Test Channel");
        assert_eq!(ui.window().get_media_title(), "Test Video Title");
        assert_eq!(ui.window().get_media_uploader(), "Test Channel");

        ui.show_queue(
            &[QueueListEntry {
                id: 1,
                position: 0,
                title: "First".to_string(),
                uploader: "Uploader".to_string(),
                thumbnail: None,
                current: true,
                url: "https://example.com/video".to_string(),
            }],
            "Queue",
            &[],
        );
        assert!(ui.window().get_queue_open());
        assert_eq!(ui.window().get_queue_items().row_count(), 1);
        assert_eq!(ui.window().get_queue_items().row_data(0).expect("the row exists").id, 1);

        ui.show_history(&[HistoryListEntry {
            title: "First".to_string(),
            uploader: "Uploader".to_string(),
            url: "https://example.com/video".to_string(),
        }]);
        assert!(ui.window().get_history_open());
        assert_eq!(ui.window().get_history_items().row_count(), 1);

        // The host clears the URL entry once it has been processed.
        ui.set_url("https://example.com/video");
        assert_eq!(ui.window().get_url(), "https://example.com/video");
        ui.set_url("");
        assert_eq!(ui.window().get_url(), "");

        ui.show_toast("hello");
        let toasts = ui.window().get_toasts();
        assert_eq!(toasts.row_count(), 1);
        assert_eq!(toasts.row_data(0).expect("one toast").text, "hello");

        let playlists = [
            PlaylistListEntry {
                id: "active_queue".to_string(),
                name: "Active Queue".to_string(),
                active: false,
                is_active_queue: true,
            },
            PlaylistListEntry {
                id: "p1".to_string(),
                name: "Mix".to_string(),
                active: true,
                is_active_queue: false,
            },
        ];

        ui.show_playlists(&playlists);
        assert!(ui.window().get_playlists_open());
        assert_eq!(ui.window().get_playlist_items().row_count(), 2);
        assert!(ui.window().get_playlist_items().row_data(1).expect("the row exists").active);

        ui.show_queue(&[], "Queue [Mix]", &playlists);
        assert_eq!(ui.window().get_queue_title(), "Queue [Mix]");
        assert!(ui.window().get_queue_in_playlist());
        assert_eq!(ui.window().get_playlist_items().row_count(), 2);

        ui.show_duplicate_playlist("Mix");
        assert!(ui.window().get_create_playlist_open());
        assert_eq!(ui.window().get_create_playlist_duplicate(), "Mix");

        ui.close_playlists();
        assert!(!ui.window().get_playlists_open());
        assert!(!ui.window().get_create_playlist_open());
        assert_eq!(ui.window().get_create_playlist_duplicate(), "");
    }

    #[test]
    fn a_toast_disappears_after_its_timeout() {
        let mut store = ToastStore::default();
        let shown_at = Instant::now();
        store.show("hello", shown_at);

        assert!(!store.prune(shown_at + TOAST_TIMEOUT - Duration::from_millis(1)));
        assert_eq!(store.toasts.len(), 1);
        assert!(store.prune(shown_at + TOAST_TIMEOUT));
        assert!(store.is_empty());
    }

    #[test]
    fn older_toasts_are_dropped_beyond_the_limit() {
        let mut store = ToastStore::default();
        let now = Instant::now();
        for index in 0..(TOAST_LIMIT + 2) {
            store.show(&format!("toast {index}"), now);
        }

        assert_eq!(store.toasts.len(), TOAST_LIMIT);
        assert_eq!(store.toasts[0].item.text, "toast 2");
        assert_eq!(store.toasts[TOAST_LIMIT - 1].item.text, format!("toast {}", TOAST_LIMIT + 1));
    }

    #[test]
    fn the_unassigned_italic_h_is_shown_as_a_planck_constant() {
        assert_eq!(display_title("a\u{1D455}b"), "a\u{210E}b");
        assert_eq!(display_title("plain"), "plain");
    }

    #[test]
    fn a_short_notification_is_left_alone() {
        assert_eq!(truncate_toast("hello"), "hello");
        assert_eq!(truncate_toast("one\ntwo"), "one\ntwo");
    }

    #[test]
    fn a_notification_keeps_at_most_four_lines() {
        let text = "one\ntwo\nthree\nfour\nfive";

        let cut = truncate_toast(text);

        assert_eq!(cut, "one\ntwo\nthree\nfour\u{2026}");
        assert_eq!(cut.lines().count(), TOAST_MAX_LINES);
    }

    #[test]
    fn a_long_notification_is_cut_at_a_word_and_marked() {
        let text = "word ".repeat(40);

        let cut = truncate_toast(&text);

        assert!(cut.ends_with('\u{2026}'), "{cut}");
        assert!(cut.chars().count() <= TOAST_LINE_CHARS + 1, "{cut}");
        assert!(!cut.contains("  "), "{cut}");
    }

    #[test]
    fn a_notification_without_a_word_boundary_is_cut_all_the_same() {
        let text = "x".repeat(200);

        let cut = truncate_toast(&text);

        assert_eq!(cut.chars().count(), TOAST_LINE_CHARS + 1);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn a_toast_holds_the_cut_text() {
        let mut store = ToastStore::default();
        let now = Instant::now();
        store.show("one\ntwo\nthree\nfour\nfive", now);

        assert_eq!(store.toasts[0].item.text, "one\ntwo\nthree\nfour\u{2026}");
    }

    #[test]
    fn the_math_font_can_be_reached_from_latin_and_cjk_runs() {
        let scripts = fallback_scripts();
        for tag in [b"Latn", b"Hani", b"Hira", b"Kana"] {
            let script = slint::fontique_011::fontique::Script::from_bytes(*tag);
            assert!(scripts.contains(&script), "script {tag:?} has no math fallback");
        }
    }
}

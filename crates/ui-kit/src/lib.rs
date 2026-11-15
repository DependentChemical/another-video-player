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

use std::path::{Path, PathBuf};

use slint::{ComponentHandle, Model};

slint::include_modules!();

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
    fullscreen: std::cell::Cell<bool>,
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
        Ok(PlayerUi { window: MainWindow::new()?, fullscreen: std::cell::Cell::new(false) })
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

    /// The title of the media that is playing.
    pub fn set_media_title(&self, title: &str) {
        self.window.set_media_title(title.into());
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

    /// Shows a short note in the corner of the window.
    ///
    /// Toasts are stacked (max 5); older ones are removed.
    pub fn show_toast(&self, text: &str) {
        let mut toasts: Vec<ToastItem> = self.window.get_toasts().iter().collect();
        let id = toasts.len() as i32;
        toasts.push(ToastItem { text: text.into(), id });
        // Keep only the last 5 toasts
        if toasts.len() > 5 {
            toasts = toasts.split_off(toasts.len() - 5);
        }
        self.window.set_toasts(slint::ModelRc::new(slint::VecModel::from(toasts)));
    }

    /// Hides all toasts.
    pub fn hide_toast(&self) {
        self.window.set_toasts(slint::ModelRc::new(slint::VecModel::from(Vec::<ToastItem>::new())));
    }

    /// Opens the dialog that asks what to do with a resolved media.
    ///
    /// The thumbnail is read from disk; when it is missing or cannot be
    /// decoded the dialog simply shows no picture.
    pub fn show_pending(&self, title: &str, thumbnail: Option<&Path>) {
        self.window.set_pending_title(title.into());
        let image =
            thumbnail.and_then(|path| slint::Image::load_from_path(path).ok()).unwrap_or_default();
        self.window.set_pending_thumbnail(image);
        self.window.set_pending_open(true);
    }

    /// Closes the pending dialog.
    pub fn hide_pending(&self) {
        self.window.set_pending_open(false);
    }

    /// Shows the active queue in a list over the video.
    ///
    /// Only thumbnails that exist on disk are decoded; a missing file simply
    /// leaves the placeholder in the row.
    pub fn show_queue(&self, entries: &[QueueListEntry]) {
        let items: Vec<QueueItem> = entries
            .iter()
            .map(|entry| QueueItem {
                position: entry.position,
                title: entry.title.as_str().into(),
                uploader: entry.uploader.as_str().into(),
                thumbnail: entry
                    .thumbnail
                    .as_deref()
                    .and_then(|path| slint::Image::load_from_path(path).ok())
                    .unwrap_or_default(),
                current: entry.current,
            })
            .collect();
        self.window.set_queue_items(slint::ModelRc::new(slint::VecModel::from(items)));
        self.window.set_queue_open(true);
    }

    /// Shows the playback history in a list over the video.
    pub fn show_history(&self, entries: &[HistoryListEntry]) {
        let items: Vec<HistoryItem> = entries
            .iter()
            .map(|entry| HistoryItem {
                title: entry.title.as_str().into(),
                uploader: entry.uploader.as_str().into(),
                url: entry.url.as_str().into(),
            })
            .collect();
        self.window.set_history_items(slint::ModelRc::new(slint::VecModel::from(items)));
        self.window.set_history_open(true);
    }

    /// Shows the release notes and available version.
    pub fn show_update(&self, version: &str, notes: &str) {
        self.window.set_update_version(version.into());
        self.window.set_update_notes(notes.into());
        self.window.set_update_installing(false);
        self.window.set_update_error("".into());
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

    /// Called when the user asks to play the pending media right away.
    pub fn on_pending_play(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_play_requested(handler);
    }

    /// Called when the user asks to append the pending media to the queue.
    pub fn on_pending_queue(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_queue_requested(handler);
    }

    /// Called when the user dismisses the pending dialog.
    pub fn on_pending_dismissed(&self, handler: impl Fn() + 'static) {
        self.window.on_pending_dismissed(handler);
    }

    /// Called when the user accepts the update.
    pub fn on_update_accepted(&self, handler: impl Fn() + 'static) {
        self.window.on_update_accepted(handler);
    }

    /// Called when the user declines the update.
    pub fn on_update_dismissed(&self, handler: impl Fn() + 'static) {
        self.window.on_update_dismissed(handler);
    }

    /// Called when the user asks for the active queue.
    pub fn on_queue_requested(&self, handler: impl Fn() + 'static) {
        self.window.on_queue_requested(handler);
    }

    /// Called when the user asks for the playback history.
    pub fn on_history_requested(&self, handler: impl Fn() + 'static) {
        self.window.on_history_requested(handler);
    }

    /// Called when the user picks an entry of the active queue.
    ///
    /// The argument is the position of the entry in the queue.
    pub fn on_queue_item_selected(&self, handler: impl Fn(i32) + 'static) {
        self.window.on_queue_item_selected(handler);
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
    use super::*;

    #[test]
    fn test_media_metadata_setters() {
        let ui = PlayerUi::new().expect("UI window should initialize");
        ui.set_media_title("Test Video Title");
        ui.set_media_uploader("Test Channel");
        assert_eq!(ui.window().get_media_title(), "Test Video Title");
        assert_eq!(ui.window().get_media_uploader(), "Test Channel");

        ui.show_queue(&[QueueListEntry {
            position: 0,
            title: "First".to_string(),
            uploader: "Uploader".to_string(),
            thumbnail: None,
            current: true,
        }]);
        assert!(ui.window().get_queue_open());
        assert_eq!(ui.window().get_queue_items().row_count(), 1);

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
    }
}

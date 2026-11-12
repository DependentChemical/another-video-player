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

use slint::ComponentHandle;

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

/// A quality the user can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityOption {
    /// Identifier of the stream, handed back to the host when picked.
    pub id: String,
    /// The label shown to the user.
    pub label: String,
}

impl QualityOption {
    /// Creates an option.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into() }
    }
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

    /// Shows a message.
    pub fn set_status(&self, status: &str, kind: StatusKind) {
        self.window.set_status(status.into());
        self.window.set_status_kind(kind.as_str().into());
    }

    /// Offers a list of qualities to the user.
    pub fn set_quality_options(&self, options: &[QualityOption]) {
        let labels: Vec<slint::SharedString> =
            options.iter().map(|option| option.label.as_str().into()).collect();
        self.window.set_quality_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
        if options.is_empty() {
            self.window.set_quality_index(-1);
        }
    }

    /// Marks one of the offered qualities as the selected one.
    pub fn set_selected_quality(&self, index: i32) {
        self.window.set_quality_index(index);
    }

    /// Whether a media is loaded.
    pub fn set_has_media(&self, has_media: bool) {
        self.window.set_has_media(has_media);
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

    /// The volume, in the range mpv uses.
    pub fn set_volume(&self, volume: f64) {
        self.window.set_volume(volume as f32);
    }

    /// The playback position, formatted for the user.
    pub fn set_time_text(&self, text: &str) {
        self.window.set_time_text(text.into());
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

    /// Called when the user picks a quality.
    pub fn on_quality_selected(&self, handler: impl Fn(i32) + 'static) {
        self.window.on_quality_selected(handler);
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

    /// Called when the user toggles full screen.
    pub fn on_fullscreen_toggled(&self, handler: impl Fn() + 'static) {
        self.window.on_fullscreen_toggled(handler);
    }
}

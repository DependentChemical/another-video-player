//! The window and the wiring between the interface and the application.
//!
//! This is the only place where the capabilities of Flatline meet: the
//! interface, the player, the resolver and the tool manager are each used
//! through their own narrow interface, and nothing here knows how any of
//! them works inside.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use playback_engine::{EndReason, EngineEvent};
#[cfg(target_os = "windows")]
use slint::winit_030::winit::platform::windows::WindowExtWindows;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use slint::{ComponentHandle, Timer, TimerMode};
use ui_kit::{PlayerUi, StatusKind};

use crate::app::{App, Message};
use crate::loading::{self, Loading};

/// How often the interface is refreshed.
const TICK: Duration = Duration::from_millis(100);

/// The window of the player, together with everything it is wired to.
pub struct Shell {
    ui: Rc<PlayerUi>,
    app: Arc<App>,
    loading: Rc<RefCell<Loading>>,
    last_pointer_activity: Rc<Cell<Instant>>,
    /// The timer has to outlive the window, otherwise it stops firing.
    _timer: Timer,
}

impl Shell {
    /// Builds the window and connects it to the application.
    pub fn new(app: Arc<App>) -> Result<Shell, Error> {
        let ui = Rc::new(PlayerUi::new().map_err(|error| Error::Ui(error.to_string()))?);
        let icon = image::load_from_memory(include_bytes!("../../../resources/flatline-256.png"))
            .map_err(|error| Error::Ui(format!("the window icon could not be decoded: {error}")))?
            .into_rgba8();
        let icon = slint::winit_030::winit::window::Icon::from_rgba(icon.into_raw(), 256, 256)
            .map_err(|error| Error::Ui(format!("the window icon is invalid: {error}")))?;
        let redraw = ui.redraw_handle();
        let last_pointer_activity = Rc::new(Cell::new(Instant::now()));
        let controls_hovered = Rc::new(Cell::new(false));

        // mpv draws the video into the part of the window below the top bar.
        // The weak handle is the only thing the render thread touches, so it
        // never needs the window itself.
        let weak = ui.window().as_weak();
        let bar_height = ui.top_bar_height();
        video_surface::attach(
            ui.window().window(),
            app.engine().clone(),
            move || match weak.upgrade() {
                Some(window) => {
                    let window = window.window();
                    let size = window.size();
                    let scale = window.scale_factor();
                    let bar = bar_height * scale;
                    (size.width as i32, ((size.height as f32 - bar).max(0.0)) as i32)
                }
                None => (0, 0),
            },
            move || redraw.request(),
        )
        .map_err(|error| Error::Ui(error.to_string()))?;

        let loading = Rc::new(RefCell::new(Loading::new()));

        // Window exit is distinct from pointer movement within the client area.
        {
            let window = ui.window().window();
            let ui_for_events = Rc::clone(&ui);
            let last_pointer_activity = Rc::clone(&last_pointer_activity);
            let mut window_icon = Some(icon);
            window.on_winit_window_event(move |slint_window, event| {
                if let Some(icon) = window_icon.take() {
                    let applied = slint_window.with_winit_window(|native_window| {
                        native_window.set_window_icon(Some(icon.clone()));
                        #[cfg(target_os = "windows")]
                        native_window.set_taskbar_icon(Some(icon));
                    });
                    if applied.is_none() {
                        crate::diag::note(
                            "the native window was not ready when its icon was applied",
                        );
                    }
                }
                match event {
                    winit::event::WindowEvent::CursorLeft { .. } => {
                        last_pointer_activity.set(Instant::now() - Duration::from_secs(5));
                        if ui_for_events.has_media() && !ui_for_events.sources_open() {
                            ui_for_events.set_controls_visible(false);
                        }
                    }
                    winit::event::WindowEvent::CursorEntered { .. } => {
                        last_pointer_activity.set(Instant::now());
                    }
                    _ => {}
                }
                EventResult::Propagate
            });
        }

        // The interface only ever calls into the application.
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let last_pointer_activity = Rc::clone(&last_pointer_activity);
            let ui_for_load = Rc::clone(&ui);
            ui.on_load(move |url| {
                last_pointer_activity.set(Instant::now());
                if ui_for_load.has_media() {
                    // Something plays already, so the new URL must not
                    // interrupt it. A toast says the background is working
                    // and the dialog appears once the media is resolved.
                    ui_for_load.show_toast("Parsing video");
                } else {
                    loading.borrow_mut().resolve_started();
                }
                app.load(url);
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_pending = Rc::clone(&ui);
            let loading = Rc::clone(&loading);
            ui.on_pending_play(move || {
                if let Some((_, loaded)) = app.play_pending() {
                    ui_for_pending.hide_toast();
                    ui_for_pending.set_media_title(&loaded.title);
                    ui_for_pending.set_sources(&loaded.sources);
                    ui_for_pending.set_selected_sources(
                        loaded.video.as_deref(),
                        loaded.audio.as_deref(),
                        loaded.muxed.as_deref(),
                    );
                    ui_for_pending.set_has_media(true);
                    ui_for_pending.set_live(loaded.is_live);
                    loading.borrow_mut().resolved();
                    if let Some(duration) = loaded.duration {
                        ui_for_pending.set_duration(duration);
                    }
                }
            });
        }
        {
            let app = Arc::clone(&app);
            ui.on_pending_queue(move || {
                let _ = app.queue_pending();
            });
        }
        {
            let app = Arc::clone(&app);
            ui.on_pending_dismissed(move || app.dismiss_pending());
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            ui.on_sources_applied(move |video, audio, muxed| {
                drain_engine_events(&app, &mut loading.borrow_mut());
                loading.borrow_mut().sources_switching();
                app.select_sources(video, audio, muxed);
            });
        }
        {
            let app = Arc::clone(&app);
            ui.on_play_toggled(move || app.toggle_play());
        }
        {
            let app = Arc::clone(&app);
            ui.on_seek(move |seconds| app.seek(seconds));
        }
        let saved_mute_volume = Rc::new(Cell::new(None::<f64>));
        {
            let app = Arc::clone(&app);
            let saved_mute_volume = Rc::clone(&saved_mute_volume);
            ui.on_volume(move |volume| {
                app.set_volume(slider_volume_changed(
                    &saved_mute_volume,
                    slider_to_player_volume(volume),
                ));
            });
        }
        {
            let app = Arc::clone(&app);
            let saved_mute_volume = Rc::clone(&saved_mute_volume);
            ui.on_volume_toggled(move || {
                let current = app.engine().volume();
                if let Some(volume) = toggle_mute(&saved_mute_volume, current) {
                    app.set_volume(volume);
                }
            });
        }
        {
            let app = Arc::clone(&app);
            ui.on_live_pressed(move || app.go_live());
        }
        {
            let handle = Rc::clone(&ui);
            ui.on_fullscreen_toggled(move || handle.toggle_fullscreen());
        }
        let spinner_step = Rc::new(Cell::new(0u32));
        {
            let last_pointer_activity = Rc::clone(&last_pointer_activity);
            ui.on_pointer_activity(move || last_pointer_activity.set(Instant::now()));
        }
        {
            let controls_hovered = Rc::clone(&controls_hovered);
            ui.on_controls_hovered(move |hovered| controls_hovered.set(hovered));
        }

        // A timer keeps the interface in sync with the player.
        let timer = Timer::default();
        {
            let ui = Rc::clone(&ui);
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let last_pointer_activity = Rc::clone(&last_pointer_activity);
            let controls_hovered = Rc::clone(&controls_hovered);
            let spinner_step = Rc::clone(&spinner_step);
            timer.start(TimerMode::Repeated, TICK, move || {
                let mut loading = loading.borrow_mut();
                tick(&ui, &app, &mut loading);
                spinner_step.set((spinner_step.get() + 1) % 3);
                ui.set_spinner_step(spinner_step.get());
                let visible = ui.sources_open()
                    || (ui.has_media()
                        && (controls_hovered.get()
                            || Instant::now().duration_since(last_pointer_activity.get())
                                < Duration::from_secs(4)));
                ui.set_controls_visible(visible);
            });
        }

        Ok(Shell { ui, app, loading, last_pointer_activity, _timer: timer })
    }

    /// Shows the window and runs until it is closed.
    pub fn run(self) {
        self.ui.show();
        self.ui.run();
    }

    /// Loads a URL that was given on the command line.
    pub fn load(&self, url: &str) {
        self.last_pointer_activity.set(Instant::now());
        self.ui.set_url(url);
        self.loading.borrow_mut().resolve_started();
        self.app.load(url.to_string());
    }

    /// Continues the active queue that was filled in an earlier run.
    pub fn resume(&self) {
        if self.app.resume() {
            self.loading.borrow_mut().resolve_started();
        }
    }

    /// The application behind the window.
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }
}

/// Chooses the next volume and remembers a nonzero level for the mute button.
fn toggle_mute(saved: &Cell<Option<f64>>, current: f64) -> Option<f64> {
    if current <= 0.0 {
        saved.take()
    } else {
        saved.set(Some(current));
        Some(0.0)
    }
}

/// Forgets the pre-mute level when the user chooses a slider value.
fn slider_volume_changed(saved: &Cell<Option<f64>>, volume: f64) -> f64 {
    saved.set(None);
    volume
}

/// Converts the UI slider value to mpv's linear gain percent.
///
/// The slider uses a linear scale where 50% is unity gain, 0% is mute, and
/// 100% is double gain.
fn slider_to_player_volume(slider: f64) -> f64 {
    (slider.clamp(0.0, 200.0)) * 2.0
}

/// Converts mpv's linear gain percent back to the UI slider position.
fn player_to_slider_volume(volume: f64) -> f64 {
    (volume.clamp(0.0, 200.0)) / 2.0
}

/// Brings the interface and the player back in sync.
fn tick(ui: &PlayerUi, app: &App, loading: &mut Loading) {
    for message in app.poll() {
        match message {
            Message::Resolved(Ok(media)) => {
                let crate::app::ResolvedMedia { url, thumbnail, resolved_at, source } = media;
                let Some(loaded) =
                    app.consume_resolved(&url, source, thumbnail.as_deref(), resolved_at)
                else {
                    crate::diag::note(
                        "discarded a resolved video that is no longer current in the active queue",
                    );
                    continue;
                };
                ui.hide_toast();
                ui.set_media_title(&loaded.title);
                ui.set_sources(&loaded.sources);
                ui.set_selected_sources(
                    loaded.video.as_deref(),
                    loaded.audio.as_deref(),
                    loaded.muxed.as_deref(),
                );
                ui.set_has_media(true);
                ui.set_live(loaded.is_live);
                loading.resolved();
                if let Some(duration) = loaded.duration {
                    ui.set_duration(duration);
                }
            }
            Message::Resolved(Err(error)) => {
                ui.hide_toast();
                loading.resolve_failed(error);
            }
            Message::Pending(Ok(pending)) => {
                ui.hide_toast();
                ui.show_pending(&pending.title, pending.thumbnail.as_deref());
                app.set_pending(pending);
            }
            Message::Pending(Err(error)) => {
                ui.hide_toast();
                loading.problem(error);
            }
            Message::ToolUpdated(path) => {
                if !loading.tool_updated(&path) {
                    crate::diag::note(&format!("yt-dlp was updated ({path})"));
                }
            }
            Message::Problem(text) => loading.problem(text),
        }
    }

    drain_engine_events(app, loading);

    // keep-open leaves the completed file loaded, so advance from the
    // persistent EOF property instead of waiting for the file-unload event.
    if loading.observe_eof(app.engine().is_eof()) && app.advance() {
        loading.parsing_next();
    }

    loading.observe(app.engine().is_active(), app.engine().is_buffering());
    match loading.status() {
        loading::Status::Busy(text) => ui.set_status(text, StatusKind::Busy),
        loading::Status::Info(text) => ui.set_status(text, StatusKind::Info),
        loading::Status::Error(text) => ui.set_status(text, StatusKind::Error),
    }

    // The time axis of a live stream is the window behind the live edge, so
    // the interface always shows the same kind of position.
    let timeline = app.timeline();
    ui.set_position(timeline.position);
    ui.set_duration(timeline.duration);
    ui.set_time_text(&crate::time::format_timeline(&timeline));
    let playing = !app.engine().is_idle() && !app.engine().is_paused();
    ui.set_at_live_edge(timeline.live && timeline.behind <= crate::timeline::LIVE_BUFFER);
    ui.set_playing(playing);

    // The volume is read back from the player, so the slider shows what the
    // player really uses and not only what was asked for.
    ui.set_volume(player_to_slider_volume(app.engine().volume()));
}

/// Handles mpv events that are queued before or during a UI refresh.
fn drain_engine_events(app: &App, loading: &mut Loading) {
    while let Some(event) = app.engine().poll_event() {
        match event {
            EngineEvent::StartFile => {
                loading.file_started();
                app.on_file_started();
            }
            EngineEvent::FileLoaded => {
                app.on_file_loaded();
            }
            EngineEvent::EndFile { reason, error } => {
                let failure = (reason == EndReason::Error)
                    .then(|| error.unwrap_or_else(|| "playback failed".to_string()));
                loading.file_ended(failure);
            }
            EngineEvent::Log { level, text } => {
                if level == "error" {
                    crate::diag::note(&format!("mpv: {text}"));
                }
            }
            EngineEvent::Shutdown => {}
        }
    }
}

/// Everything that can go wrong while building the window.
#[derive(Debug)]
pub enum Error {
    /// The window could not be created.
    Ui(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Ui(message) => write!(f, "the user interface failed: {message}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::{
        player_to_slider_volume, slider_to_player_volume, slider_volume_changed, toggle_mute,
    };
    use std::cell::Cell;

    #[test]
    fn mute_restores_the_previous_volume_once() {
        let saved = Cell::new(None);
        assert_eq!(toggle_mute(&saved, 63.0), Some(0.0));
        assert_eq!(saved.get(), Some(63.0));
        assert_eq!(toggle_mute(&saved, 0.0), Some(63.0));
        assert_eq!(saved.get(), None);
        assert_eq!(toggle_mute(&saved, 0.0), None);
    }

    #[test]
    fn slider_changes_clear_the_saved_mute_volume() {
        let saved = Cell::new(Some(63.0));
        assert_eq!(slider_volume_changed(&saved, 22.0), 22.0);
        assert_eq!(saved.get(), None);
        assert_eq!(toggle_mute(&saved, 0.0), None);
    }

    #[test]
    fn volume_slider_maps_half_to_unity_and_full_to_double_gain() {
        assert_eq!(slider_to_player_volume(0.0), 0.0);
        assert!((slider_to_player_volume(50.0) - 100.0).abs() < 1e-10);
        assert!((slider_to_player_volume(100.0) - 200.0).abs() < 1e-10);
        assert!((player_to_slider_volume(100.0) - 50.0).abs() < 1e-10);
        assert!((player_to_slider_volume(200.0) - 100.0).abs() < 1e-10);
    }

    #[test]
    fn volume_slider_round_trips_linearly() {
        for slider in [1.0, 15.0, 25.0, 30.0, 50.0, 75.0, 100.0] {
            let player = slider_to_player_volume(slider);
            assert!((player_to_slider_volume(player) - slider).abs() < 1e-9);
        }
        assert!((slider_to_player_volume(30.0) - 60.0).abs() < 1e-9);
        assert!((slider_to_player_volume(15.0) - 30.0).abs() < 1e-9);
    }
}

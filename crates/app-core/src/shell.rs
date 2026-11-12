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
        let redraw = ui.redraw_handle();
        let last_pointer_activity = Rc::new(Cell::new(Instant::now()));

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

        // The interface only ever calls into the application.
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let last_pointer_activity = Rc::clone(&last_pointer_activity);
            ui.on_load(move |url| {
                last_pointer_activity.set(Instant::now());
                loading.borrow_mut().resolve_started();
                app.load(url);
            });
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            ui.on_sources_applied(move |video, audio, muxed| {
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
        {
            let app = Arc::clone(&app);
            ui.on_volume(move |volume| app.set_volume(volume));
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

        // A timer keeps the interface in sync with the player.
        let timer = Timer::default();
        {
            let ui = Rc::clone(&ui);
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let last_pointer_activity = Rc::clone(&last_pointer_activity);
            let spinner_step = Rc::clone(&spinner_step);
            timer.start(TimerMode::Repeated, TICK, move || {
                let mut loading = loading.borrow_mut();
                tick(&ui, &app, &mut loading);
                spinner_step.set((spinner_step.get() + 1) % 3);
                ui.set_spinner_step(spinner_step.get());
                let visible = ui.has_media()
                    && Instant::now().duration_since(last_pointer_activity.get())
                        < Duration::from_secs(4)
                    || ui.sources_open();
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

    /// The application behind the window.
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }
}

/// Brings the interface and the player back in sync.
fn tick(ui: &PlayerUi, app: &App, loading: &mut Loading) {
    for message in app.poll() {
        match message {
            Message::Resolved(Ok(source)) => {
                let loaded = app.accept(source);
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
            Message::Resolved(Err(error)) => loading.resolve_failed(error),
            Message::ToolUpdated(path) => {
                if !loading.tool_updated(&path) {
                    crate::diag::note(&format!("yt-dlp was updated ({path})"));
                }
            }
            Message::Problem(text) => loading.problem(text),
        }
    }

    while let Some(event) = app.engine().poll_event() {
        match event {
            EngineEvent::StartFile => loading.file_started(),
            EngineEvent::FileLoaded => {}
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
    ui.set_volume(app.engine().volume());
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

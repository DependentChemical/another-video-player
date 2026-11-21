//! The window and the wiring between the interface and the application.
//!
//! This is the only place where the capabilities of Flatline meet: the
//! interface, the player, the resolver and the tool manager are each used
//! through their own narrow interface, and nothing here knows how any of
//! them works inside.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use playback_engine::{EndReason, EngineEvent};
#[cfg(target_os = "windows")]
use slint::winit_030::winit::platform::windows::WindowExtWindows;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use slint::{ComponentHandle, Timer, TimerMode};
use ui_kit::{
    HistoryListEntry, PlayerUi, PlaylistListEntry, QueueListEntry, SourceLists, StatusKind,
};

use crate::app::{App, Message, RetryKind};
use crate::loading::{self, Loading};

#[derive(Debug, Clone)]
enum PendingUpdate {
    App(updater::Update),
    Tool(ytdlp_tool::ToolUpdate),
}

/// What to do with the playlist that a dialog is about to create.
#[derive(Debug, Clone, Copy)]
enum PlaylistIntent {
    AddEntry(i64),
    MoveEntry(i64),
    AddPending,
}

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
    pub fn new(app: Arc<App>, program_dir: PathBuf) -> Result<Shell, Error> {
        let ui = Rc::new(PlayerUi::new().map_err(|error| Error::Ui(error.to_string()))?);
        ui.set_app_version(crate::VERSION);
        let icon = image::load_from_memory(include_bytes!("../../../resources/flatline-256.png"))
            .map_err(|error| Error::Ui(format!("the window icon could not be decoded: {error}")))?
            .into_rgba8();
        let icon = slint::winit_030::winit::window::Icon::from_rgba(icon.into_raw(), 256, 256)
            .map_err(|error| Error::Ui(format!("the window icon is invalid: {error}")))?;
        let redraw = ui.redraw_handle();
        let last_pointer_activity = Rc::new(Cell::new(Instant::now()));
        let controls_hovered = Rc::new(Cell::new(false));
        let controls_interacting = Rc::new(Cell::new(false));

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
        let update = Rc::new(RefCell::new(None::<PendingUpdate>));
        let source_lists = Rc::new(RefCell::new(SourceLists::default()));
        {
            let update = Rc::clone(&update);
            let app = Arc::clone(&app);
            let install_dir = program_dir.clone();
            ui.on_update_accepted(move || {
                let pending = update.borrow().clone();
                match pending {
                    Some(PendingUpdate::App(update)) => {
                        let directory = install_dir.clone();
                        let executable = std::env::current_exe().unwrap_or_else(|_| {
                            directory.join(if cfg!(windows) { "flatline.exe" } else { "flatline" })
                        });
                        app.install_app_update(update, directory, executable);
                    }
                    Some(PendingUpdate::Tool(update)) => {
                        app.install_tool_update(update);
                    }
                    None => {}
                }
            });
        }
        {
            let update = Rc::clone(&update);
            ui.on_update_dismissed(move || {
                update.borrow_mut().take();
            });
        }

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
                        app_logging::debug(
                            "the native window was not ready when its icon was applied",
                        );
                    }
                }
                match event {
                    winit::event::WindowEvent::CursorLeft { .. } => {
                        last_pointer_activity.set(Instant::now() - Duration::from_secs(5));
                        if ui_for_events.has_media() && !ui_for_events.controls_popup_open() {
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
                start_load(&app, &ui_for_load, &loading, url.as_str());
            });
        }
        {
            let app = Arc::clone(&app);
            ui.on_url_edited(move |url| {
                app.preview_url(url.to_string());
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_pending = Rc::clone(&ui);
            let loading = Rc::clone(&loading);
            let source_lists = Rc::clone(&source_lists);
            ui.on_pending_play(move || {
                if let Some((_, loaded)) = app.play_pending() {
                    *source_lists.borrow_mut() = loaded.sources.clone();
                    ui_for_pending.set_url("");
                    ui_for_pending.blur_url();
                    ui_for_pending.set_media_title(&loaded.title);
                    ui_for_pending
                        .set_media_uploader(loaded.uploader.as_deref().unwrap_or_default());
                    ui_for_pending.set_sources(&loaded.sources);
                    ui_for_pending.set_selected_sources(
                        loaded.video.as_deref(),
                        loaded.audio.as_deref(),
                        loaded.muxed.as_deref(),
                    );
                    ui_for_pending.set_source_quality_label(&source_quality_label(
                        &loaded.sources,
                        loaded.video.as_deref(),
                        loaded.audio.as_deref(),
                        loaded.muxed.as_deref(),
                    ));
                    ui_for_pending.set_has_media(true);
                    ui_for_pending.set_live(loaded.is_live);
                    ui_for_pending.set_video_thumbnail(loaded.thumbnail.as_deref());
                    ui_for_pending.set_audio_only(
                        loaded.video.is_none() && loaded.muxed.is_none() && loaded.audio.is_some(),
                    );
                    loading.borrow_mut().resolved();
                    if let Some(duration) = loaded.duration {
                        ui_for_pending.set_duration(duration);
                    }
                }
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_pending = Rc::clone(&ui);
            ui.on_pending_queue(move || {
                let _ = app.queue_pending();
                ui_for_pending.set_url("");
                ui_for_pending.blur_url();
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_pending = Rc::clone(&ui);
            ui.on_pending_queue_next(move || {
                let _ = app.queue_pending_next();
                ui_for_pending.set_url("");
                ui_for_pending.blur_url();
            });
        }
        {
            let app = Arc::clone(&app);
            ui.on_pending_dismissed(move || app.dismiss_pending());
        }
        {
            let app = Arc::clone(&app);
            let ui_for_queue = Rc::clone(&ui);
            ui.on_queue_requested(move || {
                ui_for_queue.show_queue(
                    &queue_list(&app),
                    &queue_title(&app),
                    &playlist_list(&app),
                );
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_playlists = Rc::clone(&ui);
            ui.on_playlists_requested(move || {
                ui_for_playlists.show_playlists(&playlist_list(&app));
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_selection = Rc::clone(&ui);
            let loading = Rc::clone(&loading);
            ui.on_playlist_selected(move |id| {
                if app.select_playlist(id.as_str()) {
                    loading.borrow_mut().resolve_started();
                }
                ui_for_selection.show_queue(
                    &queue_list(&app),
                    &queue_title(&app),
                    &playlist_list(&app),
                );
            });
        }
        let playlist_intent: Rc<RefCell<Option<PlaylistIntent>>> = Rc::new(RefCell::new(None));
        {
            let app = Arc::clone(&app);
            let ui_for_create = Rc::clone(&ui);
            let loading = Rc::clone(&loading);
            let intent = Rc::clone(&playlist_intent);
            ui.on_create_playlist(move |name| {
                create_playlist(&app, &ui_for_create, &loading, &intent, name.as_str(), false);
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_create = Rc::clone(&ui);
            let loading = Rc::clone(&loading);
            let intent = Rc::clone(&playlist_intent);
            ui.on_confirm_create_playlist(move |name| {
                create_playlist(&app, &ui_for_create, &loading, &intent, name.as_str(), true);
            });
        }
        {
            let ui_for_create = Rc::clone(&ui);
            let intent = Rc::clone(&playlist_intent);
            ui.on_create_playlist_dismissed(move || {
                *intent.borrow_mut() = None;
                ui_for_create.close_create_playlist();
            });
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let ui_for_playlist = Rc::clone(&ui);
            ui.on_queue_add_to_playlist(move |entry, playlist_id| {
                let target = playlist_name(&app, playlist_id.as_str())
                    .unwrap_or_else(|| "the playlist".to_string());
                playlist_action(
                    &app,
                    &ui_for_playlist,
                    &loading,
                    PlaylistIntent::AddEntry(entry as i64),
                    playlist_id.as_str(),
                    &target,
                );
            });
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let ui_for_playlist = Rc::clone(&ui);
            ui.on_queue_move_to_playlist(move |entry, playlist_id| {
                let target = playlist_name(&app, playlist_id.as_str())
                    .unwrap_or_else(|| "the playlist".to_string());
                playlist_action(
                    &app,
                    &ui_for_playlist,
                    &loading,
                    PlaylistIntent::MoveEntry(entry as i64),
                    playlist_id.as_str(),
                    &target,
                );
            });
        }
        {
            let ui_for_create = Rc::clone(&ui);
            let intent = Rc::clone(&playlist_intent);
            ui.on_queue_add_to_new_playlist(move |entry| {
                *intent.borrow_mut() = Some(PlaylistIntent::AddEntry(entry as i64));
                ui_for_create.show_create_playlist();
            });
        }
        {
            let ui_for_create = Rc::clone(&ui);
            let intent = Rc::clone(&playlist_intent);
            ui.on_queue_move_to_new_playlist(move |entry| {
                *intent.borrow_mut() = Some(PlaylistIntent::MoveEntry(entry as i64));
                ui_for_create.show_create_playlist();
            });
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let ui_for_pending = Rc::clone(&ui);
            ui.on_pending_add_to_playlist(move |playlist_id| {
                let target = playlist_name(&app, playlist_id.as_str())
                    .unwrap_or_else(|| "the playlist".to_string());
                playlist_action(
                    &app,
                    &ui_for_pending,
                    &loading,
                    PlaylistIntent::AddPending,
                    playlist_id.as_str(),
                    &target,
                );
            });
        }
        {
            let ui_for_pending = Rc::clone(&ui);
            let intent = Rc::clone(&playlist_intent);
            ui.on_pending_add_to_new_playlist(move || {
                *intent.borrow_mut() = Some(PlaylistIntent::AddPending);
                ui_for_pending.show_create_playlist();
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_history = Rc::clone(&ui);
            ui.on_history_requested(move || {
                let entries: Vec<HistoryListEntry> = app
                    .history_entries()
                    .into_iter()
                    .map(|entry| HistoryListEntry {
                        title: entry.title,
                        uploader: entry.uploader.unwrap_or_else(|| entry.url.clone()),
                        url: entry.url,
                    })
                    .collect();
                ui_for_history.show_history(&entries);
            });
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            ui.on_queue_item_selected(move |id| {
                if app.play_queue_entry(id as i64) {
                    loading.borrow_mut().resolve_started();
                }
            });
        }
        {
            let ui_for_queue = Rc::clone(&ui);
            ui.on_queue_copy_url(move |url| {
                if ui_for_queue.copy_to_clipboard(url.as_str()) {
                    ui_for_queue.show_toast("URL copied to the clipboard");
                } else {
                    ui_for_queue.show_toast("The URL could not be copied");
                }
            });
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let ui_for_queue = Rc::clone(&ui);
            ui.on_queue_remove_entry(move |id| {
                let removal = app.remove_queue_entry(id as i64);
                if !removal.removed {
                    return;
                }
                if removal.advanced {
                    loading.borrow_mut().resolve_started();
                }
                ui_for_queue.show_queue(
                    &queue_list(&app),
                    &queue_title(&app),
                    &playlist_list(&app),
                );
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_queue = Rc::clone(&ui);
            ui.on_queue_move_requested(move |src, tgt| {
                let mut entries = app.queue_entries();
                if src < 0
                    || tgt < 0
                    || src as usize >= entries.len()
                    || tgt as usize >= entries.len()
                {
                    return;
                }
                let entry = entries.remove(src as usize);
                entries.insert(tgt as usize, entry);
                let ids: Vec<i64> = entries.into_iter().map(|entry| entry.id).collect();
                if app.reorder_queue(&ids) {
                    ui_for_queue.show_queue(
                        &queue_list(&app),
                        &queue_title(&app),
                        &playlist_list(&app),
                    );
                }
            });
        }
        {
            let app = Arc::clone(&app);
            let ui_for_history = Rc::clone(&ui);
            let loading = Rc::clone(&loading);
            ui.on_history_item_selected(move |url| {
                start_load(&app, &ui_for_history, &loading, url.as_str());
            });
        }
        {
            // Arm the queue row long-press timer; the host does not need to
            // react to press/release beyond what PlayerUi already does.
            ui.on_queue_row_pressed(move |_| {});
            ui.on_queue_row_released(move |_| {});
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let ui_for_sources = Rc::clone(&ui);
            let source_lists = Rc::clone(&source_lists);
            ui.on_sources_applied(move |video, audio, muxed| {
                drain_engine_events(&app, &mut loading.borrow_mut());
                loading.borrow_mut().sources_switching();
                ui_for_sources.set_source_quality_label(&source_quality_label(
                    &source_lists.borrow(),
                    video.as_deref(),
                    audio.as_deref(),
                    muxed.as_deref(),
                ));
                ui_for_sources
                    .set_audio_only(video.is_none() && muxed.is_none() && audio.is_some());
                app.select_sources(video, audio, muxed);
            });
        }
        {
            let app = Arc::clone(&app);
            ui.on_playback_speed_changed(move |speed| app.set_speed(speed));
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
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            ui.on_previous_requested(move || {
                if app.previous() {
                    loading.borrow_mut().resolve_started();
                }
            });
        }
        {
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            ui.on_next_requested(move || {
                if app.next() {
                    loading.borrow_mut().resolve_started();
                }
            });
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
        {
            let controls_interacting = Rc::clone(&controls_interacting);
            ui.on_interacting_changed(move |interacting| controls_interacting.set(interacting));
        }

        // A timer keeps the interface in sync with the player.
        let timer = Timer::default();
        {
            let ui = Rc::clone(&ui);
            let app = Arc::clone(&app);
            let loading = Rc::clone(&loading);
            let last_pointer_activity = Rc::clone(&last_pointer_activity);
            let controls_hovered = Rc::clone(&controls_hovered);
            let controls_interacting = Rc::clone(&controls_interacting);
            let spinner_step = Rc::clone(&spinner_step);
            timer.start(TimerMode::Repeated, TICK, move || {
                let mut loading = loading.borrow_mut();
                tick(&ui, &app, &mut loading, &update, &source_lists);
                spinner_step.set((spinner_step.get() + 1) % 3);
                ui.set_spinner_step(spinner_step.get());
                let visible = ui.controls_popup_open()
                    || (ui.has_media()
                        && (controls_hovered.get()
                            || controls_interacting.get()
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
        if url.trim() == TEST_UPDATE_URL {
            self.show_test_update_modal();
            return;
        }
        let Some(url) = requested_url(url) else {
            return;
        };
        self.last_pointer_activity.set(Instant::now());
        self.ui.set_url(url);
        self.loading.borrow_mut().resolve_started();
        self.app.load(url.to_string());
    }

    /// Opens the update modal with a fake update so it can be tested.
    fn show_test_update_modal(&self) {
        let notes = "## Test Update\n\nThis is a test update notice shown when the `test://update-modal` URL is loaded.\n\n- First item\n- Second item\n- Third item\n- Fourth item\n- Fifth item\n- Sixth item\n- Seventh item\n- Eighth item\n- Ninth item\n- Tenth item";
        self.ui.show_app_update("99.0.0", notes);
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

/// Returns the URL to load, or `None` when the user submitted only whitespace.
///
/// Starting a resolution for an empty field would leave the spinner on with
/// nothing to wait for.
fn requested_url(url: &str) -> Option<&str> {
    let url = url.trim();
    (!url.is_empty()).then_some(url)
}

/// The internal `test://` URL that forces the update prompt to open.
///
/// These URLs never reach the resolver: they are commands that exercise the
/// interface directly, so `yt-dlp` never sees them.
const TEST_UPDATE_URL: &str = "test://update-modal";

/// Whether the given URL is one of the internal `test://` commands.
fn is_test_url(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with("test://")
}

/// Starts loading a URL for the user, from the bar or from the history.
///
/// A `test://` URL never reaches the resolver: it is an internal command that
/// exercises the interface directly, so the resolver never sees it.
fn start_load(app: &App, ui: &PlayerUi, loading: &RefCell<Loading>, url: &str) {
    let Some(url) = requested_url(url) else {
        return;
    };
    if is_test_url(url) {
        handle_test_url(ui, url);
        return;
    }
    if app.is_playing() {
        // Something settled is playing, so the new URL must not interrupt it.
        // A toast says the background is working and the dialog appears once
        // the media is resolved.
        ui.show_toast("Parsing video");
    } else {
        loading.borrow_mut().resolve_started();
    }
    app.load(url.to_string());
}

/// Handles the internal `test://` commands that exercise the interface.
fn handle_test_url(ui: &PlayerUi, url: &str) {
    match url.trim().to_ascii_lowercase().as_str() {
        TEST_UPDATE_URL => {
            let notes = "\
## Test Update

This is a test update notice shown when the `test://update-modal` URL is loaded.

- First item
- Second item
- Third item
- Fourth item
- Fifth item
- Sixth item
- Seventh item
- Eighth item
- Ninth item
- Tenth item";
            ui.show_app_update("99.0.0", notes);
        }
        other => ui.show_toast(&format!("unknown test url: {other}")),
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

/// Converts the UI slider value to mpv's cubic soft-volume percent.
///
/// mpv does not treat `volume` as a linear gain: since 0.18.1 it cubes the
/// value, so `v` percent produces a gain of `(v / 100)^3`. The slider should
/// behave like YouTube's, where the position is the linear gain, so this takes
/// the cube root to undo mpv's curve. Without it the two tapers compound and
/// the lower half of the slider becomes inaudible.
fn slider_to_player_volume(slider: f64) -> f64 {
    let slider = slider.clamp(0.0, 100.0);
    if slider <= 0.0 { 0.0 } else { 100.0 * (slider / 100.0).cbrt() }
}

/// Converts mpv's cubic soft-volume percent back to the UI slider position.
fn player_to_slider_volume(volume: f64) -> f64 {
    let volume = volume.clamp(0.0, 100.0);
    100.0 * (volume / 100.0).powi(3)
}

/// Turns the stored queue into the rows the dialog draws.
fn queue_list(app: &App) -> Vec<QueueListEntry> {
    app.queue_entries()
        .into_iter()
        .map(|entry| QueueListEntry {
            id: entry.id as i32,
            position: entry.position as i32,
            title: entry.title,
            uploader: entry.uploader.unwrap_or_default(),
            thumbnail: entry
                .thumbnail
                .as_deref()
                .and_then(|thumbnail| app.queue_thumbnail(thumbnail)),
            current: entry.current,
            url: entry.url,
        })
        .collect()
}

/// Turns the stored playlists into the rows the dialog draws.
fn playlist_list(app: &App) -> Vec<PlaylistListEntry> {
    app.playlists()
        .into_iter()
        .map(|playlist| PlaylistListEntry {
            id: playlist.id,
            name: playlist.name,
            active: playlist.active,
            is_active_queue: playlist.is_active_queue,
        })
        .collect()
}

/// The title of the queue dialog for the active playlist.
fn queue_title(app: &App) -> String {
    match app.active_playlist_name() {
        Some(name) => format!("Queue [{name}]"),
        None => "Queue".to_string(),
    }
}

/// The name of a playlist, for the confirmation toast.
fn playlist_name(app: &App, playlist_id: &str) -> Option<String> {
    app.playlists()
        .into_iter()
        .find(|playlist| playlist.id == playlist_id)
        .map(|playlist| playlist.name)
}

/// Creates a playlist, asking about a duplicate name before it is stored.
///
/// `confirmed` is set by the duplicate dialog, so the second attempt actually
/// creates the playlist instead of asking again. When the dialog was opened
/// from a picker, `intent` says what should happen with the new playlist.
fn create_playlist(
    app: &App,
    ui: &PlayerUi,
    loading: &Rc<RefCell<Loading>>,
    intent: &Rc<RefCell<Option<PlaylistIntent>>>,
    name: &str,
    confirmed: bool,
) {
    let name = name.trim();
    if !confirmed && app.playlist_name_exists(name) {
        ui.show_duplicate_playlist(name);
        return;
    }
    match app.create_playlist(name) {
        Ok(playlist) => {
            let action = intent.borrow_mut().take();
            ui.close_create_playlist();
            match action {
                Some(action) => playlist_action(
                    app,
                    ui,
                    loading,
                    action,
                    playlist.id.as_str(),
                    playlist.name.as_str(),
                ),
                None => {
                    ui.show_playlists(&playlist_list(app));
                    ui.show_toast(&format!("Created playlist {}", playlist.name));
                }
            }
        }
        Err(error) => ui.show_toast(&error),
    }
}

/// Applies an action to a playlist after the target is known.
fn playlist_action(
    app: &App,
    ui: &PlayerUi,
    loading: &Rc<RefCell<Loading>>,
    action: PlaylistIntent,
    playlist_id: &str,
    target_name: &str,
) {
    match action {
        PlaylistIntent::AddEntry(entry_id) => {
            if app.add_entry_to_playlist(entry_id, playlist_id) {
                ui.show_toast(&format!("Added to {target_name}"));
            } else {
                ui.show_toast("The video could not be added to the playlist");
            }
            ui.show_queue(&queue_list(app), &queue_title(app), &playlist_list(app));
        }
        PlaylistIntent::MoveEntry(entry_id) => {
            match app.move_entry_to_playlist(entry_id, playlist_id) {
                Some(removal) => {
                    if removal.advanced {
                        loading.borrow_mut().resolve_started();
                    }
                    ui.show_toast(&format!("Moved to {target_name}"));
                }
                None => ui.show_toast("The video could not be moved to the playlist"),
            }
            ui.show_queue(&queue_list(app), &queue_title(app), &playlist_list(app));
        }
        PlaylistIntent::AddPending => {
            if app.add_pending_to_playlist(playlist_id) {
                ui.hide_pending();
                ui.set_url("");
                ui.blur_url();
                ui.show_toast(&format!("Added to {target_name}"));
            } else {
                ui.show_toast("The video could not be added to the playlist");
            }
        }
    }
}

/// Brings the interface and the player back in sync.
fn tick(
    ui: &PlayerUi,
    app: &App,
    loading: &mut Loading,
    update_slot: &Rc<RefCell<Option<PendingUpdate>>>,
    source_lists: &Rc<RefCell<SourceLists>>,
) {
    for message in app.poll() {
        match message {
            Message::Resolved(Ok(media)) => {
                let crate::app::ResolvedMedia {
                    url,
                    thumbnail,
                    resolved_at,
                    reason,
                    source,
                    sponsorblock_segments,
                } = media;
                app_logging::debug(format!(
                    "handling a resolved video: url={url} reason={} sponsorblock_segments={}",
                    reason.as_str(),
                    sponsorblock_segments.len()
                ));
                let Some(loaded) = app.consume_resolved(
                    &url,
                    source,
                    thumbnail.as_deref(),
                    resolved_at,
                    reason,
                    sponsorblock_segments,
                ) else {
                    if !app.expects_resolved_url(&url) {
                        app_logging::debug(
                            "discarded a resolved video that is no longer relevant to playback",
                        );
                        continue;
                    }
                    if app.is_current_queue_url(&url) {
                        app_logging::debug(
                            "discarded a stale retry resolve because playback is already active",
                        );
                        continue;
                    }
                    app_logging::debug(
                        "discarded a resolved video that is no longer current in the active queue",
                    );
                    // The spinner belongs to that discarded result; without
                    // this it would keep claiming the app is resolving.
                    loading.resolve_failed(
                        "the resolved video is no longer the current queue entry".to_string(),
                    );
                    ui.show_toast("The resolved video is no longer the current queue entry");
                    continue;
                };
                app_logging::debug(format!(
                    "the player is ready for url={url} title=\"{}\"",
                    loaded.title
                ));
                source_lists.borrow_mut().clone_from(&loaded.sources);
                ui.set_media_title(&loaded.title);
                ui.set_media_uploader(loaded.uploader.as_deref().unwrap_or_default());
                ui.set_sources(&loaded.sources);
                ui.set_selected_sources(
                    loaded.video.as_deref(),
                    loaded.audio.as_deref(),
                    loaded.muxed.as_deref(),
                );
                ui.set_source_quality_label(&source_quality_label(
                    &loaded.sources,
                    loaded.video.as_deref(),
                    loaded.audio.as_deref(),
                    loaded.muxed.as_deref(),
                ));
                ui.set_has_media(true);
                ui.set_live(loaded.is_live);
                ui.set_video_thumbnail(loaded.thumbnail.as_deref());
                ui.set_audio_only(
                    loaded.video.is_none() && loaded.muxed.is_none() && loaded.audio.is_some(),
                );
                loading.resolved();
                if let Some(duration) = loaded.duration {
                    ui.set_duration(duration);
                }
            }
            Message::Resolved(Err((url, error))) => {
                if !app.expects_resolved_url(&url) {
                    app_logging::debug(format!(
                        "ignored a resolver error that is no longer relevant: url={url} error={error}"
                    ));
                    continue;
                }
                app_logging::warning(format!("resolution failed: url={url} error={error}"));
                loading.resolve_failed(error.clone());
                // Schedule a retry for resolver errors
                if app.resolving_url().as_deref() == Some(url.as_str()) {
                    app.handle_resolver_error(url, error);
                } else {
                    ui.show_toast(&error);
                }
            }
            Message::Pending(Ok(pending)) => {
                if !app.is_current_epoch(pending.epoch) {
                    app_logging::debug("discarded a pending video that was superseded");
                    continue;
                }
                let in_playlist = app.active_playlist_name().is_some();
                ui.show_pending(
                    &pending.title,
                    pending.thumbnail.as_deref(),
                    in_playlist,
                    &playlist_list(app),
                );
                app.set_pending(pending);
            }
            Message::Pending(Err(error)) => {
                ui.show_toast(&error);
            }
            Message::UrlPreviewFailed { url: _, error } => {
                ui.show_toast(&error);
            }
            Message::SigningIn(service) => {
                ui.show_toast(&format!("Sign in to {} to continue", service.label()));
            }
            Message::RegeneratingCookies(service) => {
                ui.show_toast(&format!("Regenerating cookies.txt for {}", service.name()));
            }
            Message::AppUpdateAvailable(update) => {
                ui.show_app_update(&update.version, &update.notes);
                *update_slot.borrow_mut() = Some(PendingUpdate::App(update));
            }
            Message::AppUpdateProgress(progress) => {
                ui.set_update_installing(true);
                if progress >= 1.0 {
                    ui.set_update_status_text("Installing updateÃƒÂ¢Ã¢â€šÂ¬Ã‚Â¦");
                    ui.set_update_download_progress(1.0);
                } else {
                    ui.set_update_status_text("Downloading updateÃƒÂ¢Ã¢â€šÂ¬Ã‚Â¦");
                    ui.set_update_download_progress(progress.max(0.0));
                }
            }
            Message::AppUpdateFailed(error) => ui.show_update_error(&error),
            Message::ToolUpdateAvailable(update) => {
                ui.show_tool_update(&update.latest, &update.notes);
                *update_slot.borrow_mut() = Some(PendingUpdate::Tool(update));
            }
            Message::ToolUpdateProgress(progress) => {
                ui.set_update_installing(true);
                ui.set_update_status_text("Downloading updateÃƒÂ¢Ã¢â€šÂ¬Ã‚Â¦");
                ui.set_update_download_progress(progress.max(0.0));
            }
            Message::ToolUpdateStatus(status) => {
                ui.set_update_installing(true);
                ui.set_update_status_text(&status);
            }
            Message::ToolUpdateApplied(version) => {
                update_slot.borrow_mut().take();
                ui.close_update();
                let text = format!("yt-dlp updated to {version}");
                if !loading.tool_updated(&text) {
                    app_logging::debug(format!(
                        "yt-dlp update was applied while no load was waiting: {text}"
                    ));
                }
                ui.show_toast(&text);
            }
            Message::ToolUpdateFailed(error) => ui.show_update_error(&error),
            Message::Problem(text) => {
                if loading.is_busy() {
                    loading.problem(text.clone());
                }
                ui.show_toast(&text);
            }
            Message::Toast(text) => ui.show_toast(&text),
            Message::ThumbnailMigrationStarted { count } => {
                ui.show_toast(&format!(
                    "Migrating {count} thumbnails to the new cacheÃƒÂ¢Ã¢â€šÂ¬Ã‚Â¦"
                ));
            }
            Message::ThumbnailMigrationFinished { processed } => {
                ui.show_toast(&format!("Thumbnail migration finished ({processed} processed)"));
            }
            Message::ThumbnailRefreshStarted { .. } => {
                ui.show_toast("Downloading thumbnails");
            }
            Message::RetryScheduled { error, delay_seconds, kind } => {
                loading.retry_scheduled();
                let prefix = match kind {
                    RetryKind::Resolver => "retrying yt-dlp",
                    RetryKind::Playback => "retrying playback",
                    RetryKind::SourceSelection => "retrying source selection",
                };
                ui.show_toast(&format!("{} after {}s: {}", prefix, delay_seconds, error));
            }
        }
    }

    // Check for pending retries and show progress while the next attempt runs.
    if let Some(kind) = app.check_retries() {
        match kind {
            RetryKind::Resolver | RetryKind::SourceSelection => loading.resolve_started(),
            RetryKind::Playback => loading.resolved(),
        }
    }

    drain_engine_events(app, loading);

    // Save playback position to history periodically (for non-live videos)
    app.save_history_position();
    app.apply_sponsorblock_skips();

    // keep-open leaves the completed file loaded, so advance from the
    // persistent EOF property instead of waiting for the file-unload event.
    if loading.observe_eof(app.engine().is_eof()) && app.advance() {
        loading.parsing_next();
    }

    loading.observe(app.engine().is_active(), app.engine().is_buffering());
    match loading.status() {
        loading::Status::Busy(text) => ui.set_status(text, StatusKind::Busy),
        loading::Status::Info(text) => ui.set_status(text, StatusKind::Info),
        loading::Status::Error(_) => ui.set_status("", StatusKind::Info),
    }

    // The time axis of a live stream is the window behind the live edge, so
    // the interface always shows the same kind of position.
    let timeline = app.timeline();
    ui.set_position(timeline.position);
    ui.set_duration(timeline.duration);
    ui.set_buffered_ranges(&app.buffered_ranges());
    ui.set_sponsor_segments(&app.sponsorblock_ranges());
    ui.set_time_text(&crate::time::format_timeline(&timeline));
    let playing = !app.engine().is_idle() && !app.engine().is_paused();
    ui.set_at_live_edge(timeline.live && timeline.behind <= crate::timeline::LIVE_BUFFER);
    ui.set_playing(playing);

    ui.set_playback_speed(app.speed());

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
                match (&reason, &failure) {
                    (EndReason::Eof, _) => {
                        app_logging::notice("playback reached the end of the file")
                    }
                    (EndReason::Error, Some(error)) => {
                        app_logging::error(format!("libmpv reported a playback error: {error}"))
                    }
                    (reason, _) => app_logging::debug(format!("playback ended: reason={reason:?}")),
                }
                loading.file_ended(failure.clone());
                // Reset history position when video finishes normally (not error/stop)
                if reason == EndReason::Eof {
                    app.reset_history_position();
                } else if reason == EndReason::Error {
                    // Schedule a retry for playback errors
                    if let Some(url) = app.current_url() {
                        app.handle_playback_error(
                            url,
                            failure.unwrap_or_else(|| "playback failed".to_string()),
                        );
                    }
                }
            }
            EngineEvent::Log { level, text } => match level.as_str() {
                "error" | "fatal" => {
                    app_logging::warning(format!("libmpv [{level}]: {text}"));
                }
                "warn" => app_logging::debug(format!("libmpv [{level}]: {text}")),
                _ => app_logging::debug(format!("libmpv [{level}]: {text}")),
            },
            EngineEvent::Shutdown => {}
        }
    }
}

/// The label shown in the settings menu for the currently selected source.
fn source_quality_label(
    sources: &SourceLists,
    video: Option<&str>,
    audio: Option<&str>,
    muxed: Option<&str>,
) -> String {
    if let Some(id) = muxed {
        return sources
            .muxed
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.label.clone())
            .unwrap_or_else(|| "Auto".to_string());
    }
    if let Some(id) = video {
        return sources
            .video
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.label.clone())
            .unwrap_or_else(|| "Auto".to_string());
    }
    if let Some(id) = audio {
        return sources
            .audio
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.label.clone())
            .unwrap_or_else(|| "Auto".to_string());
    }
    "Auto".to_string()
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
        player_to_slider_volume, requested_url, slider_to_player_volume, slider_volume_changed,
        toggle_mute,
    };
    use std::cell::Cell;

    #[test]
    fn a_blank_url_is_not_loaded() {
        assert_eq!(requested_url("   \n\t "), None);
        assert_eq!(requested_url(""), None);
    }

    #[test]
    fn a_url_is_trimmed_before_it_is_loaded() {
        assert_eq!(
            requested_url("  https://example.com/video  "),
            Some("https://example.com/video")
        );
    }

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
    fn volume_slider_maps_zero_to_mute_and_hundred_to_unity() {
        assert_eq!(slider_to_player_volume(0.0), 0.0);
        assert!((slider_to_player_volume(100.0) - 100.0).abs() < 1e-10);
        assert!((player_to_slider_volume(100.0) - 100.0).abs() < 1e-10);
        assert!((player_to_slider_volume(0.0) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn volume_slider_round_trips_perceptually() {
        for slider in [1.0, 25.0, 50.0, 75.0, 100.0] {
            let player = slider_to_player_volume(slider);
            assert!((player_to_slider_volume(player) - slider).abs() < 1e-9);
        }
    }

    #[test]
    fn volume_slider_matches_youtube_linear_gain() {
        // mpv's soft volume is cubic, so taking the cube root leaves the slider
        // position equal to the linear gain, exactly like YouTube's slider.
        for slider in [1.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0] {
            let player = slider_to_player_volume(slider);
            let gain = (player / 100.0).powi(3);
            assert!((gain - slider / 100.0).abs() < 1e-12);
        }
        // Low positions must stay audible rather than collapsing to silence.
        assert!(slider_to_player_volume(5.0) > 30.0);
        assert!(slider_to_player_volume(10.0) > slider_to_player_volume(5.0));
    }

    #[test]
    fn source_quality_prefers_video_then_audio() {
        let sources = ui_kit::SourceLists {
            video: vec![ui_kit::SourceOption::new("v1", "1080p", "")],
            audio: vec![ui_kit::SourceOption::new("a1", "128 kb/s", "")],
            muxed: vec![],
        };
        assert_eq!(super::source_quality_label(&sources, Some("v1"), Some("a1"), None), "1080p");
        assert_eq!(super::source_quality_label(&sources, None, Some("a1"), None), "128 kb/s");
    }
}

//! The headless part of Flatline.
//!
//! Everything here is free of the user interface: it resolves URLs, decides
//! which streams to play and drives the player. The results travel back to
//! the interface through a channel, so the background work never has to
//! touch the toolkit.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use media_model::{MediaSource, Quality};
use playback_engine::{Engine, LoadRequest};
use stream_resolver::{CommandSpec, Resolver};
use ui_kit::QualityOption;
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
    /// The qualities the user can pick from.
    pub qualities: Vec<QualityOption>,
    /// Whether the media is a live stream.
    pub is_live: bool,
}

/// The state of the loaded media.
#[derive(Debug, Default)]
struct State {
    source: Option<MediaSource>,
    qualities: Vec<QualityOption>,
    selected: usize,
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
    pub fn accept(&self, source: MediaSource) -> Loaded {
        let qualities = quality_options(&source);
        let loaded = Loaded {
            title: source.title.clone(),
            duration: source.duration,
            qualities: qualities.clone(),
            is_live: source.is_live,
        };

        {
            let mut state = self.state.lock().expect("the state is not poisoned");
            state.source = Some(source);
            state.qualities = qualities;
            state.selected = 0;
            state.is_live = loaded.is_live;
        }
        self.live.lock().expect("the live axis is not poisoned").reset();

        if let Err(error) = self.apply_quality(0) {
            self.report(error);
        }
        loaded
    }

    /// Plays the media in another quality.
    pub fn select_quality(&self, index: i32) {
        let index = index.max(0) as usize;
        {
            let mut state = self.state.lock().expect("the state is not poisoned");
            if index >= state.qualities.len() {
                return;
            }
            state.selected = index;
        }
        if let Err(error) = self.apply_quality(index) {
            self.report(error);
        }
    }

    /// The quality that is currently playing.
    pub fn selected_quality(&self) -> usize {
        self.state.lock().expect("the state is not poisoned").selected
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
    pub fn go_live(&self) {
        if !self.is_live() {
            return;
        }
        match self.engine.seek_to_live_edge() {
            Ok(()) => {
                // The window starts again at the new edge, so the handle sits
                // at the right hand end on the next tick.
                self.live.lock().expect("the live axis is not poisoned").reset();
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

    /// Loads the plan of the given quality into the player.
    fn apply_quality(&self, index: usize) -> Result<(), String> {
        let state = self.state.lock().expect("the state is not poisoned");
        let Some(source) = state.source.as_ref() else {
            return Err("there is nothing to play".to_string());
        };
        let quality = quality_of(&state.qualities, index);
        let plan = stream_resolver::plan(source, &quality).map_err(|error| error.to_string())?;

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

/// The qualities the user can pick from.
fn quality_options(source: &MediaSource) -> Vec<QualityOption> {
    let mut options = vec![QualityOption::new("best", "Best")];
    for height in source.available_heights() {
        options.push(QualityOption::new(height.to_string(), format!("{height}p")));
    }
    options
}

/// Turns the index of a quality option back into a quality.
fn quality_of(options: &[QualityOption], index: usize) -> Quality {
    match options.get(index) {
        Some(option) => match option.id.parse::<u32>() {
            Ok(height) => Quality::Height(height),
            Err(_) => Quality::Best,
        },
        None => Quality::Best,
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

    fn options() -> Vec<QualityOption> {
        vec![QualityOption::new("best", "Best"), QualityOption::new("1080", "1080p")]
    }

    #[test]
    fn the_first_option_means_the_best_quality() {
        assert_eq!(quality_of(&options(), 0), Quality::Best);
    }

    #[test]
    fn other_options_name_a_height() {
        assert_eq!(quality_of(&options(), 1), Quality::Height(1080));
    }

    #[test]
    fn an_unknown_option_falls_back_to_the_best_quality() {
        assert_eq!(quality_of(&options(), 7), Quality::Best);
    }
}

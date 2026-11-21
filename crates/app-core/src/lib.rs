//! Flatline — the composition root of the player.
//!
//! # Layer 3 of the isolation ladder
//!
//! This crate is the only place where the capabilities of Flatline meet.
//! It owns the window, decides what happens when the user presses a button
//! and keeps the player and the interface in sync.
//!
//! Every other crate is a leaf: it owns one capability, it can be used on
//! its own and it never reaches for a sibling. See `AGENTS.md` for the rules
//! this workspace follows.

pub mod app;
pub mod loading;
pub mod paths;
pub mod session;
pub mod shell;
pub mod time;
pub mod timeline;

use std::sync::Arc;

use settings::Settings;

pub use app::{App, Loaded, Message, PlaybackReason};
pub use paths::AppPaths;
pub use shell::Shell;
pub use timeline::{LIVE_BUFFER, LIVE_WINDOW, LiveAxis, Timeline};

/// The version of Flatline, taken from the workspace manifest in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Registers the internal source plugins so source-specific behavior is used.
fn register_sources() {
    static SOURCES: &[&dyn web_session::Source] =
        &[&source_youtube::SOURCE, &source_twitch::SOURCE];
    web_session::Sources::new(SOURCES).install();
}

/// Starts the player.
///
/// When a URL is given, it is loaded right away.
pub fn run(url: Option<&str>) -> Result<(), Error> {
    register_sources();
    let paths = AppPaths::discover();
    let logging_level = Settings::load(paths.settings())
        .ok()
        .and_then(|settings| app_logging::Level::parse(&settings.log_level()))
        .unwrap_or(app_logging::Level::Notice);
    app_logging::init(paths.log_file(), logging_level);
    app_logging::notice(format!(
        "Flatline {VERSION} starting; root={} program={} portable={} log_level={}",
        paths.root().display(),
        paths.program().display(),
        paths.is_portable(),
        app_logging::level()
    ));

    paths.ensure().map_err(|error| {
        app_logging::error(format!("the Flatline directories could not be prepared: {error}"));
        Error::Tools(error.to_string())
    })?;

    let tools = paths.tool_paths();
    let managed = ytdlp_tool::locate::managed(&tools);
    let ytdlp = managed.clone().unwrap_or_else(|| ytdlp_tool::locate::at(tools.managed_binary()));
    match &managed {
        Some(_) => app_logging::notice(format!(
            "using the managed yt-dlp at {}",
            ytdlp.program().display()
        )),
        None => app_logging::warning(format!(
            "no managed yt-dlp was found; using {} and offering an install",
            ytdlp.program().display()
        )),
    }

    let app = Arc::new(App::new(&ytdlp, &paths)?);
    app.start_update_checks(paths.clone());
    let shell = Shell::new(Arc::clone(&app), paths.program().to_path_buf())?;

    if managed.is_none() {
        app.prompt_missing_tool_install();
    }

    match url {
        Some(url) => shell.load(url),
        None => shell.resume(),
    }

    app_logging::notice("the window event loop is running");
    shell.run();
    app_logging::notice("the window event loop returned; Flatline is exiting");
    Ok(())
}

/// Everything that can go wrong while starting the player.
#[derive(Debug)]
pub enum Error {
    /// The application could not be created.
    App(app::Error),
    /// The window could not be created.
    Shell(shell::Error),
    /// The tools could not be prepared.
    Tools(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::App(error) => write!(f, "{error}"),
            Error::Shell(error) => write!(f, "{error}"),
            Error::Tools(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::App(error) => Some(error),
            Error::Shell(error) => Some(error),
            Error::Tools(_) => None,
        }
    }
}

impl From<app::Error> for Error {
    fn from(value: app::Error) -> Self {
        Error::App(value)
    }
}

impl From<shell::Error> for Error {
    fn from(value: shell::Error) -> Self {
        Error::Shell(value)
    }
}

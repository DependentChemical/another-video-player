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
pub mod paths;
pub mod shell;
pub mod time;

use std::sync::Arc;

pub use app::{App, Loaded, Message};
pub use paths::AppPaths;
pub use shell::Shell;

/// Starts the player.
///
/// When a URL is given, it is loaded right away.
pub fn run(url: Option<&str>) -> Result<(), Error> {
    let paths = AppPaths::discover();
    paths.ensure().map_err(|error| Error::Tools(error.to_string()))?;

    let tools = paths.tool_paths();
    let ytdlp = match ytdlp_tool::locate::discover(&tools) {
        Some(ytdlp) => ytdlp,
        None => install_ytdlp(&tools)?,
    };

    let app = Arc::new(App::new(&ytdlp)?);
    let shell = Shell::new(Arc::clone(&app))?;

    // Look for a newer yt-dlp while the player is already running.
    app.check_tool_update(&ytdlp, &paths);

    if let Some(url) = url {
        shell.load(url);
    }

    shell.run();
    Ok(())
}

/// Downloads `yt-dlp` when it is not installed yet.
fn install_ytdlp(tools: &ytdlp_tool::ToolPaths) -> Result<ytdlp_tool::YtDlp, Error> {
    eprintln!("flatline: yt-dlp is not installed, downloading it");
    let path = ytdlp_tool::update::install_latest(tools)
        .map_err(|error| Error::Tools(format!("yt-dlp could not be installed: {error}")))?;
    Ok(ytdlp_tool::locate::at(path))
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

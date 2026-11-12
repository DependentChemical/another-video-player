//! Finding a usable `yt-dlp` and describing how to run it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// A `yt-dlp` executable together with the interpreter it needs.
///
/// On Windows the published binary is self contained and `interpreter` is
/// `None`. On Unix the published artifact is a Python zip application, so
/// `interpreter` points at a `python3` binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtDlp {
    program: PathBuf,
    interpreter: Option<PathBuf>,
}

impl YtDlp {
    /// Wraps an executable without an interpreter.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), interpreter: None }
    }

    /// Wraps an executable that has to be run through an interpreter.
    pub fn with_interpreter(program: impl Into<PathBuf>, interpreter: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), interpreter: Some(interpreter.into()) }
    }

    /// The executable itself.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The interpreter the executable needs, if any.
    pub fn interpreter(&self) -> Option<&Path> {
        self.interpreter.as_deref()
    }

    /// Builds a command that runs `yt-dlp` with the given arguments.
    pub fn command(&self) -> Command {
        match &self.interpreter {
            Some(interpreter) => {
                let mut command = Command::new(interpreter);
                command.arg(&self.program);
                command
            }
            None => Command::new(&self.program),
        }
    }

    /// Asks `yt-dlp` for its version string, e.g. `2025.10.13`.
    pub fn version(&self) -> Result<String, Error> {
        let output = self.command().arg("--version").output()?;
        if !output.status.success() {
            return Err(Error::NotUsable(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Whether the executable exists and answers `--version`.
    pub fn is_usable(&self) -> bool {
        self.version().is_ok()
    }
}

/// Where Flatline keeps the tools it manages itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    root: PathBuf,
}

impl ToolPaths {
    /// Creates a tool directory below `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory managed tools live in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the managed `yt-dlp` executable.
    pub fn managed_binary(&self) -> PathBuf {
        let name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
        self.root.join(name)
    }
}

/// Looks for a managed `yt-dlp` first, then one on `PATH`.
pub fn discover(paths: &ToolPaths) -> Option<YtDlp> {
    managed(paths).or_else(from_path)
}

/// The managed `yt-dlp`, if it has been downloaded before.
pub fn managed(paths: &ToolPaths) -> Option<YtDlp> {
    let program = paths.managed_binary();
    program.is_file().then(|| describe(program))
}

/// A `yt-dlp` found on `PATH`.
pub fn from_path() -> Option<YtDlp> {
    let file_name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(file_name))
        .find(|candidate| candidate.is_file())
        .map(describe)
}

/// Builds a [`YtDlp`] for an executable, detecting the interpreter it needs.
pub fn at(program: impl Into<PathBuf>) -> YtDlp {
    describe(program.into())
}

/// Builds a [`YtDlp`] for an executable, detecting the interpreter it needs.
fn describe(program: PathBuf) -> YtDlp {
    match interpreter_for(&program) {
        Some(interpreter) => YtDlp::with_interpreter(program, interpreter),
        None => YtDlp::new(program),
    }
}

/// The interpreter an executable needs, if any.
///
/// Windows binaries published by the `yt-dlp` project are self contained.
/// Everything else is a Python application and needs a `python3`.
fn interpreter_for(program: &Path) -> Option<PathBuf> {
    if cfg!(windows) && program.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
        return None;
    }
    if !cfg!(windows) && program.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
        return None;
    }
    python_on_path()
}

/// Finds a `python3` (or `python`) executable on `PATH`.
pub fn python_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for name in ["python3", "python"] {
        let found = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file());
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Whether `name` looks like a `yt-dlp` executable.
pub fn is_ytdlp_name(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with("yt-dlp")
}

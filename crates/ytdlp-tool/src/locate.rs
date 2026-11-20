use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

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
        let command = match &self.interpreter {
            Some(interpreter) => {
                let mut command = Command::new(interpreter);
                command.arg(&self.program);
                command
            }
            None => Command::new(&self.program),
        };
        #[cfg(windows)]
        let command = {
            let mut command = command;
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
            command
        };
        command
    }

    /// Asks `yt-dlp` for its version string, e.g. `2025.10.13`.
    ///
    /// A version probe must never hang: a broken executable that does not
    /// answer is treated as unusable so it cannot block later work.
    pub fn version(&self) -> Result<String, Error> {
        self.version_with_cancel(None)
    }

    /// Asks `yt-dlp` for its version string, allowing cancellation by the
    /// caller.
    pub fn version_with_cancel(
        &self,
        cancel: Option<(&AtomicU32, &AtomicBool)>,
    ) -> Result<String, Error> {
        let mut command = self.command();
        command.arg("--version");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            app_logging::error(format!(
                "yt-dlp could not be started for a version probe: program={} error={error}",
                self.program.display()
            ));
            error
        })?;
        app_logging::debug(format!(
            "yt-dlp version probe started: program={} pid={}",
            self.program.display(),
            child.id()
        ));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut out) = stdout {
                let _ = out.read_to_end(&mut bytes);
            }
            bytes
        });
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut err) = stderr {
                let _ = err.read_to_end(&mut bytes);
            }
            bytes
        });
        if let Some((pid, _)) = cancel {
            pid.store(child.id(), Ordering::SeqCst);
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if cancel.is_some_and(|(_, flag)| flag.load(Ordering::SeqCst)) {
                let probe_id = child.id();
                app_logging::debug(format!("cancelled the yt-dlp version probe: pid={probe_id}"));
                terminate_child(&mut child, cancel);
                clear_cancel_pid(cancel);
                app_logging::debug(format!(
                    "yt-dlp version probe pid={probe_id} closed after cancellation"
                ));
                return Err(Error::NotUsable("the yt-dlp probe was cancelled".to_string()));
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let probe_id = child.id();
                    app_logging::warning(format!(
                        "the yt-dlp version probe timed out after 10s: pid={probe_id}"
                    ));
                    terminate_child(&mut child, cancel);
                    clear_cancel_pid(cancel);
                    app_logging::debug(format!(
                        "yt-dlp version probe pid={probe_id} closed after timeout"
                    ));
                    return Err(Error::NotUsable(
                        "timed out reading the yt-dlp version".to_string(),
                    ));
                }
                Err(error) => {
                    app_logging::error(format!(
                        "the yt-dlp version probe could not wait for pid={}: {error}",
                        child.id()
                    ));
                    terminate_child(&mut child, cancel);
                    clear_cancel_pid(cancel);
                    return Err(Error::Io(error));
                }
            }
        };

        clear_cancel_pid(cancel);
        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();
        if !status.success() {
            let message = String::from_utf8_lossy(&stderr).trim().to_string();
            app_logging::warning(format!(
                "the yt-dlp version probe failed: program={} pid={} status={status} stderr={message}",
                self.program.display(),
                child.id()
            ));
            return Err(Error::NotUsable(message));
        }
        let version = String::from_utf8_lossy(&stdout).trim().to_string();
        app_logging::debug(format!(
            "yt-dlp version probe finished: program={} pid={} status={status} version={version}",
            self.program.display(),
            child.id()
        ));
        Ok(version)
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

    /// Path where a downloaded update is staged before it replaces the tool.
    pub fn staged_binary(&self) -> PathBuf {
        let mut name = self.managed_binary();
        let suffix = if cfg!(windows) { "new.exe" } else { "new" };
        name.set_extension(suffix);
        name
    }
}

fn clear_cancel_pid(cancel: Option<(&AtomicU32, &AtomicBool)>) {
    if let Some((pid, _)) = cancel {
        pid.store(0, Ordering::SeqCst);
    }
}

/// Stops a probe process and, on Windows, any process tree it spawned.
fn terminate_child(child: &mut std::process::Child, cancel: Option<(&AtomicU32, &AtomicBool)>) {
    let id = cancel
        .and_then(|(pid, _)| {
            let id = pid.load(Ordering::SeqCst);
            (id != 0).then_some(id)
        })
        .unwrap_or_else(|| child.id());
    app_logging::debug(format!("stopping yt-dlp probe process: pid={id}"));
    if let Err(error) = child.kill() {
        app_logging::debug(format!("yt-dlp probe pid={id} could not be killed directly: {error}"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &id.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill").args(["-TERM", &id.to_string()]).status();
        let _ = std::process::Command::new("kill").args(["-KILL", &id.to_string()]).status();
    }
    match child.wait() {
        Ok(status) => {
            app_logging::debug(format!("yt-dlp probe pid={id} closed with {status}"));
        }
        Err(error) => {
            app_logging::warning(format!(
                "yt-dlp probe pid={id} could not be reaped after termination: {error}"
            ));
        }
    }
}

/// Looks for a managed `yt-dlp` first, then one on `PATH`.
pub fn discover(paths: &ToolPaths) -> Option<YtDlp> {
    match managed(paths) {
        Some(tool) => {
            app_logging::debug(format!(
                "using the managed yt-dlp: program={}",
                tool.program.display()
            ));
            Some(tool)
        }
        None => {
            let tool = from_path();
            if let Some(tool) = &tool {
                app_logging::debug(format!(
                    "using a yt-dlp from PATH: program={}",
                    tool.program.display()
                ));
            } else {
                app_logging::notice("no yt-dlp was found on this system");
            }
            tool
        }
    }
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
/// The artifacts the `yt-dlp` project publishes are self contained binaries
/// on every platform, but an old `yt-dlp` may be the Python zip application.
/// The first two bytes tell the two apart: a zip application starts with a
/// `#!` shebang, a native binary starts with its own magic.
fn interpreter_for(program: &Path) -> Option<PathBuf> {
    if program.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
        return None;
    }
    if needs_python(program) { python_on_path() } else { None }
}

/// Whether a file looks like a Python script rather than a native executable.
fn needs_python(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    match std::io::Read::read_exact(&mut file, &mut magic) {
        Ok(()) => is_python_magic(&magic),
        Err(_) => false,
    }
}

/// Whether the first bytes of a file are a shebang.
fn is_python_magic(magic: &[u8]) -> bool {
    magic.starts_with(b"#!")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shebang_marks_a_python_script() {
        assert!(is_python_magic(b"#!/usr/bin/env python3"));
    }

    #[test]
    fn a_native_binary_needs_no_interpreter() {
        assert!(!is_python_magic(b"\x7fELF"));
        assert!(!is_python_magic(b"MZ"));
    }
}

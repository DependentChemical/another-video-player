//! Where Flatline keeps the files it manages itself.
//!
//! Flatline is a portable program: everything it downloads lives next to the
//! executable, so the folder can be copied to another machine and keeps
//! working. Only when the program directory cannot be written to — a system
//! wide installation on Linux, for instance — the files fall back to the per
//! user directory of the platform.

use std::path::{Path, PathBuf};

/// The directories Flatline uses at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    /// The writable root. The program directory when it can be written to.
    root: PathBuf,
    /// The directory the executable lives in.
    program: PathBuf,
}

impl AppPaths {
    /// The directories of the current user.
    ///
    /// The program directory is preferred, so a copied folder keeps its
    /// `yt-dlp` and its settings. When it is not writable the per user
    /// directory of the platform is used instead:
    ///
    /// * Windows: `%LOCALAPPDATA%\Flatline`
    /// * macOS: `~/Library/Application Support/Flatline`
    /// * everything else: `$XDG_DATA_HOME/flatline`, or
    ///   `~/.local/share/flatline`
    pub fn discover() -> AppPaths {
        let program = program_directory();
        let root = if writable(&program) { program.clone() } else { user_directory() };
        AppPaths { root, program }
    }

    /// Uses an explicit directory, mostly useful for tests.
    pub fn with_root(root: impl Into<PathBuf>) -> AppPaths {
        let root = root.into();
        AppPaths { program: root.clone(), root }
    }

    /// The directory everything else lives in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory the executable lives in.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The optional YouTube cookies file beside the executable.
    pub fn youtube_cookies(&self) -> PathBuf {
        self.program.join("cookies-youtube.txt")
    }

    /// Whether the program directory is the one that is written to.
    ///
    /// When this is false the files live in the per user directory instead.
    pub fn is_portable(&self) -> bool {
        self.root == self.program
    }

    /// The directory managed tools live in.
    pub fn tools(&self) -> PathBuf {
        self.root.join("tools")
    }

    /// Where the managed tools are kept.
    pub fn tool_paths(&self) -> ytdlp_tool::ToolPaths {
        ytdlp_tool::ToolPaths::new(self.tools())
    }

    /// The directory preview images live in.
    pub fn thumbnails(&self) -> PathBuf {
        self.root.join("thumbnails")
    }

    /// The SQLite database with the queue and, later, the rest of the library.
    pub fn database(&self) -> PathBuf {
        self.root.join("app.db")
    }

    /// The directory downloaded thumbnails are cached in.
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The file the diagnostics are written to.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("flatline.log")
    }

    /// Creates the directories Flatline needs.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.tools())?;
        std::fs::create_dir_all(self.thumbnails())?;
        std::fs::create_dir_all(self.cache())?;
        Ok(())
    }
}

/// The directory the running executable lives in.
fn program_directory() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Whether a directory can be written to.
///
/// The check creates the directory when it is missing, because a program that
/// ships in a folder the user may unpack into is usually writable.
fn writable(directory: &Path) -> bool {
    if std::fs::create_dir_all(directory).is_err() {
        return false;
    }
    let probe = directory.join(".flatline-write-test");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The home directory of the current user.
fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The per user application directory of the platform.
#[cfg(windows)]
fn user_directory() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(home_directory)
        .join("Flatline")
}

#[cfg(not(windows))]
fn user_directory() -> PathBuf {
    if cfg!(target_os = "macos") {
        return home_directory().join("Library").join("Application Support").join("Flatline");
    }
    match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("flatline"),
        _ => home_directory().join(".local").join("share").join("flatline"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_root_is_used_as_it_is() {
        let paths = AppPaths::with_root("/tmp/flatline");
        assert_eq!(paths.root(), Path::new("/tmp/flatline"));
        assert_eq!(paths.program(), Path::new("/tmp/flatline"));
        assert!(paths.is_portable());
    }

    #[test]
    fn the_tools_live_in_a_folder_of_their_own() {
        let paths = AppPaths::with_root("/tmp/flatline");
        assert_eq!(paths.tools(), PathBuf::from("/tmp/flatline").join("tools"));
        assert_eq!(paths.thumbnails(), PathBuf::from("/tmp/flatline").join("thumbnails"));
        assert_eq!(paths.database(), PathBuf::from("/tmp/flatline").join("app.db"));
        assert_eq!(paths.cache(), PathBuf::from("/tmp/flatline").join("cache"));
        assert_eq!(paths.log_file(), PathBuf::from("/tmp/flatline").join("flatline.log"));
        assert_eq!(
            paths.youtube_cookies(),
            PathBuf::from("/tmp/flatline").join("cookies-youtube.txt")
        );
    }
}

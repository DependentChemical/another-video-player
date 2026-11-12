//! Where Flatline keeps the files it manages itself.

use std::path::{Path, PathBuf};

/// The directories Flatline uses at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    /// The directories of the current user.
    ///
    /// * Windows: `%LOCALAPPDATA%\Flatline`
    /// * macOS: `~/Library/Application Support/Flatline`
    /// * everything else: `$XDG_DATA_HOME/flatline`, or
    ///   `~/.local/share/flatline`
    pub fn discover() -> AppPaths {
        let root = if cfg!(windows) {
            base_directory().join("Flatline")
        } else if cfg!(target_os = "macos") {
            home_directory().join("Library").join("Application Support").join("Flatline")
        } else {
            match std::env::var_os("XDG_DATA_HOME") {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("flatline"),
                _ => home_directory().join(".local").join("share").join("flatline"),
            }
        };
        AppPaths { root }
    }

    /// Uses an explicit directory, mostly useful for tests.
    pub fn with_root(root: impl Into<PathBuf>) -> AppPaths {
        AppPaths { root: root.into() }
    }

    /// The directory everything else lives in.
    pub fn root(&self) -> &Path {
        &self.root
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

    /// Creates the directories Flatline needs.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.tools())?;
        std::fs::create_dir_all(self.thumbnails())?;
        Ok(())
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
fn base_directory() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(home_directory)
}

#[cfg(not(windows))]
fn base_directory() -> PathBuf {
    home_directory().join(".local").join("share")
}

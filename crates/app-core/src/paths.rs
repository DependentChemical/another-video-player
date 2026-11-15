//! Where Flatline keeps the files it manages itself.
//!
//! Flatline is a portable program: everything it downloads lives next to the
//! executable, so the folder can be copied to another machine and keeps
//! working. Only when the program directory cannot be written to — a system
//! wide installation on Linux, for instance — the files fall back to the per
//! user directory of the platform.

use std::path::{Path, PathBuf};

use media_model::Service;

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

    /// The directory the cookie files live in.
    ///
    /// They sit below [`AppPaths::cache`] because everything Flatline can
    /// recreate without the user's help belongs in one place.
    pub fn cookies(&self) -> PathBuf {
        self.cache().join("cookies")
    }

    /// The cookie file of a service.
    pub fn cookie_file(&self, service: Service) -> PathBuf {
        web_session::cookie_file(&self.cookies(), service)
    }

    /// The directory the browser profile of the sign-in helper lives in.
    ///
    /// The profile holds the cookies of the signed-in services, so it is
    /// treated like a credential and stays in the program's own folder.
    pub fn user_data(&self) -> PathBuf {
        self.root.join("user-data")
    }

    /// The helper that opens the sign-in window.
    ///
    /// It lives next to the player, so a copied folder keeps working and a
    /// build without it simply cannot sign in. Two fallbacks keep a plain
    /// `cargo run -p app-core` usable: an explicit `FLATLINE_SESSION_PROGRAM`
    /// override, and the sibling Cargo profile directories. A `cargo run` build
    /// of the player sits in `target/<profile>/`, and a helper compiled into a
    /// different profile is found through the `target/` parent.
    pub fn session_helper(&self) -> PathBuf {
        if let Some(program) =
            std::env::var_os("FLATLINE_SESSION_PROGRAM").filter(|value| !value.is_empty())
        {
            return PathBuf::from(program);
        }
        let beside = self.program.join(session_helper_name());
        if beside.is_file() {
            return beside;
        }
        if let Some(target) = self.program.parent() {
            for profile in ["debug", "release"] {
                let candidate = target.join(profile).join(session_helper_name());
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
        beside
    }

    /// The bundled settings document beside the executable.
    pub fn settings(&self) -> PathBuf {
        self.program.join("settings.json")
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

    /// The directory the player buffers downloaded media in.
    ///
    /// It sits below [`AppPaths::cache`] so that everything the app can
    /// recreate without the user's help is in one place.
    pub fn stream_buffer(&self) -> PathBuf {
        self.cache().join("stream_buffer")
    }

    /// The file the diagnostics are written to.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("flatline.log")
    }

    /// Creates the directories Flatline needs.
    ///
    /// Also clears any orphaned stream-buffer files that a crash or unclean
    /// shutdown may have left behind.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.tools())?;
        std::fs::create_dir_all(self.thumbnails())?;
        std::fs::create_dir_all(self.cache())?;
        std::fs::create_dir_all(self.cookies())?;
        std::fs::create_dir_all(self.stream_buffer())?;
        // The browser profile is only needed when a sign-in happens, but an
        // empty folder is cheap and keeps the first run free of surprises.
        std::fs::create_dir_all(self.user_data())?;
        // Best-effort cleanup of stale cache files from a previous run.
        if let Ok(entries) = std::fs::read_dir(self.stream_buffer()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
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

/// The name of the sign-in helper next to the executable.
fn session_helper_name() -> &'static str {
    if cfg!(windows) { "flatline-session.exe" } else { "flatline-session" }
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
        assert_eq!(paths.stream_buffer(), paths.cache().join("stream_buffer"));
        assert_eq!(paths.log_file(), PathBuf::from("/tmp/flatline").join("flatline.log"));
    }

    #[test]
    fn the_cookies_live_in_the_cache() {
        let paths = AppPaths::with_root("/tmp/flatline");
        assert_eq!(paths.cookies(), PathBuf::from("/tmp/flatline/cache").join("cookies"));
        assert_eq!(
            paths.cookie_file(Service::YouTube),
            PathBuf::from("/tmp/flatline/cache").join("cookies").join("cookies-youtube.txt")
        );
        assert_eq!(
            paths.cookie_file(Service::Twitch),
            PathBuf::from("/tmp/flatline/cache").join("cookies").join("cookies-twitch.txt")
        );
    }

    #[test]
    fn the_browser_profile_lives_in_its_own_folder() {
        let paths = AppPaths::with_root("/tmp/flatline");
        assert_eq!(paths.user_data(), PathBuf::from("/tmp/flatline").join("user-data"));
    }

    #[test]
    fn the_sign_in_helper_lives_next_to_the_player() {
        let paths = AppPaths::with_root("/tmp/flatline");
        assert_eq!(paths.session_helper().parent(), Some(Path::new("/tmp/flatline")));
        assert!(
            paths
                .session_helper()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("flatline-session")
        );
    }
}

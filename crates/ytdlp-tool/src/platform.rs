//! The platforms Flatline can be built for.
//!
//! The player is built for several systems, and each of them needs its own
//! `yt-dlp` executable and its own name for the release archive. Keeping that
//! knowledge in one place makes sure that the build and the player agree on
//! what "a Linux build" means.

/// A platform Flatline can be built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows on x86-64.
    Windows,
    /// Linux on x86-64.
    Linux,
    /// macOS on x86-64.
    MacOs,
}

impl Platform {
    /// The platform this build runs on.
    pub fn current() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }

    /// The name of the `yt-dlp` release artifact for the platform.
    ///
    /// The published artifacts are self contained executables, so the player
    /// does not need a Python installation to use them.
    pub fn ytdlp_asset(self) -> &'static str {
        match self {
            Platform::Windows => "yt-dlp.exe",
            Platform::Linux => "yt-dlp_linux",
            Platform::MacOs => "yt-dlp_macos",
        }
    }

    /// The name the managed `yt-dlp` is stored under.
    ///
    /// The release artifacts are named after the platform (`yt-dlp_linux`),
    /// but the player always looks for the same name in its tool directory,
    /// so the build renames the download.
    pub fn ytdlp_binary_name(self) -> &'static str {
        match self {
            Platform::Windows => "yt-dlp.exe",
            _ => "yt-dlp",
        }
    }

    /// The name of the player executable on the platform.
    pub fn executable_name(self) -> &'static str {
        match self {
            Platform::Windows => "flatline.exe",
            _ => "flatline",
        }
    }

    /// The name the release archive of the platform carries.
    pub fn archive_name(self) -> &'static str {
        match self {
            Platform::Windows => "windows-x64",
            Platform::Linux => "linux-x86_64",
            Platform::MacOs => "macos-x86_64",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Platform;

    #[test]
    fn every_platform_has_its_own_yt_dlp_asset() {
        let assets = [
            Platform::Windows.ytdlp_asset(),
            Platform::Linux.ytdlp_asset(),
            Platform::MacOs.ytdlp_asset(),
        ];
        let mut unique = assets.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), assets.len());
    }

    #[test]
    fn only_windows_uses_an_executable_with_a_suffix() {
        assert_eq!(Platform::Windows.executable_name(), "flatline.exe");
        assert_eq!(Platform::Linux.executable_name(), "flatline");
        assert_eq!(Platform::MacOs.executable_name(), "flatline");
    }

    #[test]
    fn the_managed_yt_dlp_uses_the_name_the_player_looks_for() {
        assert_eq!(Platform::Windows.ytdlp_binary_name(), "yt-dlp.exe");
        assert_eq!(Platform::Linux.ytdlp_binary_name(), "yt-dlp");
        assert_eq!(Platform::MacOs.ytdlp_binary_name(), "yt-dlp");
    }

    #[test]
    fn the_archive_names_are_different() {
        assert_ne!(Platform::Windows.archive_name(), Platform::Linux.archive_name());
        assert_ne!(Platform::Linux.archive_name(), Platform::MacOs.archive_name());
    }
}

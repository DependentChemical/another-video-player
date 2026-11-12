//! Keeping the `yt-dlp` executable up to date.
//!
//! On startup Flatline asks GitHub for the newest `yt-dlp` release and, when
//! it is newer than the copy in use, downloads it into the managed tool
//! directory. The check is best effort: a failing network never prevents
//! the application from starting.

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::Error;
use crate::locate::{ToolPaths, YtDlp};
use crate::platform::Platform;
use crate::version::is_newer;

/// Where the newest release is announced.
pub const RELEASES_API: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));

/// What happened during an update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// Version of the executable that is in use.
    pub current: String,
    /// Version of the newest published release, when the check succeeded.
    pub latest: Option<String>,
    /// Whether a new executable was downloaded.
    pub updated: bool,
    /// Path of the executable that should be used from now on.
    pub path: std::path::PathBuf,
}

/// A GitHub release, reduced to the fields Flatline needs.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    /// Tag of the release, e.g. `2025.10.13`.
    pub tag_name: String,
    /// Downloadable artifacts of the release.
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A downloadable artifact of a release.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    /// File name of the artifact.
    pub name: String,
    /// Direct download URL.
    pub browser_download_url: String,
}

impl Release {
    /// The artifact that matches the current platform.
    pub fn asset_for_platform(&self) -> Option<&Asset> {
        self.asset_for(Platform::current())
    }

    /// The artifact that matches the given platform.
    pub fn asset_for(&self, platform: Platform) -> Option<&Asset> {
        let wanted = platform.ytdlp_asset();
        self.assets.iter().find(|asset| asset.name == wanted)
    }
}

/// Checks for a newer release and downloads it when there is one.
///
/// The check never fails the application: network problems are reported as
/// `latest: None` and leave the current executable in place.
pub fn check_and_update(ytdlp: &YtDlp, paths: &ToolPaths) -> Result<UpdateOutcome, Error> {
    let current = ytdlp.version()?;
    let path = ytdlp.program().to_path_buf();

    let Ok(release) = latest_release() else {
        return Ok(UpdateOutcome { current, latest: None, updated: false, path });
    };

    if !is_newer(&release.tag_name, &current) {
        return Ok(UpdateOutcome { current, latest: Some(release.tag_name), updated: false, path });
    }

    let Some(asset) = release.asset_for_platform() else {
        return Ok(UpdateOutcome { current, latest: Some(release.tag_name), updated: false, path });
    };

    let destination = paths.managed_binary();
    if let Err(_error) = download(&asset.browser_download_url, &destination) {
        // A failed download must never break startup.
        return Ok(UpdateOutcome { current, latest: Some(release.tag_name), updated: false, path });
    }

    Ok(UpdateOutcome { current, latest: Some(release.tag_name), updated: true, path: destination })
}

/// Downloads the newest published release, even when no `yt-dlp` is
/// installed yet.
///
/// The path of the downloaded executable is returned.
pub fn install_latest(paths: &ToolPaths) -> Result<std::path::PathBuf, Error> {
    let release = latest_release()?;
    let asset = release.asset_for_platform().ok_or(Error::UnsupportedPlatform)?;
    let destination = paths.managed_binary();
    download(&asset.browser_download_url, &destination)?;
    Ok(destination)
}

/// Downloads the newest published release for another platform.
///
/// The build uses this to put a `yt-dlp` next to the player it packages, so
/// that the folder works without a network connection.
pub fn download_for_platform(
    platform: Platform,
    destination: &Path,
) -> Result<std::path::PathBuf, Error> {
    let release = latest_release()?;
    let asset = release.asset_for(platform).ok_or(Error::UnsupportedPlatform)?;
    download(&asset.browser_download_url, destination)?;
    Ok(destination.to_path_buf())
}

/// Asks GitHub for the newest published release.
pub fn latest_release() -> Result<Release, Error> {
    let mut response = ureq::get(RELEASES_API)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| Error::Http(e.to_string()))?;
    response.body_mut().read_json::<Release>().map_err(|e| Error::Parse(e.to_string()))
}

/// Downloads a file and puts it in place atomically.
///
/// The payload is written next to the destination first and only then moved
/// into place, so an interrupted download never leaves a broken executable
/// behind.
pub fn download(url: &str, destination: &Path) -> Result<(), Error> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| Error::Http(e.to_string()))?;

    let temporary = temporary_path(destination);
    {
        let mut file = fs::File::create(&temporary)?;
        let mut reader = response.body_mut().as_reader();
        io::copy(&mut reader, &mut file)?;
        file.sync_all()?;
    }

    replace(&temporary, destination)?;
    Ok(())
}

/// Path of the scratch file used while downloading.
fn temporary_path(destination: &Path) -> std::path::PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    destination.with_file_name(name)
}

/// Moves `from` onto `to`, replacing an existing file.
fn replace(from: &Path, to: &Path) -> Result<(), Error> {
    if to.exists() {
        // Windows refuses to rename over a running file, so drop the old copy
        // first. A failure here is not fatal: the download is simply unused.
        let _ = fs::remove_file(to);
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(from);
            Err(Error::Io(error))
        }
    }
}

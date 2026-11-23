//! Keeping the `yt-dlp` executable up to date.
//!
//! Flatline checks GitHub for the newest `yt-dlp` release on a schedule and,
//! when the user accepts the update, downloads it for the current platform
//! and replaces the managed copy the next time `yt-dlp` is not in use.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Error;
use crate::locate::ToolPaths;
use crate::platform::Platform;
use crate::version::is_newer;

/// Where the newest release is announced.
pub const RELEASES_API: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

pub const BUN_VERSION: &str = "bun-v1.3.14";

const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));

/// A GitHub release, reduced to the fields Flatline needs.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    /// Tag of the release, e.g. `2025.10.13`.
    pub tag_name: String,
    /// Release notes for the changelog shown to the user.
    #[serde(default)]
    pub body: String,
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
    /// The version string of the release without a leading `v`.
    pub fn version(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }

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

/// A newer `yt-dlp` release that can be downloaded for this platform.
#[derive(Debug, Clone)]
pub struct ToolUpdate {
    /// The version string of the installed copy.
    pub current: String,
    /// The version string of the newest release.
    pub latest: String,
    /// Release notes to show in the update dialog.
    pub notes: String,
    /// The direct download URL for the current platform.
    pub download_url: String,
}

/// Compares the installed version with the newest release for this platform.
///
/// The network check is best effort: failures are returned to the caller so
/// background polling can log them without disturbing the user.
pub fn check(current: &str) -> Result<Option<ToolUpdate>, Error> {
    let release = latest_release()?;
    let latest = release.version().to_string();
    app_logging::debug(format!("comparing yt-dlp versions: current={current} latest={latest}"));
    if !is_newer(&latest, current) {
        return Ok(None);
    }
    let Some(asset) = release.asset_for_platform() else {
        app_logging::warning(format!(
            "the latest yt-dlp release has no asset for {:?}: latest={latest}",
            Platform::current()
        ));
        return Err(Error::UnsupportedPlatform);
    };
    let download_url = asset.browser_download_url.clone();
    app_logging::notice(format!(
        "yt-dlp update detected: current={current} latest={latest} asset={} url={download_url}",
        asset.name
    ));
    Ok(Some(ToolUpdate { current: current.to_string(), latest, notes: release.body, download_url }))
}

/// Returns the newest release for this platform as an installable update.
///
/// This is used when `yt-dlp` is not installed yet and the user must download
/// it before Flatline can resolve media.
pub fn latest_available() -> Result<ToolUpdate, Error> {
    let release = latest_release()?;
    let latest = release.version().to_string();
    let Some(asset) = release.asset_for_platform() else {
        app_logging::warning(format!(
            "the latest yt-dlp release has no asset for {:?}: latest={latest}",
            Platform::current()
        ));
        return Err(Error::UnsupportedPlatform);
    };
    let download_url = asset.browser_download_url.clone();
    app_logging::notice(format!(
        "offering yt-dlp install: latest={latest} asset={} url={download_url}",
        asset.name
    ));
    Ok(ToolUpdate {
        current: "not installed".to_string(),
        latest,
        notes: release.body,
        download_url,
    })
}

/// Downloads the newest published release, even when no `yt-dlp` is
/// installed yet.
///
/// The path of the downloaded executable is returned.
pub fn install_latest(paths: &ToolPaths) -> Result<PathBuf, Error> {
    let release = latest_release()?;
    let asset = release.asset_for_platform().ok_or_else(|| {
        app_logging::warning(format!(
            "the latest yt-dlp release has no asset for {:?}: version={}",
            Platform::current(),
            release.version()
        ));
        Error::UnsupportedPlatform
    })?;
    let destination = paths.managed_binary();
    app_logging::notice(format!(
        "installing yt-dlp {} into {}",
        release.version(),
        destination.display()
    ));
    download(&asset.browser_download_url, &destination)?;
    Ok(destination)
}

/// Downloads the pinned Bun runtime for this platform into the tool directory.
///
/// The asset is a zip archive, so the `bun(.exe)` binary is extracted out of
/// it and stored under the managed tool directory. The version is pinned so the
/// JavaScript runtime cannot drift away from the EJS scripts bundled with the
/// pinned `yt-dlp` release.
pub fn install_bun(paths: &ToolPaths) -> Result<PathBuf, Error> {
    let asset = Platform::current().bun_asset();
    let url = format!("https://github.com/oven-sh/bun/releases/download/{BUN_VERSION}/{asset}");
    let binary = paths.root().join(Platform::current().bun_binary_name());
    let archive = paths.root().join(format!("{}.zip", Platform::current().bun_binary_name()));

    app_logging::notice(format!("installing bun {} into {}", BUN_VERSION, paths.root().display()));

    download(&url, &archive)?;
    let archive_file = fs::File::open(&archive).map_err(Error::Io)?;
    let mut archive_zip = zip::ZipArchive::new(&archive_file)
        .map_err(|error| Error::Parse(format!("the bun archive could not be read: {error}")))?;

    // Extract the bun executable from the archive.
    let bin_name = Platform::current().bun_binary_name().to_string();
    for index in 0..archive_zip.len() {
        let mut entry = archive_zip
            .by_index(index)
            .map_err(|error| Error::Parse(format!("the bun archive could not be read: {error}")))?;
        let full_name = entry.name().to_string();
        let file_name = std::path::Path::new(&full_name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&full_name);
        if file_name != bin_name.as_str() {
            continue;
        }
        let mut out = fs::File::create(&binary).map_err(Error::Io)?;
        let _ = std::io::copy(&mut entry, &mut out);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = out.metadata().map_err(Error::Io)?.permissions();
            permissions.set_mode(0o755);
            out.set_permissions(permissions).map_err(Error::Io)?;
        }
        break;
    }
    let _ = fs::remove_file(&archive);
    if !binary.is_file() {
        return Err(Error::Parse("the bun archive did not contain the bun executable".to_string()));
    }
    app_logging::notice(format!("bun {} installed at {}", BUN_VERSION, binary.display()));
    Ok(binary)
}

/// Downloads the newest published release for another platform.
///
/// The build uses this to put a `yt-dlp` next to the player it packages, so
/// that the folder works without a network connection.
pub fn download_for_platform(platform: Platform, destination: &Path) -> Result<PathBuf, Error> {
    let release = latest_release()?;
    let asset = release.asset_for(platform).ok_or_else(|| {
        app_logging::warning(format!(
            "yt-dlp release has no asset for {platform:?}: version={}",
            release.version()
        ));
        Error::UnsupportedPlatform
    })?;
    app_logging::notice(format!(
        "downloading yt-dlp {} for {platform:?} into {}",
        release.version(),
        destination.display()
    ));
    download(&asset.browser_download_url, destination)?;
    Ok(destination.to_path_buf())
}

/// Asks GitHub for the newest published release.
pub fn latest_release() -> Result<Release, Error> {
    app_logging::debug(format!("requesting the latest yt-dlp release: url={RELEASES_API}"));
    let mut response =
        ureq::get(RELEASES_API).header("User-Agent", USER_AGENT).call().map_err(|e| {
            app_logging::warning(format!("the yt-dlp release request failed: {e}"));
            Error::Http(e.to_string())
        })?;
    response.body_mut().read_json::<Release>().map_err(|e| {
        app_logging::warning(format!("the yt-dlp release response was invalid: {e}"));
        Error::Parse(e.to_string())
    })
}

/// Downloads a file and puts it in place atomically.
///
/// The payload is written next to the destination first and only then moved
/// into place, so an interrupted download never leaves a broken executable
/// behind.
pub fn download(url: &str, destination: &Path) -> Result<(), Error> {
    download_with_progress(url, destination, |_| {})
}

/// Downloads a file, reports progress, and puts it in place atomically.
///
/// `on_progress` receives a fraction from zero to one while the payload is
/// being written and is called with `1.0` once the file is complete.
pub fn download_with_progress<F>(
    url: &str,
    destination: &Path,
    mut on_progress: F,
) -> Result<(), Error>
where
    F: FnMut(f64),
{
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    app_logging::notice(format!(
        "downloading yt-dlp: url={url} destination={}",
        destination.display()
    ));
    let response = ureq::get(url).header("User-Agent", USER_AGENT).call().map_err(|e| {
        app_logging::error(format!("the yt-dlp download request failed: url={url} error={e}"));
        Error::Http(e.to_string())
    })?;

    let content_length = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    app_logging::debug(format!("yt-dlp download started: url={url} bytes={content_length}"));

    let temporary = temporary_path(destination);
    {
        let mut file = fs::File::create(&temporary)?;
        let mut reader = response.into_body().into_reader();
        let mut downloaded: u64 = 0;
        let mut buffer = vec![0u8; 65536];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])?;
            downloaded += read as u64;
            if content_length > 0 {
                let fraction = downloaded as f64 / content_length as f64;
                on_progress(fraction.min(1.0));
            }
        }
        file.sync_all()?;
        app_logging::debug(format!(
            "yt-dlp download body finished: url={url} bytes={downloaded} expected={content_length}"
        ));
    }

    on_progress(1.0);
    replace(&temporary, destination)?;
    make_executable(destination)?;
    app_logging::notice(format!(
        "yt-dlp download complete: url={url} destination={}",
        destination.display()
    ));
    Ok(())
}

/// Path of the scratch file used while downloading.
fn temporary_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    destination.with_file_name(name)
}

/// Moves `from` onto `to`, replacing an existing file when possible.
pub fn replace(from: &Path, to: &Path) -> Result<(), Error> {
    if to.exists() {
        let _ = fs::remove_file(to);
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(from);
            app_logging::error(format!(
                "could not move {} into place at {}: {error}",
                from.display(),
                to.display()
            ));
            Err(Error::Io(error))
        }
    }
}

/// Ensures a downloaded tool can be executed on Unix platforms.
fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.is_file() {
            let mut permissions = fs::metadata(path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions)?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

//! GitHub release discovery and portable application updates.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns update discovery and installation. It knows nothing about
//! the player or its interface.

use std::io::Write;
use std::path::{Path, PathBuf};

const RELEASE: &str =
    "https://api.github.com/repos/DependentChemical/another-video-player/releases/latest";
const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));

/// A release newer than the running application.
#[derive(Debug, Clone)]
pub struct Update {
    /// Version parsed from the release tag.
    pub version: String,
    /// Release notes shown before installation.
    pub notes: String,
    /// Platform package URL selected from the release assets.
    pub download_url: String,
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: String,
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Checks GitHub for a newer release and selects this platform's ZIP asset.
pub fn check(current: &str) -> Result<Option<Update>, Error> {
    let release: Release = ureq::get(RELEASE)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| Error::Request(error.to_string()))?
        .body_mut()
        .read_json()
        .map_err(|error| Error::Request(error.to_string()))?;
    let version = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name);
    if !newer(version, current) {
        return Ok(None);
    }
    let suffix = if cfg!(windows) { "windows-x64.zip" } else { "linux-x86_64.zip" };
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name.ends_with(suffix))
        .ok_or_else(|| Error::MissingAsset(suffix.to_string()))?;
    Ok(Some(Update {
        version: version.to_string(),
        notes: release.body,
        download_url: asset.browser_download_url,
    }))
}

/// Downloads a package and starts a platform helper that replaces the app,
/// then starts the new executable after this process exits.
pub fn install(update: &Update, directory: &Path, executable: &Path) -> Result<(), Error> {
    let response = ureq::get(&update.download_url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| Error::Request(error.to_string()))?;
    let archive = directory.join(".flatline-update.zip");
    let mut output = std::fs::File::create(&archive).map_err(io_error)?;
    let mut reader = response.into_body().into_reader();
    std::io::copy(&mut reader, &mut output).map_err(io_error)?;
    output.flush().map_err(io_error)?;
    let staging = directory.join(".flatline-update-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(io_error)?;
    }
    std::fs::create_dir_all(&staging).map_err(io_error)?;
    let file = std::fs::File::open(&archive).map_err(io_error)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| Error::Archive(error.to_string()))?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| Error::Archive(error.to_string()))?;
        let Some(relative) = entry.enclosed_name() else { continue };
        let relative = relative.components().skip(1).collect::<PathBuf>();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = staging.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(destination).map_err(io_error)?;
        } else {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(io_error)?;
            }
            let mut file = std::fs::File::create(destination).map_err(io_error)?;
            std::io::copy(&mut entry, &mut file).map_err(io_error)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["flatline", "tools/yt-dlp"] {
            let path = staging.join(name);
            if path.is_file() {
                let mut permissions = std::fs::metadata(&path).map_err(io_error)?.permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(path, permissions).map_err(io_error)?;
            }
        }
    }
    let helper = write_helper(directory, &staging, &archive, executable)?;
    launch_helper(&helper)?;
    Ok(())
}

fn write_helper(
    directory: &Path,
    staging: &Path,
    archive: &Path,
    executable: &Path,
) -> Result<PathBuf, Error> {
    #[cfg(windows)]
    let (path, body) = {
        let path = directory.join(".flatline-update.cmd");
        let body = format!(
            "@echo off\r\n:wait\r\ntimeout /t 1 /nobreak >nul\r\nmove /y \"{}\\flatline.exe\" \"{}\\flatline.exe\" >nul 2>&1 || goto wait\r\nrobocopy \"{}\" \"{}\" /E /MOVE >nul\r\ndel \"{}\"\r\nstart \"\" \"{}\"\r\nrmdir /s /q \"{}\"\r\ndel \"%~f0\"\r\n",
            staging.display(),
            directory.display(),
            staging.display(),
            directory.display(),
            archive.display(),
            executable.display(),
            staging.display()
        );
        (path, body)
    };
    #[cfg(not(windows))]
    let (path, body) = {
        let path = directory.join(".flatline-update.sh");
        let body = format!(
            "#!/bin/sh\nmv -f \"{}\" \"{}\"\ncp -R \"{}\"/. \"{}\"/\nrm -rf \"{}\" \"{}\"\n\"{}\" &\nrm -- \"$0\"\n",
            staging.join("flatline").display(),
            executable.display(),
            staging.display(),
            directory.display(),
            staging.display(),
            archive.display(),
            executable.display()
        );
        (path, body)
    };
    std::fs::write(&path, body).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(io_error)?;
    }
    Ok(path)
}

#[cfg(windows)]
fn launch_helper(path: &Path) -> Result<(), Error> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", path.to_string_lossy().as_ref()])
        .spawn()
        .map_err(io_error)?;
    std::process::exit(0)
}
#[cfg(not(windows))]
fn launch_helper(path: &Path) -> Result<(), Error> {
    std::process::Command::new("sh").arg(path).spawn().map_err(io_error)?;
    std::process::exit(0)
}

fn newer(candidate: &str, current: &str) -> bool {
    let parse = |value: &str| {
        value.split('.').map(|part| part.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>()
    };
    parse(candidate) > parse(current)
}
fn io_error(error: std::io::Error) -> Error {
    Error::Io(error.to_string())
}

/// Why an update check or installation failed.
#[derive(Debug)]
pub enum Error {
    /// GitHub could not be reached or its response could not be read.
    Request(String),
    /// The release does not include the package for this platform.
    MissingAsset(String),
    /// The downloaded package could not be unpacked safely.
    Archive(String),
    /// A local file operation or helper launch failed.
    Io(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(message) => write!(f, "GitHub release request failed: {message}"),
            Self::MissingAsset(name) => write!(f, "release has no {name} package"),
            Self::Archive(message) => write!(f, "release package is invalid: {message}"),
            Self::Io(message) => write!(f, "update could not be installed: {message}"),
        }
    }
}
impl std::error::Error for Error {}

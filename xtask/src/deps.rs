//! Preparing the external dependencies.
//!
//! Flatline links against `libmpv`. On Windows the library is not part of
//! the system, so the task downloads the development package of the mpv
//! Windows builds and unpacks it into `.flatline/deps/mpv`.
//!
//! Everything else is expected to be on the machine already: `ffmpeg` for
//! the preview images, and `yt-dlp`, which the player downloads for itself
//! when it is missing.

use std::path::{Path, PathBuf};

#[cfg(windows)]
use serde::Deserialize;

/// Where the mpv Windows builds are published.
#[cfg(windows)]
const MPV_RELEASES: &str =
    "https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases/latest";

#[cfg(windows)]
const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));

/// A GitHub release.
#[cfg(windows)]
#[derive(Debug, Deserialize)]
struct Release {
    /// The downloadable artifacts.
    assets: Vec<Asset>,
}

/// A downloadable artifact.
#[cfg(windows)]
#[derive(Debug, Deserialize)]
struct Asset {
    /// File name of the artifact.
    name: String,
    /// Direct download URL.
    browser_download_url: String,
}

/// The root of the workspace.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The directory the downloaded dependencies live in.
pub fn deps_root() -> PathBuf {
    workspace_root().join(".flatline").join("deps")
}

/// The directory `libmpv` was unpacked into.
pub fn mpv_root() -> PathBuf {
    deps_root().join("mpv")
}

/// The directory that holds the import library of `libmpv`.
pub fn mpv_library_dir() -> Option<PathBuf> {
    let directory = mpv_root().join("lib");
    directory.is_dir().then_some(directory)
}

/// The version used for release tags and archive names.
///
/// The app embeds this same version at compile time; the version file itself
/// is not shipped with the app.
pub fn version() -> Result<String, String> {
    let file = workspace_root().join("version.txt");
    let text = std::fs::read_to_string(&file).map_err(|error| {
        format!("the version could not be read from {}: {error}", file.display())
    })?;
    let version = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    if version.is_empty() {
        return Err(format!("{} is empty", file.display()));
    }
    Ok(version.to_string())
}

/// Prepares every external dependency.
pub fn setup() -> Result<(), String> {
    #[cfg(windows)]
    setup_mpv()?;

    #[cfg(not(windows))]
    {
        // On the other platforms the system library is used, and cargo finds
        // it through `pkg-config`.
    }

    Ok(())
}

/// Downloads and unpacks `libmpv` for Windows.
#[cfg(windows)]
fn setup_mpv() -> Result<(), String> {
    let library = mpv_root().join("lib");
    if library.join("mpv.lib").exists() {
        return Ok(());
    }

    println!("xtask: downloading libmpv…");
    std::fs::create_dir_all(deps_root()).map_err(|error| error.to_string())?;

    let release: Release = fetch_json(MPV_RELEASES)?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name.starts_with("mpv-dev-x86_64-") && !asset.name.contains("-v3-"))
        .ok_or("the mpv release does not contain a development package for Windows")?;

    let archive = deps_root().join(&asset.name);
    download(&asset.browser_download_url, &archive)?;

    let staging = deps_root().join("staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;

    sevenz_rust2::decompress_file(&archive, &staging)
        .map_err(|error| format!("the archive could not be unpacked: {error}"))?;
    let _ = std::fs::remove_file(&archive);

    // The development package holds the import library, the runtime and the
    // headers. Their names differ between builds, so they are looked up.
    let import = find_file(&staging, &["libmpv.dll.a", "libmpv-2.dll.a", "mpv.lib", "libmpv.lib"])
        .ok_or("the archive does not contain the import library of libmpv")?;
    let runtime = find_file(&staging, &["libmpv-2.dll", "mpv-2.dll"]);

    let target = mpv_root();
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(target.join("lib")).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(target.join("bin")).map_err(|error| error.to_string())?;
    std::fs::copy(&import, target.join("lib").join("mpv.lib"))
        .map_err(|error| format!("the import library could not be installed: {error}"))?;
    if let Some(runtime) = runtime {
        let name = runtime.file_name().unwrap_or_default();
        std::fs::copy(&runtime, target.join("bin").join(name))
            .map_err(|error| format!("the mpv runtime could not be installed: {error}"))?;
    }
    if let Some(include) = find_directory(&staging, "include") {
        let _ = copy_directory(&include, &target.join("include"));
    }

    let _ = std::fs::remove_dir_all(&staging);

    println!("xtask: libmpv is ready");
    Ok(())
}

/// Searches a directory tree for a file with one of the given names.
#[cfg(windows)]
fn find_file(root: &Path, names: &[&str]) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            if names.iter().any(|wanted| wanted.to_lowercase() == name) {
                return Some(path);
            }
        } else if path.is_dir()
            && let Some(found) = find_file(&path, names)
        {
            return Some(found);
        }
    }
    None
}

/// Searches a directory tree for a directory with the given name.
#[cfg(windows)]
fn find_directory(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().unwrap_or_default() == name {
            return Some(path);
        }
        if let Some(found) = find_directory(&path, name) {
            return Some(found);
        }
    }
    None
}

/// Copies a directory tree.
#[cfg(windows)]
fn copy_directory(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Downloads a file.
#[cfg(windows)]
fn download(url: &str, destination: &Path) -> Result<(), String> {
    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| format!("the download failed: {error}"))?;

    let mut file = std::fs::File::create(destination).map_err(|error| error.to_string())?;
    let mut reader = response.body_mut().as_reader();
    std::io::copy(&mut reader, &mut file).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    Ok(())
}

/// Fetches and parses a JSON document.
#[cfg(windows)]
fn fetch_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| format!("the request failed: {error}"))?;
    response
        .body_mut()
        .read_json::<T>()
        .map_err(|error| format!("the response could not be understood: {error}"))
}

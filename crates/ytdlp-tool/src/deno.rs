//! The Deno runtime that solves the `yt-dlp` EJS challenges.
//!
//! `yt-dlp` needs a JavaScript runtime to solve the YouTube challenge
//! scripts. Flatline carries its own Deno so the player works on a machine
//! that has no runtime installed.
//!
//! The runtime is not pinned to a version: the download points at the
//! newest published release, so a fresh Deno is never held back by the
//! version that happened to ship with Flatline. The release archive holds
//! the executable under the name of the platform, which is what the
//! extraction looks for.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::locate::ToolPaths;
use crate::platform::Platform;
use crate::update::download;

/// Where the newest Deno release is downloaded from.
///
/// The `latest` alias of a release always points at the newest published
/// version, so the runtime never has to be pinned and updated by hand.
pub const RELEASES_URL: &str = "https://github.com/denoland/deno/releases/latest/download";

/// The download URL of the Deno runtime for a platform.
pub fn download_url(platform: Platform) -> String {
    format!("{RELEASES_URL}/{}", platform.deno_asset())
}

/// Downloads the newest Deno runtime for this platform into the tool
/// directory and returns the path of the executable.
///
/// The runtime lands next to `yt-dlp` in `tools/`, and the directory that
/// holds the files Deno writes at runtime is created beside it, so the
/// whole folder can be copied to another machine.
pub fn install(paths: &ToolPaths) -> Result<PathBuf, Error> {
    let platform = Platform::current();
    let url = download_url(platform);
    let binary = paths.root().join(platform.deno_binary_name());
    let archive = paths.root().join(format!("{}.zip", platform.deno_binary_name()));

    app_logging::notice(format!("installing deno from {url} into {}", paths.root().display()));

    download(&url, &archive)?;
    let extracted = extract(&archive, &binary, platform);
    let _ = fs::remove_file(&archive);
    extracted?;

    // The runtime writes its cache and its configuration here. The
    // directory is part of the portable folder, never of the user's home.
    let files = paths.deno_files_dir();
    fs::create_dir_all(&files).map_err(Error::Io)?;

    app_logging::notice(format!("deno {} installed at {}", version_of(&binary), binary.display()));
    Ok(binary)
}

/// Downloads the newest Deno runtime for another platform into a
/// directory.
///
/// The build uses this to put a Deno next to the player it packages, so
/// that the folder works without a network connection.
pub fn download_for_platform(platform: Platform, tools: &Path) -> Result<PathBuf, Error> {
    let url = download_url(platform);
    let binary = tools.join(platform.deno_binary_name());
    let archive = tools.join(format!("{}.zip", platform.deno_binary_name()));

    app_logging::notice(format!(
        "downloading deno from {url} for {platform:?} into {}",
        tools.display()
    ));

    download(&url, &archive)?;
    let extracted = extract(&archive, &binary, platform);
    let _ = fs::remove_file(&archive);
    extracted?;

    // The directory Deno writes its files to at runtime.
    fs::create_dir_all(tools.join("deno-files")).map_err(Error::Io)?;

    app_logging::notice(format!(
        "deno {} downloaded for {platform:?} into {}",
        version_of(&binary),
        binary.display()
    ));
    Ok(binary)
}

/// Takes the Deno executable out of a release archive.
///
/// The archive holds the executable under the name of the platform, so the
/// entry is found by its file name and not by its path inside the archive:
/// the path differs between the archives of the platforms.
pub fn extract(archive: &Path, destination: &Path, platform: Platform) -> Result<(), Error> {
    let wanted = platform.deno_binary_name();
    let archive_file = fs::File::open(archive).map_err(Error::Io)?;
    let mut archive_zip = zip::ZipArchive::new(&archive_file)
        .map_err(|error| Error::Parse(format!("the deno archive could not be read: {error}")))?;

    for index in 0..archive_zip.len() {
        let mut entry = archive_zip.by_index(index).map_err(|error| {
            Error::Parse(format!("the deno archive could not be read: {error}"))
        })?;
        let full_name = entry.name().to_string();
        let file_name =
            Path::new(&full_name).file_name().and_then(|name| name.to_str()).unwrap_or(&full_name);
        if file_name != wanted {
            continue;
        }
        let mut out = fs::File::create(destination).map_err(Error::Io)?;
        std::io::copy(&mut entry, &mut out).map_err(Error::Io)?;
        out.sync_all().map_err(Error::Io)?;
        make_executable(destination)?;
        return Ok(());
    }

    Err(Error::Parse(format!("the deno archive did not contain the {wanted} executable")))
}

/// The version of an installed runtime.
///
/// The runtime is asked for its version, because the download does not
/// name the version it carries. A runtime that does not answer leaves the
/// version unknown, which is no reason to fail the installation.
pub fn version_of(binary: &Path) -> String {
    let Ok(output) = std::process::Command::new(binary).arg("--version").output() else {
        return "an unknown version".to_string();
    };
    if !output.status.success() {
        return "an unknown version".to_string();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    if version.is_empty() { "an unknown version".to_string() } else { version }
}

/// Makes the extracted executable runnable on Unix platforms.
fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{RELEASES_URL, download_url, version_of};
    use crate::platform::Platform;

    #[test]
    fn the_download_points_at_the_newest_release() {
        assert!(RELEASES_URL.contains("/releases/latest/download"));
    }

    #[test]
    fn the_url_carries_the_asset_of_the_platform() {
        assert_eq!(
            download_url(Platform::Windows),
            format!("{RELEASES_URL}/deno-x86_64-pc-windows-msvc.zip")
        );
        assert_eq!(
            download_url(Platform::Linux),
            format!("{RELEASES_URL}/deno-x86_64-unknown-linux-gnu.zip")
        );
        assert_eq!(
            download_url(Platform::MacOs),
            format!("{RELEASES_URL}/deno-x86_64-apple-darwin.zip")
        );
    }

    #[test]
    fn a_runtime_that_does_not_answer_has_no_version() {
        let missing = std::env::temp_dir().join("flatline-does-not-exist-deno");
        assert_eq!(version_of(&missing), "an unknown version");
    }
}

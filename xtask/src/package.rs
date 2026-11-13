//! Collecting the player in a single archive.
//!
//! The result is one `.zip` per platform. It holds the player, the mpv
//! runtime it needs and the `yt-dlp` it resolves URLs with, so the folder
//! can be copied to another machine and works there without a network
//! connection.

use std::path::{Path, PathBuf};

use ytdlp_tool::Platform;

use crate::deps::{mpv_root, version, workspace_root};

/// The name of the folder the player lives in inside the archive.
const FOLDER: &str = "flatline";

/// Collects the player and writes the archive.
pub fn package() -> Result<PathBuf, String> {
    let root = workspace_root();
    let version = version()?;
    let platform = Platform::current();

    let binary = root.join("target").join("release").join(platform.executable_name());
    if !binary.is_file() {
        return Err("there is no release build yet; run `cargo xtask build` first".to_string());
    }

    let distribution = root.join("dist");
    let staging = distribution
        .join("staging")
        .join(format!("{FOLDER}-{version}-{}", platform.archive_name()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;

    // The player itself.
    copy(&binary, &staging.join(platform.executable_name()))?;

    // The mpv runtime has to sit next to the executable: the loader searches
    // the directory of the executable, not a subdirectory of it.
    copy_mpv_runtime(&staging)?;

    // On the other platforms the system library is used at build time; the
    // runtime is copied in when `pkg-config` can point at it, so the folder
    // is portable there too.
    copy_system_mpv_runtime(&staging)?;

    // The tool that resolves the URLs, so the folder works offline.
    copy_ytdlp(&staging, platform)?;

    // Files bundled with the player, placed next to the executable.
    copy_includes(&root, &staging)?;

    let archive = distribution.join(format!("{FOLDER}-{version}-{}.zip", platform.archive_name()));
    compress(&staging, &archive)?;

    let _ = std::fs::remove_dir_all(distribution.join("staging"));

    Ok(archive)
}

/// Copies the mpv runtime into the folder.
fn copy_mpv_runtime(staging: &Path) -> Result<(), String> {
    let runtime = mpv_root().join("bin");
    if !runtime.is_dir() {
        // On the other platforms the system library is used.
        return Ok(());
    }
    for entry in std::fs::read_dir(&runtime).map_err(|error| error.to_string())?.flatten() {
        let path = entry.path();
        if path.is_file() {
            copy(&path, &staging.join(path.file_name().unwrap()))?;
        }
    }
    Ok(())
}

/// Copies the mpv runtime the system provides into the folder.
///
/// The player links against the system library on Linux and macOS, so the
/// archive has to carry the shared object as well. The lookup is best effort:
/// when `pkg-config` cannot answer, the package still builds and simply
/// expects the system library to be installed.
#[cfg(not(windows))]
fn copy_system_mpv_runtime(staging: &Path) -> Result<(), String> {
    let output =
        std::process::Command::new("pkg-config").args(["--variable=libdir", "mpv"]).output();
    let Ok(output) = output else {
        return Ok(());
    };
    if !output.status.success() {
        return Ok(());
    }
    let directory = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        let is_mpv = name.starts_with("libmpv.so.")
            || (name.starts_with("libmpv.") && name.ends_with(".dylib"));
        // Only real files: the `.so` name without a version is a development
        // symlink and has no business in a release.
        if is_mpv && path.is_file() {
            copy(&path, &staging.join(path.file_name().unwrap()))?;
        }
    }
    Ok(())
}

/// The same step does nothing on Windows, where the runtime was downloaded.
#[cfg(windows)]
fn copy_system_mpv_runtime(_staging: &Path) -> Result<(), String> {
    Ok(())
}

/// Puts a `yt-dlp` next to the player.
fn copy_ytdlp(staging: &Path, platform: Platform) -> Result<(), String> {
    let tools = staging.join("tools");
    std::fs::create_dir_all(&tools).map_err(|error| error.to_string())?;
    // The file is stored under the name the player looks for, not under the
    // name the release artifact happens to have: on Linux the published
    // artifact is `yt-dlp_linux`, but the player manages `tools/yt-dlp`.
    let destination = tools.join(platform.ytdlp_binary_name());

    ytdlp_tool::update::download_for_platform(platform, &destination)
        .map_err(|error| format!("yt-dlp could not be downloaded: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions =
            std::fs::metadata(&destination).map_err(|error| error.to_string())?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&destination, permissions).map_err(|error| error.to_string())?;
    }

    Ok(())
}

/// Packs the folder into a zip archive.
fn compress(staging: &Path, archive: &Path) -> Result<(), String> {
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    let file = std::fs::File::create(archive).map_err(|error| error.to_string())?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let folder = staging
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| FOLDER.to_string());

    add_directory(&mut writer, staging, &folder, options)?;
    writer.finish().map_err(|error| error.to_string())?;
    Ok(())
}

/// Adds a directory tree to the archive, keeping the directory structure.
///
/// The `yt-dlp` the player manages lives in a `tools/` subdirectory, so the
/// archive has to carry subdirectories and not only the top level files.
fn add_directory(
    writer: &mut zip::ZipWriter<std::fs::File>,
    directory: &Path,
    prefix: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<(), String> {
    for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())?.flatten() {
        let path = entry.path();
        let name = format!("{prefix}/{}", path.file_name().unwrap_or_default().to_string_lossy());
        if path.is_dir() {
            writer.add_directory(format!("{name}/"), options).map_err(|error| error.to_string())?;
            add_directory(writer, &path, &name, options)?;
        } else if path.is_file() {
            writer.start_file(name, options).map_err(|error| error.to_string())?;
            let mut source = std::fs::File::open(&path).map_err(|error| error.to_string())?;
            std::io::copy(&mut source, writer).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Copies bundled files from `includes/` into the staged package.
fn copy_includes(root: &Path, staging: &Path) -> Result<(), String> {
    let source = root.join("includes");
    if !source.is_dir() {
        return Ok(());
    }
    copy_directory_contents(&source, staging)
}

/// Recursively copies the contents of a directory into another directory.
fn copy_directory_contents(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|error| error.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|error| error.to_string())?.flatten() {
        let path = entry.path();
        let destination = to.join(path.file_name().unwrap_or_default());
        if path.is_dir() {
            copy_directory_contents(&path, &destination)?;
        } else if path.is_file() {
            copy(&path, &destination)?;
        }
    }
    Ok(())
}

/// Copies a file, creating the directory of the destination.
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _ = std::fs::remove_file(to);
    std::fs::copy(from, to)
        .map_err(|error| format!("{} could not be copied: {error}", from.display()))?;
    Ok(())
}

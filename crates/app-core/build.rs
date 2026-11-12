//! Reads the version of Flatline from `version.txt`.
//!
//! `version.txt` is the reference for the version of the project: the
//! release workflow reads it to name the tag and the release, and the player
//! shows it. Cargo needs a version of its own, so the test
//! `the_version_file_matches_the_crate_version` keeps the two in step.

use std::path::Path;

fn main() {
    let version_file = manifest_dir().join("version.txt");
    println!("cargo:rerun-if-changed={}", version_file.display());

    let version = read_version(&version_file).unwrap_or_else(|error| {
        panic!("the version could not be read from {}: {error}", version_file.display())
    });
    println!("cargo:rustc-env=FLATLINE_VERSION={version}");

    // The build puts the mpv runtime next to the executable, so the loader
    // has to look there first. On Windows the loader does that by itself.
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    } else if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
    }
}

/// The directory the workspace root lives in.
fn manifest_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Reads the version from a file.
///
/// The file holds a single line with the version. Anything around it is
/// ignored, so the file can carry a comment.
fn read_version(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let version = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    if version.is_empty() {
        return Err("the file is empty".to_string());
    }
    if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(format!("{version:?} is not a version number"));
    }
    Ok(version.to_string())
}

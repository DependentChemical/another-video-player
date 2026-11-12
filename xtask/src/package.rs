//! Collecting the player in `dist/`.
//!
//! The result is a directory that holds the executable and the mpv runtime
//! next to it, so it can be started without anything else being installed.

use std::path::Path;

use crate::deps::{mpv_root, workspace_root};

/// Copies the release build into `dist/`.
pub fn package() -> Result<(), String> {
    let root = workspace_root();
    let binary = root.join("target").join("release").join(if cfg!(windows) {
        "flatline.exe"
    } else {
        "flatline"
    });

    if !binary.is_file() {
        return Err("there is no release build yet; run `cargo xtask build` first".to_string());
    }

    let distribution = root.join("dist");
    // Start from an empty directory, so a stale file from an earlier build
    // can never end up in a release.
    if distribution.exists() {
        std::fs::remove_dir_all(&distribution).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir_all(&distribution).map_err(|error| error.to_string())?;
    copy(&binary, &distribution.join(binary.file_name().unwrap()))?;

    // The mpv runtime has to sit next to the executable: the Windows loader
    // searches the directory of the executable, not a subdirectory of it.
    let runtime = mpv_root().join("bin");
    if runtime.is_dir()
        && let Ok(entries) = std::fs::read_dir(&runtime)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                copy(&path, &distribution.join(path.file_name().unwrap()))?;
            }
        }
    }

    println!("xtask: the player is ready in {}", distribution.display());
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

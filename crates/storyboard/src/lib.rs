//! Generating preview images for the seek bar with `ffmpeg`.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns one capability: *cutting a filmstrip out of a media
//! file*. It knows nothing about playback, resolution or the user
//! interface — it only produces a list of image paths and the time each of
//! them represents.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Everything that can go wrong while building a filmstrip.
#[derive(Debug)]
pub enum Error {
    /// `ffmpeg` could not be started.
    Spawn(std::io::Error),
    /// `ffmpeg` ran but failed.
    Failed {
        /// Exit code, when the process was terminated by one.
        code: Option<i32>,
        /// Whatever `ffmpeg` wrote to stderr.
        stderr: String,
    },
    /// The media has no usable duration.
    NoDuration,
}

/// A generated filmstrip.
#[derive(Debug, Clone, PartialEq)]
pub struct Storyboard {
    /// The generated images, in chronological order.
    pub frames: Vec<PathBuf>,
    /// Time in seconds each frame represents.
    pub interval: f64,
    /// Duration of the media the frames were taken from.
    pub duration: f64,
}

impl Storyboard {
    /// The time in the media a frame represents.
    pub fn time_of(&self, index: usize) -> f64 {
        index as f64 * self.interval
    }
}

/// Cuts `count` frames out of `input` and writes them into `directory`.
///
/// The frames are evenly spaced over the whole media.
pub fn generate(
    ffmpeg: &Path,
    input: &str,
    duration: f64,
    count: usize,
    directory: &Path,
) -> Result<Storyboard, Error> {
    if count == 0 || duration <= 0.0 {
        return Err(Error::NoDuration);
    }

    std::fs::create_dir_all(directory).map_err(Error::Spawn)?;

    let interval = duration / count as f64;
    let pattern = directory.join("frame-%04d.jpg");

    let output = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(input)
        .arg("-vf")
        .arg(format!("fps=1/{interval:.6}"))
        .arg("-frames:v")
        .arg(count.to_string())
        .arg("-q:v")
        .arg("5")
        .arg("-y")
        .arg(&pattern)
        .output()
        .map_err(Error::Spawn)?;

    if !output.status.success() {
        return Err(Error::Failed {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let frames = collect_frames(directory, count);
    Ok(Storyboard { frames, interval, duration })
}

/// Collects the generated frames, sorted by name.
fn collect_frames(directory: &Path, count: usize) -> Vec<PathBuf> {
    let mut frames: Vec<PathBuf> = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("jpg")))
                .collect()
        })
        .unwrap_or_default();
    frames.sort();
    frames.truncate(count);
    frames
}

/// Removes the frames of a storyboard.
pub fn clear(storyboard: &Storyboard) {
    for frame in &storyboard.frames {
        let _ = std::fs::remove_file(frame);
    }
}

//! Shared vocabulary for Flatline.
//!
//! # Layer 0 of the isolation ladder
//!
//! This crate is the bottom of the dependency ladder described in
//! `AGENTS.md`. It contains **only** the data types that have to cross a
//! crate boundary, and it depends on nothing except `serde`.
//!
//! Rules that keep the ladder intact:
//!
//! * No behaviour lives here. Types describe *what* a stream is, never *how*
//!   it is fetched or played.
//! * No crate may depend on this one *and* on another Flatline crate for the
//!   same piece of data. If a type is needed by two independent capabilities,
//!   it belongs here.
//! * Anything that needs `yt-dlp`, `libmpv`, `ffmpeg` or a UI toolkit does
//!   **not** belong here.

pub mod catalog;
pub mod plan;
pub mod quality;

pub use catalog::{AudioStream, MediaSource, VideoStream, best_audio, best_video};
pub use plan::PlaybackPlan;
pub use quality::Quality;

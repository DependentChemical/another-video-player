//! The catalog of streams a media source can be played from.

use serde::{Deserialize, Serialize};

/// A single media item that was resolved from a URL.
///
/// The catalog is deliberately extractor agnostic: `yt-dlp` normalises a very
/// large number of sites into the same shape, so the rest of Flatline never
/// needs to know which site a stream came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaSource {
    /// Extractor specific identifier of the media.
    pub id: String,
    /// Human readable title.
    pub title: String,
    /// Uploader / channel / author, when the extractor reports one.
    #[serde(default)]
    pub uploader: Option<String>,
    /// Duration in seconds, `None` for live streams.
    #[serde(default)]
    pub duration: Option<f64>,
    /// URL of a preview thumbnail.
    #[serde(default)]
    pub thumbnail: Option<String>,
    /// Canonical page the media was resolved from.
    #[serde(default)]
    pub webpage_url: Option<String>,
    /// Extractor key, e.g. `Youtube` or `Generic`.
    #[serde(default)]
    pub extractor: Option<String>,
    /// Whether the media is a live stream.
    #[serde(default)]
    pub is_live: bool,
    /// Every stream that carries a video track.
    #[serde(default)]
    pub video_streams: Vec<VideoStream>,
    /// Every stream that carries an audio track only.
    #[serde(default)]
    pub audio_streams: Vec<AudioStream>,
}

impl MediaSource {
    /// Distinct vertical resolutions available, best first.
    pub fn available_heights(&self) -> Vec<u32> {
        let mut heights: Vec<u32> = self.video_streams.iter().filter_map(|s| s.height).collect();
        heights.sort_unstable_by(|a, b| b.cmp(a));
        heights.dedup();
        heights
    }
}

/// A stream that carries a video track.
///
/// `has_audio` distinguishes a *combined* stream (video + audio in one file,
/// the common case for most sites) from a *video only* stream (the usual
/// case on YouTube, where audio arrives as a separate stream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStream {
    /// Extractor specific format identifier.
    pub format_id: String,
    /// Direct, signed URL of the stream.
    pub url: String,
    /// Container extension, e.g. `mp4` or `webm`.
    #[serde(default)]
    pub ext: String,
    /// Width in pixels, when known.
    #[serde(default)]
    pub width: Option<u32>,
    /// Height in pixels, when known.
    #[serde(default)]
    pub height: Option<u32>,
    /// Frames per second, when known.
    #[serde(default)]
    pub fps: Option<f64>,
    /// Video codec name, e.g. `avc1`.
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Overall bitrate in kbit/s.
    #[serde(default)]
    pub total_bitrate: Option<u64>,
    /// Size in bytes, when the extractor reports it.
    #[serde(default)]
    pub file_size: Option<u64>,
    /// Whether this stream already contains an audio track.
    #[serde(default)]
    pub has_audio: bool,
    /// Transport protocol, e.g. `https` or `m3u8_native`.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Human readable note from the extractor, e.g. `1080p`.
    #[serde(default)]
    pub note: Option<String>,
}

impl VideoStream {
    /// Short label for the quality picker, e.g. `1080p`.
    pub fn quality_label(&self) -> String {
        match self.height {
            Some(h) => format!("{h}p"),
            None => self.format_id.clone(),
        }
    }

    /// Ranking key used to pick the best of several streams.
    pub fn rank(&self) -> (u32, u32, u64) {
        (
            self.height.unwrap_or(0),
            self.fps.unwrap_or(0.0).round() as u32,
            self.total_bitrate.unwrap_or(0),
        )
    }
}

/// A stream that carries an audio track only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioStream {
    /// Extractor specific format identifier.
    pub format_id: String,
    /// Direct, signed URL of the stream.
    pub url: String,
    /// Container extension, e.g. `m4a` or `webm`.
    #[serde(default)]
    pub ext: String,
    /// Audio codec name, e.g. `mp4a`.
    #[serde(default)]
    pub audio_codec: Option<String>,
    /// Bitrate in kbit/s.
    #[serde(default)]
    pub bitrate: Option<u64>,
    /// Size in bytes, when the extractor reports it.
    #[serde(default)]
    pub file_size: Option<u64>,
    /// Language tag, when the extractor reports one.
    #[serde(default)]
    pub language: Option<String>,
    /// Human readable note from the extractor, e.g. `Default, high`.
    #[serde(default)]
    pub note: Option<String>,
    /// Transport protocol, e.g. `https` or `m3u8_native`.
    #[serde(default)]
    pub protocol: Option<String>,
}

impl AudioStream {
    /// Ranking key used to pick the best of several audio streams.
    pub fn rank(&self) -> u64 {
        self.bitrate.unwrap_or(0)
    }
}

/// Picks the best video stream of a [`MediaSource`].
pub fn best_video(source: &MediaSource) -> Option<&VideoStream> {
    source.video_streams.iter().max_by_key(|s| s.rank())
}

/// Picks the best audio only stream of a [`MediaSource`].
pub fn best_audio(source: &MediaSource) -> Option<&AudioStream> {
    source.audio_streams.iter().max_by_key(|s| s.rank())
}

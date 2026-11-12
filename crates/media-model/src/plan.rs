//! The result of turning a [`MediaSource`] into something playable.

use crate::catalog::{AudioStream, VideoStream};

/// A concrete playback decision: which URLs mpv should open.
///
/// A plan is either
///
/// * **combined** – one URL carries both video and audio, or
/// * **split** – the video stream has no audio and an extra audio stream is
///   attached to it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackPlan {
    /// The URL mpv opens as the primary media.
    pub media_url: String,
    /// The selected video stream, when the media has a video track.
    pub video: Option<VideoStream>,
    /// The selected audio stream, when it has to be attached separately.
    pub audio: Option<AudioStream>,
    /// Title of the media, for the window title.
    pub title: String,
}

impl PlaybackPlan {
    /// Whether the audio arrives as a separate stream.
    pub fn is_split(&self) -> bool {
        self.audio.is_some()
    }

    /// Pixel dimensions of the video track, when known.
    pub fn video_dimensions(&self) -> Option<(u32, u32)> {
        self.video.as_ref().and_then(|v| v.width.zip(v.height)).filter(|(w, h)| *w > 0 && *h > 0)
    }
}

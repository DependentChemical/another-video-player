//! Deciding which streams of a [`MediaSource`] are going to be played.

use media_model::{MediaSource, PlaybackPlan, Quality, best_audio, best_video};

use crate::Error;

/// The qualities the user may pick, best first.
pub fn qualities(source: &MediaSource) -> Vec<u32> {
    source.available_heights()
}

/// Builds the playback plan for a quality choice.
///
/// A plan always has exactly one primary media URL. When the chosen video
/// stream carries no audio, the best audio stream is attached to it
/// separately, which is the normal case on YouTube.
pub fn plan(source: &MediaSource, quality: &Quality) -> Result<PlaybackPlan, Error> {
    match quality {
        Quality::Best => {
            if let Some(video) = best_video(source) {
                let audio = if video.has_audio { None } else { best_audio(source).cloned() };
                Ok(assemble(source, Some(video.clone()), audio))
            } else if let Some(audio) = best_audio(source) {
                // Audio only media: the audio stream is the primary media.
                Ok(assemble(source, None, Some(audio.clone())))
            } else {
                Err(Error::NoVideo)
            }
        }
        Quality::Height(height) => {
            let video = source
                .video_streams
                .iter()
                .filter(|stream| stream.height == Some(*height))
                .max_by_key(|stream| stream.rank())
                .ok_or_else(|| Error::UnknownQuality(format!("{height}p")))?;
            let audio = if video.has_audio { None } else { best_audio(source).cloned() };
            Ok(assemble(source, Some(video.clone()), audio))
        }
        Quality::Streams { video, audio } => {
            let video_stream = source
                .video_streams
                .iter()
                .find(|stream| &stream.format_id == video)
                .ok_or_else(|| Error::UnknownQuality(video.clone()))?;
            let audio_stream = match audio {
                Some(id) => Some(
                    source
                        .audio_streams
                        .iter()
                        .find(|stream| &stream.format_id == id)
                        .cloned()
                        .ok_or_else(|| Error::UnknownQuality(id.clone()))?,
                ),
                None => None,
            };
            Ok(assemble(source, Some(video_stream.clone()), audio_stream))
        }
    }
}

/// Puts the selected streams into a plan.
fn assemble(
    source: &MediaSource,
    video: Option<media_model::VideoStream>,
    audio: Option<media_model::AudioStream>,
) -> PlaybackPlan {
    let media_url = match (&video, &audio) {
        (Some(video), _) => video.url.clone(),
        (None, Some(audio)) => audio.url.clone(),
        (None, None) => String::new(),
    };
    PlaybackPlan { media_url, video, audio, title: source.title.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use media_model::{AudioStream, VideoStream};

    fn source() -> MediaSource {
        MediaSource {
            id: "id".into(),
            title: "Title".into(),
            uploader: None,
            duration: Some(10.0),
            thumbnail: None,
            webpage_url: None,
            extractor: None,
            is_live: false,
            video_streams: vec![
                VideoStream {
                    format_id: "137".into(),
                    url: "https://example.com/1080.mp4".into(),
                    ext: "mp4".into(),
                    width: Some(1920),
                    height: Some(1080),
                    fps: Some(30.0),
                    video_codec: Some("avc1".into()),
                    total_bitrate: Some(4000),
                    file_size: None,
                    has_audio: false,
                    protocol: None,
                    note: None,
                },
                VideoStream {
                    format_id: "18".into(),
                    url: "https://example.com/360.mp4".into(),
                    ext: "mp4".into(),
                    width: Some(640),
                    height: Some(360),
                    fps: Some(30.0),
                    video_codec: Some("avc1".into()),
                    total_bitrate: Some(700),
                    file_size: None,
                    has_audio: true,
                    protocol: None,
                    note: None,
                },
            ],
            audio_streams: vec![AudioStream {
                format_id: "140".into(),
                url: "https://example.com/audio.m4a".into(),
                ext: "m4a".into(),
                audio_codec: Some("mp4a".into()),
                bitrate: Some(128),
                file_size: None,
                language: None,
                protocol: None,
            }],
        }
    }

    #[test]
    fn best_picks_the_highest_resolution_and_attaches_audio() {
        let plan = plan(&source(), &Quality::Best).expect("plan builds");
        assert_eq!(plan.video.as_ref().unwrap().format_id, "137");
        assert!(plan.is_split());
        assert_eq!(plan.media_url, "https://example.com/1080.mp4");
    }

    #[test]
    fn a_combined_stream_does_not_get_extra_audio() {
        let plan = plan(&source(), &Quality::Height(360)).expect("plan builds");
        assert!(!plan.is_split());
        assert!(plan.audio.is_none());
    }

    #[test]
    fn an_unknown_quality_is_an_error() {
        assert!(plan(&source(), &Quality::Height(4320)).is_err());
    }
}

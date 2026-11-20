//! Deciding which streams of a [`MediaSource`] are going to be played.

use media_model::{MediaSource, PlaybackPlan, Quality, best_audio, best_video};

use crate::Error;

/// Builds the playback plan for a quality choice.
///
/// A plan always has exactly one primary media URL. When the chosen video
/// stream carries no audio, the selected audio stream (or the best one for
/// [`Quality::Best`]) is attached to it separately, which is the normal case
/// on YouTube. A combined stream already carries its audio and never gets a
/// second track.
pub fn plan(source: &MediaSource, quality: &Quality) -> Result<PlaybackPlan, Error> {
    match quality {
        Quality::Best => best(source),
        Quality::Streams { video, audio } => explicit(source, video.as_deref(), audio.as_deref()),
    }
}

/// Chooses the best streams the source offers.
fn best(source: &MediaSource) -> Result<PlaybackPlan, Error> {
    if let Some(video) = best_video(source) {
        let audio = if video.has_audio { None } else { best_audio(source).cloned() };
        return Ok(assemble(source, Some(video.clone()), audio));
    }
    if let Some(audio) = best_audio(source) {
        // Audio only media: the audio stream is the primary media.
        return Ok(assemble(source, None, Some(audio.clone())));
    }
    Err(Error::NoStream)
}

/// Builds a plan from the identifiers the user selected.
fn explicit(
    source: &MediaSource,
    video: Option<&str>,
    audio: Option<&str>,
) -> Result<PlaybackPlan, Error> {
    let video = match video {
        Some(id) => Some(
            source
                .video_streams
                .iter()
                .find(|stream| stream.format_id == id)
                .cloned()
                .ok_or_else(|| Error::UnknownStream(id.to_string()))?,
        ),
        None => None,
    };

    // A combined stream carries both tracks already; an audio selection is
    // meaningless next to it and would play a second, unrelated track.
    if video.as_ref().is_some_and(|stream| stream.has_audio) {
        return Ok(assemble(source, video, None));
    }

    let audio = match audio {
        Some(id) => Some(
            source
                .audio_streams
                .iter()
                .find(|stream| stream.format_id == id)
                .cloned()
                .ok_or_else(|| Error::UnknownStream(id.to_string()))?,
        ),
        None => None,
    };

    if video.is_none() && audio.is_none() {
        return Err(Error::NoStream);
    }

    Ok(assemble(source, video, audio))
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
                note: None,
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
        let quality = Quality::Streams { video: Some("18".into()), audio: Some("140".into()) };
        let plan = plan(&source(), &quality).expect("plan builds");
        assert!(!plan.is_split());
        assert!(plan.audio.is_none());
    }

    #[test]
    fn an_audio_only_choice_plays_the_audio_url() {
        let quality = Quality::Streams { video: None, audio: Some("140".into()) };
        let plan = plan(&source(), &quality).expect("plan builds");
        assert!(plan.video.is_none());
        assert_eq!(plan.media_url, "https://example.com/audio.m4a");
    }

    #[test]
    fn a_video_only_choice_has_no_audio() {
        let quality = Quality::Streams { video: Some("137".into()), audio: None };
        let plan = plan(&source(), &quality).expect("plan builds");
        assert!(!plan.is_split());
        assert_eq!(plan.video.as_ref().unwrap().format_id, "137");
    }

    #[test]
    fn an_empty_choice_is_an_error() {
        let quality = Quality::Streams { video: None, audio: None };
        assert!(matches!(plan(&source(), &quality), Err(Error::NoStream)));
    }

    #[test]
    fn an_unknown_stream_is_an_error() {
        let quality = Quality::Streams { video: Some("4320".into()), audio: None };
        assert!(plan(&source(), &quality).is_err());
    }
}

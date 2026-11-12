//! Turning the metadata document of `yt-dlp` into a [`MediaSource`].

use media_model::{AudioStream, MediaSource, VideoStream};
use serde::Deserialize;
use serde_json::Value;

use crate::Error;

/// The subset of the `yt-dlp` metadata document Flatline reads.
#[derive(Debug, Deserialize)]
struct RawInfo {
    id: Option<String>,
    title: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    webpage_url: Option<String>,
    extractor: Option<String>,
    extractor_key: Option<String>,
    is_live: Option<bool>,
    live_status: Option<String>,
    #[serde(default)]
    formats: Vec<RawFormat>,
}

/// The subset of a single format description Flatline reads.
#[derive(Debug, Deserialize)]
struct RawFormat {
    format_id: Option<String>,
    url: Option<String>,
    ext: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
    vcodec: Option<String>,
    acodec: Option<String>,
    tbr: Option<f64>,
    vbr: Option<f64>,
    abr: Option<f64>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    protocol: Option<String>,
    format_note: Option<String>,
    language: Option<String>,
}

/// Parses a `yt-dlp` metadata document.
///
/// Both a single media document and a playlist document (a JSON array) are
/// accepted; for a playlist the first entry is used.
pub fn parse_catalog(json: &str) -> Result<MediaSource, Error> {
    let value: Value = serde_json::from_str(json).map_err(Error::Json)?;
    let entry = match value {
        Value::Array(mut items) if !items.is_empty() => items.remove(0),
        Value::Array(_) => return Err(Error::Empty),
        other => other,
    };
    let raw: RawInfo = serde_json::from_value(entry).map_err(Error::Json)?;
    Ok(build(raw))
}

/// Converts the raw document into the vocabulary of `media-model`.
fn build(raw: RawInfo) -> MediaSource {
    let mut video_streams = Vec::new();
    let mut audio_streams = Vec::new();

    for format in raw.formats {
        let Some(url) = format.url.filter(|url| !url.is_empty()) else {
            continue;
        };
        let format_id = format.format_id.unwrap_or_default();
        let file_size = format.filesize.or(format.filesize_approx);
        let has_video = format.vcodec.as_deref().is_some_and(|codec| codec != "none");
        let has_audio = format.acodec.as_deref().is_some_and(|codec| codec != "none");
        // A plain file the extractor did not look into reports neither codec.
        // It carries both, so it is treated as a combined stream.
        let unknown = format.vcodec.is_none() && format.acodec.is_none();

        if has_video || unknown {
            video_streams.push(VideoStream {
                format_id,
                url,
                ext: format.ext.unwrap_or_default(),
                width: format.width,
                height: format.height,
                fps: format.fps,
                video_codec: format.vcodec,
                total_bitrate: format.tbr.or(format.vbr).map(|value| value.round() as u64),
                file_size,
                has_audio: has_audio || unknown,
                protocol: format.protocol,
                note: format.format_note,
            });
        } else if has_audio {
            audio_streams.push(AudioStream {
                format_id,
                url,
                ext: format.ext.unwrap_or_default(),
                audio_codec: format.acodec,
                bitrate: format
                    .abr
                    .map(|value| value.round() as u64)
                    .or(format.tbr.map(|v| v.round() as u64)),
                file_size,
                language: format.language,
                protocol: format.protocol,
            });
        }
    }

    video_streams.sort_by_key(|stream| std::cmp::Reverse(stream.rank()));
    audio_streams.sort_by_key(|stream| std::cmp::Reverse(stream.rank()));

    MediaSource {
        id: raw.id.unwrap_or_default(),
        title: raw.title.unwrap_or_else(|| "Untitled".to_string()),
        uploader: raw.uploader.or(raw.channel),
        duration: raw.duration,
        thumbnail: raw.thumbnail,
        webpage_url: raw.webpage_url,
        extractor: raw.extractor_key.or(raw.extractor),
        is_live: raw.is_live.unwrap_or(false) || raw.live_status.as_deref() == Some("is_live"),
        video_streams,
        audio_streams,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "id": "abc123",
        "title": "Sample Video",
        "uploader": "Someone",
        "duration": 61.5,
        "extractor_key": "Youtube",
        "is_live": false,
        "formats": [
            {"format_id": "137", "url": "https://example.com/1080.mp4", "ext": "mp4", "height": 1080, "width": 1920, "fps": 30, "vcodec": "avc1.640028", "acodec": "none", "tbr": 4000.5, "filesize": 12345, "protocol": "https"},
            {"format_id": "140", "url": "https://example.com/audio.m4a", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 128.0, "protocol": "https"},
            {"format_id": "18", "url": "https://example.com/combined.mp4", "ext": "mp4", "height": 360, "width": 640, "vcodec": "avc1", "acodec": "mp4a", "tbr": 700.0, "protocol": "https"},
            {"format_id": "sb2", "ext": "mhtml", "url": null}
        ]
    }"#;

    #[test]
    fn video_and_audio_streams_are_separated() {
        let source = parse_catalog(SAMPLE).expect("catalog parses");
        assert_eq!(source.title, "Sample Video");
        assert_eq!(source.video_streams.len(), 2);
        assert_eq!(source.audio_streams.len(), 1);
        assert_eq!(source.audio_streams[0].format_id, "140");
    }

    #[test]
    fn combined_streams_are_marked_as_carrying_audio() {
        let source = parse_catalog(SAMPLE).expect("catalog parses");
        let combined = source.video_streams.iter().find(|s| s.format_id == "18").unwrap();
        assert!(combined.has_audio);
        let video_only = source.video_streams.iter().find(|s| s.format_id == "137").unwrap();
        assert!(!video_only.has_audio);
    }

    #[test]
    fn formats_without_a_url_are_skipped() {
        let source = parse_catalog(SAMPLE).expect("catalog parses");
        assert!(source.video_streams.iter().all(|s| s.format_id != "sb2"));
    }

    #[test]
    fn a_playlist_document_yields_its_first_entry() {
        let json = format!("[{SAMPLE}]");
        let source = parse_catalog(&json).expect("catalog parses");
        assert_eq!(source.id, "abc123");
    }

    #[test]
    fn available_heights_are_unique_and_sorted() {
        let source = parse_catalog(SAMPLE).expect("catalog parses");
        assert_eq!(source.available_heights(), vec![1080, 360]);
    }

    #[test]
    fn a_plain_file_without_codecs_is_a_combined_stream() {
        let json = r#"{
            "id": "sample",
            "title": "sample-5s",
            "ext": "mp4",
            "formats": [
                {"format_id": "mp4", "url": "https://example.com/sample.mp4", "ext": "mp4", "vcodec": null, "protocol": "https"}
            ]
        }"#;
        let source = parse_catalog(json).expect("catalog parses");
        assert_eq!(source.video_streams.len(), 1);
        assert!(source.video_streams[0].has_audio);
        assert!(source.audio_streams.is_empty());
    }
}

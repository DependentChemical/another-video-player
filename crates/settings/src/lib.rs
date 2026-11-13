//! Settings loading and preference based stream selection.
//!
//! # Layer 1 of the isolation ladder
//!
//! This crate owns configuration parsing and turns a media catalog plus a URL
//! into a stream preference. It depends only on the shared media vocabulary.

use std::path::Path;

use media_model::{AudioStream, MediaSource, Quality, VideoStream};
use regex::Regex;
use serde_json::Value;

const BUILTIN: &str = include_str!("../../../includes/settings.json");

/// The loaded settings document.
#[derive(Debug, Clone)]
pub struct Settings {
    document: Value,
}

/// A configuration or selection error.
#[derive(Debug)]
pub enum Error {
    /// The JSON document could not be read or parsed.
    Invalid(String),
    /// No usable stream was found in the source catalog.
    NoStream,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "settings are invalid: {message}"),
            Self::NoStream => write!(f, "this source has no playable stream"),
        }
    }
}

impl std::error::Error for Error {}

impl Settings {
    /// Loads settings beside the executable, falling back to bundled defaults.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BUILTIN.to_string(),
            Err(error) => return Err(Error::Invalid(error.to_string())),
        };
        Self::from_json(&content)
    }

    /// Parses settings from JSON text.
    pub fn from_json(json: &str) -> Result<Self, Error> {
        let document: Value =
            serde_json::from_str(json).map_err(|error| Error::Invalid(error.to_string()))?;
        if !document.get("Video-Settings").is_some_and(Value::is_object) {
            return Err(Error::Invalid("missing Video-Settings object".into()));
        }
        Ok(Self { document })
    }

    /// Chooses a playable stream using the matching site defaults or globals.
    pub fn select(&self, source: &MediaSource, url: &str) -> Result<Quality, Error> {
        let settings = self
            .document
            .get("Video-Settings")
            .ok_or_else(|| Error::Invalid("missing Video-Settings".into()))?;
        if let Some(items) = settings.pointer("/per-site/main-list").and_then(Value::as_object) {
            for item in items.values() {
                let Some(pattern) = item.get("regex").and_then(Value::as_str) else { continue };
                let Ok(regex) = Regex::new(pattern) else { continue };
                if regex.is_match(url) {
                    if let Some(defaults) = item.get("sources-default")
                        && let Some(selection) = choose(source, defaults)
                    {
                        return Ok(selection);
                    }
                    break;
                }
            }
        }
        let global = settings
            .pointer("/global/sources-default")
            .ok_or_else(|| Error::Invalid("missing global sources-default".into()))?;
        choose(source, global).ok_or(Error::NoStream)
    }
}

fn preferences(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn choose(source: &MediaSource, defaults: &Value) -> Option<Quality> {
    let videos = preferences(defaults, "video");
    let audios = preferences(defaults, "audio");
    let combined = preferences(defaults, "combined");

    // Try configured video/audio pairs first, honoring video preference order.
    for video_pref in &videos {
        let Some(video) =
            pick_video(source.video_streams.iter().filter(|stream| !stream.has_audio), video_pref)
        else {
            continue;
        };
        for audio_pref in &audios {
            if let Some(audio) = pick_audio(source.audio_streams.iter(), audio_pref) {
                return Some(Quality::Streams {
                    video: Some(video.format_id.clone()),
                    audio: Some(audio.format_id.clone()),
                });
            }
        }
    }

    // If a pair cannot be built, use a combined stream before single tracks.
    for pref in &combined {
        if let Some(video) =
            pick_video(source.video_streams.iter().filter(|stream| stream.has_audio), pref)
        {
            return Some(Quality::Streams { video: Some(video.format_id.clone()), audio: None });
        }
    }

    for pref in &videos {
        if let Some(video) =
            pick_video(source.video_streams.iter().filter(|stream| !stream.has_audio), pref)
        {
            return Some(Quality::Streams { video: Some(video.format_id.clone()), audio: None });
        }
    }
    for pref in &audios {
        if let Some(audio) = pick_audio(source.audio_streams.iter(), pref) {
            return Some(Quality::Streams { video: None, audio: Some(audio.format_id.clone()) });
        }
    }
    None
}

fn pick_video<'a>(
    streams: impl Iterator<Item = &'a VideoStream>,
    pref: &str,
) -> Option<&'a VideoStream> {
    let streams: Vec<_> = streams.collect();
    match pref {
        "best" => streams.into_iter().max_by_key(|stream| stream.rank()),
        "default" => streams.iter().copied().find(|stream| {
            stream.note.as_deref().is_some_and(|note| note.to_ascii_lowercase().contains("default"))
        }),
        _ => {
            let target = resolution(pref)?;
            let sized: Vec<_> = streams
                .into_iter()
                .filter_map(|stream| stream.height.map(|height| (height, stream)))
                .collect();
            let exact = sized
                .iter()
                .filter(|(height, _)| *height == target)
                .max_by_key(|(_, stream)| stream.rank());
            let lower = sized
                .iter()
                .filter(|(height, _)| *height < target)
                .max_by_key(|(height, stream)| (*height, stream.rank()));
            let higher = sized
                .iter()
                .filter(|(height, _)| *height > target)
                .min_by_key(|(height, stream)| (*height, std::cmp::Reverse(stream.rank())));
            exact.or(lower).or(higher).map(|(_, stream)| *stream)
        }
    }
}

fn resolution(value: &str) -> Option<u32> {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "2k" => Some(1440),
        "4k" => Some(2160),
        "8k" => Some(4320),
        _ => lower.strip_suffix('p').unwrap_or(&lower).parse().ok(),
    }
}

fn pick_audio<'a>(
    streams: impl Iterator<Item = &'a AudioStream>,
    pref: &str,
) -> Option<&'a AudioStream> {
    let streams: Vec<_> = streams.collect();
    match pref {
        "best" => streams.into_iter().max_by_key(|stream| stream.rank()),
        "default" => streams.iter().copied().find(|stream| {
            stream.note.as_deref().is_some_and(|note| note.to_ascii_lowercase().contains("default"))
        }),
        _ => {
            let target: u64 = pref.strip_suffix("kbps").unwrap_or(pref).parse().ok()?;
            let sized: Vec<_> = streams
                .into_iter()
                .filter_map(|stream| stream.bitrate.map(|rate| (rate, stream)))
                .collect();
            let exact = sized
                .iter()
                .filter(|(rate, _)| *rate == target)
                .max_by_key(|(_, stream)| stream.rank());
            let lower = sized
                .iter()
                .filter(|(rate, _)| *rate < target)
                .max_by_key(|(rate, stream)| (*rate, stream.rank()));
            let higher = sized
                .iter()
                .filter(|(rate, _)| *rate > target)
                .min_by_key(|(rate, stream)| (*rate, std::cmp::Reverse(stream.rank())));
            exact.or(lower).or(higher).map(|(_, stream)| *stream)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> MediaSource {
        MediaSource {
            id: "id".into(),
            title: "title".into(),
            uploader: None,
            duration: None,
            thumbnail: None,
            webpage_url: None,
            extractor: None,
            is_live: false,
            video_streams: [1080, 720, 360]
                .into_iter()
                .map(|height| VideoStream {
                    format_id: height.to_string(),
                    url: format!("https://media/{height}"),
                    ext: "mp4".into(),
                    width: Some(height * 16 / 9),
                    height: Some(height),
                    fps: Some(30.0),
                    video_codec: None,
                    total_bitrate: Some(height as u64),
                    file_size: None,
                    has_audio: false,
                    protocol: None,
                    note: None,
                })
                .chain(std::iter::once(VideoStream {
                    format_id: "muxed".into(),
                    url: "https://media/muxed".into(),
                    ext: "mp4".into(),
                    width: Some(1280),
                    height: Some(720),
                    fps: Some(30.0),
                    video_codec: None,
                    total_bitrate: Some(900),
                    file_size: None,
                    has_audio: true,
                    protocol: None,
                    note: None,
                }))
                .collect(),
            audio_streams: [64, 128]
                .into_iter()
                .map(|bitrate| AudioStream {
                    format_id: bitrate.to_string(),
                    url: format!("https://media/audio/{bitrate}"),
                    ext: "m4a".into(),
                    audio_codec: None,
                    bitrate: Some(bitrate),
                    file_size: None,
                    language: None,
                    note: (bitrate == 64).then(|| "Default".into()),
                    protocol: None,
                })
                .collect(),
        }
    }

    #[test]
    fn site_defaults_override_global_defaults_and_resolution_aliases_work() {
        let settings = Settings::from_json(r#"{"Video-Settings":{"global":{"sources-default":{"video":"best","audio":"best","combined":"best"}},"per-site":{"main-list":{"Site":{"regex":"example\\.com","sources-default":{"video":"2k","audio":"default,best","combined":"best"}}}}}}"#).expect("settings parse");
        let selected = settings.select(&source(), "https://example.com/watch").expect("selects");
        assert_eq!(
            selected,
            Quality::Streams { video: Some("1080".into()), audio: Some("64".into()) }
        );
    }

    #[test]
    fn unavailable_split_quality_falls_back_to_combined() {
        let settings = Settings::from_json(r#"{"Video-Settings":{"global":{"sources-default":{"video":"","audio":"","combined":"720"}}}}"#).expect("settings parse");
        let selected = settings.select(&source(), "https://other.test").expect("selects");
        assert_eq!(selected, Quality::Streams { video: Some("muxed".into()), audio: None });
    }

    #[test]
    fn resolution_prefers_exact_then_lower_then_higher() {
        let source = source();
        assert_eq!(
            pick_video(source.video_streams.iter().filter(|s| !s.has_audio), "720")
                .unwrap()
                .format_id,
            "720"
        );
        assert_eq!(
            pick_video(source.video_streams.iter().filter(|s| !s.has_audio), "800")
                .unwrap()
                .format_id,
            "720"
        );
        assert_eq!(
            pick_video(source.video_streams.iter().filter(|s| !s.has_audio), "240")
                .unwrap()
                .format_id,
            "360"
        );
    }
}

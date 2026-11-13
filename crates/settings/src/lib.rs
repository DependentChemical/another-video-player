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
        let global = settings
            .pointer("/global/sources-default")
            .ok_or_else(|| Error::Invalid("missing global sources-default".into()))?;
        if let Some(items) = settings.pointer("/per-site/main-list").and_then(Value::as_object) {
            for item in items.values() {
                let Some(pattern) = item.get("regex").and_then(Value::as_str) else { continue };
                let Ok(regex) = Regex::new(pattern) else { continue };
                if regex.is_match(url) {
                    let merged = merge_defaults(global, item.get("sources-default"));
                    if let Some(selection) = choose(source, &merged) {
                        return Ok(selection);
                    }
                }
            }
        }
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

fn merge_defaults(global: &Value, site: Option<&Value>) -> Value {
    let mut merged = serde_json::Map::new();
    for key in ["video", "audio", "combined"] {
        let site_value =
            site.and_then(|value| value.get(key)).and_then(Value::as_str).unwrap_or_default();
        let value = if site_value.split(',').any(|part| !part.trim().is_empty()) {
            site.and_then(|value| value.get(key)).cloned().unwrap_or(Value::Null)
        } else {
            global.get(key).cloned().unwrap_or(Value::Null)
        };
        merged.insert(key.to_string(), value);
    }
    Value::Object(merged)
}

fn choose(source: &MediaSource, defaults: &Value) -> Option<Quality> {
    let videos = preferences(defaults, "video");
    let audios = preferences(defaults, "audio");
    let combined = preferences(defaults, "combined");

    // The pair tier exhausts both ordered lists, including each size fallback.
    let split_video =
        video_choices(source.video_streams.iter().filter(|stream| !stream.has_audio), &videos);
    let split_audio = audio_choices(source.audio_streams.iter(), &audios);
    if let (Some(video), Some(audio)) = (split_video.first(), split_audio.first()) {
        return Some(Quality::Streams {
            video: Some(video.format_id.clone()),
            audio: Some(audio.format_id.clone()),
        });
    }

    // A combined stream is its own tier and never gets a second audio track.
    let combined_choices =
        video_choices(source.video_streams.iter().filter(|stream| stream.has_audio), &combined);
    if let Some(video) = combined_choices.first() {
        return Some(Quality::Streams { video: Some(video.format_id.clone()), audio: None });
    }

    // The video-only tier may pick a muxed stream, but never pairs audio itself.
    let video_choices = video_choices(source.video_streams.iter(), &videos);
    if let Some(video) = video_choices.first() {
        return Some(Quality::Streams { video: Some(video.format_id.clone()), audio: None });
    }

    let audio_choices = audio_choices(source.audio_streams.iter(), &audios);
    audio_choices
        .first()
        .map(|audio| Quality::Streams { video: None, audio: Some(audio.format_id.clone()) })
}

fn video_choices<'a>(
    streams: impl Iterator<Item = &'a VideoStream>,
    preferences: &[String],
) -> Vec<&'a VideoStream> {
    let streams: Vec<_> = streams.collect();
    if preferences.is_empty() || streams.is_empty() {
        return Vec::new();
    }

    let mut choices = Vec::new();
    for preference in preferences {
        if let Some(stream) = pick_video_exact(streams.iter().copied(), preference) {
            push_unique_video(&mut choices, stream);
        }
    }
    if let Some(stream) = pick_video_fallback(streams.iter().copied(), last_resolution(preferences))
    {
        push_unique_video(&mut choices, stream);
    }
    // A symbolic-only list such as `default` still resolves when the source
    // has no default-marked format.
    if choices.is_empty()
        && let Some(stream) = streams.iter().copied().max_by_key(|stream| stream.rank())
    {
        choices.push(stream);
    }
    choices
}

fn push_unique_video<'a>(choices: &mut Vec<&'a VideoStream>, candidate: &'a VideoStream) {
    if !choices.iter().any(|stream| stream.format_id == candidate.format_id) {
        choices.push(candidate);
    }
}

fn audio_choices<'a>(
    streams: impl Iterator<Item = &'a AudioStream>,
    preferences: &[String],
) -> Vec<&'a AudioStream> {
    let streams: Vec<_> = streams.collect();
    if preferences.is_empty() || streams.is_empty() {
        return Vec::new();
    }

    let mut choices = Vec::new();
    for preference in preferences {
        if let Some(stream) = pick_audio_exact(streams.iter().copied(), preference) {
            push_unique_audio(&mut choices, stream);
        }
    }
    if let Some(stream) = pick_audio_fallback(streams.iter().copied(), last_bitrate(preferences)) {
        push_unique_audio(&mut choices, stream);
    }
    if choices.is_empty()
        && let Some(stream) = streams.iter().copied().max_by_key(|stream| stream.rank())
    {
        choices.push(stream);
    }
    choices
}

fn push_unique_audio<'a>(choices: &mut Vec<&'a AudioStream>, candidate: &'a AudioStream) {
    if !choices.iter().any(|stream| stream.format_id == candidate.format_id) {
        choices.push(candidate);
    }
}

fn pick_video_exact<'a>(
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
            sized
                .iter()
                .filter(|(height, _)| *height == target)
                .max_by_key(|(_, stream)| stream.rank())
                .map(|(_, stream)| *stream)
        }
    }
}

fn last_resolution(preferences: &[String]) -> Option<u32> {
    preferences.iter().filter_map(|pref| resolution(pref)).next_back()
}

fn pick_video_fallback<'a>(
    streams: impl Iterator<Item = &'a VideoStream>,
    target: Option<u32>,
) -> Option<&'a VideoStream> {
    let target = target?;
    let sized: Vec<_> =
        streams.filter_map(|stream| stream.height.map(|height| (height, stream))).collect();
    sized
        .iter()
        .filter(|(height, _)| *height < target)
        .max_by_key(|(height, stream)| (*height, stream.rank()))
        .or_else(|| {
            sized
                .iter()
                .filter(|(height, _)| *height > target)
                .min_by_key(|(height, stream)| (*height, std::cmp::Reverse(stream.rank())))
        })
        .map(|(_, stream)| *stream)
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

fn pick_audio_exact<'a>(
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
            sized
                .iter()
                .filter(|(rate, _)| *rate == target)
                .max_by_key(|(_, stream)| stream.rank())
                .map(|(_, stream)| *stream)
        }
    }
}

fn last_bitrate(preferences: &[String]) -> Option<u64> {
    preferences
        .iter()
        .filter_map(|pref| pref.strip_suffix("kbps").unwrap_or(pref).parse().ok())
        .next_back()
}

fn pick_audio_fallback<'a>(
    streams: impl Iterator<Item = &'a AudioStream>,
    target: Option<u64>,
) -> Option<&'a AudioStream> {
    let target = target?;
    let sized: Vec<_> =
        streams.filter_map(|stream| stream.bitrate.map(|rate| (rate, stream))).collect();
    sized
        .iter()
        .filter(|(rate, _)| *rate < target)
        .max_by_key(|(rate, stream)| (*rate, stream.rank()))
        .or_else(|| {
            sized
                .iter()
                .filter(|(rate, _)| *rate > target)
                .min_by_key(|(rate, stream)| (*rate, std::cmp::Reverse(stream.rank())))
        })
        .map(|(_, stream)| *stream)
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
            video_streams: [1080, 720, 600, 480, 360]
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
            audio_streams: [64, 96, 128]
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
    fn combined_is_tried_before_the_unpaired_video_tier() {
        let mut source = source();
        source.audio_streams.clear();
        let settings = serde_json::json!({"video":"best", "audio":"best", "combined":"720"});
        assert_eq!(
            choose(&source, &settings),
            Some(Quality::Streams { video: Some("muxed".into()), audio: None })
        );
    }

    #[test]
    fn video_tier_never_attaches_audio_when_pairing_was_not_selected() {
        let source = source();
        let settings = serde_json::json!({"video":"best", "audio":"", "combined":""});
        assert_eq!(
            choose(&source, &settings),
            Some(Quality::Streams { video: Some("1080".into()), audio: None })
        );
    }

    #[test]
    fn audio_only_sources_follow_the_configured_audio_preferences() {
        let mut source = source();
        source.video_streams.clear();
        let settings =
            serde_json::json!({"video":"best", "audio":"default,best", "combined":"best"});
        assert_eq!(
            choose(&source, &settings),
            Some(Quality::Streams { video: None, audio: Some("64".into()) })
        );
    }

    #[test]
    fn resolution_prefers_exact_then_lower_then_higher() {
        let source = source();
        assert_eq!(
            pick_video_exact(source.video_streams.iter().filter(|s| !s.has_audio), "480")
                .unwrap()
                .format_id,
            "480"
        );
        assert_eq!(
            pick_video_fallback(source.video_streams.iter().filter(|s| !s.has_audio), Some(500))
                .unwrap()
                .format_id,
            "480"
        );
        assert_eq!(
            pick_video_fallback(source.video_streams.iter().filter(|s| !s.has_audio), Some(200))
                .unwrap()
                .format_id,
            "360"
        );
    }

    #[test]
    fn later_exact_video_and_audio_choices_beat_earlier_numeric_fallbacks() {
        let source = source();
        let settings = serde_json::json!({"video":"900,480", "audio":"160,96", "combined":""});
        assert_eq!(
            choose(&source, &settings),
            Some(Quality::Streams { video: Some("480".into()), audio: Some("96".into()) })
        );
    }

    #[test]
    fn numeric_fallback_uses_the_last_requested_size_and_bitrate() {
        let source = source();
        let settings = serde_json::json!({"video":"900,500", "audio":"160,100", "combined":""});
        assert_eq!(
            choose(&source, &settings),
            Some(Quality::Streams { video: Some("480".into()), audio: Some("96".into()) })
        );
    }

    #[test]
    fn partial_and_empty_site_values_inherit_the_global_preferences() {
        let settings = Settings::from_json(r#"{"Video-Settings":{"global":{"sources-default":{"video":"best","audio":"default,best","combined":"best"}},"per-site":{"main-list":{"Site":{"regex":"example\\.com","sources-default":{"video":"720","audio":"","combined":""}}}}}}"#).expect("settings parse");
        assert_eq!(
            settings.select(&source(), "https://example.com/watch").expect("selects"),
            Quality::Streams { video: Some("720".into()), audio: Some("64".into()) }
        );
    }

    #[test]
    fn empty_preferences_do_not_select_streams_outside_the_settings_policy() {
        let source = source();
        let settings = serde_json::json!({"video":"", "audio":"", "combined":""});
        assert_eq!(choose(&source, &settings), None);
    }

    #[test]
    fn an_empty_catalog_returns_no_stream() {
        let mut source = source();
        source.video_streams.clear();
        source.audio_streams.clear();
        let settings = Settings::from_json(r#"{"Video-Settings":{"global":{"sources-default":{"video":"best","audio":"best","combined":"best"}}}}"#).expect("settings parse");
        assert!(matches!(settings.select(&source, "https://example.test"), Err(Error::NoStream)));
    }
}

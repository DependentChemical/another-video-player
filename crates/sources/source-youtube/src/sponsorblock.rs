//! SponsorBlock segment lookup for YouTube videos.
//!
//! Segments are fetched from the public SponsorBlock API using the
//! privacy-preserving hash-prefix endpoint. The raw YouTube video ID is hashed
//! locally and only the first four characters of the SHA-256 digest are sent,
//! so the server cannot tell which exact video is being watched.

use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

const API_BASE: &str = "https://sponsor.ajay.app/api/skipSegments";
const USER_AGENT: &str = concat!("Flatline/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(10);

/// The SponsorBlock categories Flatline reads for YouTube playback.
pub const CATEGORIES: &[&str] =
    &["sponsor", "selfpromo", "interaction", "intro", "outro", "preview", "hook", "filler"];

/// A labeled time range that can be skipped during playback.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// The SponsorBlock category name.
    pub category: String,
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
}

/// A merged skip range covering one or more overlapping enabled segments.
#[derive(Debug, Clone, PartialEq)]
pub struct SkipRange {
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
    /// Representative category for user-facing labels, or `mixed`.
    pub category: String,
}

/// Merges enabled segments into non-overlapping skip ranges.
///
/// Overlapping or adjacent enabled segments are combined so playback can skip
/// them in a single seek. When a merged range spans multiple categories its
/// category becomes `mixed`.
pub fn merged_skip_ranges<F>(segments: &[Segment], mut enabled: F) -> Vec<SkipRange>
where
    F: FnMut(&str) -> bool,
{
    let mut filtered: Vec<Segment> =
        segments.iter().filter(|segment| enabled(&segment.category)).cloned().collect();
    filtered.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.end.total_cmp(&b.end)));

    let mut merged: Vec<SkipRange> = Vec::new();
    for segment in filtered {
        if let Some(last) = merged.last_mut()
            && segment.start <= last.end + 0.05
        {
            if segment.end > last.end {
                last.end = segment.end;
            }
            if last.category != segment.category {
                last.category = "mixed".to_string();
            }
            continue;
        }
        merged.push(SkipRange {
            start: segment.start,
            end: segment.end,
            category: segment.category,
        });
    }
    merged
}

/// Returns the segments for a YouTube URL, or an empty list when none apply.
///
/// Network and parsing failures are treated as "no segments" so playback is
/// never blocked by SponsorBlock.
pub fn fetch_for_url(url: &str) -> Vec<Segment> {
    let Some(video_id) = video_id(url) else {
        return Vec::new();
    };
    fetch_for_video_id(&video_id).unwrap_or_default()
}

/// Extracts the YouTube video ID from a URL.
pub fn video_id(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host == "youtu.be" || host.ends_with(".youtu.be") {
        let id = parsed.path_segments()?.next().unwrap_or_default();
        return normalize_id(id);
    }
    if !(host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com"))
    {
        return None;
    }
    if let Some(id) = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "v").then(|| value.into_owned()))
        .as_deref()
        .and_then(normalize_id)
    {
        return Some(id);
    }
    let mut segments = parsed.path_segments()?;
    let first = segments.next().unwrap_or_default();
    let second = segments.next().unwrap_or_default();
    match first {
        "shorts" | "embed" | "live" | "v" => normalize_id(second),
        _ => None,
    }
}

fn normalize_id(id: &str) -> Option<String> {
    let id = id.trim_matches('/');
    if id.is_empty() { None } else { Some(id.to_string()) }
}

/// Fetches and filters segments for an exact YouTube video ID.
fn fetch_for_video_id(video_id: &str) -> Result<Vec<Segment>, String> {
    let hash_prefix = hash_prefix(video_id);
    let categories = serde_json::to_string(CATEGORIES).map_err(|e| e.to_string())?;
    let action_types = serde_json::to_string(&["skip"]).map_err(|e| e.to_string())?;
    let url = format!(
        "{API_BASE}/{hash_prefix}?categories={}&actionTypes={}&service=YouTube",
        encode_query_component(&categories),
        encode_query_component(&action_types)
    );
    let response = ureq::get(&url)
        .header("User-Agent", USER_AGENT)
        .config()
        .timeout_global(Some(TIMEOUT))
        .build()
        .call()
        .map_err(|e| e.to_string())?;

    if response.status() == 404 {
        return Ok(Vec::new());
    }
    if response.status() != 200 {
        return Err(format!("status {}", response.status()));
    }

    let body = response.into_body().read_to_string().map_err(|e| e.to_string())?;
    let videos: Vec<VideoSegments> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let mut segments = Vec::new();
    for video in videos {
        if video.video_id != video_id {
            continue;
        }
        for segment in video.segments {
            if segment.action_type.as_deref() != Some("skip") && segment.action_type.is_some() {
                continue;
            }
            let Some(category) = segment.category else { continue };
            if !CATEGORIES.iter().any(|known| *known == category) {
                continue;
            }
            let Some(times) = segment.segment else { continue };
            if times.len() != 2 {
                continue;
            }
            let start = times[0];
            let end = times[1];
            if !start.is_finite() || !end.is_finite() || end <= start {
                continue;
            }
            segments.push(Segment { category, start, end });
        }
    }
    segments.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.end.total_cmp(&b.end)));
    segments.dedup_by(|a, b| a.category == b.category && a.start == b.start && a.end == b.end);
    Ok(segments)
}

fn hash_prefix(video_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(video_id.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(4);
    for byte in digest.iter().take(2) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn encode_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push_str("%20"),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

#[derive(Debug, Deserialize)]
struct VideoSegments {
    #[serde(rename = "videoID")]
    video_id: String,
    #[serde(default)]
    segments: Vec<ApiSegment>,
}

#[derive(Debug, Deserialize)]
struct ApiSegment {
    #[serde(default)]
    category: Option<String>,
    #[serde(default, rename = "actionType")]
    action_type: Option<String>,
    #[serde(default)]
    segment: Option<Vec<f64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_video_id_from_common_youtube_urls() {
        assert_eq!(
            video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".into())
        );
        assert_eq!(video_id("https://youtu.be/dQw4w9WgXcQ?t=43"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(
            video_id("https://www.youtube.com/shorts/abc123XYZ_-"),
            Some("abc123XYZ_-".into())
        );
        assert_eq!(
            video_id("https://www.youtube.com/embed/abc123XYZ_-?rel=0"),
            Some("abc123XYZ_-".into())
        );
    }

    #[test]
    fn ignores_non_youtube_urls() {
        assert_eq!(video_id("https://example.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(video_id("file:///video.mp4"), None);
    }

    #[test]
    fn hash_prefix_matches_sha256_of_video_id() {
        assert_eq!(hash_prefix("XqZsoesa55w"), "167a");
        assert_eq!(hash_prefix("dQw4w9WgXcQ"), "5f6b");
    }

    #[test]
    fn query_component_encoding_is_stable() {
        let encoded = encode_query_component("[\"sponsor\",\"intro\"]");
        assert!(encoded.contains("%5B"));
        assert!(encoded.contains("%22sponsor%22"));
    }

    #[test]
    fn merged_skip_ranges_combines_overlaps_and_adjacent_segments() {
        let segments = vec![
            Segment { category: "interaction".into(), start: 0.0, end: 2.299 },
            Segment { category: "intro".into(), start: 0.0, end: 9.349 },
            Segment { category: "selfpromo".into(), start: 9.36, end: 12.0 },
            Segment { category: "outro".into(), start: 105.833, end: 136.081 },
        ];
        let ranges = merged_skip_ranges(&segments, |_| true);
        assert_eq!(
            ranges,
            vec![
                SkipRange { start: 0.0, end: 12.0, category: "mixed".into() },
                SkipRange { start: 105.833, end: 136.081, category: "outro".into() },
            ]
        );
    }

    #[test]
    fn merged_skip_ranges_respects_disabled_categories() {
        let segments = vec![
            Segment { category: "interaction".into(), start: 0.0, end: 2.299 },
            Segment { category: "intro".into(), start: 0.0, end: 9.349 },
            Segment { category: "outro".into(), start: 105.833, end: 136.081 },
        ];
        let ranges = merged_skip_ranges(&segments, |category| category != "intro");
        assert_eq!(
            ranges,
            vec![
                SkipRange { start: 0.0, end: 2.299, category: "interaction".into() },
                SkipRange { start: 105.833, end: 136.081, category: "outro".into() },
            ]
        );
    }
}

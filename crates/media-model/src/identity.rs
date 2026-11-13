use url::Url;

/// Cleans a video URL to its canonical form by removing tracking parameters,
/// playlist references, and other non-essential query parameters.
///
/// This ensures the same video gets the same identity hash regardless of
/// how the URL was accessed (direct, from playlist, with timestamp, etc.).
pub fn clean_video_url(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_string();
    };

    // Remove all query parameters except the video ID parameter
    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    // Keep only known video ID parameters for major platforms
    let video_id_params = ["v", "video_id", "id"];
    pairs.retain(|(key, _)| video_id_params.iter().any(|p| key.eq_ignore_ascii_case(p)));

    // Rebuild query string with only video ID params
    let query = if pairs.is_empty() {
        String::new()
    } else {
        pairs
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&")
    };

    parsed.set_query(if query.is_empty() { None } else { Some(&query) });
    parsed.set_fragment(None);

    parsed.to_string()
}

/// Computes a stable MD5 hash of a cleaned video URL.
///
/// The hash serves as a unique identifier for a video across sessions.
/// It uses the cleaned URL so the same video gets the same hash regardless
/// of how the URL was accessed.
pub fn video_id_hash(url: &str) -> String {
    let cleaned = clean_video_url(url);
    let digest = md5::compute(cleaned.as_bytes());
    format!("{:x}", digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_url_strips_playlist_and_timestamp() {
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc123&index=5&t=42s";
        let cleaned = clean_video_url(url);
        assert_eq!(cleaned, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    #[test]
    fn youtube_url_keeps_only_v_parameter() {
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&feature=share&ab_channel=Test";
        let cleaned = clean_video_url(url);
        assert_eq!(cleaned, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    #[test]
    fn youtube_short_url_keeps_video_id() {
        let url = "https://youtu.be/dQw4w9WgXcQ?si=abc123";
        let cleaned = clean_video_url(url);
        assert_eq!(cleaned, "https://youtu.be/dQw4w9WgXcQ");
    }

    #[test]
    fn generic_url_preserves_id_parameter() {
        let url = "https://example.com/video?id=abc123&source=feed";
        let cleaned = clean_video_url(url);
        assert_eq!(cleaned, "https://example.com/video?id=abc123");
    }

    #[test]
    fn generic_url_preserves_video_id_parameter() {
        let url = "https://example.com/watch?video_id=xyz789&ref=homepage";
        let cleaned = clean_video_url(url);
        assert_eq!(cleaned, "https://example.com/watch?video_id=xyz789");
    }

    #[test]
    fn hash_is_stable_for_same_video() {
        let url1 = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let url2 = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc&t=30s";
        assert_eq!(video_id_hash(url1), video_id_hash(url2));
    }

    #[test]
    fn hash_differs_for_different_videos() {
        let url1 = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let url2 = "https://www.youtube.com/watch?v=abcdefghijk";
        assert_ne!(video_id_hash(url1), video_id_hash(url2));
    }

    #[test]
    fn invalid_url_returns_original() {
        let url = "not-a-url";
        assert_eq!(clean_video_url(url), url);
        assert_eq!(video_id_hash(url), video_id_hash(url));
    }
}
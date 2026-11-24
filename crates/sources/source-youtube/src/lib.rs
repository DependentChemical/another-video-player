//! YouTube-specific session and cookie behavior for Flatline.
//!
//! Layer 1 source capability: owns only the YouTube deviations from the
//! generic web-session flow and is wired in by the composition root.

pub mod sponsorblock;

use media_model::Service;
use url::Url;
use web_session::Source;

/// The YouTube source plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct YouTube;

/// The singleton YouTube source used by the internal plugin registry.
pub static SOURCE: YouTube = YouTube;

impl Source for YouTube {
    fn service(&self) -> Service {
        Service::YouTube
    }

    fn matches_url(&self, url: &str) -> bool {
        let Ok(parsed) = Url::parse(url) else {
            return false;
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return false;
        }
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        host_is(&host, "youtube.com")
            || host_is(&host, "youtu.be")
            || host_is(&host, "youtube-nocookie.com")
    }

    fn cookie_file_name(&self) -> &'static str {
        "cookies-youtube.txt"
    }

    fn auth_cookie_names(&self) -> &'static [&'static str] {
        &["SAPISID", "__Secure-1PAPISID", "__Secure-3PAPISID", "__Secure-1PSID", "__Secure-3PSID"]
    }

    fn base_domains(&self) -> &'static [&'static str] {
        &["youtube.com", "youtu.be", "youtube-nocookie.com", "google.com"]
    }

    fn own_domains(&self) -> &'static [&'static str] {
        &["youtube.com", "youtu.be", "youtube-nocookie.com"]
    }

    fn home_url(&self) -> Url {
        Url::parse("https://www.youtube.com/").expect("valid constant URL")
    }

    fn login_url(&self) -> Url {
        Url::parse(
            "https://accounts.google.com/ServiceLogin?service=youtube&continue=https://www.youtube.com/",
        )
        .expect("valid constant URL")
    }

    fn check_url(&self) -> Url {
        Url::parse("https://www.youtube.com/feed/subscriptions").expect("valid constant URL")
    }

    fn is_login_page(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        host == "accounts.google.com"
            || host.ends_with(".accounts.google.com")
            || host == "consent.youtube.com"
    }

    fn is_signed_in_location(&self, url: &Url) -> bool {
        if self.is_login_page(url) {
            return false;
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        host == "youtube.com"
            || host.ends_with(".youtube.com")
            || host == "youtu.be"
            || host.ends_with(".youtu.be")
    }

    fn challenge_phrases(&self) -> &'static [&'static str] {
        &[]
    }
}

/// What the app should do about the cookies when a YouTube URL times out
/// before the retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutRecovery {
    /// The cookies are regenerated and the retry uses them.
    Regenerate,
    /// The user is asked to sign in, then the cookies are regenerated.
    /// Declining lets the retry proceed without the new cookies.
    SignInThenRegenerate,
}

/// Decides the timeout recovery for a YouTube URL from whether the stored
/// session currently looks signed in.
pub fn timeout_recovery(signed_in: bool) -> TimeoutRecovery {
    if signed_in { TimeoutRecovery::Regenerate } else { TimeoutRecovery::SignInThenRegenerate }
}

/// Whether a host is `base` itself or one of its subdomains.
fn host_is(host: &str, base: &str) -> bool {
    host == base || host.ends_with(&format!(".{base}"))
}

/// Parameters that Flatline always keeps when preparing a YouTube URL for
/// processing. Keeping the playlist reference (`list`, `index`) lets yt-dlp
/// treat the URL as a playlist instead of dropping it to a single video.
const YOUTUBE_PROCESSING_PARAMS: &[&str] = &["v", "list", "index", "start"];

/// Parameters that are tracking noise and may be dropped before processing.
const YOUTUBE_TRACKING_PARAMS: &[&str] =
    &["si", "feature", "ab_channel", "pp", "utm_source", "utm_medium", "utm_campaign"];

/// Prepares a YouTube URL for processing without stripping playlist
/// information.
///
/// Unlike the identity-based cleaner used for thumbnails and history, this
/// keeps the playlist reference so yt-dlp can still see the whole playlist.
/// When the URL is not YouTube, it is returned trimmed and unchanged.
pub fn processing_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    let Ok(mut parsed) = Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let Some(host) = parsed.host_str() else {
        return trimmed.to_string();
    };
    let host = host.to_ascii_lowercase();
    let is_youtube = host_is(&host, "youtube.com")
        || host_is(&host, "youtu.be")
        || host_is(&host, "youtube-nocookie.com");
    if !is_youtube {
        return trimmed.to_string();
    }

    let mut pairs: Vec<(String, String)> =
        parsed.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        pairs.retain(|(key, _value)| {
        let key = key.to_ascii_lowercase();
        if YOUTUBE_TRACKING_PARAMS.iter().any(|p| key == p.to_ascii_lowercase()) {
            return false;
        }
        YOUTUBE_PROCESSING_PARAMS.iter().any(|p| key == p.to_ascii_lowercase())
    });

    let query = if pairs.is_empty() {
        String::new()
    } else {
        pairs.into_iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&")
    };
    parsed.set_query(if query.is_empty() { None } else { Some(&query) });
    parsed.set_fragment(None);
    parsed.to_string()
}

/// Whether a URL belongs to YouTube.
pub fn matches_url(url: &str) -> bool {
    SOURCE.matches_url(url)
}

#[cfg(test)]
mod tests {
    use super::{TimeoutRecovery, YouTube, matches_url, processing_url, timeout_recovery};
    use media_model::Service;
    use web_session::Source;

    #[test]
    fn youtube_hosts_are_recognised() {
        for url in [
            "https://youtube.com/watch?v=1",
            "https://www.youtube.com/watch?v=1",
            "https://music.youtube.com/watch?v=1",
            "https://youtu.be/1",
            "https://www.youtube-nocookie.com/embed/1",
        ] {
            assert!(matches_url(url), "{url}");
            assert_eq!(YouTube.service(), Service::YouTube);
        }
    }

    #[test]
    fn lookalike_hosts_are_not_recognised() {
        for url in [
            "https://youtube.com.example.org/watch?v=1",
            "https://notyoutube.com/watch?v=1",
            "file:///video.mp4",
        ] {
            assert!(!matches_url(url), "{url}");
        }
    }

    #[test]
    fn cookie_file_name_is_stable() {
        assert_eq!(YouTube.cookie_file_name(), "cookies-youtube.txt");
    }

    #[test]
    fn a_signed_in_profile_only_needs_the_cookie_regeneration() {
        assert_eq!(timeout_recovery(true), TimeoutRecovery::Regenerate);
    }

    #[test]
    fn an_unsigned_profile_offers_the_sign_in_then_the_cookies() {
        assert_eq!(timeout_recovery(false), TimeoutRecovery::SignInThenRegenerate);
    }

    #[test]
    fn processing_url_keeps_list_param() {
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc123&t=30s";
        let cleaned = processing_url(url);
        assert_eq!(cleaned, "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc123");
    }

    #[test]
    fn processing_url_strips_tracking_params() {
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&feature=share&ab_channel=Test&list=PLabc&t=30s";
        let cleaned = processing_url(url);
        assert_eq!(cleaned, "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc");
    }

    #[test]
    fn processing_url_preserves_index() {
        let url = "https://youtu.be/dQw4w9WgXcQ?list=PLabc&index=5";
        let cleaned = processing_url(url);
        assert_eq!(cleaned, "https://youtu.be/dQw4w9WgXcQ?list=PLabc&index=5");
    }

    #[test]
    fn processing_url_strips_other_params() {
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc&start_radio=1&si=abc";
        let cleaned = processing_url(url);
        assert_eq!(cleaned, "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc");
    }

    #[test]
    fn processing_url_non_youtube_unchanged() {
        let url = "https://example.com/video?id=abc&other=param";
        let cleaned = processing_url(url);
        assert_eq!(cleaned, "https://example.com/video?id=abc&other=param");
    }
}

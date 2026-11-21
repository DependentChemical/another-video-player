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

/// Whether a host is `base` itself or one of its subdomains.
fn host_is(host: &str, base: &str) -> bool {
    host == base || host.ends_with(&format!(".{base}"))
}

/// Whether a URL belongs to YouTube.
pub fn matches_url(url: &str) -> bool {
    SOURCE.matches_url(url)
}

#[cfg(test)]
mod tests {
    use super::{YouTube, matches_url};
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
}

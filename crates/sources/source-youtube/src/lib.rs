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

/// The `yt-dlp` arguments that every YouTube resolve needs.
///
/// YouTube's default web player breaks the signature and the challenge
/// extraction often enough to make plain resolves fail. The `tv` player
/// variant is the one that keeps working, so it is used for every
/// YouTube video Flatline processes.
pub const YTDLP_ARGS: &[&str] = &["--extractor-args", "youtube:player_js_variant=tv"];

/// The `yt-dlp` arguments of a YouTube resolve, as command line values.
///
/// The composition root hands these to the resolver, which adds them to
/// every request for a YouTube URL.
pub fn ytdlp_args() -> Vec<String> {
    YTDLP_ARGS.iter().map(|arg| arg.to_string()).collect()
}

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

/// Whether a URL belongs to YouTube.
pub fn matches_url(url: &str) -> bool {
    SOURCE.matches_url(url)
}

#[cfg(test)]
mod tests {
    use super::{TimeoutRecovery, YouTube, matches_url, timeout_recovery, ytdlp_args};
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
    fn every_youtube_resolve_uses_the_tv_player_variant() {
        let args = ytdlp_args();

        assert_eq!(args, vec!["--extractor-args", "youtube:player_js_variant=tv"]);
    }
}

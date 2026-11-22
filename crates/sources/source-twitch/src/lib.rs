//! Twitch-specific session and cookie behavior for Flatline.
//!
//! Layer 1 source capability: owns only the Twitch deviations from the
//! generic web-session flow and is wired in by the composition root.

use media_model::Service;
use url::Url;
use web_session::Source;

/// The Twitch source plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct Twitch;

/// The singleton Twitch source used by the internal plugin registry.
pub static SOURCE: Twitch = Twitch;

impl Source for Twitch {
    fn service(&self) -> Service {
        Service::Twitch
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
        host_is(&host, "twitch.tv")
    }

    fn cookie_file_name(&self) -> &'static str {
        "cookies-twitch.txt"
    }

    fn auth_cookie_names(&self) -> &'static [&'static str] {
        &["auth-token", "auth_token"]
    }

    fn base_domains(&self) -> &'static [&'static str] {
        &["twitch.tv", "ttvnw.net", "jtvnw.net", "twitchcdn.net"]
    }

    fn own_domains(&self) -> &'static [&'static str] {
        &["twitch.tv"]
    }

    fn home_url(&self) -> Url {
        Url::parse("https://www.twitch.tv/").expect("valid constant URL")
    }

    fn login_url(&self) -> Url {
        Url::parse("https://www.twitch.tv/login").expect("valid constant URL")
    }

    fn check_url(&self) -> Url {
        Url::parse("https://www.twitch.tv/settings/profile").expect("valid constant URL")
    }

    fn is_login_page(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        (host == "twitch.tv" || host.ends_with(".twitch.tv")) && url.path().starts_with("/login")
    }

    fn is_signed_in_location(&self, url: &Url) -> bool {
        if self.is_login_page(url) {
            return false;
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        host == "twitch.tv" || host.ends_with(".twitch.tv")
    }

    fn challenge_phrases(&self) -> &'static [&'static str] {
        &[
            "oauth may be required",
            "subscriber-only",
            "only available to subscribers",
            "unauthorized_entitlements",
            "vod_manifest_restricted",
        ]
    }
}

/// What the app should do about the cookies when a Twitch URL times out
/// before the retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutRecovery {
    /// The cookies are regenerated and the retry uses them.
    Regenerate,
    /// The user is asked to sign in, then the cookies are regenerated.
    /// Declining lets the retry proceed without the new cookies.
    SignInThenRegenerate,
}

/// Decides the timeout recovery for a Twitch URL from whether the stored
/// session currently looks signed in.
pub fn timeout_recovery(signed_in: bool) -> TimeoutRecovery {
    if signed_in { TimeoutRecovery::Regenerate } else { TimeoutRecovery::SignInThenRegenerate }
}

/// Whether a host is `base` itself or one of its subdomains.
fn host_is(host: &str, base: &str) -> bool {
    host == base || host.ends_with(&format!(".{base}"))
}

/// Whether a URL belongs to Twitch.
pub fn matches_url(url: &str) -> bool {
    SOURCE.matches_url(url)
}

#[cfg(test)]
mod tests {
    use super::{TimeoutRecovery, Twitch, matches_url, timeout_recovery};
    use media_model::Service;
    use web_session::Source;

    #[test]
    fn twitch_hosts_are_recognised() {
        for url in [
            "https://twitch.tv/channel",
            "https://www.twitch.tv/channel",
            "https://m.twitch.tv/channel",
            "https://clips.twitch.tv/Clip",
        ] {
            assert!(matches_url(url), "{url}");
            assert_eq!(Twitch.service(), Service::Twitch);
        }
    }

    #[test]
    fn lookalike_hosts_are_not_recognised() {
        for url in [
            "https://nottwitch.tv/channel",
            "https://twitch.tv.example.org/channel",
            "file:///video.mp4",
        ] {
            assert!(!matches_url(url), "{url}");
        }
    }

    #[test]
    fn cookie_file_name_is_stable() {
        assert_eq!(Twitch.cookie_file_name(), "cookies-twitch.txt");
    }

    #[test]
    fn a_signed_in_profile_only_needs_the_cookie_regeneration() {
        assert_eq!(timeout_recovery(true), TimeoutRecovery::Regenerate);
    }

    #[test]
    fn an_unsigned_profile_offers_the_sign_in_then_the_cookies() {
        assert_eq!(timeout_recovery(false), TimeoutRecovery::SignInThenRegenerate);
    }
}

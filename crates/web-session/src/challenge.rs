//! Recognising the answers that mean "sign in first".
//!
//! `yt-dlp` reports an authentication problem as a sentence, so this module
//! is deliberately a list of sentences rather than an error code. Matching
//! is case-insensitive because the text is copied from different sites and
//! different `yt-dlp` versions.

use media_model::Service;

/// The phrases that mean the resolver needs a signed-in session.
const CHALLENGE_PHRASES: &[&str] = &[
    "sign in to confirm you're not a bot",
    "sign in to confirm you’re not a bot",
    "please sign in",
    "requires authentication",
    "insufficient access",
    "oauth may be required",
    "subscriber-only",
    "only available to subscribers",
    "unauthorized_entitlements",
    "vod_manifest_restricted",
];

/// Whether a resolver error means the cookies need to be refreshed.
pub fn is_auth_challenge(message: &str) -> bool {
    // yt-dlp on Windows may emit the apostrophe in a non-UTF-8 codepage,
    // which `from_utf8_lossy` turns into U+FFFD. Normalising both the
    // message and the phrases keeps the match working regardless.
    let normalize =
        |text: &str| text.replace(['\u{FFFD}', '\u{2019}', '\u{2018}'], "'").to_ascii_lowercase();
    let lowercase = normalize(message);
    CHALLENGE_PHRASES.iter().any(|phrase| lowercase.contains(&normalize(phrase)))
}

/// Whether a resolver error is worth a sign-in attempt for this service.
///
/// A generic "please sign in" on an unrelated site is not something either
/// service can fix, so the URL has to belong to a service before a session is
/// opened.
pub fn challenge_service(url: &str, message: &str) -> Option<Service> {
    Service::from_url(url).filter(|_| is_auth_challenge(message))
}

#[cfg(test)]
mod tests {
    use super::{challenge_service, is_auth_challenge};
    use media_model::Service;

    #[test]
    fn youtube_bot_and_sign_in_messages_are_challenges() {
        assert!(is_auth_challenge("ERROR: [youtube] abc: Sign in to confirm you're not a bot"));
        assert!(is_auth_challenge("ERROR: Please sign in to view this video"));
        assert!(is_auth_challenge("Insufficient access to this resource"));
    }

    #[test]
    fn twitch_authentication_messages_are_challenges() {
        assert!(is_auth_challenge("OAuth may be required to access this VOD"));
        assert!(is_auth_challenge("This video is subscriber-only"));
        assert!(is_auth_challenge("unauthorized_entitlements"));
        assert!(is_auth_challenge("vod_manifest_restricted"));
    }

    #[test]
    fn matching_ignores_case() {
        assert!(is_auth_challenge("SIGN IN TO CONFIRM YOU'RE NOT A BOT"));
    }

    #[test]
    fn replacement_characters_from_a_non_utf8_codepage_still_match() {
        // A Windows ANSI apostrophe read as UTF-8 becomes U+FFFD.
        assert!(is_auth_challenge("Sign in to confirm you\u{FFFD}re not a bot"));
        assert!(is_auth_challenge("Sign in to confirm you\u{2019}re not a bot"));
    }

    #[test]
    fn unrelated_errors_are_not_challenges() {
        assert!(!is_auth_challenge("Unable to download webpage: HTTP Error 404"));
        assert!(!is_auth_challenge("Requested format is not available"));
        assert!(!is_auth_challenge(""));
    }

    #[test]
    fn a_service_is_required_before_a_session_is_opened() {
        assert_eq!(
            challenge_service("https://www.youtube.com/watch?v=1", "Please sign in"),
            Some(Service::YouTube)
        );
        assert_eq!(
            challenge_service(
                "https://www.twitch.tv/channel",
                "Sign in to confirm you're not a bot"
            ),
            Some(Service::Twitch)
        );
        assert_eq!(challenge_service("https://example.org/video", "Please sign in"), None);
        assert_eq!(challenge_service("https://www.youtube.com/watch?v=1", "HTTP Error 404"), None);
    }
}

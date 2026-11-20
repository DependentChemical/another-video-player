//! Recognising the answers that mean "sign in first".
//!
//! Challenge detection is generic with per-source phrase extensions provided
//! by the active registry. When no registry is installed, built-in defaults
//! preserve existing behavior.

use media_model::Service;

/// Generic phrases that mean the resolver needs a signed-in session.
const GENERIC_PHRASES: &[&str] = &[
    "sign in to confirm you're not a bot",
    "sign in to confirm you’re not a bot",
    "please sign in",
    "requires authentication",
    "insufficient access",
];

/// Whether a resolver error means the cookies need to be refreshed.
pub fn is_auth_challenge(message: &str) -> bool {
    let lowercase = normalize(message);
    for phrase in GENERIC_PHRASES {
        if lowercase.contains(&normalize(phrase)) {
            return true;
        }
    }
    for source in crate::registry::all() {
        for phrase in source.challenge_phrases() {
            if lowercase.contains(&normalize(phrase)) {
                return true;
            }
        }
    }
    false
}

/// Whether a resolver error is worth a sign-in attempt for this service.
pub fn challenge_service(url: &str, message: &str) -> Option<Service> {
    let service = crate::registry::service_from_url(url).or_else(|| Service::from_url(url))?;
    if !is_auth_challenge(message) {
        return None;
    }
    Some(service)
}

fn normalize(text: &str) -> String {
    text.replace(['\u{FFFD}', '\u{2019}', '\u{2018}'], "'").to_ascii_lowercase()
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

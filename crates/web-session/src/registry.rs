//! The internal source plugin registry.
//!
//! `web-session` defines the hook points and a generic default; concrete
//! sources register themselves through the composition root so the rest of
//! the codebase can ask for behavior without knowing which source provides it.

use std::sync::OnceLock;

use media_model::Service;
use url::Url;

/// The hook points a source may override.
///
/// Unimplemented hooks fall back to generic behavior (no special cookies,
/// pages, or challenge handling beyond generic detection).
pub trait Source: Send + Sync {
    /// The service this source handles.
    fn service(&self) -> Service;

    /// Whether a URL belongs to this source.
    fn matches_url(&self, _url: &str) -> bool {
        false
    }

    /// The stable cookie file name used below the cookies directory.
    fn cookie_file_name(&self) -> &'static str {
        "cookies.txt"
    }

    /// Cookie names that indicate an authenticated profile for this source.
    fn auth_cookie_names(&self) -> &'static [&'static str] {
        &[]
    }

    /// Domains whose cookies should travel with this source's session.
    fn base_domains(&self) -> &'static [&'static str] {
        &[]
    }

    /// Domains that belong to the source itself (for auth-cookie checks).
    fn own_domains(&self) -> &'static [&'static str] {
        &[]
    }

    /// Phrases in resolver errors that indicate an auth/session challenge.
    fn challenge_phrases(&self) -> &'static [&'static str] {
        &[]
    }

    /// The home page for a session of this source.
    fn home_url(&self) -> Url {
        Url::parse("about:blank").expect("valid constant URL")
    }

    /// The login page for this source.
    fn login_url(&self) -> Url {
        Url::parse("about:blank").expect("valid constant URL")
    }

    /// The page used to verify whether the browser profile is signed in.
    fn check_url(&self) -> Url {
        Url::parse("about:blank").expect("valid constant URL")
    }

    /// Whether a URL is a login page for this source.
    fn is_login_page(&self, _url: &Url) -> bool {
        false
    }

    /// Whether the current page confirms that the profile is signed in.
    fn is_signed_in_location(&self, _url: &Url) -> bool {
        false
    }
}

/// Generic fallback used when no concrete source is registered.
#[derive(Debug, Clone, Copy, Default)]
pub struct Generic;

impl Source for Generic {
    fn service(&self) -> Service {
        // Not used for matching/routing; callers always have a concrete service.
        Service::YouTube
    }
}

/// Built-in YouTube behavior used when no source plugins are installed.
#[derive(Debug, Clone, Copy, Default)]
struct BuiltinYouTube;

impl Source for BuiltinYouTube {
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
}

/// Built-in Twitch behavior used when no source plugins are installed.
#[derive(Debug, Clone, Copy, Default)]
struct BuiltinTwitch;

impl Source for BuiltinTwitch {
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

    fn challenge_phrases(&self) -> &'static [&'static str] {
        &[
            "oauth may be required",
            "subscriber-only",
            "only available to subscribers",
            "unauthorized_entitlements",
            "vod_manifest_restricted",
        ]
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
}

fn host_is(host: &str, base: &str) -> bool {
    host == base || host.ends_with(&format!(".{base}"))
}

static GENERIC: Generic = Generic;
static BUILTIN_YOUTUBE: BuiltinYouTube = BuiltinYouTube;
static BUILTIN_TWITCH: BuiltinTwitch = BuiltinTwitch;
static BUILTIN_SOURCES: &[&dyn Source] = &[&BUILTIN_YOUTUBE, &BUILTIN_TWITCH];
static SOURCES: OnceLock<Sources> = OnceLock::new();

/// The set of sources known to the application.
#[derive(Clone, Copy)]
pub struct Sources {
    entries: &'static [&'static dyn Source],
}

impl Sources {
    /// Creates a registry from a static list of sources.
    pub fn new(entries: &'static [&'static dyn Source]) -> Self {
        Self { entries }
    }

    /// Installs the global source registry once.
    ///
    /// Later calls are ignored so existing behavior remains stable in tests.
    pub fn install(self) {
        let _ = SOURCES.set(self);
    }

    /// Returns the globally installed sources, if any.
    pub fn installed() -> Option<Self> {
        SOURCES.get().copied()
    }

    /// All registered sources.
    pub fn all(self) -> &'static [&'static dyn Source] {
        self.entries
    }

    /// The source for a service, or a generic fallback.
    pub fn source(self, service: Service) -> &'static dyn Source {
        self.entries.iter().copied().find(|source| source.service() == service).unwrap_or(&GENERIC)
    }

    /// The source a URL belongs to, if any registered source claims it.
    pub fn from_url(self, url: &str) -> Option<&'static dyn Source> {
        self.entries.iter().copied().find(|source| source.matches_url(url))
    }
}

/// Returns the active registry, defaulting to built-in known sources.
fn registry() -> Sources {
    SOURCES.get().copied().unwrap_or(Sources { entries: BUILTIN_SOURCES })
}

/// The source for a service using the active registry.
pub fn source(service: Service) -> &'static dyn Source {
    registry().source(service)
}

/// All registered sources from the active registry.
pub fn all() -> &'static [&'static dyn Source] {
    registry().all()
}

/// The source a URL belongs to using the active registry.
pub fn from_url(url: &str) -> Option<&'static dyn Source> {
    registry().from_url(url)
}

/// The service a URL belongs to using the active registry.
pub fn service_from_url(url: &str) -> Option<Service> {
    from_url(url).map(|source| source.service())
}

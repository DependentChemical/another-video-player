//! The sites Flatline knows how to sign in to.
//!
//! A service is part of the shared vocabulary because two unrelated
//! capabilities need the same answer: the resolver decides which cookie file
//! belongs to a URL, and the session capability decides which sign-in page
//! to open. Neither has to know the other exists.

use std::fmt;

use url::Url;

/// A site whose answers can require an authenticated session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Service {
    /// YouTube, including its short links and cookie-less embed host.
    YouTube,
    /// Twitch, including clips and other subdomains.
    Twitch,
}

impl Service {
    /// The service a URL belongs to, when Flatline supports signing in there.
    pub fn from_url(url: &str) -> Option<Service> {
        let parsed = Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?.to_ascii_lowercase();
        if host_is(&host, "youtube.com")
            || host_is(&host, "youtu.be")
            || host_is(&host, "youtube-nocookie.com")
        {
            return Some(Service::YouTube);
        }
        if host_is(&host, "twitch.tv") {
            return Some(Service::Twitch);
        }
        None
    }

    /// The stable name used in command lines, file names and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Service::YouTube => "youtube",
            Service::Twitch => "twitch",
        }
    }

    /// The service with the given stable name.
    pub fn from_name(name: &str) -> Option<Service> {
        match name {
            "youtube" => Some(Service::YouTube),
            "twitch" => Some(Service::Twitch),
            _ => None,
        }
    }

    /// The human readable name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Service::YouTube => "YouTube",
            Service::Twitch => "Twitch",
        }
    }
}

impl fmt::Display for Service {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Whether a host is `base` itself or one of its subdomains.
fn host_is(host: &str, base: &str) -> bool {
    host == base || host.ends_with(&format!(".{base}"))
}

#[cfg(test)]
mod tests {
    use super::Service;

    #[test]
    fn youtube_hosts_are_recognised() {
        for url in [
            "https://youtube.com/watch?v=1",
            "https://www.youtube.com/watch?v=1",
            "https://music.youtube.com/watch?v=1",
            "https://youtu.be/1",
            "https://www.youtube-nocookie.com/embed/1",
        ] {
            assert_eq!(Service::from_url(url), Some(Service::YouTube), "{url}");
        }
    }

    #[test]
    fn twitch_hosts_are_recognised() {
        for url in [
            "https://twitch.tv/channel",
            "https://www.twitch.tv/channel",
            "https://m.twitch.tv/channel",
            "https://clips.twitch.tv/Clip",
        ] {
            assert_eq!(Service::from_url(url), Some(Service::Twitch), "{url}");
        }
    }

    #[test]
    fn lookalike_hosts_are_not_recognised() {
        for url in [
            "https://youtube.com.example.org/watch?v=1",
            "https://notyoutube.com/watch?v=1",
            "https://nottwitch.tv/channel",
            "https://twitch.tv.example.org/channel",
            "file:///video.mp4",
        ] {
            assert_eq!(Service::from_url(url), None, "{url}");
        }
    }

    #[test]
    fn names_round_trip() {
        for service in [Service::YouTube, Service::Twitch] {
            assert_eq!(Service::from_name(service.name()), Some(service));
        }
        assert_eq!(Service::from_name("other"), None);
    }
}

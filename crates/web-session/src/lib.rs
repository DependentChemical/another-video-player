//! Deciding when Flatline needs a signed-in session, and keeping the cookie
//! file `yt-dlp` reads in the format it expects.
//!
//! This crate sits on layer 1: it knows the shared vocabulary and the
//! Netscape cookie format, and nothing else in Flatline. It never opens a
//! browser itself — `web-session-view` does that — so the rules about *when*
//! a sign-in is needed and *what* is written stay testable without one.

use std::fmt;
use std::io;
use std::path::PathBuf;

use media_model::Service;
use url::Url;

pub mod challenge;
pub mod cookie;

pub use challenge::{challenge_service, is_auth_challenge};
pub use cookie::{Cookie, cookie_file, cookie_file_name, netscape_text, signed_in};

/// What a sign-in session has to do.
#[derive(Debug, Clone)]
pub struct Options {
    /// The service to sign in to.
    pub service: Service,
    /// The page whose cookies are exported after signing in.
    pub target: Url,
    /// The Netscape cookie file to write.
    pub cookies: PathBuf,
    /// The browser profile kept between runs.
    pub user_data: PathBuf,
}

impl Options {
    /// Parses the target URL and collects the paths of the session.
    pub fn new(
        service: Service,
        target: &str,
        cookies: impl Into<PathBuf>,
        user_data: impl Into<PathBuf>,
    ) -> Result<Self> {
        let target = Url::parse(target).map_err(Error::Url)?;
        Ok(Self { service, target, cookies: cookies.into(), user_data: user_data.into() })
    }
}

/// The page a session starts on, signed in or not.
pub fn home_url(service: Service) -> Url {
    match service {
        Service::YouTube => Url::parse("https://www.youtube.com/"),
        Service::Twitch => Url::parse("https://www.twitch.tv/"),
    }
    .expect("the service home is a constant, valid URL")
}

/// The page a user signs in on.
pub fn login_url(service: Service) -> Url {
    match service {
        Service::YouTube => Url::parse(
            "https://accounts.google.com/ServiceLogin?service=youtube&continue=https://www.youtube.com/",
        ),
        Service::Twitch => Url::parse("https://www.twitch.tv/login"),
    }
    .expect("the service login is a constant, valid URL")
}

/// What can go wrong before a cookie file exists.
#[derive(Debug)]
pub enum Error {
    /// The target URL was not a URL.
    Url(url::ParseError),
    /// The cookie file could not be written.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Url(error) => write!(formatter, "invalid target URL: {error}"),
            Error::Io(error) => write!(formatter, "cookie file could not be written: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Url(error) => Some(error),
            Error::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

/// The result of a session.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Options, home_url, login_url};
    use media_model::Service;

    #[test]
    fn every_service_has_constant_pages() {
        for service in [Service::YouTube, Service::Twitch] {
            assert_eq!(home_url(service).scheme(), "https");
            assert_eq!(login_url(service).scheme(), "https");
        }
    }

    #[test]
    fn the_login_page_of_a_service_is_not_its_home() {
        for service in [Service::YouTube, Service::Twitch] {
            assert_ne!(home_url(service), login_url(service));
        }
    }

    #[test]
    fn a_session_needs_a_parsable_target() {
        let options = Options::new(
            Service::YouTube,
            "https://www.youtube.com/watch?v=1",
            "cookies-youtube.txt",
            "user-data",
        )
        .expect("the target is a valid URL");
        assert_eq!(options.service, Service::YouTube);
        assert_eq!(options.cookies, std::path::PathBuf::from("cookies-youtube.txt"));
        assert_eq!(options.user_data, std::path::PathBuf::from("user-data"));
    }

    #[test]
    fn a_broken_target_is_reported() {
        assert!(Options::new(Service::Twitch, "not a url", "c", "u").is_err());
    }
}

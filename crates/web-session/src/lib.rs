use std::fmt;
use std::io;
use std::path::PathBuf;

use media_model::Service;
use url::Url;

pub mod challenge;
pub mod cookie;
pub mod registry;

pub use challenge::{challenge_service, is_auth_challenge};
pub use cookie::{
    Cookie, cookie_file, cookie_file_name, netscape_text, parse_netscape, read_file, signed_in,
    signed_in_file,
};
pub use registry::{Source, Sources};

/// The page used to verify whether the browser profile is signed in.
///
/// The URL is chosen so a signed-out profile is redirected to a sign-in page,
/// which makes the check a real page-level session test instead of a local
/// cookie existence check.
pub fn check_url(service: Service) -> Url {
    registry::source(service).check_url()
}

/// Whether a URL is a sign-in page for the service.
pub fn is_login_page(service: Service, url: &Url) -> bool {
    registry::source(service).is_login_page(url)
}

/// Whether the current page confirms that the profile is signed in.
///
/// A successful check stays on a service page after loading the check URL.
/// A redirect to a sign-in page means the session is missing or no longer
/// accepted by the service.
pub fn is_signed_in_location(service: Service, url: &Url) -> bool {
    registry::source(service).is_signed_in_location(url)
}

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
    registry::source(service).home_url()
}

/// The page a user signs in on.
pub fn login_url(service: Service) -> Url {
    registry::source(service).login_url()
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
    use super::Options;
    use media_model::Service;

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

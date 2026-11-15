//! Asking the user to sign in when a service demands an authenticated session.
//!
//! The sign-in happens in a helper program of its own: the player starts it,
//! waits for it and reads the cookie file it wrote. That keeps the browser
//! out of the player's process, so a browser that hangs cannot hang the
//! player with it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use media_model::Service;

/// The sign-in helper and the files it works with.
#[derive(Debug, Clone)]
pub struct Session {
    /// The helper that opens the sign-in window.
    helper: PathBuf,
    /// The directory the cookie files live in.
    cookies: PathBuf,
    /// The browser profile kept between runs.
    user_data: PathBuf,
    /// The URLs a sign-in was already offered for.
    ///
    /// The player retries a failing resolution, and a sign-in window that
    /// opens again on every attempt would be a window the user has to close
    /// again and again. One attempt per URL is enough.
    attempts: Arc<Mutex<HashSet<String>>>,
}

impl Session {
    /// Collects the helper and the files it works with.
    pub fn new(helper: PathBuf, cookies: PathBuf, user_data: PathBuf) -> Session {
        Session { helper, cookies, user_data, attempts: Arc::new(Mutex::new(HashSet::new())) }
    }

    /// The cookie file of a service.
    pub fn cookie_file(&self, service: Service) -> PathBuf {
        web_session::cookie_file(&self.cookies, service)
    }

    /// The directory the cookie files live in.
    pub fn cookies_dir(&self) -> &std::path::Path {
        &self.cookies
    }

    /// Whether the helper is there.
    ///
    /// A build without it simply cannot sign in; the player reports the
    /// problem instead of pretending it can.
    pub fn is_available(&self) -> bool {
        self.helper.is_file()
    }

    /// Signs in to a service and waits for the cookie file to be written.
    ///
    /// A URL is only offered once per challenge: the helper runs in the
    /// foreground of this call, so the retry loop cannot schedule another
    /// attempt until the user has finished or closed the window. The URL is
    /// released afterwards so a later challenge can ask again.
    pub fn sign_in(&self, service: Service, target: &str) -> Result<(), Error> {
        if !self.offer(target) {
            return Err(Error::AlreadyOffered);
        }
        let result = self.run_sign_in(service, target);
        self.release(target);
        result
    }

    fn run_sign_in(&self, service: Service, target: &str) -> Result<(), Error> {
        if !self.is_available() {
            return Err(Error::Missing(self.helper.clone()));
        }
        let output = Command::new(&self.helper)
            .arg("--service")
            .arg(service.name())
            .arg("--target")
            .arg(target)
            .arg("--cookies")
            .arg(self.cookie_file(service))
            .arg("--user-data")
            .arg(&self.user_data)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(Error::Start)?;
        if output.status.success() {
            Ok(())
        } else {
            let problem = String::from_utf8_lossy(&output.stderr);
            let problem = problem.trim();
            if problem.is_empty() {
                Err(Error::Failed(format!("the {service} sign-in did not finish")))
            } else {
                Err(Error::Failed(problem.to_string()))
            }
        }
    }

    /// Remembers that a sign-in is in progress for a URL.
    ///
    /// Returns whether this is the first offer.
    fn offer(&self, target: &str) -> bool {
        let Ok(mut attempts) = self.attempts.lock() else {
            return true;
        };
        attempts.insert(target.to_string())
    }

    /// Allows the same URL to be offered again on a later challenge.
    fn release(&self, target: &str) {
        if let Ok(mut attempts) = self.attempts.lock() {
            attempts.remove(target);
        }
    }
}

/// Why the sign-in could not be done.
#[derive(Debug)]
pub enum Error {
    /// The helper is not installed next to the player.
    Missing(PathBuf),
    /// The helper could not be started.
    Start(std::io::Error),
    /// The helper ran but did not finish the sign-in.
    Failed(String),
    /// The user was already asked to sign in for this URL.
    AlreadyOffered,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Missing(path) => {
                write!(formatter, "the sign-in helper is missing at {}", path.display())
            }
            Error::Start(error) => write!(formatter, "the sign-in helper could not start: {error}"),
            Error::Failed(problem) => write!(formatter, "{problem}"),
            Error::AlreadyOffered => write!(formatter, "the sign-in was already offered"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Start(error) => Some(error),
            Error::Missing(_) | Error::Failed(_) | Error::AlreadyOffered => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Session;
    use media_model::Service;

    #[test]
    fn the_cookie_files_are_named_after_their_service() {
        let session = Session::new("helper".into(), "cache/cookies".into(), "user-data".into());
        assert_eq!(
            session.cookie_file(Service::YouTube),
            std::path::PathBuf::from("cache/cookies").join("cookies-youtube.txt")
        );
        assert_eq!(
            session.cookie_file(Service::Twitch),
            std::path::PathBuf::from("cache/cookies").join("cookies-twitch.txt")
        );
    }

    #[test]
    fn a_missing_helper_is_reported_instead_of_ignored() {
        let session = Session::new(
            "there-is-no-helper-here".into(),
            "cache/cookies".into(),
            "user-data".into(),
        );
        assert!(!session.is_available());
        let error = session
            .sign_in(Service::YouTube, "https://www.youtube.com/watch?v=1")
            .expect_err("the helper is missing");
        assert!(matches!(error, super::Error::Missing(_)));
    }

    #[test]
    fn a_url_can_be_offered_again_after_the_attempt_finishes() {
        let session = Session::new(
            "there-is-no-helper-here".into(),
            "cache/cookies".into(),
            "user-data".into(),
        );
        let url = "https://www.youtube.com/watch?v=1";
        let first = session.sign_in(Service::YouTube, url).expect_err("the helper is missing");
        assert!(matches!(first, super::Error::Missing(_)));
        let second =
            session.sign_in(Service::YouTube, url).expect_err("the helper is still missing");
        assert!(matches!(second, super::Error::Missing(_)));
    }

    #[test]
    fn another_url_is_offered_on_its_own() {
        let session = Session::new(
            "there-is-no-helper-here".into(),
            "cache/cookies".into(),
            "user-data".into(),
        );
        let _ = session.sign_in(Service::YouTube, "https://www.youtube.com/watch?v=1");
        let other = session
            .sign_in(Service::YouTube, "https://www.youtube.com/watch?v=2")
            .expect_err("the helper is missing");
        assert!(matches!(other, super::Error::Missing(_)));
    }
}

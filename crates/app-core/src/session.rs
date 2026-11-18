//! Asking the user to sign in when a service demands an authenticated session.
//!
//! The sign-in happens in a helper program of its own: the player starts it,
//! waits for it and reads the cookie file it wrote. That keeps the browser
//! out of the player's process, so a browser that hangs cannot hang the
//! player with it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use media_model::Service;

use crate::app::Message;

/// The outcome remembered for a URL after a sign-in attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// A sign-in is currently in progress.
    Offered,
    /// The user chose not to sign in for this URL.
    Declined,
}

/// The sign-in helper and the files it works with.
#[derive(Debug, Clone)]
pub struct Session {
    /// The helper that opens the sign-in window.
    helper: PathBuf,
    /// The directory the cookie files live in.
    cookies: PathBuf,
    /// The browser profile kept between runs.
    user_data: PathBuf,
    /// The URLs a sign-in was already offered for, or declined.
    attempts: Arc<Mutex<HashMap<String, Attempt>>>,
}

impl Session {
    /// Collects the helper and the files it works with.
    pub fn new(helper: PathBuf, cookies: PathBuf, user_data: PathBuf) -> Session {
        Session { helper, cookies, user_data, attempts: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// The cookie file of a service.
    pub fn cookie_file(&self, service: Service) -> PathBuf {
        web_session::cookie_file(&self.cookies, service)
    }

    /// Whether the stored cookie file currently looks like a signed-in session.
    ///
    /// This is a local check over the Netscape cookie file. The interactive
    /// sign-in helper performs the authoritative background check against the
    /// service's sign-in page before it decides that a session is still valid.
    pub fn has_signed_in_cookies(&self, service: Service) -> bool {
        web_session::cookie::signed_in_file(service, &self.cookie_file(service))
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
    /// released afterwards so a later challenge can ask again, unless the
    /// user declined to sign in.
    pub fn sign_in(
        &self,
        service: Service,
        target: &str,
        notice: &Sender<Box<Message>>,
    ) -> Result<(), Error> {
        match self.begin(target) {
            Offer::Proceed => {}
            Offer::AlreadyOffered => return Err(Error::AlreadyOffered),
            Offer::Declined => return Err(Error::Declined),
        }
        let result = self.run_sign_in(service, target, notice);
        self.finish(target, matches!(result, Err(Error::Declined)));
        result
    }

    fn run_sign_in(
        &self,
        service: Service,
        target: &str,
        notice: &Sender<Box<Message>>,
    ) -> Result<(), Error> {
        if !self.is_available() {
            return Err(Error::Missing(self.helper.clone()));
        }

        let mut command = Command::new(&self.helper);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .arg("--service")
            .arg(service.name())
            .arg("--target")
            .arg(target)
            .arg("--cookies")
            .arg(self.cookie_file(service))
            .arg("--user-data")
            .arg(&self.user_data)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(Error::Start)?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let notice_for_events = notice.clone();

        let stdout_thread = thread::spawn(move || {
            let mut declined = false;
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    match line.trim() {
                        "event:sign-in-required" => {
                            let _ = notice_for_events.send(Box::new(Message::SigningIn(service)));
                        }
                        "event:regenerating" => {
                            let _ = notice_for_events
                                .send(Box::new(Message::RegeneratingCookies(service)));
                        }
                        "event:declined" => {
                            declined = true;
                        }
                        _ => {}
                    }
                }
            }
            declined
        });

        let stderr_thread = thread::spawn(move || {
            let mut text = String::new();
            if let Some(stderr) = stderr {
                let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut text);
            }
            text
        });

        let status = child.wait().map_err(Error::Start)?;
        let declined = stdout_thread.join().unwrap_or(false);
        let stderr = stderr_thread.join().unwrap_or_default();

        if status.success() {
            Ok(())
        } else if declined || stderr.contains("the sign-in was declined") {
            Err(Error::Declined)
        } else {
            let problem = stderr.trim();
            if problem.is_empty() {
                Err(Error::Failed(format!("the {service} sign-in did not finish")))
            } else {
                Err(Error::Failed(problem.to_string()))
            }
        }
    }

    /// Remembers that a sign-in is in progress for a URL.
    fn begin(&self, target: &str) -> Offer {
        let Ok(mut attempts) = self.attempts.lock() else {
            return Offer::Proceed;
        };
        match attempts.get(target).copied() {
            Some(Attempt::Declined) => Offer::Declined,
            Some(Attempt::Offered) => Offer::AlreadyOffered,
            None => {
                attempts.insert(target.to_string(), Attempt::Offered);
                Offer::Proceed
            }
        }
    }

    /// Records the outcome of a sign-in attempt.
    fn finish(&self, target: &str, declined: bool) {
        if let Ok(mut attempts) = self.attempts.lock() {
            if declined {
                attempts.insert(target.to_string(), Attempt::Declined);
            } else {
                attempts.remove(target);
            }
        }
    }
}

/// The result of trying to start a sign-in attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offer {
    Proceed,
    AlreadyOffered,
    Declined,
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
    /// The user chose not to sign in.
    Declined,
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
            Error::Declined => write!(formatter, "the sign-in was declined"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Start(error) => Some(error),
            Error::Missing(_) | Error::Failed(_) | Error::AlreadyOffered | Error::Declined => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Session;
    use crate::app::Message;
    use media_model::Service;
    use std::sync::mpsc::channel;

    fn notice() -> std::sync::mpsc::Sender<Box<Message>> {
        let (sender, _receiver) = channel();
        sender
    }

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
            .sign_in(Service::YouTube, "https://www.youtube.com/watch?v=1", &notice())
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
        let first =
            session.sign_in(Service::YouTube, url, &notice()).expect_err("the helper is missing");
        assert!(matches!(first, super::Error::Missing(_)));
        let second = session
            .sign_in(Service::YouTube, url, &notice())
            .expect_err("the helper is still missing");
        assert!(matches!(second, super::Error::Missing(_)));
    }

    #[test]
    fn another_url_is_offered_on_its_own() {
        let session = Session::new(
            "there-is-no-helper-here".into(),
            "cache/cookies".into(),
            "user-data".into(),
        );
        let _ = session.sign_in(Service::YouTube, "https://www.youtube.com/watch?v=1", &notice());
        let other = session
            .sign_in(Service::YouTube, "https://www.youtube.com/watch?v=2", &notice())
            .expect_err("the helper is missing");
        assert!(matches!(other, super::Error::Missing(_)));
    }

    #[test]
    fn a_declined_url_is_not_offered_again() {
        let session = Session::new(
            "there-is-no-helper-here".into(),
            "cache/cookies".into(),
            "user-data".into(),
        );
        session.finish("https://www.youtube.com/watch?v=1", true);
        let error = session
            .sign_in(Service::YouTube, "https://www.youtube.com/watch?v=1", &notice())
            .expect_err("the sign-in was declined");
        assert!(matches!(error, super::Error::Declined));
    }
}

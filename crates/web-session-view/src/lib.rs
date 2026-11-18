//! Signing in to a service in a webview and exporting the cookies.
//!
//! Layer 2 adapter: it binds a browser to the sign-in capability of
//! `web-session`, the same way `video-surface` binds the mpv renderer to the
//! playback engine. The capability owns the rules — when a sign-in is needed
//! and what the cookie file looks like — this crate owns the browser.
//!
//! The helper is a program of its own rather than part of the player: a
//! webview needs an event loop of its own, and a browser that misbehaves
//! should never be able to take the player down with it.

use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use media_model::Service;
use tauri::{AppHandle, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;
use web_session::{Cookie, Options, check_url, cookie, home_url, login_url};

/// How long the hidden session check waits before the sign-in window appears.
const HIDDEN_CHECK_TIMEOUT: Duration = Duration::from_secs(12);
/// How long the user has to finish signing in.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a page load is given to reach its destination.
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// How often the webview is asked for its cookies and current URL.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long a page is given to finish its own requests.
const SETTLE_DELAY: Duration = Duration::from_secs(3);
/// The label of the window the helper opens.
const WINDOW_LABEL: &str = "flatline-session";
/// The size of the sign-in window, in logical pixels.
const WINDOW_WIDTH: f64 = 520.0;
const WINDOW_HEIGHT: f64 = 760.0;

/// What can go wrong before a cookie file exists.
#[derive(Debug)]
pub enum Error {
    /// The webview could not be created or queried.
    Webview(tauri::Error),
    /// The user did not finish signing in.
    TimedOut(&'static str),
    /// The user closed the sign-in window.
    Cancelled,
    /// The user chose not to sign in.
    SignInDeclined,
    /// The profile holds no cookies for the service.
    NoCookies(Service),
    /// The cookie file could not be written.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Webview(error) => write!(formatter, "the webview failed: {error}"),
            Error::TimedOut(what) => write!(formatter, "{what} did not finish in time"),
            Error::Cancelled => write!(formatter, "the sign-in window was closed"),
            Error::SignInDeclined => write!(formatter, "the sign-in was declined"),
            Error::NoCookies(service) => write!(formatter, "no {service} cookies were found"),
            Error::Io(error) => write!(formatter, "the cookie file could not be written: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Webview(error) => Some(error),
            Error::Io(error) => Some(error),
            Error::TimedOut(_) | Error::Cancelled | Error::SignInDeclined | Error::NoCookies(_) => {
                None
            }
        }
    }
}

impl From<tauri::Error> for Error {
    fn from(error: tauri::Error) -> Self {
        Error::Webview(error)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

/// The result of a session.
pub type Result<T> = std::result::Result<T, Error>;

/// Reports a session event to the parent process.
fn emit_event(name: &str) {
    println!("event:{name}");
    let _ = io::stdout().flush();
}

/// Opens the webview, signs in when needed and writes the cookie file.
///
/// The call blocks until the cookie file exists or the session fails, and
/// then the helper is done.
pub fn run(options: Options) -> Result<()> {
    std::fs::create_dir_all(&options.user_data)?;

    let (sender, receiver) = std::sync::mpsc::channel::<Result<()>>();
    let closed = Arc::new(AtomicBool::new(false));
    let worker_closed = Arc::clone(&closed);
    let run_closed = Arc::clone(&closed);

    let app = tauri::Builder::default()
        .setup(move |app| {
            let window = create_window(app.handle(), &options)?;
            let handle = app.handle().clone();
            let worker_options = options.clone();
            let worker_sender = sender.clone();
            std::thread::spawn(move || {
                let result = session(&window, &worker_options, &worker_closed);
                let _ = worker_sender.send(result);
                handle.exit(0);
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(Error::Webview)?;

    app.run(move |_handle, event| {
        // A closed window means the user gave up; the worker stops with it.
        if let RunEvent::WindowEvent { label, event: tauri::WindowEvent::Destroyed, .. } = event
            && label == WINDOW_LABEL
        {
            run_closed.store(true, Ordering::SeqCst);
        }
    });

    receiver.recv().unwrap_or(Err(Error::Cancelled))
}

/// Creates the session window with its own persistent browser profile.
fn create_window(app: &AppHandle, options: &Options) -> Result<WebviewWindow> {
    std::fs::create_dir_all(&options.user_data)?;
    let window = WebviewWindowBuilder::new(
        app,
        WINDOW_LABEL,
        WebviewUrl::External(home_url(options.service)),
    )
    .title(format!("Sign in to {}", options.service))
    .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
    .min_inner_size(420.0, 520.0)
    .resizable(true)
    .visible(false)
    .data_directory(options.user_data.clone())
    .build()?;
    Ok(window)
}

/// One complete sign-in and export, running on a worker thread.
fn session(window: &WebviewWindow, options: &Options, closed: &AtomicBool) -> Result<()> {
    // The hidden check loads a real page that requires authentication, so a
    // profile with stale or revoked cookies is not mistaken for a signed-in
    // session just because a cookie file still exists.
    match verify_session(window, options.service, HIDDEN_CHECK_TIMEOUT, closed) {
        Wait::SignedIn => {}
        Wait::Cancelled => return Err(Error::Cancelled),
        Wait::TimedOut => {
            emit_event("sign-in-required");
            show_sign_in(window, options.service)?;
            window.navigate(login_url(options.service))?;
            match wait_for_interactive_sign_in(window, options.service, SIGN_IN_TIMEOUT, closed) {
                Wait::SignedIn => {}
                Wait::Cancelled => {
                    emit_event("declined");
                    return Err(Error::SignInDeclined);
                }
                Wait::TimedOut => return Err(Error::TimedOut("the sign-in")),
            }
        }
    }

    // Loading the target lets the service refresh the cookies that only a
    // real playback page sets before yt-dlp reads the file.
    emit_event("regenerating");
    window.hide()?;
    window.navigate(options.target.clone())?;
    let _ = wait_for_page(window, &options.target, LOAD_TIMEOUT, closed);

    let cookies = window.cookies()?;
    let cookies = cookie::for_service(options.service, &convert(options.service, &cookies));
    if cookies.is_empty() {
        return Err(Error::NoCookies(options.service));
    }
    cookie::write_file(&options.cookies, &cookies)?;
    Ok(())
}

/// The outcome of waiting for a signed-in profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// The profile is signed in.
    SignedIn,
    /// The window was closed before that happened.
    Cancelled,
    /// The wait ran out or the session is not signed in.
    TimedOut,
}

/// Verifies the session by loading a page that requires authentication.
fn verify_session(
    window: &WebviewWindow,
    service: Service,
    timeout: Duration,
    closed: &AtomicBool,
) -> Wait {
    let check = check_url(service);
    if window.navigate(check.clone()).is_err() {
        return Wait::TimedOut;
    }
    match wait_for_page(window, &check, timeout, closed) {
        Wait::SignedIn => {}
        Wait::Cancelled => return Wait::Cancelled,
        Wait::TimedOut => {}
    }
    if closed.load(Ordering::SeqCst) {
        return Wait::Cancelled;
    }
    let Ok(current) = window.url() else {
        return Wait::TimedOut;
    };
    if web_session::is_login_page(service, &current) {
        return Wait::TimedOut;
    }
    if !web_session::is_signed_in_location(service, &current) {
        return Wait::TimedOut;
    }
    match window.cookies() {
        Ok(cookies) if web_session::signed_in(service, &convert(service, &cookies)) => {
            Wait::SignedIn
        }
        Ok(_) | Err(_) => Wait::TimedOut,
    }
}

/// Waits until the interactive sign-in produces authentication cookies.
fn wait_for_interactive_sign_in(
    window: &WebviewWindow,
    service: Service,
    timeout: Duration,
    closed: &AtomicBool,
) -> Wait {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if closed.load(Ordering::SeqCst) {
            return Wait::Cancelled;
        }
        if let Ok(cookies) = window.cookies()
            && web_session::signed_in(service, &convert(service, &cookies))
            && window.url().is_ok_and(|current| !web_session::is_login_page(service, &current))
        {
            return Wait::SignedIn;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Wait::TimedOut
}

/// Waits until the webview reaches a page and gives it time to settle.
///
/// Reaching a sign-in page is reported as not signed in so the caller can
/// decide whether to ask the user to authenticate.
fn wait_for_page(
    window: &WebviewWindow,
    target: &Url,
    timeout: Duration,
    closed: &AtomicBool,
) -> Wait {
    let deadline = Instant::now() + timeout;
    let service = Service::from_url(target.as_str());
    while Instant::now() < deadline {
        if closed.load(Ordering::SeqCst) {
            return Wait::Cancelled;
        }
        if let Ok(current) = window.url() {
            if let Some(service) = service
                && web_session::is_login_page(service, &current)
            {
                return Wait::TimedOut;
            }
            if reached_target(target, &current) {
                std::thread::sleep(SETTLE_DELAY);
                return Wait::SignedIn;
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Wait::TimedOut
}

fn reached_target(target: &Url, current: &Url) -> bool {
    current.as_str() == target.as_str()
        || (current.host_str() == target.host_str() && current.path() == target.path())
}

/// Makes the window visible and puts it in front of the player.
fn show_sign_in(window: &WebviewWindow, service: Service) -> Result<()> {
    window.set_title(&format!("Sign in to {service}"))?;
    window.show()?;
    window.set_focus()?;
    Ok(())
}

/// Converts the webview cookies into the vocabulary of the capability.
///
/// The webview reports a domain without the leading dot a browser uses for a
/// cookie that covers the subdomains of its domain, so the service decides
/// that part again.
fn convert(service: Service, cookies: &[tauri::webview::Cookie<'static>]) -> Vec<Cookie> {
    cookies
        .iter()
        .map(|cookie| Cookie {
            domain: web_session::cookie::netscape_domain(
                service,
                cookie.domain().unwrap_or_default(),
            ),
            path: cookie.path().unwrap_or("/").to_string(),
            secure: cookie.secure().unwrap_or(false),
            http_only: cookie.http_only().unwrap_or(false),
            expires: cookie.expires_datetime().map(|expires| expires.unix_timestamp()),
            name: cookie.name().to_string(),
            value: cookie.value().to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Wait, convert, reached_target};
    use media_model::Service;
    use tauri::webview::Cookie as WebviewCookie;
    use url::Url;

    #[test]
    fn a_webview_cookie_becomes_a_flat_cookie() {
        let mut cookie = WebviewCookie::build(("SID", "value")).path("/").build();
        cookie.set_domain(".youtube.com");
        cookie.set_secure(true);
        cookie.set_http_only(true);
        let converted = convert(Service::YouTube, std::slice::from_ref(&cookie));
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].name, "SID");
        assert_eq!(converted[0].value, "value");
        assert_eq!(converted[0].path, "/");
        assert!(converted[0].secure);
        assert!(converted[0].http_only);
    }

    #[test]
    fn a_cookie_on_the_base_domain_covers_its_subdomains() {
        // The webview reports the domain without the leading dot, so the
        // service has to put it back for a domain cookie.
        let mut cookie = WebviewCookie::build(("SID", "value")).build();
        cookie.set_domain(".youtube.com");
        let converted = convert(Service::YouTube, std::slice::from_ref(&cookie));
        assert_eq!(converted[0].domain, ".youtube.com");
    }

    #[test]
    fn a_cookie_on_a_single_host_stays_on_that_host() {
        let mut cookie = WebviewCookie::build(("theme", "dark")).build();
        cookie.set_domain("www.twitch.tv");
        let converted = convert(Service::Twitch, std::slice::from_ref(&cookie));
        assert_eq!(converted[0].domain, "www.twitch.tv");
    }

    #[test]
    fn the_wait_outcomes_are_distinct() {
        assert_ne!(Wait::SignedIn, Wait::Cancelled);
        assert_ne!(Wait::Cancelled, Wait::TimedOut);
        assert_ne!(Wait::SignedIn, Wait::TimedOut);
    }

    #[test]
    fn a_check_page_is_reached_when_host_and_path_match() {
        let target = Url::parse("https://www.youtube.com/feed/subscriptions").expect("valid URL");
        let current =
            Url::parse("https://www.youtube.com/feed/subscriptions?flow=1").expect("valid URL");
        assert!(reached_target(&target, &current));
    }

    #[test]
    fn a_redirect_to_a_different_page_is_not_the_target() {
        let target = Url::parse("https://www.twitch.tv/settings/profile").expect("valid URL");
        let current = Url::parse("https://www.twitch.tv/login").expect("valid URL");
        assert!(!reached_target(&target, &current));
    }
}

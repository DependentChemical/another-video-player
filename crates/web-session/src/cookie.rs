//! Reading and writing the cookies a service session needs.
//!
//! The only format `yt-dlp` understands is the Netscape cookie file. This
//! module owns that format, the domain filtering that keeps a cookie file
//! scoped to one service and the small amount of knowledge needed to tell
//! whether a browser profile is signed in.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use media_model::Service;

/// The first line every Netscape cookie file has to start with.
pub const HEADER: &str = "# Netscape HTTP Cookie File";

/// One cookie taken from a browser profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    /// The domain the cookie belongs to, as the browser reports it.
    pub domain: String,
    /// The path the cookie is scoped to.
    pub path: String,
    /// Whether the cookie is only sent over HTTPS.
    pub secure: bool,
    /// Whether the cookie is hidden from JavaScript.
    pub http_only: bool,
    /// Expiry in seconds since the Unix epoch. `None` is a session cookie.
    pub expires: Option<i64>,
    /// The cookie name.
    pub name: String,
    /// The cookie value.
    pub value: String,
}

/// The name of the cookie file a service uses below the cookies directory.
pub fn cookie_file_name(service: Service) -> &'static str {
    match service {
        Service::YouTube => "cookies-youtube.txt",
        Service::Twitch => "cookies-twitch.txt",
    }
}

/// The cookie file a service uses inside `directory`.
pub fn cookie_file(directory: &Path, service: Service) -> PathBuf {
    directory.join(cookie_file_name(service))
}

/// Every cookie the profile holds for the service and its sign-in domains.
///
/// The whole set is kept, not a hand-picked list: services rotate their
/// authentication cookies and drop the ones nobody told them about, so a
/// narrow selection is the quickest way to break a working sign-in.
pub fn for_service(service: Service, cookies: &[Cookie]) -> Vec<Cookie> {
    cookies.iter().filter(|cookie| domain_belongs_to(service, &cookie.domain)).cloned().collect()
}

/// Whether a browser profile holds a cookie that marks it as signed in.
///
/// This is deliberately a cookie check and not a page check: it needs no
/// navigation, no JavaScript and no network round trip, so it is the fastest
/// answer a webview can give.
///
/// The cookie has to belong to the service itself. YouTube signs in through
/// Google, so a profile can hold a Google session without ever having been
/// signed in to YouTube — that profile still needs the sign-in window.
pub fn signed_in(service: Service, cookies: &[Cookie]) -> bool {
    cookies.iter().any(|cookie| {
        is_auth_cookie(service, &cookie.name) && own_domain_belongs_to(service, &cookie.domain)
    })
}

/// Whether `name` is one of the cookies that can only exist on a signed-in
/// profile.
pub fn is_auth_cookie(service: Service, name: &str) -> bool {
    let names: &[&str] = match service {
        Service::YouTube => &[
            "SAPISID",
            "__Secure-1PAPISID",
            "__Secure-3PAPISID",
            "__Secure-1PSID",
            "__Secure-3PSID",
        ],
        Service::Twitch => &["auth-token", "auth_token"],
    };
    names.iter().any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// Whether a cookie domain belongs to the service.
///
/// YouTube's sign-in bounces through Google, so the Google cookies have to
/// travel with the YouTube ones. The extra domains are matched with a dot
/// boundary, never with a plain substring.
pub fn domain_belongs_to(service: Service, domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    base_domains(service)
        .iter()
        .any(|base| domain == *base || domain.ends_with(&format!(".{base}")))
}

/// Whether a cookie domain belongs to the service itself.
///
/// This is the narrower question [`domain_belongs_to`] answers: a sign-in
/// cookie only counts when the service itself set it.
pub fn own_domain_belongs_to(service: Service, domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    own_domains(service).iter().any(|base| domain == *base || domain.ends_with(&format!(".{base}")))
}

/// The domains of the service that cover their subdomains.
///
/// A browser writes a cookie on one of these with a leading dot, because the
/// server asked for the whole tree. The webview no longer says which cookies
/// those are, so the service decides it here.
pub fn base_domains(service: Service) -> &'static [&'static str] {
    match service {
        Service::YouTube => &["youtube.com", "youtu.be", "youtube-nocookie.com", "google.com"],
        Service::Twitch => &["twitch.tv", "ttvnw.net", "jtvnw.net", "twitchcdn.net"],
    }
}

/// The domains of the service itself.
pub fn own_domains(service: Service) -> &'static [&'static str] {
    match service {
        Service::YouTube => &["youtube.com", "youtu.be", "youtube-nocookie.com"],
        Service::Twitch => &["twitch.tv"],
    }
}

/// The domain a cookie is written with in the Netscape format.
///
/// A cookie on the base domain of a service covers its subdomains and is
/// written with a leading dot; a cookie on a single host does not. This is
/// what a browser writes and what `yt-dlp` writes when it saves a file.
pub fn netscape_domain(service: Service, domain: &str) -> String {
    let bare = domain.trim_start_matches('.');
    if base_domains(service).iter().any(|base| bare.eq_ignore_ascii_case(base)) {
        format!(".{bare}")
    } else {
        bare.to_string()
    }
}

/// Serializes cookies in the Netscape format `yt-dlp` reads.
///
/// Windows gets CRLF line endings because that is what `yt-dlp` expects
/// there; every other platform gets LF.
pub fn netscape_text(cookies: &[Cookie]) -> String {
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    let mut text = String::new();
    text.push_str(HEADER);
    text.push_str(newline);
    text.push_str("# This file is generated by Flatline. Do not edit.");
    text.push_str(newline);
    text.push_str(newline);

    for cookie in cookies {
        text.push_str(&netscape_line(cookie));
        text.push_str(newline);
    }
    text
}

/// Writes the cookies of a service to its file, replacing it atomically.
pub fn write_file(path: &Path, cookies: &[Cookie]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = netscape_text(cookies);
    let temporary = temporary_path(path);
    std::fs::write(&temporary, text)?;
    restrict_permissions(&temporary)?;
    // `rename` does not replace an existing file on Windows, so the old file
    // is removed first. The temporary file is complete by then, which keeps
    // a reader from ever seeing a half written cookie file.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&temporary, path)
}

/// Reads cookies from a Netscape cookie file.
///
/// Malformed lines are ignored so a partially written or foreign file does
/// not break the check that depends on it.
pub fn read_file(path: &Path) -> io::Result<Vec<Cookie>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(read_reader(BufReader::new(file)))
}

/// Parses cookies from Netscape cookie file content.
pub fn parse_netscape(text: &str) -> Vec<Cookie> {
    read_reader(text.as_bytes())
}

/// Whether the cookie file holds a signed-in session for the service.
pub fn signed_in_file(service: Service, path: &Path) -> bool {
    read_file(path).is_ok_and(|cookies| signed_in(service, &cookies))
}

fn read_reader(reader: impl BufRead) -> Vec<Cookie> {
    let mut cookies = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let http_only = trimmed.starts_with("#HttpOnly_");
        if trimmed.starts_with('#') && !http_only {
            continue;
        }
        let line = if http_only { &trimmed["#HttpOnly_".len()..] } else { trimmed };
        let mut fields = line.splitn(7, '\t');
        let Some(domain) = fields.next() else { continue };
        let Some(_include_subdomains) = fields.next() else { continue };
        let Some(path) = fields.next() else { continue };
        let Some(secure) = fields.next() else { continue };
        let Some(expires) = fields.next() else { continue };
        let Some(name) = fields.next() else { continue };
        let Some(value) = fields.next() else { continue };
        if domain.is_empty() || name.is_empty() {
            continue;
        }
        let expires = expires.parse::<i64>().ok().filter(|value| *value > 0);
        cookies.push(Cookie {
            domain: domain.to_string(),
            path: if path.is_empty() { "/".to_string() } else { path.to_string() },
            secure: secure.eq_ignore_ascii_case("TRUE"),
            http_only,
            expires,
            name: name.to_string(),
            value: value.to_string(),
        });
    }
    cookies
}

/// One tab separated line in the Netscape format.
fn netscape_line(cookie: &Cookie) -> String {
    let domain = clean_field(&cookie.domain);
    let domain = if cookie.http_only { format!("#HttpOnly_{domain}") } else { domain };
    let include_subdomains = if cookie.domain.starts_with('.') { "TRUE" } else { "FALSE" };
    let path = clean_field(if cookie.path.is_empty() { "/" } else { &cookie.path });
    let secure = if cookie.secure { "TRUE" } else { "FALSE" };
    let expires = cookie.expires.unwrap_or(0).max(0);
    let name = clean_field(&cookie.name);
    let value = clean_field(&cookie.value);
    [
        domain.as_str(),
        include_subdomains,
        path.as_str(),
        secure,
        &expires.to_string(),
        name.as_str(),
        value.as_str(),
    ]
    .join("\t")
}

/// Removes the characters that would break a tab separated cookie line.
fn clean_field(value: &str) -> String {
    value.chars().filter(|character| !matches!(character, '\t' | '\r' | '\n')).collect()
}

/// The sibling file a cookie file is written to before it is renamed.
fn temporary_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Cookie files are credentials, so only the owner may read them.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)
}

/// Windows has no POSIX permission bits; the file inherits the user profile ACL.
#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Cookie, HEADER, cookie_file, cookie_file_name, domain_belongs_to, for_service,
        netscape_domain, netscape_text, parse_netscape, read_file, signed_in, signed_in_file,
        write_file,
    };
    use media_model::Service;

    fn cookie(domain: &str, name: &str, value: &str) -> Cookie {
        Cookie {
            domain: domain.to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: false,
            expires: Some(1_800_000_000),
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn cookie_file_names_are_stable() {
        assert_eq!(cookie_file_name(Service::YouTube), "cookies-youtube.txt");
        assert_eq!(cookie_file_name(Service::Twitch), "cookies-twitch.txt");
        assert_eq!(
            cookie_file(std::path::Path::new("cache/cookies"), Service::Twitch),
            std::path::Path::new("cache/cookies/cookies-twitch.txt")
        );
    }

    #[test]
    fn every_cookie_of_the_service_is_kept() {
        let cookies = vec![
            cookie(".youtube.com", "SID", "one"),
            cookie("www.youtube.com", "PREF", "two"),
            cookie(".google.com", "NID", "three"),
            cookie(".example.org", "OTHER", "four"),
        ];
        let selected = for_service(Service::YouTube, &cookies);
        assert_eq!(selected.len(), 3);
        assert!(selected.iter().all(|cookie| cookie.name != "OTHER"));
    }

    #[test]
    fn twitch_domains_are_scoped_to_twitch() {
        let cookies = vec![
            cookie(".twitch.tv", "auth-token", "token"),
            cookie("gql.twitch.tv", "other", "value"),
            cookie(".youtube.com", "SID", "youtube"),
        ];
        let selected = for_service(Service::Twitch, &cookies);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn domain_matching_never_uses_a_bare_suffix() {
        assert!(domain_belongs_to(Service::YouTube, ".youtube.com"));
        assert!(domain_belongs_to(Service::YouTube, "accounts.google.com"));
        assert!(!domain_belongs_to(Service::YouTube, "notyoutube.com"));
        assert!(!domain_belongs_to(Service::Twitch, "nottwitch.tv"));
    }

    #[test]
    fn a_cookie_on_the_base_domain_covers_its_subdomains() {
        assert_eq!(netscape_domain(Service::YouTube, "youtube.com"), ".youtube.com");
        assert_eq!(netscape_domain(Service::YouTube, ".youtube.com"), ".youtube.com");
        assert_eq!(netscape_domain(Service::Twitch, "twitch.tv"), ".twitch.tv");
        assert_eq!(netscape_domain(Service::YouTube, "google.com"), ".google.com");
    }

    #[test]
    fn a_cookie_on_a_single_host_stays_on_that_host() {
        assert_eq!(netscape_domain(Service::YouTube, "www.youtube.com"), "www.youtube.com");
        assert_eq!(netscape_domain(Service::Twitch, "clips.twitch.tv"), "clips.twitch.tv");
        assert_eq!(netscape_domain(Service::YouTube, "accounts.google.com"), "accounts.google.com");
    }

    #[test]
    fn the_netscape_domain_keeps_the_two_columns_of_a_line_in_step() {
        // The cookie jar of yt-dlp insists that the second column agrees
        // with the leading dot of the first one.
        let domain = netscape_domain(Service::YouTube, "youtube.com");
        let cookies = vec![Cookie { domain, ..cookie("youtube.com", "SID", "x") }];
        let line = netscape_text(&cookies);
        assert!(line.contains(".youtube.com\tTRUE\t"));
    }

    #[test]
    fn a_domain_without_the_dot_says_so_in_the_second_column() {
        let cookies = vec![cookie("www.youtube.com", "theme", "dark")];
        let line = netscape_text(&cookies);
        assert!(line.contains("www.youtube.com\tFALSE\t"));
    }

    #[test]
    fn signed_in_only_accepts_authentication_cookies() {
        assert!(signed_in(Service::YouTube, &[cookie(".youtube.com", "SAPISID", "x")]));
        assert!(signed_in(Service::Twitch, &[cookie(".twitch.tv", "auth-token", "x")]));
        assert!(!signed_in(Service::YouTube, &[cookie(".youtube.com", "PREF", "x")]));
        assert!(!signed_in(Service::Twitch, &[cookie(".twitch.tv", "theme", "dark")]));
    }

    #[test]
    fn a_google_session_alone_is_not_a_youtube_session() {
        // Signing in to Google does not sign in to YouTube.
        assert!(!signed_in(Service::YouTube, &[cookie(".google.com", "SAPISID", "x")]));
        assert!(signed_in(Service::YouTube, &[cookie(".youtube.com", "SAPISID", "x")]));
    }

    #[test]
    fn netscape_lines_have_the_expected_columns() {
        let cookies = vec![Cookie {
            domain: ".youtube.com".to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: true,
            expires: Some(1_800_000_000),
            name: "SID".to_string(),
            value: "value".to_string(),
        }];
        let text = netscape_text(&cookies);
        assert!(text.starts_with(HEADER));
        assert!(text.contains("#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1800000000\tSID\tvalue"));
    }

    #[test]
    fn session_cookies_get_an_empty_expiry_zero() {
        let cookies = vec![Cookie { expires: None, ..cookie(".twitch.tv", "auth-token", "token") }];
        let line = netscape_text(&cookies);
        // A domain that starts with a dot covers its subdomains, so the
        // second column says so too.
        assert!(line.contains(".twitch.tv\tTRUE\t/\tTRUE\t0\tauth-token\ttoken"));
    }

    #[test]
    fn a_host_only_cookie_does_not_cover_its_subdomains() {
        let cookies = vec![cookie("www.twitch.tv", "auth-token", "token")];
        let line = netscape_text(&cookies);
        assert!(line.contains("www.twitch.tv\tFALSE\t/\tTRUE\t"));
    }

    #[test]
    fn tabs_and_newlines_cannot_break_the_file() {
        let cookies = vec![cookie(".twitch.tv", "auth-token", "a\tb\nc")];
        let line = netscape_text(&cookies);
        assert!(line.contains("\tauth-token\tabc"));
        assert_eq!(line.lines().filter(|line| line.contains("auth-token")).count(), 1);
    }

    #[test]
    fn a_cookie_file_is_replaced_as_a_whole() {
        let directory =
            std::env::temp_dir().join(format!("flatline-cookie-write-{}", std::process::id()));
        let path = cookie_file(&directory, Service::YouTube);
        write_file(&path, &[cookie(".youtube.com", "SID", "one")])
            .expect("the cookie file can be written");
        write_file(&path, &[cookie(".youtube.com", "SID", "two")])
            .expect("the cookie file can be replaced");
        let text = std::fs::read_to_string(&path).expect("the cookie file can be read");
        assert!(text.contains("\tSID\ttwo"));
        assert!(!text.contains("\tSID\tone"));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_parent_directories_are_created() {
        let directory = std::env::temp_dir()
            .join(format!("flatline-cookie-nested-{}", std::process::id()))
            .join("cache")
            .join("cookies");
        let path = cookie_file(&directory, Service::Twitch);
        write_file(&path, &[cookie(".twitch.tv", "auth-token", "token")])
            .expect("the cookie file can be written");
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(directory.parent().and_then(|p| p.parent()).unwrap());
    }

    #[test]
    fn a_netscape_cookie_file_can_be_read_back() {
        let directory =
            std::env::temp_dir().join(format!("flatline-cookie-read-{}", std::process::id()));
        let path = cookie_file(&directory, Service::YouTube);
        write_file(&path, &[cookie(".youtube.com", "SAPISID", "one")])
            .expect("the cookie file can be written");
        let cookies = read_file(&path).expect("the cookie file can be read");
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "SAPISID");
        assert_eq!(cookies[0].value, "one");
        assert!(cookies[0].domain.starts_with('.'));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn signed_in_file_checks_authentication_cookies() {
        let directory =
            std::env::temp_dir().join(format!("flatline-cookie-signed-{}", std::process::id()));
        let path = cookie_file(&directory, Service::Twitch);
        write_file(&path, &[cookie(".twitch.tv", "theme", "dark")])
            .expect("the cookie file can be written");
        assert!(!signed_in_file(Service::Twitch, &path));
        write_file(&path, &[cookie(".twitch.tv", "auth-token", "token")])
            .expect("the cookie file can be replaced");
        assert!(signed_in_file(Service::Twitch, &path));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_missing_cookie_file_is_not_signed_in() {
        let directory =
            std::env::temp_dir().join(format!("flatline-cookie-missing-{}", std::process::id()));
        let path = cookie_file(&directory, Service::YouTube);
        assert!(!signed_in_file(Service::YouTube, &path));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn parse_netscape_ignores_comments_and_broken_lines() {
        let text = format!(
            "{HEADER}\n# comment\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tabc\nbroken\n"
        );
        let cookies = parse_netscape(&text);
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].http_only);
        assert_eq!(cookies[0].name, "SAPISID");
    }
}

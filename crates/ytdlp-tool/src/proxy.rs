//! Runtime proxy rotation for yt-dlp network requests.
//!
//! Flatline resolves media through `yt-dlp`, which makes the outbound
//! requests to the media service. Some services block direct access from
//! certain networks, so the resolver can route those requests through a
//! pool of public proxies. The actual media stream is never proxied: once
//! a stream URL has been resolved it is handed to the player directly.
//!
//! The pool is intentionally ephemeral. Proxies are public, short-lived and
//! unreliable, so the list is regenerated on demand and kept only in memory.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use media_model::Service;

/// How many proxies are kept in one generated pool.
const POOL_SIZE: usize = 10;
/// How many proxies are sampled from each source.
const PER_SOURCE: usize = 5;
/// Maximum number of requests served through one proxy before rotating.
const MAX_USES: u32 = 10;
/// Maximum lifetime of one proxy entry before it is discarded.
const MAX_AGE: Duration = Duration::from_secs(3 * 60 * 60);
/// Network timeout when downloading a proxy list.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// The Proxifly SOCKS5 list, refreshed every few minutes upstream.
const PROXIFLY_SOCKS5: &str =
    "https://cdn.jsdelivr.net/gh/proxifly/free-proxy-list@main/proxies/protocols/socks5/data.txt";
/// The ProxyScrape live list endpoint.
const PROXYSCRAPE: &str = "https://api.proxyscrape.com/v4/free-proxy-list/get?request=display_proxies&proxy_format=protocolipport&format=text";

/// A rotating pool of public proxies for yt-dlp requests.
///
/// The pool is cheap to clone: every clone shares the same inner state, so a
/// proxy that failed in one resolve is not handed to the next one.
#[derive(Debug)]
pub struct ProxyPool {
    inner: Mutex<PoolState>,
}

#[derive(Debug, Default)]
struct PoolState {
    entries: Vec<ProxyEntry>,
    /// Index of the proxy currently being consumed.
    current: usize,
}

#[derive(Debug, Clone)]
struct ProxyEntry {
    url: String,
    uses: u32,
    created: Instant,
}

/// The outcome of one proxied yt-dlp request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyOutcome {
    /// The request completed without a proxy-related rotation.
    Success,
    /// The request failed and the proxy should be rotated.
    Failure,
}

impl ProxyPool {
    /// Creates an empty pool. Proxies are fetched lazily on first use.
    pub fn new() -> Self {
        Self { inner: Mutex::new(PoolState::default()) }
    }

    /// Returns a proxy for `url`, if this request should be proxied.
    ///
    /// Only YouTube requests are proxied. Other services and non-service URLs
    /// resolve directly so the proxy layer does not interfere with unrelated
    /// traffic or with the signed-in session of another service.
    pub fn proxy_for(&self, url: &str) -> Option<String> {
        if Service::from_url(url) != Some(Service::YouTube) {
            return None;
        }
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_ready(&mut state);
        self.current_url(&mut state)
    }

    /// Records the outcome of a request that used `proxy`.
    ///
    /// A failure rotates to the next proxy immediately. A success counts as one
    /// use; reaching the use limit rotates on the next pick. Stale proxies are
    /// discarded eagerly so an old entry never lingers past its lifetime.
    pub fn report(&self, proxy: &str, outcome: ProxyOutcome) {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.prune_expired(&mut state);
        let Some(index) = state.entries.iter().position(|entry| entry.url == proxy) else {
            return;
        };
        match outcome {
            ProxyOutcome::Failure => {
                app_logging::warning(format!("proxy failed, rotating: proxy={proxy}"));
                state.entries.remove(index);
                if state.current >= state.entries.len() {
                    state.current = 0;
                }
            }
            ProxyOutcome::Success => {
                if let Some(entry) = state.entries.get_mut(index) {
                    entry.uses = entry.uses.saturating_add(1);
                    if entry.uses >= MAX_USES {
                        app_logging::notice(format!(
                            "proxy reached use limit, rotating: proxy={proxy} uses={}",
                            entry.uses
                        ));
                        state.entries.remove(index);
                        if state.current >= state.entries.len() {
                            state.current = 0;
                        }
                    }
                }
            }
        }
    }

    fn ensure_ready(&self, state: &mut PoolState) {
        self.prune_expired(state);
        self.advance_if_exhausted(state);
        if state.entries.is_empty() {
            self.regenerate(state);
        }
    }

    fn current_url(&self, state: &mut PoolState) -> Option<String> {
        self.prune_expired(state);
        self.advance_if_exhausted(state);
        if state.entries.is_empty() {
            self.regenerate(state);
        }
        state.entries.get(state.current).map(|entry| entry.url.clone())
    }

    fn advance_if_exhausted(&self, state: &mut PoolState) {
        let Some(entry) = state.entries.get(state.current) else {
            state.current = 0;
            return;
        };
        if entry.uses >= MAX_USES || entry.created.elapsed() >= MAX_AGE {
            state.entries.remove(state.current);
            if state.current >= state.entries.len() {
                state.current = 0;
            }
        }
    }

    fn prune_expired(&self, state: &mut PoolState) {
        let before = state.entries.len();
        state.entries.retain(|entry| entry.created.elapsed() < MAX_AGE);
        if state.entries.len() != before {
            app_logging::notice(format!(
                "expired proxies removed: removed={} remaining={}",
                before - state.entries.len(),
                state.entries.len()
            ));
            if state.current >= state.entries.len() {
                state.current = 0;
            }
        }
    }

    fn regenerate(&self, state: &mut PoolState) {
        app_logging::notice("regenerating yt-dlp proxy pool");
        let proxifly = fetch_list(PROXIFLY_SOCKS5, "proxifly");
        let proxyscrape = fetch_list(PROXYSCRAPE, "proxyscrape");
        let selected_proxifly = sample(&proxifly, PER_SOURCE);
        let selected_proxyscrape = sample(&proxyscrape, PER_SOURCE);
        let mut combined = Vec::with_capacity(POOL_SIZE);
        combined.extend(selected_proxifly);
        combined.extend(selected_proxyscrape);
        shuffle(&mut combined);
        combined.truncate(POOL_SIZE);
        let now = Instant::now();
        state.entries =
            combined.into_iter().map(|url| ProxyEntry { url, uses: 0, created: now }).collect();
        state.current = 0;
        app_logging::notice(format!(
            "yt-dlp proxy pool ready: count={} proxifly_candidates={} proxyscrape_candidates={}",
            state.entries.len(),
            proxifly.len(),
            proxyscrape.len()
        ));
        for (index, entry) in state.entries.iter().enumerate() {
            app_logging::debug(format!("proxy[{index}]={}", entry.url));
        }
    }
}

impl Default for ProxyPool {
    fn default() -> Self {
        Self::new()
    }
}

fn fetch_list(url: &str, name: &str) -> Vec<String> {
    let agent =
        ureq::Agent::config_builder().timeout_global(Some(LIST_TIMEOUT)).build().new_agent();
    let response = match agent.get(url).call() {
        Ok(response) => response,
        Err(error) => {
            app_logging::warning(format!(
                "proxy list download failed: source={name} error={error}"
            ));
            return Vec::new();
        }
    };
    let mut body = response.into_body();
    let text = match body.read_to_string() {
        Ok(body) => body,
        Err(error) => {
            app_logging::warning(format!("proxy list read failed: source={name} error={error}"));
            return Vec::new();
        }
    };
    let proxies = parse_proxy_list(&text);
    app_logging::notice(format!("proxy list loaded: source={name} count={}", proxies.len()));
    proxies
}

/// Parses proxy endpoints from a newline-delimited list.
///
/// Both sources produce plain text with one proxy per line. ProxyScrape's
/// `protocolipport` format yields `protocol://ip:port`; Proxifly's data files
/// may be bare `ip:port` or already prefixed. Empty lines and comments are
/// ignored, and duplicates are removed while preserving order.
fn parse_proxy_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let Some(proxy) = normalize_proxy(line) else {
            continue;
        };
        if !out.iter().any(|existing: &String| existing == &proxy) {
            out.push(proxy);
        }
    }
    out
}

fn normalize_proxy(line: &str) -> Option<String> {
    let candidate = line.trim();
    if candidate.is_empty() {
        return None;
    }
    let normalized = if candidate.contains("://") {
        candidate.to_string()
    } else {
        // Bare host:port entries are treated as HTTP proxies; protocol-specific
        // sources like Proxifly's socks5 list already include the scheme.
        format!("http://{candidate}")
    };
    let parsed = url::Url::parse(&normalized).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" | "socks4" | "socks5" => {}
        _ => return None,
    }
    let host = parsed.host_str()?.trim();
    if host.is_empty() || (!host.contains('.') && !host.contains(':')) {
        return None;
    }
    parsed.port()?;
    Some(parsed.to_string())
}

fn sample(items: &[String], count: usize) -> Vec<String> {
    if items.is_empty() || count == 0 {
        return Vec::new();
    }
    let mut indices: Vec<usize> = (0..items.len()).collect();
    shuffle_usize(&mut indices);
    indices.truncate(count.min(indices.len()));
    indices.into_iter().map(|index| items[index].clone()).collect()
}

fn shuffle(items: &mut [String]) {
    if items.len() < 2 {
        return;
    }
    for i in (1..items.len()).rev() {
        let j = fastrand::usize(..=i);
        items.swap(i, j);
    }
}

fn shuffle_usize(items: &mut [usize]) {
    if items.len() < 2 {
        return;
    }
    for i in (1..items.len()).rev() {
        let j = fastrand::usize(..=i);
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_protocol_prefixed_and_bare_proxies() {
        let input = "socks5://1.2.3.4:1080\nhttp://5.6.7.8:8080\n9.9.9.9:3128\n\n# comment\n";
        let parsed = parse_proxy_list(input);
        assert_eq!(
            parsed,
            vec![
                "socks5://1.2.3.4:1080".to_string(),
                "http://5.6.7.8:8080/".to_string(),
                "http://9.9.9.9:3128/".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_invalid_and_duplicate_proxies() {
        let input = "socks5://1.2.3.4:1080\nsocks5://1.2.3.4:1080\nftp://1.2.3.4:21\nbad\n";
        let parsed = parse_proxy_list(input);
        assert_eq!(parsed, vec!["socks5://1.2.3.4:1080".to_string()]);
    }

    #[test]
    fn sample_is_bounded() {
        let items: Vec<String> = (0..20).map(|i| format!("http://127.0.0.1:{i}")).collect();
        let picked = sample(&items, 5);
        assert_eq!(picked.len(), 5);
        for item in picked {
            assert!(items.contains(&item));
        }
    }
}

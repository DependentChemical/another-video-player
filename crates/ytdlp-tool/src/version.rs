//! Comparing `yt-dlp` version strings.
//!
//! `yt-dlp` uses calendar versions (`2025.10.13`) with optional suffixes
//! (`2025.10.13.post1`, `2025.11.21.dev20251201-abcdef`). Only the leading
//! numeric components are compared; development builds always lose against
//! the matching release.

use std::cmp::Ordering;

/// A parsed `yt-dlp` version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Numeric components, most significant first.
    pub parts: Vec<u64>,
    /// Whether this is a development build.
    pub prerelease: bool,
}

impl Version {
    /// Parses a version string, ignoring anything it does not understand.
    pub fn parse(raw: &str) -> Version {
        let raw = raw.trim();
        let prerelease = raw.contains(".dev") || raw.contains("-dev");
        let mut parts = Vec::new();

        for component in raw.split('.') {
            let digits: String = component.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                match digits.parse::<u64>() {
                    Ok(value) => parts.push(value),
                    Err(_) => break,
                }
                continue;
            }
            // A `post` release follows the plain release of the same day.
            if let Some(rest) = component.strip_prefix("post") {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(value) = digits.parse::<u64>() {
                    parts.push(value);
                    continue;
                }
            }
            break;
        }

        Version { parts, prerelease }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // A development build is older than the release of the same day.
        match (self.prerelease, other.prerelease) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            _ => self.parts.cmp(&other.parts),
        }
    }
}

/// Whether `candidate` is newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    Version::parse(candidate) > Version::parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_calendar_versions_compare_numerically() {
        assert!(is_newer("2025.10.13", "2025.10.12"));
        assert!(!is_newer("2025.10.12", "2025.10.13"));
        assert!(is_newer("2026.01.01", "2025.12.31"));
    }

    #[test]
    fn post_releases_beat_the_release() {
        assert!(is_newer("2025.10.13.post1", "2025.10.13"));
        assert!(!is_newer("2025.10.13", "2025.10.13.post1"));
    }

    #[test]
    fn development_builds_lose_against_releases() {
        assert!(is_newer("2025.10.13", "2025.10.13.dev20251020-abcdef"));
        assert!(!is_newer("2025.10.13.dev20251020-abcdef", "2025.10.13"));
    }

    #[test]
    fn unparsable_versions_compare_equal() {
        assert!(!is_newer("nightly", "nightly"));
    }
}

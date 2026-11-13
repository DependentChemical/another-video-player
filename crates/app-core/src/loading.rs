//! The state behind the loading indicator.
//!
//! The indicator is shown from the moment a URL is requested until the
//! player really plays, and it comes back whenever playback has to wait
//! for data. This module owns those transitions so they can be tested
//! without a window or a player.

/// What the interface should show above the video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The dots run next to this text.
    Busy(&'static str),
    /// A neutral line of text, without the dots.
    Info(String),
    /// Something went wrong, without the dots.
    Error(String),
}

/// The text shown while a URL is being resolved.
const RESOLVING: &str = "Resolving video...";
/// The text shown while the player is waiting for data.
const BUFFERING: &str = "Buffering...";

/// Where the loading indicator is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing is happening.
    Idle,
    /// The URL is on its way to the resolver.
    Resolving,
    /// The player has a plan and has not played it yet.
    Starting,
    /// Playback runs but has to wait for data.
    Buffering,
    /// The video plays.
    Playing,
}

/// The loading indicator: a phase machine that produces the status text.
#[derive(Debug)]
pub struct Loading {
    phase: Phase,
    status: Status,
    // Set when mpv starts the file that belongs to the current load, so a
    // file that belonged to an earlier load never disturbs the indicator.
    current_file: bool,
    /// Prevents a persistent mpv EOF state from advancing on every UI tick.
    eof_consumed: bool,
}

impl Default for Loading {
    fn default() -> Self {
        Loading {
            phase: Phase::Idle,
            status: Status::Info(String::new()),
            current_file: false,
            eof_consumed: false,
        }
    }
}

impl Loading {
    /// Creates an indicator that is not waiting for anything.
    pub fn new() -> Self {
        Self::default()
    }

    /// The text the interface has to show.
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Whether the dots are running.
    pub fn is_busy(&self) -> bool {
        matches!(self.status, Status::Busy(_))
    }

    /// A URL is on its way to the resolver.
    pub fn resolve_started(&mut self) {
        self.phase = Phase::Resolving;
        self.status = Status::Busy(RESOLVING);
        self.current_file = false;
    }

    /// The URL resolved and the player was given a plan to play.
    pub fn resolved(&mut self) {
        self.phase = Phase::Starting;
        self.status = Status::Busy(BUFFERING);
        self.current_file = false;
    }

    /// Resolving the URL failed.
    pub fn resolve_failed(&mut self, error: String) {
        self.phase = Phase::Idle;
        self.status = Status::Error(error);
        self.current_file = false;
    }

    /// The player was given another selection to play.
    pub fn sources_switching(&mut self) {
        self.phase = Phase::Starting;
        self.status = Status::Busy(BUFFERING);
        self.current_file = false;
    }

    /// Something went wrong that the user should know about.
    pub fn problem(&mut self, text: String) {
        self.phase = Phase::Idle;
        self.status = Status::Error(text);
        self.current_file = false;
    }

    /// Notes that a newer `yt-dlp` is now in use.
    ///
    /// The message is ignored while the dots are running, so it can never
    /// make the indicator disappear; the caller can log it instead.
    /// Returns whether the message was shown.
    pub fn tool_updated(&mut self, path: &str) -> bool {
        if self.is_busy() {
            return false;
        }
        self.status = Status::Info(format!("yt-dlp was updated ({path})"));
        true
    }

    /// mpv began loading the file that belongs to the current load.
    pub fn file_started(&mut self) {
        self.current_file = true;
        self.eof_consumed = false;
    }

    /// Claims an observed EOF once, until the next file starts.
    pub fn claim_eof(&mut self) -> bool {
        if self.eof_consumed {
            return false;
        }
        self.eof_consumed = true;
        true
    }

    /// Playback of a file ended.
    ///
    /// An error is shown to the user. A file that belonged to an earlier
    /// load is ignored, so replacing a file never disturbs the indicator.
    pub fn file_ended(&mut self, failure: Option<String>) {
        if !self.current_file || self.phase == Phase::Resolving {
            return;
        }
        self.current_file = false;
        match failure {
            Some(message) => {
                self.phase = Phase::Idle;
                self.status = Status::Error(message);
            }
            None => {
                if matches!(self.phase, Phase::Starting | Phase::Buffering) {
                    self.status = Status::Info(String::new());
                }
                self.phase = Phase::Idle;
            }
        }
    }

    /// Brings the indicator up to date with the player.
    ///
    /// `active` means the player is decoding and drawing frames; `buffering`
    /// means it has to wait for data. The indicator disappears only when the
    /// player is active for the current load, and comes back whenever it
    /// buffers.
    pub fn observe(&mut self, active: bool, buffering: bool) {
        match self.phase {
            Phase::Starting if self.current_file && active && !buffering => {
                self.phase = Phase::Playing;
                self.status = Status::Info(String::new());
            }
            Phase::Playing if buffering => {
                self.phase = Phase::Buffering;
                self.status = Status::Busy(BUFFERING);
            }
            Phase::Buffering if !buffering => {
                self.phase = Phase::Playing;
                self.status = Status::Info(String::new());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BUFFERING, Loading, RESOLVING, Status};

    #[test]
    fn the_indicator_waits_for_the_resolver() {
        let mut loading = Loading::new();
        loading.resolve_started();

        assert_eq!(loading.status(), &Status::Busy(RESOLVING));
        assert!(loading.is_busy());
    }

    #[test]
    fn the_indicator_stays_busy_until_the_player_really_plays() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolved();
        loading.file_started();

        loading.observe(false, false);
        assert_eq!(loading.status(), &Status::Busy(BUFFERING));

        loading.observe(true, false);
        assert_eq!(loading.status(), &Status::Info(String::new()));
        assert!(!loading.is_busy());
    }

    #[test]
    fn the_indicator_waits_for_the_file_to_start() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolved();

        loading.observe(true, false);
        assert_eq!(loading.status(), &Status::Busy(BUFFERING));
    }

    #[test]
    fn the_indicator_returns_when_playback_buffers() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolved();
        loading.file_started();
        loading.observe(true, false);

        loading.observe(true, true);
        assert_eq!(loading.status(), &Status::Busy(BUFFERING));

        loading.observe(false, false);
        assert_eq!(loading.status(), &Status::Info(String::new()));
    }

    #[test]
    fn buffering_is_ignored_before_the_first_frame() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolved();
        loading.file_started();

        loading.observe(true, true);
        assert_eq!(loading.status(), &Status::Busy(BUFFERING));
    }

    #[test]
    fn errors_stop_the_indicator() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolve_failed("nope".to_string());

        assert_eq!(loading.status(), &Status::Error("nope".to_string()));
        assert!(!loading.is_busy());
    }

    #[test]
    fn a_file_end_never_interrupts_a_resolution() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.file_started();

        loading.file_ended(None);
        assert_eq!(loading.status(), &Status::Busy(RESOLVING));

        loading.file_ended(Some("boom".to_string()));
        assert_eq!(loading.status(), &Status::Busy(RESOLVING));
    }

    #[test]
    fn the_end_of_an_earlier_file_is_ignored() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolved();

        loading.file_ended(None);
        assert_eq!(loading.status(), &Status::Busy(BUFFERING));

        loading.file_started();
        loading.file_ended(None);
        assert_eq!(loading.status(), &Status::Info(String::new()));
    }

    #[test]
    fn a_failing_file_shows_the_error() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolved();
        loading.file_started();

        loading.file_ended(Some("boom".to_string()));
        assert_eq!(loading.status(), &Status::Error("boom".to_string()));
    }

    #[test]
    fn an_eof_is_claimed_once_per_file() {
        let mut loading = Loading::new();
        loading.file_started();

        assert!(loading.claim_eof());
        assert!(!loading.claim_eof());

        loading.file_started();
        assert!(loading.claim_eof());
    }

    #[test]
    fn tool_updates_wait_for_a_free_moment() {
        let mut loading = Loading::new();
        loading.resolve_started();

        assert!(!loading.tool_updated("/tmp/yt-dlp"));
        assert_eq!(loading.status(), &Status::Busy(RESOLVING));

        loading.resolve_failed("nope".to_string());
        assert!(loading.tool_updated("/tmp/yt-dlp"));
        assert_eq!(loading.status(), &Status::Info("yt-dlp was updated (/tmp/yt-dlp)".to_string()));
    }

    #[test]
    fn switching_sources_waits_for_the_player() {
        let mut loading = Loading::new();
        loading.resolve_started();
        loading.resolved();
        loading.file_started();
        loading.observe(true, false);

        loading.sources_switching();
        assert_eq!(loading.status(), &Status::Busy(BUFFERING));

        loading.file_started();
        loading.observe(true, false);
        assert_eq!(loading.status(), &Status::Info(String::new()));
    }
}

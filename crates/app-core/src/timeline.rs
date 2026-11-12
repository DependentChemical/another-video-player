//! The time axis the interface shows.
//!
//! A recorded media is shown as it is: the axis starts at zero and ends with
//! the duration of the media.
//!
//! A live stream has no end, so the axis shows the last [`LIVE_WINDOW`]
//! seconds of the stream. The live edge sits at the right hand end of the
//! axis and the part that has already been played is filled from the left.
//! When the user scrubs back, the window stops following the edge so that the
//! handle does not drift away under the finger.

use std::time::Instant;

/// How far back a live stream can be scrubbed, in seconds.
pub const LIVE_WINDOW: f64 = 300.0;

/// The time axis that is shown to the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeline {
    /// Whether the media is a live stream.
    pub live: bool,
    /// The position on the axis, in seconds.
    pub position: f64,
    /// The length of the axis, in seconds.
    pub duration: f64,
    /// How far behind the live edge the playback is, in seconds.
    pub behind: f64,
}

impl Timeline {
    /// The axis of a recorded media.
    pub fn recorded(position: f64, duration: f64) -> Timeline {
        Timeline { live: false, position, duration, behind: 0.0 }
    }
}

/// The axis of a live stream.
///
/// The struct remembers where the live edge is and where the shown window
/// starts, so that both can be mapped to and from the time axis.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LiveAxis {
    /// The furthest the stream has come.
    edge: f64,
    /// The stream time the left hand end of the axis shows.
    start: f64,
    /// When the edge was last observed from playback or the demuxer cache.
    edge_observed_at: Option<Instant>,
}

impl LiveAxis {
    /// An axis that has not seen any playback yet.
    pub fn new() -> LiveAxis {
        LiveAxis::default()
    }

    /// The furthest the stream has come, in seconds.
    pub fn edge(&self) -> f64 {
        self.edge
    }

    /// Notes the position the player is at.
    ///
    /// The live edge only ever moves forward, so a seek back does not move it.
    pub fn observe(&mut self, position: f64) {
        if position > self.edge {
            self.edge = position;
            self.edge_observed_at = Some(Instant::now());
        } else if self.edge_observed_at.is_none() {
            self.edge_observed_at = Some(Instant::now());
        }
    }

    /// Lets the live edge advance in real time while playback is paused.
    pub fn advance_while_paused(&mut self, now: Instant) {
        if let Some(observed_at) = self.edge_observed_at {
            let elapsed = now.saturating_duration_since(observed_at).as_secs_f64();
            self.edge += elapsed;
            self.edge_observed_at = Some(now);
        }
    }

    /// Notes the playback position and keeps the window in step with the edge.
    ///
    /// While the playback is at the live edge the window follows it, so the
    /// handle stays at the right hand end. As soon as the user is behind the
    /// edge the window is left alone, so the handle does not drift away.
    pub fn sync(&mut self, position: f64) {
        self.observe(position);
        if self.at_edge(position) {
            self.anchor();
        }
    }

    /// Whether the playback is at the live edge.
    ///
    /// A small tolerance keeps the player from flickering between "live" and
    /// "behind" because of the rounding of the reported position.
    pub fn at_edge(&self, position: f64) -> bool {
        self.edge - position <= 1.0
    }

    /// Moves the window back to the live edge.
    ///
    /// This is what the live button does, and what happens when the user
    /// scrubs: the window is anchored at the edge that is current right now.
    pub fn anchor(&mut self) {
        self.start = (self.edge - LIVE_WINDOW).max(0.0);
    }

    /// Forgets everything, so the next media starts from scratch.
    pub fn reset(&mut self) {
        self.edge = 0.0;
        self.start = 0.0;
    }

    /// The axis that is shown for the given playback position.
    pub fn timeline(&self, position: f64) -> Timeline {
        Timeline {
            live: true,
            position: (position - self.start).clamp(0.0, LIVE_WINDOW),
            duration: LIVE_WINDOW,
            behind: (self.edge - position).max(0.0),
        }
    }

    /// The stream time an axis position stands for.
    ///
    /// The result never leaves the window that is shown, so dragging the
    /// handle past either end cannot seek outside it.
    pub fn to_stream(&self, axis_position: f64) -> f64 {
        (self.start + axis_position).clamp(self.start, self.edge.max(self.start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_media_shows_its_own_time() {
        let timeline = Timeline::recorded(12.0, 300.0);
        assert!(!timeline.live);
        assert_eq!(timeline.position, 12.0);
        assert_eq!(timeline.duration, 300.0);
        assert_eq!(timeline.behind, 0.0);
    }

    #[test]
    fn the_live_edge_only_moves_forward() {
        let mut axis = LiveAxis::new();
        axis.observe(100.0);
        axis.observe(40.0);
        assert_eq!(axis.edge(), 100.0);
    }

    #[test]
    fn a_live_stream_is_at_the_edge_while_it_plays() {
        let mut axis = LiveAxis::new();
        axis.observe(50.0);
        assert!(axis.at_edge(50.0));
        assert!(!axis.at_edge(45.0));
    }

    #[test]
    fn the_axis_of_a_live_stream_is_the_window() {
        let mut axis = LiveAxis::new();
        axis.observe(1000.0);
        axis.anchor();
        let timeline = axis.timeline(1000.0);
        assert!(timeline.live);
        assert_eq!(timeline.duration, LIVE_WINDOW);
        assert_eq!(timeline.position, LIVE_WINDOW);
        assert_eq!(timeline.behind, 0.0);
    }

    #[test]
    fn a_stream_that_just_started_still_has_a_full_window() {
        let mut axis = LiveAxis::new();
        axis.observe(10.0);
        axis.anchor();
        let timeline = axis.timeline(10.0);
        assert_eq!(timeline.position, 10.0);
        assert_eq!(timeline.duration, LIVE_WINDOW);
    }

    #[test]
    fn scrubbing_back_shows_the_distance_to_the_live_edge() {
        let mut axis = LiveAxis::new();
        axis.observe(1000.0);
        axis.anchor();
        let timeline = axis.timeline(940.0);
        assert_eq!(timeline.behind, 60.0);
        assert_eq!(timeline.position, LIVE_WINDOW - 60.0);
    }

    #[test]
    fn the_window_stays_put_while_the_user_watches_a_replay() {
        let mut axis = LiveAxis::new();
        axis.sync(1000.0);
        let before = axis.timeline(940.0).position;
        // The playback keeps reporting the replay position, not the edge, so
        // the window is not anchored again and the handle does not drift.
        axis.sync(940.0);
        assert_eq!(axis.timeline(940.0).position, before);
    }

    #[test]
    fn the_live_edge_can_advance_while_playback_is_paused() {
        let mut axis = LiveAxis::new();
        axis.sync(1000.0);
        axis.observe(1040.0);

        let timeline = axis.timeline(1000.0);
        assert_eq!(timeline.behind, 40.0);
        assert!(!axis.at_edge(1000.0));
    }

    #[test]
    fn the_live_edge_advances_with_wall_time_when_paused() {
        let mut axis = LiveAxis::new();
        axis.observe(100.0);
        let observed_at = axis.edge_observed_at.expect("observing the edge records its time");
        axis.advance_while_paused(observed_at + std::time::Duration::from_secs(8));

        assert_eq!(axis.edge(), 108.0);
        assert_eq!(axis.timeline(100.0).behind, 8.0);
    }

    #[test]
    fn the_window_follows_the_edge_again_at_the_live_edge() {
        let mut axis = LiveAxis::new();
        axis.sync(1000.0);
        axis.sync(1010.0);
        // At the edge the window is anchored again.
        let timeline = axis.timeline(1010.0);
        assert_eq!(timeline.position, LIVE_WINDOW);
        assert_eq!(timeline.behind, 0.0);
    }

    #[test]
    fn an_axis_position_maps_back_to_a_stream_time() {
        let mut axis = LiveAxis::new();
        axis.observe(1000.0);
        axis.anchor();
        let stream = axis.to_stream(LIVE_WINDOW - 60.0);
        assert_eq!(stream, 940.0);
    }

    #[test]
    fn a_stream_time_never_leaves_the_window() {
        let mut axis = LiveAxis::new();
        axis.observe(1000.0);
        axis.anchor();
        assert_eq!(axis.to_stream(-10.0), 700.0);
        assert_eq!(axis.to_stream(LIVE_WINDOW + 100.0), 1000.0);
    }
}

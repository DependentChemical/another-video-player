//! Formatting helpers for the user interface.

use crate::timeline::{LIVE_BUFFER, Timeline};

/// Formats a number of seconds as `m:ss`, or `h:mm:ss` for long media.
pub fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0:00".to_string();
    }
    let total = seconds.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let rest = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{rest:02}")
    } else {
        format!("{minutes}:{rest:02}")
    }
}

/// Formats the time axis for the user.
///
/// A recorded media shows the position and the duration. A live stream shows
/// `LIVE` while the playback is at the edge, and how far behind the edge it
/// is otherwise.
pub fn format_timeline(timeline: &Timeline) -> String {
    if !timeline.live {
        return format!("{} / {}", format_time(timeline.position), format_time(timeline.duration));
    }
    if timeline.behind <= LIVE_BUFFER {
        return "LIVE".to_string();
    }
    format!("-{}", format_time(timeline.behind))
}

#[cfg(test)]
mod tests {
    use super::{format_time, format_timeline};
    use crate::timeline::Timeline;

    #[test]
    fn short_media_uses_minutes_and_seconds() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(9.4), "0:09");
        assert_eq!(format_time(65.0), "1:05");
    }

    #[test]
    fn long_media_uses_hours() {
        assert_eq!(format_time(3725.0), "1:02:05");
    }

    #[test]
    fn nonsense_becomes_zero() {
        assert_eq!(format_time(-5.0), "0:00");
        assert_eq!(format_time(f64::NAN), "0:00");
    }

    #[test]
    fn a_recorded_media_shows_position_and_duration() {
        let timeline = Timeline::recorded(65.0, 300.0);
        assert_eq!(format_timeline(&timeline), "1:05 / 5:00");
    }

    #[test]
    fn a_live_stream_at_the_edge_says_live() {
        let timeline = Timeline { live: true, position: 300.0, duration: 300.0, behind: 0.0 };
        assert_eq!(format_timeline(&timeline), "LIVE");
    }

    #[test]
    fn a_live_stream_behind_the_edge_shows_the_distance() {
        let timeline = Timeline { live: true, position: 240.0, duration: 300.0, behind: 60.0 };
        assert_eq!(format_timeline(&timeline), "-1:00");
    }
}

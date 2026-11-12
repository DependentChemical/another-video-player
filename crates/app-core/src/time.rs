//! Formatting helpers for the user interface.

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

#[cfg(test)]
mod tests {
    use super::format_time;

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
}

/// "M:SS" under an hour, "H:MM:SS" at or past one hour (a long DJ mix/podcast-length file shouldn't read as e.g.
/// "83:07"). Rounds to the nearest second (halves up, like JavaScript's `Math.round`).
pub fn format_duration(total_seconds: f64) -> String {
    let rounded = (total_seconds + 0.5).floor();
    let hours = (rounded / 3600.0).floor() as i64;
    let minutes = ((rounded % 3600.0) / 60.0).floor() as i64;
    let seconds = (rounded % 60.0) as i64;
    let seconds_str = format!("{seconds:0>2}");
    if hours > 0 {
        return format!("{hours}:{minutes:0>2}:{seconds_str}");
    }
    format!("{minutes}:{seconds_str}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_under_a_minute() {
        assert_eq!(format_duration(5.0), "0:05");
        assert_eq!(format_duration(0.0), "0:00");
    }

    #[test]
    fn formats_minutes_seconds_under_an_hour() {
        assert_eq!(format_duration(65.0), "1:05");
        assert_eq!(format_duration(599.0), "9:59");
        assert_eq!(format_duration(3599.0), "59:59");
    }

    #[test]
    fn formats_hours_minutes_seconds_at_or_past_one_hour() {
        assert_eq!(format_duration(3600.0), "1:00:00");
        assert_eq!(format_duration(3665.0), "1:01:05");
        assert_eq!(format_duration(7325.0), "2:02:05");
    }

    #[test]
    fn rounds_to_the_nearest_second() {
        assert_eq!(format_duration(59.6), "1:00");
        assert_eq!(format_duration(59.4), "0:59");
        assert_eq!(format_duration(0.5), "0:01"); // Math.round(0.5) is 1
        assert_eq!(format_duration(2.5), "0:03");
    }

    #[test]
    fn a_negative_duration_keeps_the_js_signs() {
        // JS: Math.round(-5) = -5; floor(-5/3600) = -1 (hours = -1 is not > 0); (-5 % 3600) / 60 floors to -1;
        // -5 % 60 = -5, and "-5".padStart(2, "0") stays "-5".
        assert_eq!(format_duration(-5.0), "-1:-5");
    }
}

//! Time formatting and parsing.
//!
//! Deliberately hand-rolled rather than pulling in a date-time crate: the only
//! things `eye` needs are a UTC calendar conversion and a couple of
//! human-readable durations, and both are small enough to test exhaustively.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, as a float.
pub fn now_unix() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(_) => 0.0,
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_unix_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => 0,
    }
}

/// Format a Unix timestamp as RFC 3339 in UTC, e.g. `2026-09-29T01:12:33Z`.
pub fn format_rfc3339(unix: f64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `(year, month, day, hour, minute, second)` for a Unix timestamp.
pub fn civil(unix: f64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix as i64).div_euclid(86_400);
    let rem = (unix as i64).rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

/// `YYYY-MM-DD` for a Unix timestamp, used for default session filenames.
pub fn format_date(unix: f64) -> String {
    let (y, mo, d, ..) = civil(unix);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Format a session offset as `MM:SS.mmm`, the timeline column in `eye inspect`.
pub fn format_offset(seconds: f64) -> String {
    let negative = seconds < 0.0;
    let s = seconds.abs();
    let total_ms = (s * 1000.0).round() as u64;
    let (h, m, sec, ms) = (
        total_ms / 3_600_000,
        (total_ms / 60_000) % 60,
        (total_ms / 1000) % 60,
        total_ms % 1000,
    );
    let sign = if negative { "-" } else { "" };
    if h > 0 {
        format!("{sign}{h}:{m:02}:{sec:02}.{ms:03}")
    } else {
        format!("{sign}{m:02}:{sec:02}.{ms:03}")
    }
}

/// Format a duration for human eyes: `3.4s`, `03:42`, `1:02:03`.
pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "-".into();
    }
    let total = seconds.round() as u64;
    if total < 60 {
        format!("{seconds:.1}s")
    } else if total < 3600 {
        format!("{}:{:02}", total / 60, total % 60)
    } else {
        format!("{}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
    }
}

/// Parse a timestamp like `90`, `1:30`, `01:02:03` into seconds.
///
/// Accepts the forms a user would plausibly type for `eye replay --start`.
pub fn parse_timestamp(input: &str) -> Option<f64> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for part in input.split(':') {
        let value: f64 = part.parse().ok()?;
        if value < 0.0 {
            return None;
        }
        total = total * 60.0 + value;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(format_rfc3339(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_000_000_000.0), "2001-09-09T01:46:40Z");
        assert_eq!(format_rfc3339(1_790_640_000.0), "2026-09-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_790_640_001.0), "2026-09-29T00:00:01Z");
    }

    #[test]
    fn rfc3339_handles_leap_days_and_century_rules() {
        assert_eq!(format_rfc3339(1_709_164_800.0), "2024-02-29T00:00:00Z");
        // 2023-02-28T08:00:00Z + 16h == 2023-03-01T00:00:00Z
        assert_eq!(format_rfc3339(1_677_571_200.0), "2023-02-28T08:00:00Z");
        assert_eq!(format_rfc3339(1_677_628_800.0), "2023-03-01T00:00:00Z");
        // 1900 was not a leap year; 2000 was.
        assert_eq!(format_rfc3339(-2_203_891_200.0), "1900-03-01T00:00:00Z");
        assert_eq!(format_rfc3339(951_782_400.0), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn offsets_are_zero_padded_milliseconds() {
        assert_eq!(format_offset(0.0), "00:00.000");
        assert_eq!(format_offset(1.231), "00:01.231");
        assert_eq!(format_offset(61.5), "01:01.500");
        assert_eq!(format_offset(3723.004), "1:02:03.004");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(3.42), "3.4s");
        assert_eq!(format_duration(222.0), "3:42");
        assert_eq!(format_duration(3723.0), "1:02:03");
        assert_eq!(format_duration(f64::NAN), "-");
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(parse_timestamp("90"), Some(90.0));
        assert_eq!(parse_timestamp("1:32"), Some(92.0));
        assert_eq!(parse_timestamp("01:02:03"), Some(3723.0));
        assert_eq!(parse_timestamp("1:00:30.5"), Some(3630.5));
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("abc"), None);
        assert_eq!(parse_timestamp("1:-2"), None);
    }

    #[test]
    fn dates_are_iso() {
        assert_eq!(format_date(1_790_640_000.0), "2026-09-29");
    }
}

//! Human-friendly relative time formatting shared by the TUI and GUI front-ends.
use chrono::{DateTime, Local, Utc};

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Approximate month/year: buckets only need rough granularity for labels.
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// Humanize an elapsed-seconds count into a compact label: now / 5m / 3h /
/// 2d / 3mo / 1y. Negative input (clock skew / future timestamps) → "now".
pub fn humanize_secs(secs: i64) -> String {
    if secs < MINUTE {
        "now".to_string()
    } else if secs < HOUR {
        format!("{}m", secs / MINUTE)
    } else if secs < DAY {
        format!("{}h", secs / HOUR)
    } else if secs < MONTH {
        format!("{}d", secs / DAY)
    } else if secs < YEAR {
        format!("{}mo", secs / MONTH)
    } else {
        format!("{}y", secs / YEAR)
    }
}

/// Parse a RFC3339 UTC timestamp and render it relative to `now` (e.g. "3h").
/// On parse failure, returns the input unchanged.
///
/// Takes `now` explicitly so callers rendering many rows can sample the clock
/// once per frame instead of once per row.
pub fn format_relative_time_since(rfc3339: &str, now: DateTime<Utc>) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => {
            let secs = now
                .signed_duration_since(dt.with_timezone(&Utc))
                .num_seconds();
            humanize_secs(secs)
        }
        Err(_) => rfc3339.to_string(),
    }
}

/// Convenience wrapper around [`format_relative_time_since`] that samples the
/// current time. Prefer the `_since` variant in per-row render loops.
pub fn format_relative_time(rfc3339: &str) -> String {
    format_relative_time_since(rfc3339, Utc::now())
}

/// Parse a RFC3339 timestamp and render it in the local timezone with seconds
/// and offset (e.g. "2026-06-24 15:30:45 +09:00"). On parse failure, returns
/// the input unchanged.
pub fn format_local_datetime(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => dt
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // Bucket boundaries: each pair is (seconds, expected label), with the
    // low/high edges of every bucket listed adjacently.
    #[rstest]
    #[case::now_zero(0, "now")]
    #[case::now_upper(59, "now")]
    #[case::minute_lower(60, "1m")]
    #[case::minute_upper(3599, "59m")]
    #[case::hour_lower(3600, "1h")]
    #[case::hour_upper(86_399, "23h")]
    #[case::day_lower(86_400, "1d")]
    #[case::day_upper(2_591_999, "29d")]
    #[case::month_lower(2_592_000, "1mo")]
    // 30-day months mean the last sub-year bucket reads "12mo".
    #[case::month_upper(31_535_999, "12mo")]
    #[case::year_lower(31_536_000, "1y")]
    // Clock skew / future timestamps clamp to "now".
    #[case::negative_clamps_to_now(-100, "now")]
    fn humanize_secs_buckets(#[case] secs: i64, #[case] expected: &str) {
        assert_eq!(humanize_secs(secs), expected);
    }

    #[test]
    fn format_relative_time_since_parses_and_buckets() {
        let now = DateTime::parse_from_rfc3339("2026-06-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            format_relative_time_since("2026-06-19T09:00:00Z", now),
            "3h"
        );
        // Unparseable input passes through unchanged.
        assert_eq!(format_relative_time_since("not-a-date", now), "not-a-date");
    }

    #[test]
    fn format_local_datetime_renders_in_local_zone() {
        // The local timezone is environment-dependent, so instead of asserting
        // an exact string we round-trip: the output must be parseable as the
        // declared `%Y-%m-%d %H:%M:%S %:z` format and denote the same instant
        // as the input. This verifies both the format and that the local-zone
        // conversion preserves the instant, regardless of the test machine's TZ.
        let input = "2026-06-19T09:00:00Z";
        let formatted = format_local_datetime(input);
        let reparsed = DateTime::parse_from_str(&formatted, "%Y-%m-%d %H:%M:%S %:z")
            .expect("output should match the declared format");
        assert_eq!(
            reparsed.with_timezone(&Utc),
            DateTime::parse_from_rfc3339(input)
                .unwrap()
                .with_timezone(&Utc),
        );

        // Unparseable input passes through unchanged.
        assert_eq!(format_local_datetime("not-a-date"), "not-a-date");
    }
}

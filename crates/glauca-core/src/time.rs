//! Human-friendly relative time formatting shared by the TUI and GUI front-ends.
use chrono::{DateTime, Utc};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_secs_buckets() {
        assert_eq!(humanize_secs(0), "now");
        assert_eq!(humanize_secs(59), "now");
        assert_eq!(humanize_secs(60), "1m");
        assert_eq!(humanize_secs(3599), "59m");
        assert_eq!(humanize_secs(3600), "1h");
        assert_eq!(humanize_secs(86_399), "23h");
        assert_eq!(humanize_secs(86_400), "1d");
        assert_eq!(humanize_secs(2_591_999), "29d");
        assert_eq!(humanize_secs(2_592_000), "1mo");
        // 30-day months mean the last sub-year bucket reads "12mo".
        assert_eq!(humanize_secs(31_535_999), "12mo");
        assert_eq!(humanize_secs(31_536_000), "1y");
        // Clock skew / future timestamps clamp to "now".
        assert_eq!(humanize_secs(-100), "now");
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
}

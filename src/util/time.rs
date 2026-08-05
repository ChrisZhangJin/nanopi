//! Timestamp helpers — chrono wrapper.
//!
//! All nanopi timestamps are UTC, RFC 3339 / ISO 8601 second-precision.
//! Format: `2026-08-05T04:52:02Z`.

use chrono::{DateTime, SecondsFormat, Utc};

/// Current UTC time.
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// Format a DateTime as `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339, second precision, Z).
pub fn to_iso8601(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Convenience: now_utc() formatted as ISO 8601.
pub fn now_iso8601() -> String {
    to_iso8601(&now_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_format_has_z_suffix() {
        let now = now_utc();
        let s = to_iso8601(&now);
        assert!(s.ends_with('Z'), "expected Z suffix, got: {s}");
    }

    #[test]
    fn iso8601_format_has_correct_shape() {
        let now = now_utc();
        let s = to_iso8601(&now);
        // YYYY-MM-DDTHH:MM:SSZ = 4+1+2+1+2 + T + 2+1+2+1+2 + Z = 20 chars
        assert_eq!(s.len(), 20, "expected 20 chars, got: {s}");
        assert_eq!(s.chars().nth(4), Some('-'));
        assert_eq!(s.chars().nth(7), Some('-'));
        assert_eq!(s.chars().nth(10), Some('T'));
        assert_eq!(s.chars().nth(13), Some(':'));
        assert_eq!(s.chars().nth(16), Some(':'));
    }

    #[test]
    fn now_iso8601_matches_now_utc() {
        let a = now_iso8601();
        let b = to_iso8601(&now_utc());
        // Within 1 second of each other (truncated to second precision).
        let pa: DateTime<Utc> = a.parse().unwrap();
        let pb: DateTime<Utc> = b.parse().unwrap();
        assert!((pa - pb).num_seconds().abs() <= 1);
    }
}
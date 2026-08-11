//! UUID v7 (time-ordered) wrapper.
//!
//! UUID v7 layout:
//!   - 48-bit unix-ms timestamp
//!   - 4-bit version (7)
//!   - 12-bit random
//!   - 2-bit variant (10)
//!   - 62-bit random
//!
//! Sorted lexicographically = sorted chronologically. Useful for JSONL
//! session entries where we want stable, time-ordered ids.

pub use uuid::Uuid;

/// Generate a new UUID v7 (time-ordered).
///
/// Calls `Uuid::now_v7()` which extracts the unix-ms timestamp from the
/// first 48 bits and fills the rest with system entropy.
pub fn v7() -> Uuid {
    Uuid::now_v7()
}

/// Parse a UUID from its canonical hyphenated form.
///
/// Returns an error if the string is not a valid UUID.
pub fn parse(s: &str) -> Result<Uuid, uuid::Error> {
    Uuid::parse_str(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v7_produces_valid_uuid() {
        let id = v7();
        // Canonical hyphenated form is 36 chars.
        let s = id.to_string();
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
    }

    #[test]
    fn v7_is_version_7() {
        let id = v7();
        assert_eq!(id.get_version_num(), 7);
    }

    #[test]
    fn v7_is_time_ordered() {
        // Two ids generated in sequence should sort lexicographically.
        // (Sleep guarantees the millisecond clock ticks.)
        let a = v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = v7();
        assert!(a.to_string() < b.to_string(), "{a} should be < {b}");
    }

    #[test]
    fn parse_roundtrips() {
        let id = v7();
        let s = id.to_string();
        let parsed = parse(&s).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn parse_rejects_invalid() {
        assert!(parse("not-a-uuid").is_err());
        assert!(parse("0190abc").is_err()); // too short
        assert!(parse("").is_err());
    }
}

//! HTTP retry policy for LLM provider calls.
//!
//! Modeled after PI's `retryProviderRequest` (see
//! `pi/packages/ai/src/utils/provider-retry.ts`) with the same three
//! moving parts: retryable-error detection, `Retry-After` header
//! parsing, and exponential-backoff-with-jitter delay computation.
//!
//! Only the OPEN of the SSE stream is retried — once the stream is
//! producing events, retries become "did I already stream half a
//! response?" and require buffering. That's a v0.7 problem.

use std::time::Duration;

use reqwest::header::HeaderMap;

/// Retry knobs. All configurable via `[retry]` in config.toml (v0.7);
/// for now these are the defaults everyone gets.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Max number of retries AFTER the initial attempt. `3` means up to
    /// 4 total requests (1 original + 3 retries).
    pub max_attempts: u32,
    /// Base for exponential backoff. Delay = base * 2^attempt.
    pub base_delay: Duration,
    /// Cap for a single delay. Server-suggested Retry-After beyond this
    /// aborts the retry loop.
    pub max_delay: Duration,
    /// Jitter fraction: delay is multiplied by (1 - rand*jitter).
    /// 0.25 = up to 25% earlier than the raw exponential value.
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            jitter: 0.25,
        }
    }
}

/// Does this HTTP status warrant a retry? Mirrors PI's list.
///  408 Request Timeout, 409 Conflict, 429 Rate Limited, 5xx Server errors.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500..=599)
}

/// Does this transport / stream error message look retryable? These
/// patterns are the ones PI's `RETRYABLE_PROVIDER_ERROR_PATTERN` lists,
/// trimmed to what actually appears in reqwest / SSE stack traces.
pub fn is_retryable_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    // Non-retryable "you're out of money" errors trump retryable ones.
    for bad in [
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ] {
        if m.contains(bad) {
            return false;
        }
    }
    for pat in [
        "overloaded",
        "rate limit",
        "rate_limit",
        "service unavailable",
        "service_unavailable",
        "try your request again",
        "please retry",
        "you can retry",
        "queue_timeout",
        "queue timeout",
        // Some gateways return a transient "OAuth access token has
        // expired. Re-authenticate to continue." on the first request
        // after cold-start / pool churn; the SECOND request works. Try
        // 2-3 more times with backoff before giving up.
        "oauth access token has expired",
        "re-authenticate to continue",
        // Stream / socket death
        "socket hang up",
        "connection reset",
        "connection lost",
        "reset before headers",
        "stream ended before",
        "ended without",
        "http2 request did not get a response",
        // DNS / connect
        "enotfound",
        "eai_again",
        "getaddrinfo",
        "connection refused",
        "timed out",
        "timeout",
    ] {
        if m.contains(pat) {
            return true;
        }
    }
    false
}

/// Extract a suggested delay from `Retry-After` / `Retry-After-Ms`
/// headers. Anthropic uses `retry-after` (seconds); some proxies expose
/// `retry-after-ms` (milliseconds). Returns None if unset or unparseable.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    if let Some(v) = headers.get("retry-after-ms") {
        if let Ok(s) = v.to_str() {
            if let Ok(ms) = s.parse::<u64>() {
                return Some(Duration::from_millis(ms));
            }
        }
    }
    if let Some(v) = headers.get("retry-after") {
        if let Ok(s) = v.to_str() {
            if let Ok(secs) = s.parse::<u64>() {
                return Some(Duration::from_secs(secs));
            }
        }
    }
    None
}

/// Compute the delay for attempt `n` (0-indexed): exponential base *
/// 2^n, then jitter (subtract up to `jitter` fraction), then cap.
/// A server-provided `Retry-After` always wins (still capped by
/// `max_delay`).
pub fn compute_delay(
    attempt: u32,
    cfg: &RetryConfig,
    server_hint: Option<Duration>,
    now_rand: f64, // 0.0..1.0; parameter so tests can inject
) -> Duration {
    let hint = server_hint.map(|d| d.min(cfg.max_delay));
    if let Some(d) = hint {
        return d;
    }
    // exponential (saturating to avoid overflow on high attempts)
    let mul = 1u64 << attempt.min(20) as u64;
    let raw = cfg.base_delay.saturating_mul(mul as u32);
    // apply jitter: multiply by (1 - now_rand * jitter)
    let factor = 1.0 - now_rand.clamp(0.0, 1.0) * cfg.jitter;
    let jittered = Duration::from_secs_f64((raw.as_secs_f64() * factor).max(0.0));
    jittered.min(cfg.max_delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(408));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(200));
    }

    #[test]
    fn retryable_messages() {
        assert!(is_retryable_message("overloaded_error"));
        assert!(is_retryable_message("rate_limit_error"));
        assert!(is_retryable_message("Try your request again in a moment"));
        assert!(is_retryable_message("socket hang up"));
        assert!(is_retryable_message("ENOTFOUND api.example.com"));
        assert!(is_retryable_message("queue_timeout"));
        // Transient gateway OAuth churn — first request fails, retry
        // succeeds.
        assert!(is_retryable_message(
            "OAuth access token has expired. Re-authenticate to continue."
        ));
        // Non-retryable trumps retryable
        assert!(!is_retryable_message(
            "insufficient_quota: your account is out of budget"
        ));
        // Bare unrelated
        assert!(!is_retryable_message("invalid_api_key"));
    }

    #[test]
    fn retry_after_seconds_header() {
        let h = hdr(&[("retry-after", "5")]);
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(5)));
    }

    #[test]
    fn retry_after_ms_header_preferred() {
        let h = hdr(&[("retry-after-ms", "1500"), ("retry-after", "2")]);
        assert_eq!(parse_retry_after(&h), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn retry_after_absent() {
        let h = hdr(&[]);
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn compute_delay_exponential() {
        let cfg = RetryConfig::default(); // base=2s, jitter=0.25
        let d0 = compute_delay(0, &cfg, None, 0.0);
        let d1 = compute_delay(1, &cfg, None, 0.0);
        let d2 = compute_delay(2, &cfg, None, 0.0);
        assert_eq!(d0, Duration::from_secs(2));
        assert_eq!(d1, Duration::from_secs(4));
        assert_eq!(d2, Duration::from_secs(8));
    }

    #[test]
    fn compute_delay_jitter_reduces() {
        let cfg = RetryConfig::default();
        let no_jitter = compute_delay(0, &cfg, None, 0.0);
        let full_jitter = compute_delay(0, &cfg, None, 1.0);
        assert!(full_jitter < no_jitter);
        // With jitter=0.25 and rand=1.0: delay = 2s * 0.75 = 1.5s
        assert_eq!(full_jitter, Duration::from_millis(1500));
    }

    #[test]
    fn compute_delay_caps_at_max() {
        let cfg = RetryConfig {
            base_delay: Duration::from_secs(30),
            max_delay: Duration::from_secs(60),
            ..RetryConfig::default()
        };
        let d = compute_delay(5, &cfg, None, 0.0); // 30s * 32 = 960s, capped
        assert_eq!(d, Duration::from_secs(60));
    }

    #[test]
    fn compute_delay_honors_server_hint() {
        let cfg = RetryConfig::default();
        let hint = Some(Duration::from_secs(7));
        let d = compute_delay(0, &cfg, hint, 0.0);
        assert_eq!(d, Duration::from_secs(7));
    }

    #[test]
    fn compute_delay_caps_server_hint() {
        let cfg = RetryConfig {
            max_delay: Duration::from_secs(10),
            ..RetryConfig::default()
        };
        let hint = Some(Duration::from_secs(600));
        let d = compute_delay(0, &cfg, hint, 0.0);
        assert_eq!(d, Duration::from_secs(10));
    }
}

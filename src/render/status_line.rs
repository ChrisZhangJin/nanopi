//! Status-line component builders shared by both the `--tui` footer
//! and the print-mode pre-/post-turn dim status line.
//!
//! Each function returns a `String` (or an `Option<String>`) of a
//! single info fragment. The caller assembles them with the separator
//! it wants and applies whatever styling fits its renderer.

use std::path::Path;

use crate::event::Usage;
use crate::pricing::{self, ModelPricing};

/// Human-readable current-directory string, with `$HOME` folded to `~`.
pub fn cwd_display(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    if let Some(home) = std::env::var_os("HOME") {
        let home_s = home.to_string_lossy().to_string();
        if let Some(rest) = s.strip_prefix(&home_s) {
            return format!("~{}", rest);
        }
    }
    s.into_owned()
}

/// `feature/foo` if we're on a branch, `HEAD@abc1234` if detached,
/// empty string if not in a git repo.
pub fn git_branch(cwd: &Path) -> String {
    crate::util::git::branch_of(cwd).unwrap_or_default()
}

/// First 8 chars of a session UUID — enough to disambiguate visually
/// without overwhelming the status line.
pub fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Tokens summary: `↑1.2k ↓340 R89 W12 CH27.4%`. Cache figures are
/// omitted when zero; CH% (cache-hit rate) only when there's been at
/// least one cache read.
pub fn tokens_summary(u: &Usage) -> String {
    let mut parts = vec![
        format!("↑{}", pricing::fmt_tokens(u.input_tokens)),
        format!("↓{}", pricing::fmt_tokens(u.output_tokens)),
    ];
    if u.cache_read_tokens > 0 {
        parts.push(format!("R{}", pricing::fmt_tokens(u.cache_read_tokens)));
    }
    if u.cache_write_tokens > 0 {
        parts.push(format!("W{}", pricing::fmt_tokens(u.cache_write_tokens)));
    }
    // Cache-hit rate = cache_read / (cache_read + input) — proportion of
    // tokens served from cache rather than freshly billed as input.
    let denom = u.cache_read_tokens as u64 + u.input_tokens as u64;
    if u.cache_read_tokens > 0 && denom > 0 {
        let rate = (u.cache_read_tokens as f64 / denom as f64) * 100.0;
        parts.push(format!("CH{:.1}%", rate));
    }
    parts.join(" ")
}

/// USD cost of `usage` at `model`'s list price. Empty string if the
/// model isn't in the price table.
pub fn cost_string(model: &str, u: &Usage) -> String {
    match pricing::lookup(model) {
        Some(p) => pricing::fmt_cost(p.cost(u)),
        None => String::new(),
    }
}

/// Context usage as a fraction of the model's window. Returns None if
/// the window isn't known. `chars` is the current
/// `Context::estimate_chars` value; we approximate tokens = chars/4.
/// Full precision so callers can format `.1f` — PI-style.
pub fn context_percent(model: &str, chars: usize) -> Option<f64> {
    let window = pricing::context_window(model)? as usize;
    let est_tokens = chars / 4;
    let pct = (est_tokens as f64 / window as f64) * 100.0;
    Some(pct.clamp(0.0, 999.0))
}

/// Formatted `{:.1}%/<windowStr>` string — matches PI's footer
/// (`1.4%/205k`). Returns None if the model's window isn't in our
/// table. `(auto)` suffix appended when auto_compact is on.
pub fn context_ratio(model: &str, chars: usize, auto_compact: bool) -> Option<String> {
    let pct = context_percent(model, chars)?;
    let window = pricing::context_window(model)?;
    let base = format!("{:.1}%/{}", pct, pricing::fmt_tokens(window));
    if auto_compact {
        Some(format!("{} (auto)", base))
    } else {
        Some(base)
    }
}

/// Pick a color name (returned as a `&'static str` matching ratatui /
/// ANSI conventions) for a context percentage.
pub fn context_color(pct: f64) -> &'static str {
    if pct >= 90.0 {
        "red"
    } else if pct >= 70.0 {
        "yellow"
    } else {
        "green"
    }
}

/// Ad-hoc pricing lookup for callers that just want the struct.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    pricing::lookup(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn u(inp: u32, out: u32) -> Usage {
        Usage {
            input_tokens: inp,
            output_tokens: out,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    #[test]
    fn cwd_folds_home() {
        // Only meaningful if HOME is set; on the test host it should be.
        if let Some(home) = std::env::var_os("HOME") {
            let p = PathBuf::from(home).join("workspace/x");
            let s = cwd_display(&p);
            assert!(s.starts_with("~/"), "got {s}");
        }
    }

    #[test]
    fn tokens_summary_omits_zero_cache() {
        let s = tokens_summary(&u(1234, 567));
        assert!(s.contains("↑1.2k"));
        assert!(s.contains("↓567"));
        assert!(!s.contains("R"));
        assert!(!s.contains("W"));
    }

    #[test]
    fn tokens_summary_includes_cache() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 200,
            cache_write_tokens: 30,
        };
        let s = tokens_summary(&usage);
        assert!(s.contains("R200"));
        assert!(s.contains("W30"));
        // CH = 200 / (200 + 100) = 66.7%
        assert!(s.contains("CH66.7%"), "got {s}");
    }

    #[test]
    fn tokens_summary_no_ch_without_cache_read() {
        let s = tokens_summary(&u(500, 200));
        assert!(!s.contains("CH"));
    }

    #[test]
    fn cost_falls_back_to_empty_for_unknown_model() {
        assert!(cost_string("unknown-xyz", &u(1000, 1000)).is_empty());
    }

    #[test]
    fn cost_computed_for_known_model() {
        let s = cost_string("claude-opus-4-7", &u(1_000_000, 1_000_000));
        assert_eq!(s, "$90.00");
    }

    #[test]
    fn context_percent_reasonable() {
        // claude-opus-4-7 window = 1_000_000.
        // 400k chars → 100k tokens → 10.0%.
        let p = context_percent("claude-opus-4-7", 400_000).unwrap();
        assert!((p - 10.0).abs() < 0.01);
    }

    #[test]
    fn context_ratio_formatting() {
        // 10.0% for opus 4.7 (1M window).
        let s = context_ratio("claude-opus-4-7", 400_000, false).unwrap();
        assert_eq!(s, "10.0%/1.0M");
        // With auto suffix.
        let s = context_ratio("claude-opus-4-7", 400_000, true).unwrap();
        assert_eq!(s, "10.0%/1.0M (auto)");
        // Small usage no longer rounds to 0.
        let s = context_ratio("claude-opus-4-7", 12_000, false).unwrap();
        assert_eq!(s, "0.3%/1.0M");
    }

    #[test]
    fn context_color_ranges() {
        assert_eq!(context_color(0.0), "green");
        assert_eq!(context_color(70.0), "yellow");
        assert_eq!(context_color(89.9), "yellow");
        assert_eq!(context_color(90.0), "red");
    }
}

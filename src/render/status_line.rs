//! Status-line component builders shared by both the `--tui` footer
//! and the rustyline pre-/post-turn dim status line.
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
pub fn short_session_id(uuid: &uuid::Uuid) -> String {
    let s = uuid.to_string();
    s.chars().take(8).collect()
}

/// Tokens summary: `↑1.2k ↓340 R89 W12`. Cache figures are omitted
/// when zero.
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

/// Context usage as `NN%` of the model's window. Returns None if the
/// window isn't known. `chars` is the current `Context::estimate_chars`
/// value; we approximate tokens = chars/4.
pub fn context_percent(model: &str, chars: usize) -> Option<u8> {
    let window = pricing::context_window(model)? as usize;
    let est_tokens = chars / 4;
    let pct = (est_tokens as f64 / window as f64) * 100.0;
    Some(pct.clamp(0.0, 999.0) as u8)
}

/// Pick a color name (returned as a `&'static str` matching ratatui /
/// ANSI conventions) for a context percentage.
pub fn context_color(pct: u8) -> &'static str {
    if pct >= 90 {
        "red"
    } else if pct >= 70 {
        "yellow"
    } else {
        "green"
    }
}

/// Full pre-turn status assembled for the rustyline / classic mode.
/// Each piece dim-gray, separated by ` · `. Optional pieces (empty
/// branch, absent cost) are dropped.
///
/// Example output:
///   `claude-opus-4-7 · ↑1.2k ↓340 · $0.05 · ~/nanopi · main`
pub fn classic_status_line(
    model: &str,
    usage: &Usage,
    cwd: &Path,
) -> String {
    let mut parts = vec![model.to_string(), tokens_summary(usage)];
    let cost = cost_string(model, usage);
    if !cost.is_empty() {
        parts.push(cost);
    }
    parts.push(cwd_display(cwd));
    let br = git_branch(cwd);
    if !br.is_empty() {
        parts.push(br);
    }
    parts.join(" · ")
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
        // 200k tokens ≈ 800k chars for claude-opus-4-7 (1M window).
        // 400k chars → 100k tokens → 10%.
        let p = context_percent("claude-opus-4-7", 400_000);
        assert_eq!(p, Some(10));
    }

    #[test]
    fn context_color_ranges() {
        assert_eq!(context_color(0), "green");
        assert_eq!(context_color(70), "yellow");
        assert_eq!(context_color(89), "yellow");
        assert_eq!(context_color(90), "red");
    }

    #[test]
    fn classic_status_line_builds() {
        let cwd = std::path::PathBuf::from("/nonexistent");
        let line = classic_status_line("claude-opus-4-7", &u(1000, 500), &cwd);
        assert!(line.contains("claude-opus-4-7"));
        assert!(line.contains("↑1.0k"));
        assert!(line.contains("↓500"));
        assert!(line.contains("$"));  // known model → cost present
    }
}

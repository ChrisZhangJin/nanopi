//! Model → per-token pricing table and cost formatting.
//!
//! Prices are USD per 1 million tokens (the units Anthropic / OpenAI
//! publish). If a model isn't in the table we fall back to zero and
//! display `$—` in the UI — better to hide the number than to lie.
//!
//! Updating: keep this file the single source of truth. The table is
//! `const` so no runtime IO; users can override via config.toml later
//! (v0.8).

use crate::event::Usage;

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelPricing {
    /// Compute USD cost for a single `Usage` sample.
    pub fn cost(&self, u: &Usage) -> f64 {
        const M: f64 = 1_000_000.0;
        (u.input_tokens as f64) * self.input / M
            + (u.output_tokens as f64) * self.output / M
            + (u.cache_read_tokens as f64) * self.cache_read / M
            + (u.cache_write_tokens as f64) * self.cache_write / M
    }
}

/// All model prefixes we know pricing for. Used by the `/model`
/// palette to enumerate switchable models — the gateway may support
/// more, but this is what the UI can *quote a price for*.
pub fn known_models() -> Vec<&'static str> {
    TABLE.iter().map(|(prefix, _)| *prefix).collect()
}

/// Look up pricing for a model id. Returns None if unknown.
pub fn lookup(model_id: &str) -> Option<ModelPricing> {
    // Newest models near the top; match on prefix so date-suffixed
    // variants (`claude-opus-4-7-20260101`) hit the same row.
    for (prefix, p) in TABLE {
        if model_id.starts_with(prefix) {
            return Some(*p);
        }
    }
    None
}

/// Approximate context window (max input tokens) for a model. Used to
/// render the "45% context" bar in the status footer. None if unknown.
pub fn context_window(model_id: &str) -> Option<u32> {
    for (prefix, w) in CONTEXT_WINDOWS {
        if model_id.starts_with(prefix) {
            return Some(*w);
        }
    }
    None
}

/// Publicly-listed prices as of 2026. Keep sorted by "newest first"
/// so prefix match hits the specific version before a generic one.
const TABLE: &[(&str, ModelPricing)] = &[
    // Claude 4.x family — Anthropic's list price (USD / 1M tokens).
    ("claude-opus-4-8", ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-opus-4-7", ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-opus-4-6", ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-opus-4-5", ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-opus-4-1", ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-opus-4",   ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-sonnet-4-6", ModelPricing { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("claude-sonnet-4-5", ModelPricing { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("claude-sonnet-4",   ModelPricing { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("claude-haiku-4-5",  ModelPricing { input: 1.0, output: 5.0,  cache_read: 0.1, cache_write: 1.25 }),
    ("claude-haiku-4",    ModelPricing { input: 1.0, output: 5.0,  cache_read: 0.1, cache_write: 1.25 }),
    // Claude 3.x legacy
    ("claude-3-5-sonnet", ModelPricing { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("claude-3-5-haiku",  ModelPricing { input: 0.8, output: 4.0,  cache_read: 0.08, cache_write: 1.0 }),
    ("claude-3-7-sonnet", ModelPricing { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("claude-3-opus",     ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-3-haiku",    ModelPricing { input: 0.25, output: 1.25, cache_read: 0.03, cache_write: 0.3 }),
    // OpenAI GPT-5 family (list prices as reported at launch).
    ("gpt-5.4-pro", ModelPricing { input: 15.0, output: 60.0, cache_read: 1.5, cache_write: 0.0 }),
    ("gpt-5.4",     ModelPricing { input: 5.0,  output: 20.0, cache_read: 0.5, cache_write: 0.0 }),
    ("gpt-5.3",     ModelPricing { input: 5.0,  output: 20.0, cache_read: 0.5, cache_write: 0.0 }),
    ("gpt-5.1",     ModelPricing { input: 3.0,  output: 12.0, cache_read: 0.3, cache_write: 0.0 }),
    ("gpt-5",       ModelPricing { input: 3.0,  output: 12.0, cache_read: 0.3, cache_write: 0.0 }),
    // Gemini
    ("gemini-3",   ModelPricing { input: 2.5,  output: 10.0, cache_read: 0.25, cache_write: 0.0 }),
    ("gemini-2.5-pro",   ModelPricing { input: 2.5,  output: 10.0, cache_read: 0.25, cache_write: 0.0 }),
    ("gemini-2.5-flash", ModelPricing { input: 0.15, output: 0.6,  cache_read: 0.015, cache_write: 0.0 }),
];

/// Rough context-window sizes. Not all models are listed; unknown
/// falls back to hiding the % gauge.
const CONTEXT_WINDOWS: &[(&str, u32)] = &[
    ("claude-opus-4-7",   1_000_000),
    ("claude-opus-4-8",   1_000_000),
    ("claude-opus-4",       200_000),
    ("claude-sonnet-4",     200_000),
    ("claude-haiku-4",      200_000),
    ("claude-3-5-sonnet",   200_000),
    ("claude-3-5-haiku",    200_000),
    ("claude-3-7-sonnet",   200_000),
    ("claude-3-opus",       200_000),
    ("claude-3-haiku",      200_000),
    ("gpt-5",               400_000),
    ("gpt-5.1",             400_000),
    ("gpt-5.3",             400_000),
    ("gpt-5.4",             400_000),
    ("gemini-2.5-flash",  1_000_000),
    ("gemini-2.5-pro",    2_000_000),
    ("gemini-3",          2_000_000),
];

// ─── Formatting helpers ─────────────────────────────────────────────

/// Format a token count: 1234 → "1.2k", 1_234_567 → "1.2M".
pub fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format USD cost. Very small (< $0.01) shows `$0.00+` to convey
/// "nonzero but tiny"; otherwise two decimals.
pub fn fmt_cost(usd: f64) -> String {
    if usd <= 0.0 {
        "$0.00".into()
    } else if usd < 0.01 {
        "$0.00+".into()
    } else {
        format!("${:.2}", usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_exact_model() {
        let p = lookup("claude-opus-4-7").unwrap();
        assert_eq!(p.input, 15.0);
    }

    #[test]
    fn lookup_dated_variant_hits_prefix() {
        let p = lookup("claude-sonnet-4-5-20260929").unwrap();
        assert_eq!(p.input, 3.0);
    }

    #[test]
    fn lookup_unknown_model_is_none() {
        assert!(lookup("no-such-model").is_none());
    }

    #[test]
    fn cost_computation() {
        let p = ModelPricing { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 };
        let u = Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, cache_read_tokens: 0, cache_write_tokens: 0 };
        assert!((p.cost(&u) - 90.0).abs() < 1e-9);
    }

    #[test]
    fn fmt_tokens_bins() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_234), "1.2k");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(1_234_567), "1.2M");
    }

    #[test]
    fn fmt_cost_bins() {
        assert_eq!(fmt_cost(0.0), "$0.00");
        assert_eq!(fmt_cost(0.001), "$0.00+");
        assert_eq!(fmt_cost(0.10), "$0.10");
        assert_eq!(fmt_cost(12.34567), "$12.35");
    }

    #[test]
    fn context_window_known() {
        assert_eq!(context_window("claude-opus-4-7"), Some(1_000_000));
        assert_eq!(context_window("gpt-5.1"), Some(400_000));
        assert_eq!(context_window("unknown-model"), None);
    }
}

//! Extended thinking / reasoning budgets for Claude 3.7+ / 4.x models.
//!
//! Mirrors PI's ThinkingLevel enum (see
//! `packages/ai/src/types.ts:79-80`). Token budgets match PI's
//! `simple-options.ts:59-64` defaults; Xhigh and Max clamp to High
//! on token-based Anthropic providers (PI does the same at
//! `bedrock-converse-stream.ts:1119`).

use std::fmt;

/// One of Anthropic's 6 thinking budget levels. `Off` is represented
/// as `Option<ThinkingLevel>::None` at the API surface — no need for
/// a redundant enum variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// Token budget the model gets for its private reasoning trace.
    /// Anthropic's public docs cap this at 16384 for Claude Opus 4.x
    /// today; Xhigh and Max are provided for parity with PI but land
    /// on the same value.
    pub fn budget_tokens(self) -> u32 {
        match self {
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            ThinkingLevel::High => 16384,
            // Anthropic clamps > High to High for now — mirror that
            // instead of sending values Anthropic will reject or
            // silently clip.
            ThinkingLevel::Xhigh => 16384,
            ThinkingLevel::Max => 16384,
        }
    }

    /// Ordered list for UI pickers — Off (as `None`) then the six
    /// active levels ascending in budget.
    pub fn all() -> &'static [ThinkingLevel] {
        &[
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ]
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        })
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "minimal" => ThinkingLevel::Minimal,
            "low" => ThinkingLevel::Low,
            "medium" | "med" => ThinkingLevel::Medium,
            "high" => ThinkingLevel::High,
            "xhigh" | "x-high" | "extra-high" => ThinkingLevel::Xhigh,
            "max" => ThinkingLevel::Max,
            _ => return Err(()),
        })
    }
}

/// Whether the given model id accepts multimodal image content.
/// Anthropic vision has been on since Claude 3, and every 3.5+
/// Claude ships with it. Conservative allowlist — unknown ids
/// default to false so we don't attach 5MB base64 payloads to
/// text-only models that would 400 on them.
pub fn supports_vision(model_id: &str) -> bool {
    let m = model_id.to_ascii_lowercase();
    if m.starts_with("claude-opus-4") || m.starts_with("claude-sonnet-4")
        || m.starts_with("claude-haiku-4")
    {
        return true;
    }
    if m.starts_with("claude-3-5-sonnet")
        || m.starts_with("claude-3-5-haiku")
        || m.starts_with("claude-3-7-sonnet")
        || m.starts_with("claude-3-opus")
        || m.starts_with("claude-3-sonnet")
        || m.starts_with("claude-3-haiku")
    {
        return true;
    }
    // OpenAI vision: gpt-4o family + gpt-4.5 accept images. gpt-4
    // (original) also does but is deprecated.
    if m.starts_with("gpt-4o") || m.starts_with("gpt-4.5") || m == "gpt-4-vision-preview" {
        return true;
    }
    false
}

/// Whether the given model id accepts Anthropic's `thinking`
/// parameter. Conservative allowlist — Anthropic's extended thinking
/// shipped in Claude 3.7 Sonnet, then rolled into the 4.x family.
///
/// Unknown ids default to false so we don't send `thinking` to a
/// gateway that doesn't recognize it (some proxies 400 on unknown
/// top-level params instead of ignoring them).
pub fn supports_thinking(model_id: &str) -> bool {
    let m = model_id.to_ascii_lowercase();
    // Claude 4.x family — every current member supports thinking.
    if m.starts_with("claude-opus-4") || m.starts_with("claude-sonnet-4") {
        return true;
    }
    // Extended-thinking preview in Claude 3.7 Sonnet.
    if m.starts_with("claude-3-7-sonnet") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_tokens_match_pi_defaults() {
        assert_eq!(ThinkingLevel::Minimal.budget_tokens(), 1024);
        assert_eq!(ThinkingLevel::Low.budget_tokens(), 2048);
        assert_eq!(ThinkingLevel::Medium.budget_tokens(), 8192);
        assert_eq!(ThinkingLevel::High.budget_tokens(), 16384);
    }

    #[test]
    fn xhigh_and_max_clamp_to_high() {
        // Claude token-based providers clamp above High — matches PI.
        assert_eq!(ThinkingLevel::Xhigh.budget_tokens(), 16384);
        assert_eq!(ThinkingLevel::Max.budget_tokens(), 16384);
    }

    #[test]
    fn display_and_parse_roundtrip() {
        for &lvl in ThinkingLevel::all() {
            let s = lvl.to_string();
            let back: ThinkingLevel = s.parse().unwrap();
            assert_eq!(back, lvl, "roundtrip broken for {lvl}");
        }
    }

    #[test]
    fn from_str_accepts_common_aliases() {
        assert_eq!("MEDIUM".parse::<ThinkingLevel>().unwrap(), ThinkingLevel::Medium);
        assert_eq!("med".parse::<ThinkingLevel>().unwrap(), ThinkingLevel::Medium);
        assert_eq!("x-high".parse::<ThinkingLevel>().unwrap(), ThinkingLevel::Xhigh);
        assert!("nonsense".parse::<ThinkingLevel>().is_err());
    }

    #[test]
    fn supports_thinking_allows_current_claude_family() {
        assert!(supports_thinking("claude-opus-4-7"));
        assert!(supports_thinking("claude-opus-4-7-20260101"));
        assert!(supports_thinking("claude-sonnet-4-6"));
        assert!(supports_thinking("claude-3-7-sonnet-latest"));
    }

    #[test]
    fn supports_thinking_rejects_older_and_unknown() {
        assert!(!supports_thinking("claude-3-5-sonnet"));
        assert!(!supports_thinking("claude-haiku-4-5"));
        assert!(!supports_thinking("gpt-4o"));
        assert!(!supports_thinking("unknown-model"));
    }
}

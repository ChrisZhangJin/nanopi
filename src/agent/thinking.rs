//! Thinking depth: one level, sent per model as the right wire field.
//!
//! - Claude 5 and Claude 4.6-4.8: adaptive thinking plus
//!   `output_config.effort` ([`uses_effort`]). These models reject (or,
//!   on 4.6, deprecate) a fixed `budget_tokens`; effort is the control.
//! - Older thinking-capable Claude models (3.7, 4.0-4.5): the original
//!   `thinking: {type: "enabled", budget_tokens}`.
//!
//! The user sets it with `/effort`, Shift+Tab or `/settings`; `None`
//! (`/effort auto`) sends nothing and leaves the model's own default.
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

impl ThinkingLevel {
    /// The `effort` value for this level. `Minimal` has no effort of its
    /// own and maps to `low`.
    pub fn effort(self) -> &'static str {
        match self {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
}

/// `max_tokens` for models that think adaptively: they think by default,
/// and the thinking counts against `max_tokens`, so the old 4096 left
/// little room for the answer (a long reply or a whole-file `write` was
/// cut off). Well under their 128K output cap; only generated tokens are
/// billed. Requests stream, so a large value is no timeout risk.
pub const ADAPTIVE_MAX_TOKENS: u32 = 64_000;

/// Claude models whose thinking is controlled by `output_config.effort`
/// with `thinking: {type: "adaptive"}` (Claude 5 family; Opus 4.6-4.8,
/// Sonnet 4.6).
pub fn uses_effort(model_id: &str) -> bool {
    let m = model_id.to_ascii_lowercase();
    [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
    ]
    .iter()
    .any(|p| m.starts_with(p))
}

/// What a thinking level becomes on the wire for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingWire {
    /// `thinking: {type: "adaptive"}` + `output_config.effort`.
    /// `clamped` is set when the model lacks the asked-for level.
    Effort { effort: &'static str, clamped: bool },
    /// `thinking: {type: "enabled", budget_tokens}`.
    Budget(u32),
    /// The model takes no thinking control; nothing is sent.
    Unsupported,
}

pub fn wire(model_id: &str, level: ThinkingLevel) -> ThinkingWire {
    if uses_effort(model_id) {
        let m = model_id.to_ascii_lowercase();
        // 4.6 has low/medium/high/max; `xhigh` arrived with Opus 4.7.
        let no_xhigh = m.starts_with("claude-opus-4-6") || m.starts_with("claude-sonnet-4-6");
        if no_xhigh && level == ThinkingLevel::Xhigh {
            return ThinkingWire::Effort { effort: "high", clamped: true };
        }
        return ThinkingWire::Effort { effort: level.effort(), clamped: false };
    }
    if supports_thinking(model_id) {
        return ThinkingWire::Budget(level.budget_tokens());
    }
    ThinkingWire::Unsupported
}

/// One line saying what the model receives — for `/effort` and the
/// confirmation after a change.
pub fn describe(model_id: &str, level: Option<ThinkingLevel>) -> String {
    match level {
        None if uses_effort(model_id) => {
            "nothing — the model uses its own default effort".to_string()
        }
        None => "nothing — thinking off".to_string(),
        Some(l) => match wire(model_id, l) {
            ThinkingWire::Effort { effort, clamped } => format!(
                "thinking: adaptive · output_config.effort = \"{effort}\"{}",
                if clamped { format!(" ({l} is not available on this model)") } else { String::new() }
            ),
            ThinkingWire::Budget(b) => format!("thinking.budget_tokens = {b}"),
            ThinkingWire::Unsupported => "nothing — this model takes no thinking setting".to_string(),
        },
    }
}

/// Write the Anthropic thinking fields for `level` into a request body.
/// Shared by the native Anthropic provider and the Anthropic vendor (the
/// latter also on an OpenAI-compatible gateway, where these fields pass
/// through to the model as-is).
pub fn write_anthropic(body: &mut serde_json::Value, level: ThinkingLevel, model_id: &str) {
    use serde_json::json;
    match wire(model_id, level) {
        ThinkingWire::Effort { effort, .. } => {
            body["thinking"] = json!({"type": "adaptive"});
            if !body["output_config"].is_object() {
                body["output_config"] = json!({});
            }
            body["output_config"]["effort"] = json!(effort);
            let cur = body["max_tokens"].as_u64().unwrap_or(0);
            if cur < u64::from(ADAPTIVE_MAX_TOKENS) {
                body["max_tokens"] = json!(ADAPTIVE_MAX_TOKENS);
            }
        }
        ThinkingWire::Budget(budget) => {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
            // Anthropic requires max_tokens > budget_tokens so the model
            // has room to answer after thinking.
            body["max_tokens"] = json!(budget + 4096);
        }
        ThinkingWire::Unsupported => {}
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
    // Claude 5 family. Missing for the same reason as in
    // `supports_thinking` below — the list predates 5.x. Not cosmetic:
    // both providers strip image blocks when this returns false, so a
    // `read` of a PNG on a Claude 5 model reached the model only as
    // "(N image(s) omitted — model does not support vision)".
    if m.starts_with("claude-opus-5")
        || m.starts_with("claude-sonnet-5")
        || m.starts_with("claude-haiku-5")
        || m.starts_with("claude-fable-5")
    {
        return true;
    }
    if m.starts_with("claude-opus-4")
        || m.starts_with("claude-sonnet-4")
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
    // Claude 5 family and Claude 4.6-4.8 take thinking through effort
    // (`uses_effort`): adaptive thinking, no `budget_tokens`.
    if uses_effort(&m) {
        return true;
    }
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
    fn effort_models_get_adaptive_thinking_and_effort() {
        use serde_json::json;
        for m in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-2", "claude-opus-4-7"] {
            assert!(uses_effort(m), "{m}");
            let mut b = json!({"max_tokens": 4096});
            write_anthropic(&mut b, ThinkingLevel::Xhigh, m);
            assert_eq!(b["thinking"], json!({"type": "adaptive"}), "{m}");
            assert_eq!(b["output_config"]["effort"], "xhigh", "{m}");
            assert!(b["thinking"].get("budget_tokens").is_none(), "{m}: budget_tokens 400s here");
            assert_eq!(b["max_tokens"], ADAPTIVE_MAX_TOKENS);
        }
        // Minimal has no effort of its own.
        assert_eq!(wire("claude-opus-5-5", ThinkingLevel::Minimal), ThinkingWire::Effort { effort: "low", clamped: false });
        // 4.6 has no xhigh.
        assert_eq!(wire("claude-opus-4-6", ThinkingLevel::Xhigh), ThinkingWire::Effort { effort: "high", clamped: true });
        assert_eq!(wire("claude-sonnet-4-6", ThinkingLevel::Max), ThinkingWire::Effort { effort: "max", clamped: false });
        // An existing output_config (e.g. a format) is kept.
        let mut b = json!({"output_config": {"format": {"type": "json_schema"}}});
        write_anthropic(&mut b, ThinkingLevel::Low, "claude-opus-5-5");
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert_eq!(b["output_config"]["effort"], "low");
    }

    #[test]
    fn older_models_keep_budget_tokens_and_others_get_nothing() {
        use serde_json::json;
        for m in ["claude-opus-4-5", "claude-sonnet-4-5", "claude-opus-4-1", "claude-3-7-sonnet-latest"] {
            assert!(!uses_effort(m), "{m}");
            assert_eq!(wire(m, ThinkingLevel::Medium), ThinkingWire::Budget(8192), "{m}");
        }
        let mut b = json!({"max_tokens": 4096});
        write_anthropic(&mut b, ThinkingLevel::Medium, "claude-opus-4-5");
        assert_eq!(b["thinking"], json!({"type": "enabled", "budget_tokens": 8192}));
        assert!(b.get("output_config").is_none());
        let mut b = json!({});
        write_anthropic(&mut b, ThinkingLevel::High, "gpt-4o");
        assert_eq!(b, json!({}));
        assert_eq!(wire("gpt-4o", ThinkingLevel::High), ThinkingWire::Unsupported);
    }

    #[test]
    fn describe_says_what_is_sent() {
        assert!(describe("claude-opus-5-5", Some(ThinkingLevel::High)).contains("output_config.effort = \"high\""));
        assert!(describe("claude-opus-4-6", Some(ThinkingLevel::Xhigh)).contains("not available"));
        assert!(describe("claude-opus-4-5", Some(ThinkingLevel::Medium)).contains("budget_tokens = 8192"));
        assert!(describe("claude-opus-5-5", None).contains("own default"));
        assert!(describe("gpt-4o", Some(ThinkingLevel::High)).contains("no thinking setting"));
    }

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
        assert_eq!(
            "MEDIUM".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::Medium
        );
        assert_eq!(
            "med".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::Medium
        );
        assert_eq!(
            "x-high".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::Xhigh
        );
        assert!("nonsense".parse::<ThinkingLevel>().is_err());
    }

    #[test]
    fn supports_thinking_allows_current_claude_family() {
        // Claude 5. These four lines are the regression: the gate in
        // `provider/anthropic.rs` reads this function to decide whether
        // to send `thinking` at all, so a missing family here disables
        // extended thinking outright while the UI still reports a level.
        assert!(supports_thinking("claude-opus-5"));
        assert!(supports_thinking("claude-sonnet-5"));
        assert!(supports_thinking("claude-fable-5"));
        assert!(supports_thinking("claude-sonnet-5-20260501"));
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

    #[test]
    fn supports_vision_claude_5_family() {
        // Regression: Claude 5 ids were missing from the allowlist, so
        // images from `read` were stripped before reaching the model.
        assert!(supports_vision("claude-opus-5-5"));
        assert!(supports_vision("claude-sonnet-5"));
        assert!(supports_vision("claude-haiku-5-5"));
        assert!(supports_vision("claude-fable-5-1"));
        assert!(supports_vision("Claude-Opus-5-5"));
        assert!(!supports_vision("unknown-model"));
    }

    #[test]
    fn thinking_level_lowercase_roundtrip() {
        // settings.toml serialises as lowercase — any casing change
        // would silently break existing user files. Assert both the
        // exact string and roundtrip integrity.
        for lvl in ThinkingLevel::all() {
            let s = serde_json::to_string(lvl).unwrap();
            let parsed: ThinkingLevel = serde_json::from_str(&s).unwrap();
            assert_eq!(*lvl, parsed);
            assert!(
                s.chars().all(|c| !c.is_ascii_uppercase()),
                "expected lowercase serialisation, got {s}"
            );
        }
    }
}

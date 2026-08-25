//! Model catalogue: which models exist, who serves them, and how big
//! their context window is.
//!
//! Deliberately NOT a pricing table. nanopi used to carry per-model USD
//! rates so the status bar could show a running cost; that data went
//! stale, had to be hand-maintained, and — worse — was what the `/model`
//! picker enumerated, so any model nobody had priced was unselectable.
//! MiniMax was invisible in `/model` for exactly that reason.
//!
//! Cache-hit rate needs nothing from here: it is computed from the
//! `Usage` the provider returns (see `status_line::tokens_summary`).

/// One model: its id, the vendor that serves it, and its context window
/// in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: &'static str,
    pub vendor: &'static str,
    pub context_window: u32,
}

/// Context window for a model id, or `None` if we don't know it.
///
/// Prefix match, so `claude-opus-4-7-20260101` resolves via
/// `claude-opus-4-7`. `MODELS` is ordered longest-id-first, which makes
/// first-match-wins equivalent to longest-prefix-wins.
pub fn context_window(model_id: &str) -> Option<u32> {
    MODELS
        .iter()
        .find(|m| model_id.starts_with(m.id))
        .map(|m| m.context_window)
}

/// Every model we know `vendor` serves, sorted by id.
///
/// Drives the `/model` picker. Scoping to the active vendor mirrors PI's
/// "Only showing models from configured providers" — offering models the
/// current API key cannot reach only produces confusing 401s.
pub fn models_for_vendor(vendor: &str) -> Vec<ModelInfo> {
    let mut v: Vec<ModelInfo> = MODELS
        .iter()
        .copied()
        .filter(|m| m.vendor == vendor)
        .collect();
    v.sort_by_key(|m| m.id);
    v
}

/// Format a token count: 1234 -> "1.2k", 1_234_567 -> "1.2M".
pub fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Model catalogue: id, vendor, context window.
///
/// Generated once from <https://models.dev/api.json> (the same source
/// PI's `generate-models.ts` uses), filtered to the vendors nanopi ships
/// and to models that support tool calls — nanopi is an agent, a model
/// that can't call tools is not usable here.
///
/// Ordered LONGEST ID FIRST. `context_window` prefix-matches so that
/// date-suffixed variants (`claude-opus-4-7-20260101`) resolve, and
/// first-match-wins would otherwise let `gpt-5` shadow `gpt-5.4`.
const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "qwen3-coder-480b-a35b-instruct",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "deepseek-v4-flash-vision-exp",
        vendor: "deepseek",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3-coder-30b-a3b-instruct",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen3-next-80b-a3b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen3-next-80b-a3b-thinking",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "claude-sonnet-4-5-20250929",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen2-5-coder-32b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "claude-haiku-4-5-20251001",
        vendor: "anthropic",
        context_window: 200000,
    },
    ModelInfo {
        id: "qwen2-5-coder-7b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen2-5-math-72b-instruct",
        vendor: "qwen",
        context_window: 4096,
    },
    ModelInfo {
        id: "qwen3-omni-flash-realtime",
        vendor: "qwen",
        context_window: 65536,
    },
    ModelInfo {
        id: "claude-opus-4-5-20251101",
        vendor: "anthropic",
        context_window: 200000,
    },
    ModelInfo {
        id: "mimo-v2.5-pro-ultraspeed",
        vendor: "xiaomi",
        context_window: 1048576,
    },
    ModelInfo {
        id: "qwen-omni-turbo-realtime",
        vendor: "qwen",
        context_window: 32768,
    },
    ModelInfo {
        id: "qwen2-5-math-7b-instruct",
        vendor: "qwen",
        context_window: 4096,
    },
    ModelInfo {
        id: "qwen2-5-vl-72b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "MiniMax-M2.5-highspeed",
        vendor: "minimax",
        context_window: 204800,
    },
    ModelInfo {
        id: "MiniMax-M2.7-highspeed",
        vendor: "minimax",
        context_window: 204800,
    },
    ModelInfo {
        id: "qwen-plus-character-ja",
        vendor: "qwen",
        context_window: 8192,
    },
    ModelInfo {
        id: "qwen2-5-vl-7b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen2-5-14b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen2-5-32b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen2-5-72b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "gpt-5.2-chat-latest",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "gpt-5.3-chat-latest",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "gpt-5.3-codex-spark",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "qwen-plus-character",
        vendor: "qwen",
        context_window: 32768,
    },
    ModelInfo {
        id: "qwen2-5-7b-instruct",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen3.6-max-preview",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen-deep-research",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3-vl-235b-a22b",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "claude-sonnet-4-5",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "deepseek-v4-flash",
        vendor: "deepseek",
        context_window: 1000000,
    },
    ModelInfo {
        id: "gpt-4o-2024-05-13",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "gpt-4o-2024-08-06",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "gpt-4o-2024-11-20",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "qwen3-coder-flash",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3.5-122b-a10b",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen3.5-397b-a17b",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "claude-haiku-4-5",
        vendor: "anthropic",
        context_window: 200000,
    },
    ModelInfo {
        id: "gpt-realtime-2.1",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "qwen3-coder-plus",
        vendor: "qwen",
        context_window: 1048576,
    },
    ModelInfo {
        id: "qwen3-omni-flash",
        vendor: "qwen",
        context_window: 65536,
    },
    ModelInfo {
        id: "qwen3-vl-30b-a3b",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "claude-opus-4-5",
        vendor: "anthropic",
        context_window: 200000,
    },
    ModelInfo {
        id: "claude-opus-4-6",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "claude-opus-4-7",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "claude-opus-4-8",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "claude-sonnet-5",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "deepseek-v4-pro",
        vendor: "deepseek",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen-math-turbo",
        vendor: "qwen",
        context_window: 4096,
    },
    ModelInfo {
        id: "qwen-omni-turbo",
        vendor: "qwen",
        context_window: 32768,
    },
    ModelInfo {
        id: "qwen2-5-omni-7b",
        vendor: "qwen",
        context_window: 32768,
    },
    ModelInfo {
        id: "qwen3-235b-a22b",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen3.5-35b-a3b",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen3.6-35b-a3b",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "claude-fable-5",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "glm-4.7-flashx",
        vendor: "zai",
        context_window: 200000,
    },
    ModelInfo {
        id: "qwen-doc-turbo",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen-math-plus",
        vendor: "qwen",
        context_window: 4096,
    },
    ModelInfo {
        id: "claude-opus-5",
        vendor: "anthropic",
        context_window: 1000000,
    },
    ModelInfo {
        id: "glm-4.5-flash",
        vendor: "zai",
        context_window: 131072,
    },
    ModelInfo {
        id: "glm-4.7-flash",
        vendor: "zai",
        context_window: 200000,
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "gpt-5.6-terra",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "mimo-v2-flash",
        vendor: "xiaomi",
        context_window: 262144,
    },
    ModelInfo {
        id: "mimo-v2.5-pro",
        vendor: "xiaomi",
        context_window: 1048576,
    },
    ModelInfo {
        id: "qwen3-vl-plus",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen3.5-flash",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3.6-flash",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3.7-flash",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "MiniMax-M2.1",
        vendor: "minimax",
        context_window: 204800,
    },
    ModelInfo {
        id: "MiniMax-M2.5",
        vendor: "minimax",
        context_window: 204800,
    },
    ModelInfo {
        id: "MiniMax-M2.7",
        vendor: "minimax",
        context_window: 204800,
    },
    ModelInfo {
        id: "glm-5v-turbo",
        vendor: "zai",
        context_window: 200000,
    },
    ModelInfo {
        id: "gpt-4.1-mini",
        vendor: "openai",
        context_window: 1047576,
    },
    ModelInfo {
        id: "gpt-4.1-nano",
        vendor: "openai",
        context_window: 1047576,
    },
    ModelInfo {
        id: "gpt-5.4-mini",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "gpt-5.4-nano",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "gpt-5.6-luna",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "mimo-v2-omni",
        vendor: "xiaomi",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen-vl-plus",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen3.5-plus",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3.6-plus",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3.7-plus",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "glm-4.5-air",
        vendor: "zai",
        context_window: 131072,
    },
    ModelInfo {
        id: "glm-5-turbo",
        vendor: "zai",
        context_window: 200000,
    },
    ModelInfo {
        id: "gpt-4-turbo",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "gpt-4o-mini",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "gpt-5.2-pro",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "gpt-5.4-pro",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "gpt-5.5-pro",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "gpt-5.6-sol",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "mimo-v2-pro",
        vendor: "xiaomi",
        context_window: 1048576,
    },
    ModelInfo {
        id: "qwen-vl-max",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen3.5-27b",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen3.6-27b",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "qwen3.7-max",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3.8-max",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "MiniMax-M2",
        vendor: "minimax",
        context_window: 196608,
    },
    ModelInfo {
        id: "MiniMax-M3",
        vendor: "minimax",
        context_window: 1000000,
    },
    ModelInfo {
        id: "gpt-5-mini",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "gpt-5-nano",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "qwen-flash",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen-turbo",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "gpt-5-pro",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "mimo-v2.5",
        vendor: "xiaomi",
        context_window: 1048576,
    },
    ModelInfo {
        id: "qwen-long",
        vendor: "qwen",
        context_window: 10000000,
    },
    ModelInfo {
        id: "qwen-plus",
        vendor: "qwen",
        context_window: 1000000,
    },
    ModelInfo {
        id: "qwen3-14b",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen3-32b",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwen3-max",
        vendor: "qwen",
        context_window: 262144,
    },
    ModelInfo {
        id: "glm-4.5v",
        vendor: "zai",
        context_window: 64000,
    },
    ModelInfo {
        id: "glm-4.6v",
        vendor: "zai",
        context_window: 128000,
    },
    ModelInfo {
        id: "qwen-max",
        vendor: "qwen",
        context_window: 32768,
    },
    ModelInfo {
        id: "qwen3-8b",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwq-plus",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "glm-4.5",
        vendor: "zai",
        context_window: 131072,
    },
    ModelInfo {
        id: "glm-4.6",
        vendor: "zai",
        context_window: 204800,
    },
    ModelInfo {
        id: "glm-4.7",
        vendor: "zai",
        context_window: 204800,
    },
    ModelInfo {
        id: "glm-5.1",
        vendor: "zai",
        context_window: 200000,
    },
    ModelInfo {
        id: "glm-5.2",
        vendor: "zai",
        context_window: 1000000,
    },
    ModelInfo {
        id: "glm-5.3",
        vendor: "zai",
        context_window: 1000000,
    },
    ModelInfo {
        id: "gpt-4.1",
        vendor: "openai",
        context_window: 1047576,
    },
    ModelInfo {
        id: "gpt-5.1",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "gpt-5.2",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "gpt-5.4",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "gpt-5.5",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "gpt-5.6",
        vendor: "openai",
        context_window: 1050000,
    },
    ModelInfo {
        id: "o3-mini",
        vendor: "openai",
        context_window: 200000,
    },
    ModelInfo {
        id: "o4-mini",
        vendor: "openai",
        context_window: 200000,
    },
    ModelInfo {
        id: "qvq-max",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "qwq-32b",
        vendor: "qwen",
        context_window: 131072,
    },
    ModelInfo {
        id: "gpt-4o",
        vendor: "openai",
        context_window: 128000,
    },
    ModelInfo {
        id: "o1-pro",
        vendor: "openai",
        context_window: 200000,
    },
    ModelInfo {
        id: "o3-pro",
        vendor: "openai",
        context_window: 200000,
    },
    ModelInfo {
        id: "glm-5",
        vendor: "zai",
        context_window: 204800,
    },
    ModelInfo {
        id: "gpt-4",
        vendor: "openai",
        context_window: 8192,
    },
    ModelInfo {
        id: "gpt-5",
        vendor: "openai",
        context_window: 400000,
    },
    ModelInfo {
        id: "o1",
        vendor: "openai",
        context_window: 200000,
    },
    ModelInfo {
        id: "o3",
        vendor: "openai",
        context_window: 200000,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_exact_and_prefix() {
        assert_eq!(context_window("MiniMax-M2.7"), Some(204_800));
        assert_eq!(context_window("claude-opus-4-7"), Some(1_000_000));
        // Date-suffixed variant resolves through the base id.
        assert_eq!(context_window("claude-opus-4-7-20260101"), Some(1_000_000));
        assert_eq!(context_window("no-such-model"), None);
    }

    /// Longest-first ordering is load-bearing: with `gpt-5` earlier in
    /// the slice, `gpt-5.4` would match it and report the wrong window.
    #[test]
    fn longer_ids_win_over_shorter_prefixes() {
        assert_ne!(context_window("gpt-5"), context_window("gpt-5.4"));
        assert_eq!(context_window("gpt-5"), Some(400_000));
        assert_eq!(context_window("gpt-5.4"), Some(1_050_000));
        for pair in MODELS.windows(2) {
            assert!(
                pair[0].id.len() >= pair[1].id.len(),
                "MODELS must stay longest-first: {:?} before {:?}",
                pair[0].id,
                pair[1].id
            );
        }
    }

    /// The regression this table exists for: MiniMax must be offered
    /// when the MiniMax vendor is active.
    #[test]
    fn every_shipped_vendor_has_models() {
        for v in [
            "anthropic",
            "openai",
            "deepseek",
            "minimax",
            "qwen",
            "xiaomi",
            "zai",
        ] {
            let models = models_for_vendor(v);
            assert!(!models.is_empty(), "vendor {v} has no models");
            assert!(models.iter().all(|m| m.vendor == v));
        }
        let mm = models_for_vendor("minimax");
        assert!(mm.iter().any(|m| m.id == "MiniMax-M2.7"), "{mm:?}");
        assert!(mm.iter().any(|m| m.id == "MiniMax-M3"), "{mm:?}");
        // `fallback` is a real vendor id with no catalogue — must not panic.
        assert!(models_for_vendor("fallback").is_empty());
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_234), "1.2k");
        assert_eq!(fmt_tokens(1_234_567), "1.2M");
    }
}

#[cfg(test)]
mod picker_integration {
    use super::*;

    /// End-to-end of the reported bug: with the MiniMax vendor active,
    /// the ids the `/model` picker offers must include MiniMax models
    /// and must NOT include other vendors' models.
    #[test]
    fn minimax_vendor_offers_minimax_models() {
        let v = crate::vendor::pick_vendor(Some("minimax"), None, "MiniMax-M2.7");
        assert_eq!(v.id(), "minimax");
        let ids: Vec<&str> = models_for_vendor(v.id()).iter().map(|m| m.id).collect();
        assert!(ids.contains(&"MiniMax-M2.7"), "{ids:?}");
        assert!(ids.contains(&"MiniMax-M3"), "{ids:?}");
        assert!(!ids.iter().any(|i| i.starts_with("claude-")), "{ids:?}");
        assert!(!ids.iter().any(|i| i.starts_with("gpt-")), "{ids:?}");
    }

    /// Sniffing by base_url alone (no explicit provider) must also land
    /// on MiniMax — that is how a config with only base_url behaves.
    #[test]
    fn minimax_sniffed_from_base_url() {
        let v = crate::vendor::pick_vendor(None, Some("https://api.minimax.chat/anthropic"), "x");
        assert_eq!(v.id(), "minimax");
        assert!(!models_for_vendor(v.id()).is_empty());
    }

    /// Context window now resolves for MiniMax, so the status bar's
    /// context-% actually renders instead of showing "?%".
    #[test]
    fn minimax_context_window_known() {
        assert_eq!(context_window("MiniMax-M2.7"), Some(204_800));
        assert_eq!(context_window("MiniMax-M3"), Some(1_000_000));
    }
}

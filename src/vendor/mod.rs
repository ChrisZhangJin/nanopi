//! Vendor dispatch layer (spec §2.4–§2.6).

use crate::agent::thinking::ThinkingLevel;
use crate::provider::ApiKind;
use serde_json::{json, Value};

pub mod anthropic;
pub mod deepseek;
pub mod fallback;
pub mod minimax;
pub mod openai;
pub mod qwen;
pub mod xiaomi;
pub mod zai;

pub use anthropic::AnthropicVendor;
pub use deepseek::DeepSeekVendor;
pub use fallback::FallbackVendor;
pub use minimax::MinimaxVendor;
pub use openai::OpenAiVendor;
pub use qwen::QwenVendor;
pub use xiaomi::XiaomiVendor;
pub use zai::ZaiVendor;

pub trait Vendor: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &'static str;
    /// The vendor's native/primary wire protocol, independent of URL.
    fn transport(&self) -> ApiKind;
    /// The protocol to actually speak to `base_url`.
    ///
    /// Several vendors (Xiaomi MiMo, z.ai, DeepSeek, dashscope) serve
    /// both protocols off one host and split them by path: `/v1` for
    /// OpenAI-compat, `/anthropic` for Anthropic-native. An
    /// OpenAI-transport vendor pointed at the `/anthropic` surface must
    /// switch, or every request lands on `/anthropic/chat/completions`
    /// and the gateway answers 404. Anthropic-transport vendors are
    /// unaffected — they're already on the right wire.
    fn transport_for(&self, base_url: &str) -> ApiKind {
        match self.transport() {
            ApiKind::Openai if url_is_anthropic_surface(base_url) => ApiKind::Anthropic,
            native => native,
        }
    }
    fn default_base_url(&self) -> Option<&'static str> { None }
    fn supports_thinking(&self, _model: &str) -> bool { false }
    fn write_thinking(&self, body: &mut Value, level: ThinkingLevel, _model: &str) {
        body["reasoning_effort"] = json!(openai_effort_string(level));
    }
}

/// True when a base_url addresses a gateway's Anthropic-protocol
/// surface, i.e. the path component is (or contains) `/anthropic`:
/// `token-plan-cn.xiaomimimo.com/anthropic`, `api.z.ai/api/anthropic`,
/// `api.minimax.chat/anthropic`.
///
/// Host-only URLs like `https://api.anthropic.com` deliberately do NOT
/// match — the name is in the host, not the path, and those vendors
/// already declare an Anthropic transport.
pub fn url_is_anthropic_surface(base_url: &str) -> bool {
    let u = base_url.trim_end_matches('/').to_ascii_lowercase();
    u.ends_with("/anthropic") || u.contains("/anthropic/")
}

pub fn openai_effort_string(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
    }
}

pub fn pick_vendor(
    cfg_provider: Option<&str>,
    base_url: Option<&str>,
    model: &str,
) -> Box<dyn Vendor> {
    if let Some(p) = cfg_provider {
        match p.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => return Box::new(AnthropicVendor),
            "openai" | "gpt" => return Box::new(OpenAiVendor),
            "deepseek" => return Box::new(DeepSeekVendor),
            "zai" | "z.ai" => return Box::new(ZaiVendor),
            "qwen" | "dashscope" => return Box::new(QwenVendor),
            "minimax" => return Box::new(MinimaxVendor),
            "xiaomi" | "xiaomimimo" => return Box::new(XiaomiVendor),
            "fallback" => return Box::new(FallbackVendor),
            _ => crate::note!("nanopi: unknown provider `{}` — falling through to sniff", p),
        }
    }
    if let Some(url) = base_url {
        let u = url.to_ascii_lowercase();
        if u.contains("api.deepseek.com") { return Box::new(DeepSeekVendor); }
        if u.contains("api.z.ai") { return Box::new(ZaiVendor); }
        if u.contains("dashscope") || u.contains("qwen") { return Box::new(QwenVendor); }
        if u.contains("minimax") { return Box::new(MinimaxVendor); }
        if u.contains("xiaomimimo") { return Box::new(XiaomiVendor); }
        if u.contains("anthropic.com") { return Box::new(AnthropicVendor); }
    }
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude-") { return Box::new(AnthropicVendor); }
    if m.starts_with("gpt-") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("chatgpt-") {
        return Box::new(OpenAiVendor);
    }
    if m.starts_with("deepseek-") { return Box::new(DeepSeekVendor); }
    if m.starts_with("glm-") { return Box::new(ZaiVendor); }
    if m.starts_with("qwen") || m.starts_with("qwq-") { return Box::new(QwenVendor); }
    if m.starts_with("minimax-") { return Box::new(MinimaxVendor); }
    Box::new(FallbackVendor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_vendor_is_openai_transport_no_thinking() {
        let v = FallbackVendor;
        assert_eq!(v.id(), "fallback");
        assert_eq!(v.transport(), ApiKind::Openai);
        assert!(!v.supports_thinking("anything"));
    }

    #[test]
    fn url_is_anthropic_surface_matches_path_not_host() {
        // Path-based dual-protocol gateways.
        assert!(url_is_anthropic_surface(
            "https://token-plan-cn.xiaomimimo.com/anthropic"
        ));
        assert!(url_is_anthropic_surface(
            "https://token-plan-sgp.xiaomimimo.com/anthropic/"
        ));
        assert!(url_is_anthropic_surface("https://api.z.ai/api/anthropic"));
        assert!(url_is_anthropic_surface("HTTPS://X/ANTHROPIC"));
        // Host-only: the vendor already declares Anthropic transport,
        // and matching here would be a coincidence of the domain name.
        assert!(!url_is_anthropic_surface("https://api.anthropic.com"));
        assert!(!url_is_anthropic_surface("https://api.anthropic.com/"));
        // OpenAI-compat surfaces.
        assert!(!url_is_anthropic_surface(
            "https://token-plan-cn.xiaomimimo.com/v1"
        ));
        assert!(!url_is_anthropic_surface("https://api.openai.com/v1"));
    }

    #[test]
    fn transport_for_switches_openai_vendors_on_anthropic_surface() {
        let v = XiaomiVendor;
        assert_eq!(v.transport(), ApiKind::Openai, "native transport unchanged");
        assert_eq!(
            v.transport_for("https://token-plan-cn.xiaomimimo.com/anthropic"),
            ApiKind::Anthropic
        );
        assert_eq!(
            v.transport_for("https://token-plan-cn.xiaomimimo.com/v1"),
            ApiKind::Openai
        );
        // Anthropic-native vendors are unaffected either way.
        assert_eq!(
            MinimaxVendor.transport_for("https://api.minimax.chat/v1"),
            ApiKind::Anthropic
        );
        assert_eq!(
            AnthropicVendor.transport_for("https://api.anthropic.com"),
            ApiKind::Anthropic
        );
    }

    #[test]
    fn openai_vendor_write_thinking_emits_reasoning_effort_high() {
        let v = OpenAiVendor;
        let mut body = json!({});
        v.write_thinking(&mut body, ThinkingLevel::High, "gpt-5");
        assert_eq!(body, json!({"reasoning_effort": "high"}));
    }

    #[test]
    fn openai_vendor_supports_thinking_for_o1_o3_gpt5() {
        let v = OpenAiVendor;
        assert!(v.supports_thinking("o1-mini"));
        assert!(v.supports_thinking("o3-mini"));
        assert!(v.supports_thinking("gpt-5"));
        assert!(!v.supports_thinking("gpt-4o"));
    }

    #[test]
    fn effort_string_clamps_xhigh_max_to_high() {
        assert_eq!(openai_effort_string(ThinkingLevel::Xhigh), "high");
        assert_eq!(openai_effort_string(ThinkingLevel::Max), "high");
    }

    #[test]
    fn effort_string_minimal_low_collapse_to_low() {
        assert_eq!(openai_effort_string(ThinkingLevel::Minimal), "low");
        assert_eq!(openai_effort_string(ThinkingLevel::Low), "low");
    }

    #[test]
    fn anthropic_vendor_write_thinking_full_shape_high() {
        let v = AnthropicVendor;
        let mut body = json!({});
        v.write_thinking(&mut body, ThinkingLevel::High, "claude-opus-4-7");
        assert_eq!(body, json!({
            "thinking": {"type": "enabled", "budget_tokens": 16384},
            "max_tokens": 20480,
        }));
    }

    #[test]
    fn deepseek_vendor_write_thinking_dual_shape_high() {
        let v = DeepSeekVendor;
        let mut body = json!({});
        v.write_thinking(&mut body, ThinkingLevel::High, "deepseek-reasoner");
        assert_eq!(body, json!({
            "thinking": {"type": "enabled"},
            "reasoning_effort": "high",
        }));
    }

    #[test]
    fn zai_vendor_write_thinking_clear_thinking_false() {
        let v = ZaiVendor;
        let mut body = json!({});
        v.write_thinking(&mut body, ThinkingLevel::High, "glm-4.6");
        assert_eq!(body, json!({
            "thinking": {"type": "enabled", "clear_thinking": false},
            "reasoning_effort": "high",
        }));
    }

    #[test]
    fn qwen_vendor_write_thinking_enable_thinking_flag() {
        let v = QwenVendor;
        let mut body = json!({});
        v.write_thinking(&mut body, ThinkingLevel::High, "qwen3-max-thinking");
        assert_eq!(body, json!({
            "enable_thinking": true,
            "reasoning_effort": "high",
        }));
    }

    #[test]
    fn minimax_vendor_uses_anthropic_transport_no_max_tokens() {
        let v = MinimaxVendor;
        assert_eq!(v.transport(), ApiKind::Anthropic);
        let mut body = json!({});
        v.write_thinking(&mut body, ThinkingLevel::High, "MiniMax-M2");
        assert_eq!(body, json!({
            "thinking": {"type": "enabled", "budget_tokens": 16384},
        }));
    }

    #[test]
    fn xiaomi_vendor_default_reasoning_effort() {
        let v = XiaomiVendor;
        let mut body = json!({});
        v.write_thinking(&mut body, ThinkingLevel::High, "xiaomi-thinking");
        assert_eq!(body, json!({"reasoning_effort": "high"}));
    }

    #[test]
    fn pick_vendor_explicit_provider_string_wins() {
        assert_eq!(pick_vendor(Some("deepseek"), None, "gpt-4o").id(), "deepseek");
        assert_eq!(pick_vendor(Some("anthropic"), None, "gpt-4o").id(), "anthropic");
        assert_eq!(pick_vendor(Some("zai"), None, "gpt-4o").id(), "zai");
    }

    #[test]
    fn pick_vendor_unknown_provider_string_falls_through() {
        assert_eq!(pick_vendor(Some("nonsense"), None, "claude-opus-4-7").id(), "anthropic");
    }

    #[test]
    fn pick_vendor_base_url_domain_sniff() {
        assert_eq!(pick_vendor(None, Some("https://api.deepseek.com"), "m").id(), "deepseek");
        assert_eq!(pick_vendor(None, Some("https://api.z.ai/api/paas/v4"), "m").id(), "zai");
        assert_eq!(pick_vendor(None, Some("https://dashscope.aliyuncs.com/x"), "m").id(), "qwen");
        assert_eq!(pick_vendor(None, Some("https://api.minimax.chat/anthropic"), "m").id(), "minimax");
        assert_eq!(pick_vendor(None, Some("https://api.minimaxi.com/anthropic"), "m").id(), "minimax");
        assert_eq!(pick_vendor(None, Some("https://xiaomimimo.example.com"), "m").id(), "xiaomi");
        assert_eq!(pick_vendor(None, Some("https://api.anthropic.com"), "m").id(), "anthropic");
    }

    #[test]
    fn pick_vendor_model_prefix_when_base_url_missing() {
        assert_eq!(pick_vendor(None, None, "claude-opus-4-7").id(), "anthropic");
        assert_eq!(pick_vendor(None, None, "gpt-4o").id(), "openai");
        assert_eq!(pick_vendor(None, None, "o3-mini").id(), "openai");
        assert_eq!(pick_vendor(None, None, "deepseek-reasoner").id(), "deepseek");
        assert_eq!(pick_vendor(None, None, "glm-4.6").id(), "zai");
        assert_eq!(pick_vendor(None, None, "qwen3-max").id(), "qwen");
        assert_eq!(pick_vendor(None, None, "qwq-32b").id(), "qwen");
        assert_eq!(pick_vendor(None, None, "MiniMax-M2").id(), "minimax");
    }

    #[test]
    fn pick_vendor_no_signal_returns_fallback() {
        assert_eq!(pick_vendor(None, None, "some-random-model").id(), "fallback");
    }
}

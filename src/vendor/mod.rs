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
    /// True when this vendor serves BOTH protocols off one host, split
    /// by base_url path — `/anthropic` for Anthropic-native, anything
    /// else (`/v1`, `/api/paas/v4`) for OpenAI-compat.
    ///
    /// For these the PATH decides the protocol outright; `transport()`
    /// is only the default surface, not a constraint. Vendors that
    /// speak one protocol at any path (api.openai.com,
    /// api.anthropic.com, api.deepseek.com) leave this false.
    fn dual_surface(&self) -> bool {
        false
    }

    /// The protocol to actually speak to `base_url`.
    ///
    /// Dual-surface vendors are decided by the path, in BOTH
    /// directions. Getting only one direction right was a bug: the
    /// match below rerouted an OpenAI-transport vendor onto Anthropic
    /// when the URL said `/anthropic`, but an Anthropic-transport
    /// vendor stayed Anthropic no matter what the URL said. So MiniMax
    /// — whose native surface is `/anthropic` — reported Anthropic even
    /// for `https://api.minimaxi.com/v1`, and a correct, self-consistent
    /// `base_url = ".../v1"` + `api_kind = "openai"` was flagged as
    /// contradicting the vendor.
    ///
    /// Asymmetry was never justified. A host that splits protocols by
    /// path splits them both ways.
    fn transport_for(&self, base_url: &str) -> ApiKind {
        if self.dual_surface() {
            if url_is_anthropic_surface(base_url) {
                return ApiKind::Anthropic;
            }
            // Only a URL that actually HAS a path says anything. A bare
            // host (`https://api.minimaxi.com`) discriminates nothing,
            // so fall through to the vendor's default surface —
            // treating it as OpenAI would send `provider = "minimax"`
            // with no path straight into a 404.
            if url_has_path(base_url) {
                return ApiKind::Openai;
            }
            return self.transport();
        }
        match self.transport() {
            // Kept for the fallback vendor and any single-protocol
            // vendor pointed at an unexpected `/anthropic` surface:
            // better to switch than to POST `/anthropic/chat/completions`.
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

/// True when `base_url` carries a path component beyond the host —
/// `https://h/v1` yes, `https://h` and `https://h/` no.
///
/// For a dual-surface vendor the path is what names the protocol
/// surface, so its absence is not "the OpenAI one", it is "unstated".
pub fn url_has_path(base_url: &str) -> bool {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    match after_scheme.split_once('/') {
        Some((_host, path)) => !path.trim_matches('/').is_empty(),
        None => false,
    }
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

    /// The reported false positive: `base_url = "https://api.minimaxi.com/v1"`
    /// with `api_kind = "openai"` is a correct, self-consistent config —
    /// MiniMax serves an OpenAI-compatible API at `/v1` — and startup
    /// warned that it contradicted the vendor. It warned because
    /// `transport_for` returned the vendor's native Anthropic for ANY
    /// URL, so the comparison in `main.rs` could never agree.
    #[test]
    fn a_dual_surface_vendor_agrees_with_a_matching_openai_url() {
        let base = "https://api.minimaxi.com/v1";
        let v = pick_vendor(None, Some(base), "minimax-M3");
        assert_eq!(v.id(), "minimax", "sanity: the sniff still finds MiniMax");
        assert_eq!(
            v.transport_for(base),
            ApiKind::Openai,
            "an explicit /v1 URL is the OpenAI surface — this is what \
             main.rs compares api_kind against, so disagreeing here is \
             exactly the spurious warning"
        );
        // And the other pairing is equally valid, with no warning.
        let anthropic_base = "https://api.minimaxi.com/anthropic";
        assert_eq!(
            pick_vendor(None, Some(anthropic_base), "minimax-M3").transport_for(anthropic_base),
            ApiKind::Anthropic
        );
    }

    /// A genuine contradiction must STILL warn — the whole point of the
    /// check is that guessing wrong yields a bare 404 downstream.
    #[test]
    fn a_real_surface_mismatch_still_disagrees() {
        // `/anthropic` path but the user insists on OpenAI.
        let base = "https://api.minimaxi.com/anthropic";
        assert_ne!(
            pick_vendor(None, Some(base), "minimax-M3").transport_for(base),
            ApiKind::Openai
        );
        // A `/v1` path with `api_kind = "anthropic"` would POST
        // `/v1/messages` — also wrong, also worth a warning.
        let base = "https://api.minimaxi.com/v1";
        assert_ne!(
            pick_vendor(None, Some(base), "minimax-M3").transport_for(base),
            ApiKind::Anthropic
        );
    }

    /// A bare host discriminates nothing, so it must fall back to the
    /// vendor's default surface rather than being read as "not
    /// /anthropic, therefore OpenAI" — which would 404 every
    /// `provider = "minimax"` config that omits the path.
    #[test]
    fn a_bare_host_falls_back_to_the_vendors_default_surface() {
        assert_eq!(
            MinimaxVendor.transport_for("https://api.minimaxi.com"),
            ApiKind::Anthropic
        );
        assert_eq!(
            MinimaxVendor.transport_for("https://api.minimaxi.com/"),
            ApiKind::Anthropic
        );
        // Xiaomi's default surface is the OpenAI one.
        assert_eq!(
            XiaomiVendor.transport_for("https://token-plan-cn.xiaomimimo.com"),
            ApiKind::Openai
        );
    }

    #[test]
    fn url_has_path_distinguishes_bare_hosts() {
        assert!(!url_has_path("https://h"));
        assert!(!url_has_path("https://h/"));
        assert!(!url_has_path("h"));
        assert!(url_has_path("https://h/v1"));
        assert!(url_has_path("https://h/anthropic"));
        assert!(url_has_path("https://h/api/paas/v4"));
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
        // MiniMax is dual-surface too, so the path decides for it in
        // BOTH directions. This assertion used to read `Anthropic` —
        // it encoded the bug: an Anthropic-transport vendor ignored a
        // `/v1` URL entirely.
        assert_eq!(
            MinimaxVendor.transport_for("https://api.minimax.chat/v1"),
            ApiKind::Openai
        );
        // A single-protocol vendor is unaffected: api.anthropic.com
        // speaks Anthropic at every path, so there is nothing for a
        // path to discriminate.
        assert_eq!(
            AnthropicVendor.transport_for("https://api.anthropic.com"),
            ApiKind::Anthropic
        );
        assert_eq!(
            AnthropicVendor.transport_for("https://api.anthropic.com/v1"),
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

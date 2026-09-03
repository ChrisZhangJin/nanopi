//! LLM provider adapters — OpenAI-compatible + Anthropic-native.

pub mod anthropic;
pub mod openai;
pub mod retry;
pub mod sse;
pub mod think_tags;

use crate::agent::loop_::Provider;

/// Wire-protocol kind. Determines which Provider impl talks to
/// `base_url`. Parsed from `api_kind` in config / `--api-kind` CLI
/// flag; unrecognized inputs collapse to `Openai` (the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    /// OpenAI's chat/completions API — endpoint suffix
    /// `/chat/completions`, works for OpenAI itself plus every
    /// gateway (oneapi, newapi, litellm, DeepSeek, Groq, …) that
    /// exposes the OpenAI-compat surface.
    Openai,
    /// Anthropic's native messages API — endpoint suffix
    /// `/v1/messages`. Use for Anthropic direct or a proxy that
    /// speaks the Anthropic protocol natively.
    Anthropic,
}

impl ApiKind {
    /// Best-effort parse from a config string. `None` and unknown
    /// values → `Openai` so an empty config stays working.
    pub fn from_config(s: Option<&str>) -> Self {
        Self::from_config_opt(s).unwrap_or(ApiKind::Openai)
    }

    /// Like `from_config`, but preserves "the user didn't say" as
    /// `None` instead of collapsing it to `Openai`.
    ///
    /// That distinction is load-bearing: `build` only lets a vendor
    /// override the wire protocol when the user was silent. Absent it,
    /// an explicit `api_kind = "anthropic"` was indistinguishable from
    /// the default and got discarded by the vendor sniff — which is how
    /// `base_url = ".../anthropic"` ended up POSTing to
    /// `.../anthropic/chat/completions` and 404ing.
    ///
    /// Unknown strings are *not* explicit (they're typos), so they also
    /// yield `None` and fall through to the vendor.
    pub fn from_config_opt(s: Option<&str>) -> Option<Self> {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("anthropic") | Some("claude") => Some(ApiKind::Anthropic),
            Some("openai") | Some("gpt") => Some(ApiKind::Openai),
            _ => None,
        }
    }
}

/// Resolve the wire protocol a request will actually use.
///
/// Precedence: an explicit `api_kind` (config or `--api-kind`) wins
/// outright; otherwise the vendor picks, given the base_url; with no
/// vendor at all we default to OpenAI-compat.
///
/// Exposed separately from `build` so the startup banner can announce
/// the protocol we're *really* about to speak rather than the one the
/// config asked for.
pub fn effective_kind(
    configured: Option<ApiKind>,
    vendor: Option<&dyn crate::vendor::Vendor>,
    base_url: &str,
) -> ApiKind {
    match (configured, vendor) {
        (Some(k), _) => k,
        (None, Some(v)) => v.transport_for(base_url),
        (None, None) => ApiKind::Openai,
    }
}

/// Build a Provider trait object for the given wire kind. Callers
/// hand the returned Box straight to Agent.provider.
///
/// v0.9.3: `vendor` (from `crate::vendor::pick_vendor`) governs
/// thinking-block / reasoning_effort emission. Pass `None` for the
/// legacy behavior (Anthropic emits its own thinking; OpenAI omits
/// reasoning params).
///
/// `kind` is `None` when neither config nor CLI named a protocol — only
/// then does the vendor get to choose the transport (that's what keeps
/// MinimaxVendor's Anthropic-only gateway working out of the box, while
/// still honoring an explicit `api_kind`).
/// `inline_think_tags` is `Config::inline_think_tags` — the escape hatch
/// for `Vendor::inlines_think_tags`. `None` defers to the vendor;
/// `Some(_)` is final. Only meaningful for the OpenAI-compat transport;
/// ignored for Anthropic (which already gets reasoning via its own
/// `thinking` block).
pub fn build(
    kind: Option<ApiKind>,
    base_url: &str,
    api_key: &str,
    model: &str,
    vendor: Option<Box<dyn crate::vendor::Vendor>>,
    inline_think_tags: Option<bool>,
) -> Box<dyn Provider> {
    let effective = effective_kind(kind, vendor.as_deref(), base_url);
    match effective {
        ApiKind::Openai => {
            let p = openai::OpenAiProvider::new(base_url, api_key, model);
            let p = match vendor {
                Some(v) => p.with_vendor(v),
                None => p,
            };
            let p = match inline_think_tags {
                Some(on) => p.with_inline_think(on),
                None => p,
            };
            Box::new(p)
        }
        ApiKind::Anthropic => {
            let p = anthropic::AnthropicProvider::new(base_url, api_key, model);
            Box::new(match vendor {
                Some(v) => p.with_vendor(v),
                None => p,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_kind_parse_defaults_to_openai() {
        assert_eq!(ApiKind::from_config(None), ApiKind::Openai);
        assert_eq!(ApiKind::from_config(Some("")), ApiKind::Openai);
        assert_eq!(ApiKind::from_config(Some("nonsense")), ApiKind::Openai);
        assert_eq!(ApiKind::from_config(Some("openai")), ApiKind::Openai);
        assert_eq!(ApiKind::from_config(Some("OPENAI")), ApiKind::Openai);
    }

    #[test]
    fn api_kind_parse_recognizes_anthropic() {
        assert_eq!(ApiKind::from_config(Some("anthropic")), ApiKind::Anthropic);
        assert_eq!(ApiKind::from_config(Some("Anthropic")), ApiKind::Anthropic);
        assert_eq!(ApiKind::from_config(Some("claude")), ApiKind::Anthropic);
    }

    #[test]
    fn from_config_opt_preserves_unset_and_rejects_typos() {
        assert_eq!(ApiKind::from_config_opt(None), None);
        assert_eq!(ApiKind::from_config_opt(Some("")), None);
        assert_eq!(ApiKind::from_config_opt(Some("anthropik")), None);
        assert_eq!(
            ApiKind::from_config_opt(Some(" Anthropic ")),
            Some(ApiKind::Anthropic)
        );
        assert_eq!(ApiKind::from_config_opt(Some("openai")), Some(ApiKind::Openai));
    }

    /// When config is SILENT, a vendor's transport wins. Regression for
    /// the MiniMax 401: the Minimax vendor exposes the Anthropic
    /// protocol (`api.minimax.chat/anthropic`), so with no `api_kind`
    /// set we must still build an AnthropicProvider — otherwise the
    /// gateway rejects the request with "Please carry the API secret key
    /// in the 'X-Api-Key' field of the request header".
    #[test]
    fn vendor_transport_wins_when_config_is_silent() {
        let v: Box<dyn crate::vendor::Vendor> = Box::new(crate::vendor::MinimaxVendor);
        let p = build(None, "https://x", "k", "minimax-M3", Some(v), None);
        assert_eq!(p.id(), "anthropic");

        // Vendor agrees with config → no surprise, still Anthropic.
        let v: Box<dyn crate::vendor::Vendor> = Box::new(crate::vendor::MinimaxVendor);
        let p = build(Some(ApiKind::Anthropic), "https://x", "k", "minimax-M3", Some(v), None);
        assert_eq!(p.id(), "anthropic");

        // No vendor, no config → OpenAI-compat default.
        let p = build(None, "https://x", "k", "gpt-4o", None, None);
        assert_eq!(p.id(), "openai");
    }

    /// The MiMo 404. `api_kind = "anthropic"` + a `/anthropic` base_url
    /// used to be overridden by XiaomiVendor's OpenAI transport, sending
    /// the request to `…/anthropic/chat/completions` (openresty 404)
    /// while the startup banner advertised `…/anthropic/v1/messages`.
    #[test]
    fn explicit_api_kind_survives_the_vendor_sniff() {
        let base = "https://token-plan-cn.xiaomimimo.com/anthropic";
        let v = crate::vendor::pick_vendor(None, Some(base), "mimo-v2.5");
        assert_eq!(v.id(), "xiaomi", "base_url sniff still picks Xiaomi");
        let p = build(Some(ApiKind::Anthropic), base, "k", "mimo-v2.5", Some(v), None);
        assert_eq!(p.id(), "anthropic");
    }

    /// Same endpoint with NO api_kind at all: the vendor must notice the
    /// `/anthropic` surface on its own rather than defaulting to
    /// `/chat/completions`.
    #[test]
    fn anthropic_surface_url_switches_transport_without_config() {
        for base in [
            "https://token-plan-cn.xiaomimimo.com/anthropic",
            "https://token-plan-sgp.xiaomimimo.com/anthropic/",
        ] {
            let v = crate::vendor::pick_vendor(None, Some(base), "mimo-v2.5-pro");
            let p = build(None, base, "k", "mimo-v2.5-pro", Some(v), None);
            assert_eq!(p.id(), "anthropic", "{base} should route Anthropic-native");
        }

        // The OpenAI-compat surface on the same host stays OpenAI.
        let base = "https://token-plan-cn.xiaomimimo.com/v1";
        let v = crate::vendor::pick_vendor(None, Some(base), "mimo-v2.5-pro");
        let p = build(None, base, "k", "mimo-v2.5-pro", Some(v), None);
        assert_eq!(p.id(), "openai");
    }

    /// An explicit `openai` is honored even when it contradicts the
    /// URL — the user gets what they asked for (main.rs warns).
    #[test]
    fn explicit_openai_overrides_anthropic_surface_url() {
        let base = "https://token-plan-cn.xiaomimimo.com/anthropic";
        let v = crate::vendor::pick_vendor(None, Some(base), "mimo-v2.5");
        let p = build(Some(ApiKind::Openai), base, "k", "mimo-v2.5", Some(v), None);
        assert_eq!(p.id(), "openai");
    }
}

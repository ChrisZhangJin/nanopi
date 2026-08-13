//! LLM provider adapters — OpenAI-compatible + Anthropic-native.

pub mod anthropic;
pub mod openai;
pub mod retry;
pub mod sse;

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
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("anthropic") | Some("claude") => ApiKind::Anthropic,
            _ => ApiKind::Openai,
        }
    }
}

/// Build a Provider trait object for the given wire kind. Callers
/// hand the returned Box straight to Agent.provider.
///
/// v0.9.3: `vendor` (from `crate::vendor::pick_vendor`) governs
/// thinking-block / reasoning_effort emission. Pass `None` for the
/// legacy behavior (Anthropic emits its own thinking; OpenAI omits
/// reasoning params).
pub fn build(
    kind: ApiKind,
    base_url: &str,
    api_key: &str,
    model: &str,
    vendor: Option<Box<dyn crate::vendor::Vendor>>,
) -> Box<dyn Provider> {
    match kind {
        ApiKind::Openai => {
            let p = openai::OpenAiProvider::new(base_url, api_key, model);
            Box::new(match vendor {
                Some(v) => p.with_vendor(v),
                None => p,
            })
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
}

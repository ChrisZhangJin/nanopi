//! Xiaomi (xiaomimimo) vendor — OpenAI transport, default reasoning_effort.
//!
//! MiMo exposes both protocols on the same hosts, split by path:
//! `…/v1` (OpenAI-compat) and `…/anthropic` (Anthropic-native, used by
//! Token Plan for Claude Code). `transport()` names the primary; the
//! trait's `transport_for()` switches on a `/anthropic` base_url.

use super::Vendor;
use crate::provider::ApiKind;

#[derive(Debug)]
pub struct XiaomiVendor;

impl Vendor for XiaomiVendor {
    fn id(&self) -> &'static str { "xiaomi" }
    fn transport(&self) -> ApiKind { ApiKind::Openai }
    fn supports_thinking(&self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        m.contains("mimo") || m.contains("thinking")
    }
}

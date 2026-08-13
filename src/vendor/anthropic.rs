//! Anthropic vendor — native /v1/messages transport, thinking block.

use super::Vendor;
use crate::agent::thinking::ThinkingLevel;
use crate::provider::ApiKind;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct AnthropicVendor;

impl Vendor for AnthropicVendor {
    fn id(&self) -> &'static str { "anthropic" }
    fn transport(&self) -> ApiKind { ApiKind::Anthropic }
    fn default_base_url(&self) -> Option<&'static str> { Some("https://api.anthropic.com") }
    fn supports_thinking(&self, model: &str) -> bool {
        crate::agent::thinking::supports_thinking(model)
    }
    fn write_thinking(&self, body: &mut Value, level: ThinkingLevel, _model: &str) {
        let budget = level.budget_tokens();
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        body["max_tokens"] = json!(budget + 4096);
    }
}

//! DeepSeek vendor — OpenAI transport + Anthropic-shape thinking block.

use super::{openai_effort_string, Vendor};
use crate::agent::thinking::ThinkingLevel;
use crate::provider::ApiKind;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct DeepSeekVendor;

impl Vendor for DeepSeekVendor {
    fn id(&self) -> &'static str { "deepseek" }
    fn transport(&self) -> ApiKind { ApiKind::Openai }
    fn default_base_url(&self) -> Option<&'static str> { Some("https://api.deepseek.com") }
    fn supports_thinking(&self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        m.starts_with("deepseek-reasoner") || m.starts_with("deepseek-v")
    }
    fn write_thinking(&self, body: &mut Value, level: ThinkingLevel, _model: &str) {
        body["thinking"] = json!({"type": "enabled"});
        body["reasoning_effort"] = json!(openai_effort_string(level));
    }
}

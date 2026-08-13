//! Z.ai vendor — OpenAI transport, GLM-4.x thinking with clear_thinking:false.

use super::{openai_effort_string, Vendor};
use crate::agent::thinking::ThinkingLevel;
use crate::provider::ApiKind;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ZaiVendor;

impl Vendor for ZaiVendor {
    fn id(&self) -> &'static str { "zai" }
    fn transport(&self) -> ApiKind { ApiKind::Openai }
    fn default_base_url(&self) -> Option<&'static str> { Some("https://api.z.ai/api/paas/v4") }
    fn supports_thinking(&self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        m.starts_with("glm-4.5-thinking") || m.starts_with("glm-4.6")
    }
    fn write_thinking(&self, body: &mut Value, level: ThinkingLevel, _model: &str) {
        body["thinking"] = json!({"type": "enabled", "clear_thinking": false});
        body["reasoning_effort"] = json!(openai_effort_string(level));
    }
}

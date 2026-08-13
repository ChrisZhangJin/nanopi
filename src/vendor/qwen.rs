//! Qwen (Alibaba DashScope) vendor — OpenAI transport + enable_thinking toggle.

use super::{openai_effort_string, Vendor};
use crate::agent::thinking::ThinkingLevel;
use crate::provider::ApiKind;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct QwenVendor;

impl Vendor for QwenVendor {
    fn id(&self) -> &'static str { "qwen" }
    fn transport(&self) -> ApiKind { ApiKind::Openai }
    fn default_base_url(&self) -> Option<&'static str> {
        Some("https://dashscope.aliyuncs.com/compatible-mode/v1")
    }
    fn supports_thinking(&self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        m.contains("thinking") || m.starts_with("qwq")
    }
    fn write_thinking(&self, body: &mut Value, level: ThinkingLevel, _model: &str) {
        body["enable_thinking"] = json!(true);
        body["reasoning_effort"] = json!(openai_effort_string(level));
    }
}

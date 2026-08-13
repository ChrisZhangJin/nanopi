//! OpenAI vendor — OpenAI transport, reasoning_effort for o1/o3/gpt-5.

use super::Vendor;
use crate::provider::ApiKind;

#[derive(Debug)]
pub struct OpenAiVendor;

impl Vendor for OpenAiVendor {
    fn id(&self) -> &'static str { "openai" }
    fn transport(&self) -> ApiKind { ApiKind::Openai }
    fn default_base_url(&self) -> Option<&'static str> { Some("https://api.openai.com/v1") }
    fn supports_thinking(&self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        m.starts_with("o1") || m.starts_with("o3") || m.starts_with("gpt-5")
    }
}

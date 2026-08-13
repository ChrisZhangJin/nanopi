//! Xiaomi (xiaomimimo) vendor — OpenAI transport, default reasoning_effort.

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

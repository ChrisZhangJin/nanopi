//! Fallback vendor — OpenAI wire shape, no default base URL, no thinking.

use super::Vendor;
use crate::provider::ApiKind;

#[derive(Debug)]
pub struct FallbackVendor;

impl Vendor for FallbackVendor {
    fn id(&self) -> &'static str { "fallback" }
    fn transport(&self) -> ApiKind { ApiKind::Openai }
}

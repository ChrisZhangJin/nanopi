//! Minimax vendor — Anthropic transport, no max_tokens bump.

use super::Vendor;
use crate::agent::thinking::ThinkingLevel;
use crate::provider::ApiKind;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct MinimaxVendor;

impl Vendor for MinimaxVendor {
    fn id(&self) -> &'static str { "minimax" }
    fn transport(&self) -> ApiKind { ApiKind::Anthropic }
    /// Both surfaces on both hosts: `/anthropic/v1/messages` and
    /// `/v1/chat/completions`. `transport()` above names only the
    /// DEFAULT (what `default_base_url` points at) — a `base_url`
    /// ending in `/v1` is the OpenAI surface and is equally valid.
    fn dual_surface(&self) -> bool { true }
    /// MiniMax answers on two hosts — the older `api.minimax.chat` and
    /// the current `api.minimaxi.com` (the one behind the
    /// `platform.minimaxi.com` console). Both still serve
    /// `/anthropic/v1/messages`; we default to the current one. An
    /// explicit `base_url` for either host keeps working, since
    /// `pick_vendor` sniffs on the substring "minimax".
    fn default_base_url(&self) -> Option<&'static str> {
        Some("https://api.minimaxi.com/anthropic")
    }
    fn supports_thinking(&self, model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        m.starts_with("minimax-m")
    }
    fn write_thinking(&self, body: &mut Value, level: ThinkingLevel, _model: &str) {
        let budget = level.budget_tokens();
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
}

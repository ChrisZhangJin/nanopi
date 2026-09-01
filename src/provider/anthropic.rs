//! Anthropic-compatible provider adapter.
//!
//! Speaks the Anthropic Messages API: `POST {base_url}/v1/messages` with
//! Anthropic's SSE event choreography:
//!
//! - `message_start`    — initial message with id + model
//! - `content_block_start` — begins a text or tool_use block
//! - `content_block_delta`  — incremental text or tool input
//! - `content_block_stop`   — end of a content block
//! - `message_delta`    — final usage + stop_reason
//! - `message_stop`     — end of message
//!
//! We translate these to the unified `AgentEvent` stream so the agent
//! loop doesn't care which provider is in use.
//!
//! See `docs/v0.5-research.md` §1 for the wire-format comparison.


use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
use crate::provider::sse::SseStream;

/// Provider identifier constant.
pub const ANTHROPIC_ID: &str = "anthropic";

/// The Anthropic provider.
pub struct AnthropicProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub client: reqwest::Client,
    /// v0.9.3: optional vendor for thinking-block emission. If `None`,
    /// falls back to the legacy in-`build_request` thinking logic.
    pub vendor: Option<Box<dyn crate::vendor::Vendor>>,
}

impl AnthropicProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            client: reqwest::Client::builder()
                .build()
                .expect("build reqwest client"),
            vendor: None,
        }
    }

    /// v0.9.3: attach a vendor for thinking-block emission.
    pub fn with_vendor(mut self, vendor: Box<dyn crate::vendor::Vendor>) -> Self {
        self.vendor = Some(vendor);
        self
    }
}

/// Translate a `Context` to the Anthropic `/v1/messages` wire format.
pub fn build_request<'a>(ctx: &'a Context, model: &'a str) -> serde_json::Value {
    use serde_json::json;

    let mut messages: Vec<serde_json::Value> = Vec::new();
    for m in &ctx.messages {
        match m {
            crate::agent::context::ContextMessage::User { content } => {
                let text: String = content
                    .iter()
                    .filter_map(|b| match b {
                        crate::agent::context::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                messages.push(json!({"role": "user", "content": text}));
            }
            crate::agent::context::ContextMessage::Assistant { content } => {
                // Anthropic assistant content is an array of content blocks.
                let blocks: Vec<serde_json::Value> = content
                    .iter()
                    .map(|b| match b {
                        crate::agent::context::AssistantBlock::Text { text } => {
                            json!({"type": "text", "text": text})
                        }
                        crate::agent::context::AssistantBlock::Thinking { text } => {
                            json!({"type": "thinking", "thinking": text})
                        }
                        crate::agent::context::AssistantBlock::ToolCall { call } => {
                            json!({
                                "type": "tool_use",
                                "id": call.id,
                                "name": call.name,
                                "input": call.arguments
                            })
                        }
                    })
                    .collect();
                if blocks.is_empty() {
                    messages.push(json!({"role": "assistant", "content": ""}));
                } else {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            crate::agent::context::ContextMessage::Tool {
                tool_call_id,
                content,
                is_error,
                images,
            } => {
                // Anthropic tool results go inside a `user` message with a
                // `tool_result` content block. Text-only results use the
                // string-content form (compact wire); multimodal results
                // switch to the content-array form with text + image
                // blocks so vision models see the picture. See
                // packages/ai/src/api/anthropic-messages.ts:137-151 for
                // the source shape.
                let vision_ok = crate::agent::thinking::supports_vision(model);
                let has_images = !images.is_empty();
                let tool_result_content: serde_json::Value = if !has_images {
                    // Text-only path — cheapest wire.
                    serde_json::Value::String(content.clone())
                } else if vision_ok {
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    if !content.is_empty() {
                        blocks.push(json!({ "type": "text", "text": content }));
                    }
                    for img in images {
                        blocks.push(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": img.media_type,
                                "data": img.data_base64,
                            }
                        }));
                    }
                    serde_json::Value::Array(blocks)
                } else {
                    // Model doesn't do vision — leave a placeholder so
                    // the model at least knows an image WAS returned.
                    let n = images.len();
                    let note = if content.is_empty() {
                        format!("(image omitted — {n} image(s), model does not support vision)")
                    } else {
                        format!(
                            "{}\n\n(image omitted — {n} image(s), model does not support vision)",
                            content
                        )
                    };
                    serde_json::Value::String(note)
                };
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": tool_result_content,
                        "is_error": is_error,
                    }]
                }));
            }
        }
    }

    let tools: Vec<serde_json::Value> = ctx
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect();

    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": 4096,
        "stream": true,
    });
    if let Some(sys) = &ctx.system {
        body["system"] = json!(sys);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    // Extended thinking: only when the user picked a level AND the
    // model supports it. Sending `thinking` to a model that doesn't
    // support the field would 400. See agent::thinking::supports_thinking.
    if let Some(level) = ctx.thinking {
        if crate::agent::thinking::supports_thinking(model) {
            let budget = level.budget_tokens();
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
            // Anthropic requires max_tokens > budget_tokens so the
            // model has room to write an actual answer AFTER thinking.
            body["max_tokens"] = json!(budget + 4096);
        }
    }
    body
}

// ─────── Wire-format types ───────

#[derive(Debug, Deserialize)]
struct WireEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: Option<WireDelta>,
    #[serde(default)]
    content_block: Option<WireContentBlock>,
    #[serde(default)]
    message: Option<WireMessage>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireDelta {
    #[serde(default)]
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct WireError {
    message: String,
}

/// Extract a human-readable error message from any SSE event body that
/// looks like an error but doesn't match Anthropic's own
/// `{"type":"error","error":{"message":"..."}}` shape.
///
/// Real gateways this catches:
/// - LiteLLM-style: `{"error":"Claude API error","status":401,"details":"..."}`
/// - Bare Anthropic-like: `{"error":{"message":"..."}}`
/// - Wrapped inside a `details` field that itself carries the real
///   Anthropic error payload as a JSON-encoded string.
///
/// Returns `None` if nothing error-shaped is present — caller
/// silently skips non-error unknown events (heartbeats, etc.).
pub(super) fn extract_gateway_error(v: &Value) -> Option<String> {
    let err = v.get("error")?;

    // `error` as object with `message` — Anthropic's own shape.
    if let Some(msg) = err.get("message").and_then(|m| m.as_str()) {
        return Some(msg.to_string());
    }

    // `error` as bare string — LiteLLM-style wrapper.
    if let Some(s) = err.as_str() {
        let mut msg = s.to_string();
        if let Some(status) = v.get("status").and_then(|s| s.as_u64()) {
            msg = format!("{msg} (status {status})");
        }
        // If `details` is itself a JSON string carrying the real
        // Anthropic error, pull the inner message out.
        if let Some(d) = v.get("details").and_then(|d| d.as_str()) {
            if let Ok(inner) = serde_json::from_str::<Value>(d) {
                if let Some(inner_msg) = inner
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                {
                    msg = format!("{msg}: {inner_msg}");
                } else {
                    msg = format!("{msg}: {d}");
                }
            } else {
                msg = format!("{msg}: {d}");
            }
        }
        return Some(msg);
    }

    None
}

#[async_trait::async_trait]
impl crate::agent::loop_::Provider for AnthropicProvider {
    fn id(&self) -> &'static str {
        ANTHROPIC_ID
    }

    async fn stream_turn(
        &self,
        ctx: &Context,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<Usage, String> {
        use futures_util::StreamExt;

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut body = build_request(ctx, &self.model);
        // v0.9.3: if a vendor is attached and it wants to emit thinking
        // differently (e.g. Minimax skips the max_tokens bump), let it
        // overwrite the block built by `build_request`.
        if let (Some(vendor), Some(level)) = (self.vendor.as_deref(), ctx.thinking) {
            if vendor.id() != "anthropic" && vendor.supports_thinking(&self.model) {
                body.as_object_mut().map(|m| {
                    m.remove("thinking");
                    m.remove("max_tokens");
                });
                vendor.write_thinking(&mut body, level, &self.model);
                // Some transports still need max_tokens; keep the
                // default 4096 unless the vendor already added one.
                if body.get("max_tokens").is_none() {
                    body["max_tokens"] = serde_json::json!(4096);
                }
            }
        }

        // ── Retry envelope: mirrors openai.rs. Covers the HTTP open
        // (429 / 5xx / transient network / 60s send-timeout on hung
        // gateways) AND mid-stream errors that arrive before we've
        // emitted anything to `tx`. Once Start is sent, retrying would
        // duplicate content — we stop.
        use crate::provider::retry::{
            compute_delay, is_retryable_message, is_retryable_status, parse_retry_after, rand01,
            truncate, RetryConfig,
        };
        use std::collections::HashMap;
        let retry = RetryConfig::default();
        let mut attempt: u32 = 0;
        let final_usage;

        'retry: loop {
            // Reqwest has no default send timeout — a gateway that
            // accepts the TCP connection but never sends response
            // headers would hang forever. Cap the header-wait at 60s;
            // streaming body reads afterwards have no cap.
            let send_fut = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send();
            let send_res =
                match tokio::time::timeout(std::time::Duration::from_secs(60), send_fut).await {
                    Ok(res) => res,
                    Err(_elapsed) => {
                        if attempt >= retry.max_attempts {
                            return Err("send timeout: no response headers after 60s".into());
                        }
                        let delay = compute_delay(attempt, &retry, None, rand01());
                        eprintln!(
                            "[retrying ({}/{}) after {:.1}s: send timeout after 60s]",
                            attempt + 1,
                            retry.max_attempts,
                            delay.as_secs_f64(),
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue 'retry;
                    }
                };

            let resp = match send_res {
                Ok(resp) if resp.status().is_success() => resp,
                Ok(resp) => {
                    let status = resp.status();
                    let hint = parse_retry_after(resp.headers());
                    let body_text = resp.text().await.unwrap_or_default();
                    let retryable =
                        is_retryable_status(status.as_u16()) || is_retryable_message(&body_text);
                    if attempt >= retry.max_attempts || !retryable {
                        // `retryable` above saw the full body; only the
                        // user-facing string gets flattened.
                        return Err(format!(
                            "HTTP {}: {}",
                            status.as_u16(),
                            crate::provider::retry::flatten_error_body(&body_text, 300)
                        ));
                    }
                    let delay = compute_delay(attempt, &retry, hint, rand01());
                    eprintln!(
                        "[retrying ({}/{}) after {:.1}s: HTTP {} {}]",
                        attempt + 1,
                        retry.max_attempts,
                        delay.as_secs_f64(),
                        status.as_u16(),
                        crate::provider::retry::flatten_error_body(&body_text, 100),
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue 'retry;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let retryable = is_retryable_message(&msg);
                    if attempt >= retry.max_attempts || !retryable {
                        return Err(msg);
                    }
                    let delay = compute_delay(attempt, &retry, None, rand01());
                    eprintln!(
                        "[retrying ({}/{}) after {:.1}s: {}]",
                        attempt + 1,
                        retry.max_attempts,
                        delay.as_secs_f64(),
                        truncate(&msg, 100),
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue 'retry;
                }
            };

            let byte_stream = resp.bytes_stream();
            let mut sse = SseStream::new(byte_stream);

            // Per-block state for accumulating tool_use partial_json. Fresh per attempt.
            let mut pending: HashMap<u32, PendingAnthropicTool> = HashMap::new();
            let mut emitted_call_ids: std::collections::HashSet<String> = Default::default();
            let mut started = false;
            let mut usage_local = Usage::default();

            while let Some(ev) = sse.next().await {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(e) => {
                        let msg = e.to_string();
                        if !started
                            && attempt < retry.max_attempts
                            && is_retryable_message(&msg)
                        {
                            let delay = compute_delay(attempt, &retry, None, rand01());
                            eprintln!(
                                "[retrying ({}/{}) after {:.1}s: {}]",
                                attempt + 1,
                                retry.max_attempts,
                                delay.as_secs_f64(),
                                truncate(&msg, 100),
                            );
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue 'retry;
                        }
                        return Err(msg);
                    }
                };
                let chunk: WireEvent = match serde_json::from_str(&ev.data) {
                    Ok(c) => c,
                    Err(_) => {
                        // Non-Anthropic-shape payload. Before dropping
                        // it, check for a gateway-wrapped error —
                        // several proxies (e.g. corporate LiteLLM
                        // setups) return HTTP 200 with an SSE
                        // `event: error` whose data uses a schema
                        // Anthropic never emits, like
                        //   {"error":"...","status":401,"details":"..."}
                        // Without this catch, the whole stream ends
                        // silently — the user saw nothing printed.
                        if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                            if let Some(msg) = extract_gateway_error(&v) {
                                if !started
                                    && attempt < retry.max_attempts
                                    && is_retryable_message(&msg)
                                {
                                    let delay = compute_delay(attempt, &retry, None, rand01());
                                    eprintln!(
                                        "[retrying ({}/{}) after {:.1}s: {}]",
                                        attempt + 1,
                                        retry.max_attempts,
                                        delay.as_secs_f64(),
                                        truncate(&msg, 100),
                                    );
                                    tokio::time::sleep(delay).await;
                                    attempt += 1;
                                    continue 'retry;
                                }
                                return Err(format!("gateway: {msg}"));
                            }
                        }
                        continue;
                    }
                };
                if let Some(err) = chunk.error {
                    let msg = err.message.clone();
                    if !started && attempt < retry.max_attempts && is_retryable_message(&msg) {
                        let delay = compute_delay(attempt, &retry, None, rand01());
                        eprintln!(
                            "[retrying ({}/{}) after {:.1}s: {}]",
                            attempt + 1,
                            retry.max_attempts,
                            delay.as_secs_f64(),
                            truncate(&msg, 100),
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue 'retry;
                    }
                    return Err(format!("api: {msg}"));
                }
            if !started {
                if let Some(msg) = chunk.message {
                    started = true;
                    let id = msg.id.unwrap_or_else(|| "msg".into());
                    let _ = tx.send(AgentEvent::Start { message_id: id }).await;
                } else {
                    started = true;
                    let _ = tx
                        .send(AgentEvent::Start {
                            message_id: "msg".into(),
                        })
                        .await;
                }
            }
            match chunk.kind.as_str() {
                "content_block_start" => {
                    if let Some(cb) = chunk.content_block {
                        if cb.kind == "tool_use" {
                            let entry = pending.entry(chunk.index).or_insert_with(|| {
                                PendingAnthropicTool {
                                    id: cb.id.clone(),
                                    name: cb.name.clone(),
                                    args_buf: String::new(),
                                }
                            });
                            entry.id = cb.id.or(entry.id.clone());
                            entry.name = cb.name.or(entry.name.clone());
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = chunk.delta {
                        match delta.kind.as_deref() {
                            Some("text_delta") => {
                                if let Some(t) = delta.text {
                                    let _ = tx
                                        .send(AgentEvent::TextDelta {
                                            content_index: chunk.index,
                                            text: t,
                                        })
                                        .await;
                                }
                            }
                            Some("thinking_delta") => {
                                if let Some(t) = delta.text {
                                    let _ = tx
                                        .send(AgentEvent::ThinkingDelta {
                                            content_index: chunk.index,
                                            text: t,
                                        })
                                        .await;
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(p) = delta.partial_json {
                                    if let Some(entry) = pending.get_mut(&chunk.index) {
                                        entry.args_buf.push_str(&p);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    if let Some(mut entry) = pending.remove(&chunk.index) {
                        if let Some(id) = entry.id.take() {
                            if !emitted_call_ids.contains(&id) {
                                emitted_call_ids.insert(id.clone());
                                let name = entry.name.unwrap_or_else(|| "unknown".into());
                                let args: serde_json::Value = if entry.args_buf.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(&entry.args_buf)
                                        .unwrap_or_else(|_| serde_json::json!({}))
                                };
                                let _ = tx
                                    .send(AgentEvent::ToolCall {
                                        content_index: chunk.index,
                                        call: ToolCall {
                                            id,
                                            name,
                                            arguments: args,
                                        },
                                    })
                                    .await;
                            }
                        }
                    }
                }
                "message_delta" => {
                    if let Some(delta) = chunk.delta {
                        if let Some(sr) = delta.stop_reason {
                            let finish = match sr.as_str() {
                                "end_turn" | "stop_sequence" => FinishReason::Stop,
                                "tool_use" => FinishReason::ToolCalls,
                                "max_tokens" => FinishReason::Length,
                                _ => FinishReason::Unknown,
                            };
                            if let Some(u) = chunk.usage {
                                usage_local = Usage {
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                    cache_read_tokens: u.cache_read_input_tokens,
                                    cache_write_tokens: u.cache_creation_input_tokens,
                                };
                            }
                            let _ = tx
                                .send(AgentEvent::Done {
                                    finish_reason: finish,
                                    usage: usage_local.clone(),
                                })
                                .await;
                        }
                    }
                }
                "message_stop" => {
                    // Already emitted Done via message_delta.
                }
                _ => {}
            }
            }
            final_usage = usage_local;
            break 'retry;
        }
        Ok(final_usage)
    }
}

#[derive(Default)]
struct PendingAnthropicTool {
    id: Option<String>,
    name: Option<String>,
    args_buf: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::ToolSpec;
    use crate::agent::loop_::Provider as _;
    use serde_json::json;

    fn tmp() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-anthropic-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn new_provider_has_id_anthropic() {
        let p = AnthropicProvider::new("http://x", "k", "claude-3");
        assert_eq!(p.id(), "anthropic");
    }

    /// Regression: a corporate LiteLLM-style gateway returned HTTP 200
    /// with an SSE `event: error` payload
    ///   {"error":"Claude API error","status":401,"details":"..."}
    /// The Anthropic parser silently skipped it (schema didn't match
    /// Anthropic's own `{"type":"error","error":{"message":"..."}}`),
    /// so the stream ended with no content and no Done — the user saw
    /// nothing printed at all. Fix: `extract_gateway_error` catches
    /// this shape and turns it into a real error the caller propagates.
    #[test]
    fn extract_gateway_error_recognizes_litellm_shape() {
        let v: Value =
            serde_json::from_str(r#"{"error":"Claude API error","status":401,"details":"nope"}"#)
                .unwrap();
        let msg = extract_gateway_error(&v).expect("should extract");
        assert!(msg.contains("Claude API error"));
        assert!(msg.contains("401"));
        assert!(msg.contains("nope"));
    }

    /// Same gateway shape but `details` is a JSON-encoded string
    /// carrying the real Anthropic error underneath (the actual case
    /// observed on 10.0.3.248 during smoke-testing). The extractor
    /// should peel one layer of JSON and surface the underlying
    /// message so the user sees "OAuth access token has expired"
    /// instead of a raw JSON blob.
    #[test]
    fn extract_gateway_error_unwraps_nested_details() {
        let v: Value = serde_json::from_str(r#"{
            "error":"Claude API error",
            "status":401,
            "details":"{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth access token has expired.\"},\"request_id\":null}"
        }"#).unwrap();
        let msg = extract_gateway_error(&v).expect("should extract");
        assert!(
            msg.contains("OAuth access token has expired"),
            "expected inner Anthropic message to be surfaced: {msg}"
        );
    }

    /// Anthropic's own error shape must still work — this is the
    /// non-gateway happy path where the object under `error` has a
    /// `message`. Extractor should just pass it through.
    #[test]
    fn extract_gateway_error_handles_anthropic_native_shape() {
        let v: Value =
            serde_json::from_str(r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#)
                .unwrap();
        assert_eq!(extract_gateway_error(&v).as_deref(), Some("slow down"));
    }

    /// Non-error events (heartbeats, unknown types) must not trip a
    /// false positive — otherwise every unknown SSE event turns into
    /// a spurious error.
    #[test]
    fn extract_gateway_error_ignores_non_error_events() {
        let v: Value = serde_json::from_str(r#"{"type":"ping","time":123}"#).unwrap();
        assert_eq!(extract_gateway_error(&v), None);
        let v: Value = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(extract_gateway_error(&v), None);
    }

    #[test]
    fn build_request_serializes_system_messages_and_tools() {
        let mut ctx = Context::default();
        ctx.system = Some("be helpful".into());
        ctx.push_user_text("hi");
        ctx.tools.push(ToolSpec {
            name: "bash".into(),
            description: "shell".into(),
            parameters: json!({"type":"object"}),
        });
        let v = build_request(&ctx, "claude-3");
        assert_eq!(v["model"], "claude-3");
        assert_eq!(v["system"], "be helpful");
        assert_eq!(v["stream"], true);
        assert!(v["messages"].as_array().unwrap().len() == 1);
        assert_eq!(v["tools"][0]["name"], "bash");
    }

    #[test]
    fn wire_message_start_extracts_message_id() {
        let json = r#"{"type":"message_start","message":{"id":"msg_abc","model":"claude-3"}}"#;
        let ev: WireEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.kind, "message_start");
        assert_eq!(ev.message.unwrap().id.unwrap(), "msg_abc");
    }

    #[test]
    fn wire_text_delta_extracted() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
        let ev: WireEvent = serde_json::from_str(json).unwrap();
        let delta = ev.delta.unwrap();
        assert_eq!(delta.kind.as_deref(), Some("text_delta"));
        assert_eq!(delta.text.as_deref(), Some("hello"));
    }

    #[test]
    fn wire_input_json_delta_accumulates() {
        let json = r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#;
        let ev: WireEvent = serde_json::from_str(json).unwrap();
        let delta = ev.delta.unwrap();
        assert_eq!(delta.kind.as_deref(), Some("input_json_delta"));
        assert!(delta.partial_json.unwrap().contains("command"));
    }

    #[test]
    fn build_request_omits_thinking_block_when_off() {
        let ctx = Context::default();
        let v = build_request(&ctx, "claude-opus-4-7");
        assert!(v.get("thinking").is_none());
    }

    #[test]
    fn build_request_includes_thinking_when_set_and_model_supports() {
        let mut ctx = Context::default();
        ctx.thinking = Some(crate::agent::thinking::ThinkingLevel::Medium);
        let v = build_request(&ctx, "claude-opus-4-7");
        let t = v.get("thinking").expect("thinking block should be present");
        assert_eq!(t["type"], "enabled");
        assert_eq!(t["budget_tokens"], 8192); // medium
                                              // max_tokens must exceed the budget or Anthropic will reject.
        assert!(v["max_tokens"].as_u64().unwrap() > 8192);
    }

    #[test]
    fn build_request_skips_thinking_for_unsupported_model() {
        let mut ctx = Context::default();
        ctx.thinking = Some(crate::agent::thinking::ThinkingLevel::High);
        let v = build_request(&ctx, "claude-haiku-4-5");
        // Haiku doesn't support extended thinking → block must not be sent.
        assert!(v.get("thinking").is_none());
    }

    #[test]
    fn tool_result_with_images_on_vision_model_emits_content_array() {
        let mut ctx = Context::default();
        ctx.push_tool_result_with_images(
            "call_1",
            "Read image file [image/png]",
            false,
            vec![crate::tool::ImageAttachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
        );
        let v = build_request(&ctx, "claude-opus-4-7");
        let msgs = v["messages"].as_array().unwrap();
        let tr = msgs
            .iter()
            .flat_map(|m| m["content"].as_array())
            .flatten()
            .find(|b| b["type"] == "tool_result")
            .expect("tool_result block present");
        let inner = tr["content"]
            .as_array()
            .expect("content array on multimodal");
        assert_eq!(inner[0]["type"], "text");
        assert_eq!(inner[0]["text"], "Read image file [image/png]");
        assert_eq!(inner[1]["type"], "image");
        assert_eq!(inner[1]["source"]["type"], "base64");
        assert_eq!(inner[1]["source"]["media_type"], "image/png");
        assert_eq!(inner[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn tool_result_with_images_on_text_model_downgrades_to_placeholder() {
        let mut ctx = Context::default();
        ctx.push_tool_result_with_images(
            "call_1",
            "Read image file [image/png]",
            false,
            vec![crate::tool::ImageAttachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
        );
        // Made-up id nobody supports — supports_vision returns false.
        let v = build_request(&ctx, "some-text-only-model-2100");
        let msgs = v["messages"].as_array().unwrap();
        let tr = msgs
            .iter()
            .flat_map(|m| m["content"].as_array())
            .flatten()
            .find(|b| b["type"] == "tool_result")
            .unwrap();
        let text = tr["content"].as_str().expect("downgraded to string");
        assert!(text.contains("image omitted"), "got {text:?}");
        // Original text preserved for context.
        assert!(text.contains("image/png"), "got {text:?}");
    }

    #[test]
    fn tool_result_without_images_still_emits_string_content() {
        let mut ctx = Context::default();
        ctx.push_tool_result("call_1", "ok", false);
        let v = build_request(&ctx, "claude-opus-4-7");
        let msgs = v["messages"].as_array().unwrap();
        let tr = msgs
            .iter()
            .flat_map(|m| m["content"].as_array())
            .flatten()
            .find(|b| b["type"] == "tool_result")
            .unwrap();
        // Compact string form — no unnecessary array wrapping.
        assert_eq!(tr["content"], "ok");
    }

    #[test]
    fn tool_result_block_serializes_with_is_error() {
        let mut ctx = Context::default();
        ctx.push_tool_result("call_1", "ok", false);
        ctx.push_tool_result("call_2", "err", true);
        let v = build_request(&ctx, "claude-3");
        let msgs = v["messages"].as_array().unwrap();
        // Both tool results land in user messages with tool_result blocks.
        let tool_blocks: Vec<_> = msgs
            .iter()
            .filter(|m| m["role"] == "user")
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|b| b["type"] == "tool_result")
            .collect();
        assert_eq!(tool_blocks.len(), 2);
        assert_eq!(tool_blocks[0]["tool_use_id"], "call_1");
        assert_eq!(tool_blocks[0]["is_error"], false);
        assert_eq!(tool_blocks[1]["tool_use_id"], "call_2");
        assert_eq!(tool_blocks[1]["is_error"], true);
        let _ = std::fs::remove_dir_all(&tmp()); // silence unused warning
    }
}

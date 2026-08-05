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

use std::fmt;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
use crate::provider::sse::SseStream;

/// Provider identifier constant.
pub const ANTHROPIC_ID: &str = "anthropic";

#[derive(Debug, Error)]
pub enum AnthropicError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("SSE parse error: {0}")]
    Sse(String),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("API error in stream: {0}")]
    Api(String),
}

/// The Anthropic provider.
#[derive(Clone)]
pub struct AnthropicProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            client: reqwest::Client::builder()
                .build()
                .expect("build reqwest client"),
        }
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
            crate::agent::context::ContextMessage::Tool { tool_call_id, content, is_error } => {
                // Anthropic tool results go inside a `user` message with a
                // `tool_result` content block.
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content,
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
    #[serde(default)]
    model: Option<String>,
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
        let body = build_request(ctx, &self.model);

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {}: {}", status.as_u16(), body));
        }

        let byte_stream = resp.bytes_stream();
        let mut sse = SseStream::new(byte_stream);

        // Per-block state for accumulating tool_use partial_json.
        use std::collections::HashMap;
        let mut pending: HashMap<u32, PendingAnthropicTool> = HashMap::new();
        let mut emitted_call_ids: std::collections::HashSet<String> = Default::default();
        let mut started = false;
        let mut final_usage = Usage::default();

        while let Some(ev) = sse.next().await {
            let ev = ev.map_err(|e| e.to_string())?;
            let chunk: WireEvent = match serde_json::from_str(&ev.data) {
                Ok(c) => c,
                Err(_) => continue, // skip non-JSON event lines
            };
            if let Some(err) = chunk.error {
                return Err(format!("api: {}", err.message));
            }
            if !started {
                if let Some(msg) = chunk.message {
                    started = true;
                    let id = msg.id.unwrap_or_else(|| "msg".into());
                    let _ = tx.send(AgentEvent::Start { message_id: id }).await;
                } else {
                    started = true;
                    let _ = tx
                        .send(AgentEvent::Start { message_id: "msg".into() })
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
                                final_usage = Usage {
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                    cache_read_tokens: u.cache_read_input_tokens,
                                    cache_write_tokens: u.cache_creation_input_tokens,
                                };
                            }
                            let _ = tx
                                .send(AgentEvent::Done {
                                    finish_reason: finish,
                                    usage: final_usage.clone(),
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
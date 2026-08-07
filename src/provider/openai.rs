//! OpenAI-compatible provider adapter.
//!
//! Speaks the de-facto standard: `POST {base_url}/chat/completions` with
//! `stream: true`, consuming SSE `data: {json}` chunks. Works with
//! OpenAI, DeepSeek, ollama, vLLM, Azure OpenAI Responses, etc.
//!
//! See `docs/v0.5-research.md` §1 for the wire format comparison.

use std::fmt;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
use crate::provider::sse::SseStream;

#[derive(Debug, Error)]
pub enum OpenAiError {
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

/// Provider identifier constant.
pub const OPENAI_ID: &str = "openai";

/// One chunk from the OpenAI streaming response. We only model the fields
/// we actually use; the rest is left as `serde_json::Value`.
#[derive(Debug, Deserialize)]
struct WireChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    /// Some providers put an error here even with HTTP 200.
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: WireDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireToolCallFunc>,
    /// Some providers stream `arguments` directly on the tool_call.
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireToolCallFunc {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    /// OpenAI's prompt_tokens_details.cached_tokens (some providers use flat).
    #[serde(default)]
    cached_tokens: u32,
}

/// Gateways send errors mid-stream in one of two shapes:
///   { "error": { "message": "..." } }   (OpenAI convention)
///   { "error": "..." }                   (some proxies flatten it)
/// This untagged enum accepts either.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WireError {
    Struct { message: String },
    Flat(String),
}

impl WireError {
    fn message(&self) -> &str {
        match self {
            WireError::Struct { message } => message,
            WireError::Flat(s) => s,
        }
    }
}

/// Try to extract a human-readable message from a provider error body.
/// Accepts:
///   - raw JSON:      {"error": {"message": "..."}} or {"error": "..."}
///   - SSE payload:   `event: error\ndata: {"type":"error","error":{"message":"..."}}`
/// Returns None if nothing matches (caller falls back to the raw body).
fn extract_error_message(body: &str) -> Option<String> {
    // Case 1: whole body is JSON.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body.trim()) {
        if let Some(m) = json_error_message(&v) {
            return Some(m);
        }
    }
    // Case 2: SSE — find the first `data:` line that parses as JSON with
    // an error field.
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if let Some(m) = json_error_message(&v) {
            return Some(m);
        }
    }
    None
}

fn json_error_message(v: &serde_json::Value) -> Option<String> {
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        return Some(s.to_string());
    }
    if let Some(m) = err.get("message").and_then(|x| x.as_str()) {
        return Some(m.to_string());
    }
    err.get("type").and_then(|x| x.as_str()).map(String::from)
}

/// Undo the OpenAI-compat-gateway rewrite that turns Anthropic
/// `tool_use.name = "bash"` into `"Bash_tool"` in the SSE
/// `chat.completion.chunk.delta.tool_calls[].function.name` field.
/// Lowercase + strip trailing `_tool`. Passes canonical names
/// through unchanged.
fn normalize_mangled_tool_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix("_tool").map(String::from).unwrap_or(lower)
}

/// Cheap 0..1 pseudo-random from the current nanosecond of the clock.
/// Good enough for retry jitter — we don't need cryptographic entropy.
fn rand01() -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.subsec_nanos() as f64) / 1_000_000_000.0
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// The OpenAI-compatible provider.
#[derive(Clone)]
pub struct OpenAiProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub client: reqwest::Client,
}

impl OpenAiProvider {
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

#[derive(Debug, Clone, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

#[derive(Debug, Clone, Serialize)]
struct WireMessage {
    role: String,
    /// OpenAI accepts string content for text-only messages.
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// For assistant messages that include tool_calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireAssistantToolCall>>,
}

#[derive(Debug, Clone, Serialize)]
struct WireAssistantToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String, // "function"
    function: WireAssistantFunction,
}

#[derive(Debug, Clone, Serialize)]
struct WireAssistantFunction {
    name: String,
    arguments: String, // JSON-encoded
}

#[derive(Debug, Clone, Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: String, // "function"
    function: WireToolFunction,
}

#[derive(Debug, Clone, Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

/// Translate a `Context` into the wire-format OpenAI request body.
pub fn build_request<'a>(ctx: &'a Context, model: &'a str) -> WireRequest<'a> {
    let mut messages: Vec<WireMessage> = Vec::new();

    // OpenAI puts system in the messages array as role:system.
    if let Some(sys) = &ctx.system {
        messages.push(WireMessage {
            role: "system".into(),
            content: sys.clone(),
            tool_call_id: None,
            tool_calls: None,
        });
    }

    for m in &ctx.messages {
        match m {
            crate::agent::context::ContextMessage::User { content } => {
                // Simplification: concatenate text blocks. Image blocks
                // aren't supported in v0.5.
                let text = content
                    .iter()
                    .filter_map(|b| match b {
                        crate::agent::context::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                messages.push(WireMessage {
                    role: "user".into(),
                    content: text,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
            crate::agent::context::ContextMessage::Assistant { content } => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for b in content {
                    match b {
                        crate::agent::context::AssistantBlock::Text { text: t } => text.push_str(t),
                        crate::agent::context::AssistantBlock::Thinking { .. } => {} // skip
                        crate::agent::context::AssistantBlock::ToolCall { call } => {
                            tool_calls.push(WireAssistantToolCall {
                                id: call.id.clone(),
                                kind: "function".into(),
                                function: WireAssistantFunction {
                                    name: call.name.clone(),
                                    arguments: serde_json::to_string(&call.arguments)
                                        .unwrap_or_else(|_| "{}".into()),
                                },
                            });
                        }
                    }
                }
                messages.push(WireMessage {
                    role: "assistant".into(),
                    content: text,
                    tool_call_id: None,
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
                });
            }
            crate::agent::context::ContextMessage::Tool { tool_call_id, content, is_error } => {
                messages.push(WireMessage {
                    role: if *is_error { "tool".into() } else { "tool".into() },
                    content: content.clone(),
                    tool_call_id: Some(tool_call_id.clone()),
                    tool_calls: None,
                });
            }
        }
    }

    let tools = ctx
        .tools
        .iter()
        .map(|t| WireTool {
            kind: "function".into(),
            function: WireToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect();

    WireRequest {
        model,
        messages,
        stream: true,
        tools,
    }
}

impl OpenAiProvider {
    /// Stream a single assistant turn. Pushes AgentEvents to `tx`. Returns
    /// final usage.
    pub async fn stream_turn(
        &self,
        ctx: &Context,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<Usage, OpenAiError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = build_request(ctx, &self.model);

        // ── Retry envelope covers BOTH the HTTP open (429 / 5xx / transient
        // network) AND mid-stream errors that arrive before we've emitted
        // any event to `tx`. Once we've sent the Start event we stop
        // retrying — the caller has seen the first chunk, resuming
        // would produce duplicates.
        let retry = crate::provider::retry::RetryConfig::default();
        let mut attempt: u32 = 0;

        // These vars are set by the loop body on success, then used
        // AFTER the loop for the drain phase. Rust's borrow checker
        // won't let us assign in-loop-then-use-outside; instead we
        // finish the whole drain inside the loop and just track
        // final_usage.
        let final_usage;

        'retry: loop {
            let send_res = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;

            let resp = match send_res {
                Ok(resp) if resp.status().is_success() => resp,
                Ok(resp) => {
                    let status = resp.status();
                    let hint = crate::provider::retry::parse_retry_after(resp.headers());
                    let body_text = resp.text().await.unwrap_or_default();
                    let clean = extract_error_message(&body_text)
                        .unwrap_or_else(|| body_text.clone());
                    let retryable = crate::provider::retry::is_retryable_status(status.as_u16())
                        || crate::provider::retry::is_retryable_message(&clean);
                    if attempt >= retry.max_attempts || !retryable {
                        return Err(OpenAiError::Status {
                            status: status.as_u16(),
                            body: clean,
                        });
                    }
                    let delay = crate::provider::retry::compute_delay(
                        attempt, &retry, hint, rand01(),
                    );
                    eprintln!(
                        "[retrying ({}/{}) after {:.1}s: HTTP {} {}]",
                        attempt + 1, retry.max_attempts,
                        delay.as_secs_f64(), status.as_u16(),
                        truncate(&clean, 100),
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue 'retry;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let retryable = crate::provider::retry::is_retryable_message(&msg);
                    if attempt >= retry.max_attempts || !retryable {
                        return Err(OpenAiError::Http(e));
                    }
                    let delay = crate::provider::retry::compute_delay(
                        attempt, &retry, None, rand01(),
                    );
                    eprintln!(
                        "[retrying ({}/{}) after {:.1}s: {}]",
                        attempt + 1, retry.max_attempts,
                        delay.as_secs_f64(), truncate(&msg, 100),
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue 'retry;
                }
            };

            let byte_stream = resp
                .bytes_stream()
                .map_err(|e| OpenAiError::Http(e));
            let sse = SseStream::new(byte_stream);

            let mut pending: std::collections::HashMap<u32, PendingToolCall> = Default::default();
            let mut emitted_call_ids: std::collections::HashSet<String> = Default::default();
            let mut started = false;
            let mut usage_local = Usage::default();

            let mut stream = Box::pin(sse);
            while let Some(event) = stream.next().await {
                let ev = event.map_err(|e| OpenAiError::Sse(e.to_string()))?;
                let chunk: WireChunk = serde_json::from_str(&ev.data)?;
                if let Some(err) = chunk.error {
                    let msg = err.message().to_string();
                    // Retryable mid-stream error BEFORE we sent anything
                    // to `tx` → back to the top of 'retry.
                    if !started
                        && attempt < retry.max_attempts
                        && crate::provider::retry::is_retryable_message(&msg)
                    {
                        let delay = crate::provider::retry::compute_delay(
                            attempt, &retry, None, rand01(),
                        );
                        eprintln!(
                            "[retrying ({}/{}) after {:.1}s: {}]",
                            attempt + 1, retry.max_attempts,
                            delay.as_secs_f64(), truncate(&msg, 100),
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue 'retry;
                    }
                    return Err(OpenAiError::Api(msg));
                }
            if !started {
                started = true;
                let id = chunk.id.clone().unwrap_or_else(|| "msg".into());
                let _ = tx.send(AgentEvent::Start { message_id: id }).await;
            }
            for choice in chunk.choices {
                let delta = &choice.delta;
                if let Some(t) = &delta.content {
                    if !t.is_empty() {
                        let _ = tx
                            .send(AgentEvent::TextDelta {
                                content_index: choice.index,
                                text: t.clone(),
                            })
                            .await;
                    }
                }
                if let Some(t) = &delta.reasoning_content {
                    if !t.is_empty() {
                        let _ = tx
                            .send(AgentEvent::ThinkingDelta {
                                content_index: choice.index,
                                text: t.clone(),
                            })
                            .await;
                    }
                }
                // Tool call deltas.
                for tc in &delta.tool_calls {
                    let idx = tc.index.unwrap_or(0);
                    let entry = pending.entry(idx).or_insert_with(|| PendingToolCall {
                        id: None,
                        name: None,
                        args_buf: String::new(),
                    });
                    if let Some(id) = &tc.id {
                        entry.id = Some(id.clone());
                    }
                    // Either `tc.arguments` directly (DeepSeek / some others)
                    // or `tc.function.arguments` (OpenAI proper).
                    if let Some(args) = &tc.arguments {
                        entry.args_buf.push_str(args);
                    }
                    if let Some(func) = &tc.function {
                        if let Some(name) = &func.name {
                            entry.name = Some(name.clone());
                        }
                        if let Some(args) = &func.arguments {
                            entry.args_buf.push_str(args);
                        }
                    }
                }
                // Flush completed tool calls on finish_reason.
                if let Some(fr) = &choice.finish_reason {
                    flush_pending_tool_calls(&mut pending, &mut emitted_call_ids, &tx, choice.index).await;
                    let finish = match fr.as_str() {
                        "stop" => FinishReason::Stop,
                        "tool_calls" | "function_call" => FinishReason::ToolCalls,
                        "length" => FinishReason::Length,
                        "content_filter" => FinishReason::Refusal,
                        _ => FinishReason::Unknown,
                    };
                    if let Some(u) = &chunk.usage {
                        usage_local = Usage {
                            input_tokens: u.prompt_tokens,
                            output_tokens: u.completion_tokens,
                            cache_read_tokens: u.cached_tokens,
                            cache_write_tokens: 0,
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
            if let Some(u) = chunk.usage {
                usage_local = Usage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                    cache_read_tokens: u.cached_tokens,
                    cache_write_tokens: 0,
                };
            }
        }

        // Stream closed without an explicit finish_reason; emit Done with what we have.
        flush_pending_tool_calls(&mut pending, &mut emitted_call_ids, &tx, 0).await;
        final_usage = usage_local;
        break 'retry;
        }
        Ok(final_usage)
    }
}

#[async_trait::async_trait]
impl crate::agent::loop_::Provider for OpenAiProvider {
    fn id(&self) -> &'static str {
        "openai"
    }

    async fn stream_turn(
        &self,
        ctx: &Context,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<Usage, String> {
        // Delegate to the inherent method by UFCS to avoid name collision.
        OpenAiProvider::stream_turn(self, ctx, tx)
            .await
            .map_err(|e| e.to_string())
    }
}

#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    args_buf: String,
}

async fn flush_pending_tool_calls(
    pending: &mut std::collections::HashMap<u32, PendingToolCall>,
    emitted: &mut std::collections::HashSet<String>,
    tx: &mpsc::Sender<AgentEvent>,
    content_index: u32,
) {
    let drained: Vec<(u32, PendingToolCall)> = pending.drain().collect();
    for (idx, p) in drained {
        let id = p.id.unwrap_or_else(|| format!("call_{idx}"));
        if emitted.contains(&id) {
            continue;
        }
        emitted.insert(id.clone());
        let args: Value = if p.args_buf.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&p.args_buf).unwrap_or_else(|_| json!({}))
        };
        // Normalize gateway-mangled names (e.g. `Bash_tool` → `bash`)
        // AT THE SOURCE so every downstream consumer — spinner labels,
        // TUI tool bar, session persistence, next-turn context — sees
        // the canonical form. Rule matches `ToolRegistry::canonical_name`
        // but stateless (no registry lookup needed here).
        let raw = p.name.unwrap_or_else(|| "unknown".into());
        let name = normalize_mangled_tool_name(&raw);
        let _ = tx
            .send(AgentEvent::ToolCall {
                content_index,
                call: ToolCall { id, name, arguments: args },
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::{ContextMessage, ToolSpec};
    use crate::event::AgentEvent;

    fn collect_events(events: Vec<Result<AgentEvent, ()>>) -> Vec<AgentEvent> {
        events.into_iter().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn extract_error_from_plain_json() {
        let body = r#"{"error":{"message":"boom","type":"internal"}}"#;
        assert_eq!(extract_error_message(body).as_deref(), Some("boom"));
    }

    #[test]
    fn extract_error_from_flat_string() {
        let body = r#"{"error":"OAuth expired"}"#;
        assert_eq!(extract_error_message(body).as_deref(), Some("OAuth expired"));
    }

    #[test]
    fn extract_error_from_sse_stream() {
        let body = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"queue_timeout\",\"message\":\"queue full, retry later\"}}\n\ndata: [DONE]\n";
        assert_eq!(extract_error_message(body).as_deref(), Some("queue full, retry later"));
    }

    #[test]
    fn extract_error_returns_none_on_html() {
        let body = "<html><body>502 Bad Gateway</body></html>";
        assert!(extract_error_message(body).is_none());
    }

    #[test]
    fn build_request_basic() {
        let mut ctx = Context::default();
        ctx.system = Some("be helpful".into());
        ctx.push_user_text("hi");
        let req = build_request(&ctx, "m");
        assert_eq!(req.model, "m");
        assert_eq!(req.messages.len(), 2); // system + user
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn build_request_with_tools() {
        let mut ctx = Context::default();
        ctx.push_user_text("hi");
        ctx.tools.push(ToolSpec {
            name: "bash".into(),
            description: "shell".into(),
            parameters: json!({"type":"object","properties":{"command":{"type":"string"}}}),
        });
        let req = build_request(&ctx, "m");
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].function.name, "bash");
    }

    #[test]
    fn build_request_includes_tool_messages() {
        let mut ctx = Context::default();
        ctx.push_user_text("do it");
        ctx.messages.push(ContextMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "ok".into(),
            is_error: false,
        });
        let req = build_request(&ctx, "m");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[1].role, "tool");
        assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn wire_chunk_parses_content_delta() {
        let json = r#"{"id":"x","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let chunk: WireChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.id.as_deref(), Some("x"));
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
    }

    #[test]
    fn wire_chunk_parses_tool_call_delta() {
        let json = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"comma"}}]}}]}"#;
        let chunk: WireChunk = serde_json::from_str(json).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.id.as_deref(), Some("call_1"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("bash"));
        assert_eq!(tc.function.as_ref().unwrap().arguments.as_deref(), Some("{\"comma"));
    }

    #[test]
    fn wire_chunk_parses_reasoning_content() {
        let json = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"hmm"}}]}"#;
        let chunk: WireChunk = serde_json::from_str(json).unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("hmm")
        );
    }

    // Suppress unused warnings on tests that import AgentEvent for type
    // clarity but don't use it directly.
    #[allow(dead_code)]
    fn _ensure_event_compiles() -> AgentEvent {
        AgentEvent::Start { message_id: "x".into() }
    }
}
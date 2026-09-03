//! OpenAI-compatible provider adapter.
//!
//! Speaks the de-facto standard: `POST {base_url}/chat/completions` with
//! `stream: true`, consuming SSE `data: {json}` chunks. Works with
//! OpenAI, DeepSeek, ollama, vLLM, Azure OpenAI Responses, etc.
//!
//! See `docs/v0.5-research.md` §1 for the wire format comparison.


use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
use crate::provider::sse::SseStream;
use crate::provider::think_tags::{InlineThinkSplitter, Segment};

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
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    /// `deserialize_with` rather than a bare `default`: some gateways
    /// (Xiaomi MiMo, notably) send an explicit `"tool_calls": null` on
    /// every text-only chunk. `default` only covers an *absent* field,
    /// so a literal null aborted the whole stream with "invalid type:
    /// null, expected a sequence".
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    tool_calls: Vec<WireToolCall>,
}

/// Treat `null` as an empty list. See `WireDelta::tool_calls`.
fn null_as_empty_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
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
    lower
        .strip_suffix("_tool")
        .map(String::from)
        .unwrap_or(lower)
}

use crate::provider::retry::{rand01, truncate};

/// The OpenAI-compatible provider.
pub struct OpenAiProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub client: reqwest::Client,
    /// v0.9.3: optional vendor for reasoning_effort / thinking-block
    /// emission. If `None`, request body omits reasoning params.
    pub vendor: Option<Box<dyn crate::vendor::Vendor>>,
    /// Route `delta.content` through `InlineThinkSplitter`, reclassifying
    /// `<think>…</think>` spans as `ThinkingDelta` instead of `TextDelta`.
    /// Set from `vendor.inlines_think_tags()` in `with_vendor`, and
    /// overridable via `with_inline_think` (the `Config::inline_think_tags`
    /// escape hatch). Default `false`.
    pub split_inline_think: bool,
}

impl OpenAiProvider {
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
            split_inline_think: false,
        }
    }

    /// v0.9.3: attach a vendor. Also sets `split_inline_think` from the
    /// vendor's `inlines_think_tags()` — read it BEFORE the vendor moves
    /// into the struct.
    pub fn with_vendor(mut self, vendor: Box<dyn crate::vendor::Vendor>) -> Self {
        self.split_inline_think = vendor.inlines_think_tags();
        self.vendor = Some(vendor);
        self
    }

    /// Config escape hatch for `split_inline_think`: `Config::inline_think_tags`.
    pub fn with_inline_think(mut self, on: bool) -> Self {
        self.split_inline_think = on;
        self
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
    /// OpenAI accepts either a string (text-only) OR a content array
    /// with `{type:"text",...}` and `{type:"image_url",...}` blocks
    /// (multimodal, user-role only). Using `Value` here so both
    /// shapes serialize naturally.
    content: Value,
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
fn build_request<'a>(ctx: &'a Context, model: &'a str) -> WireRequest<'a> {
    let mut messages: Vec<WireMessage> = Vec::new();

    // OpenAI puts system in the messages array as role:system.
    if let Some(sys) = &ctx.system {
        messages.push(WireMessage {
            role: "system".into(),
            content: json!(sys),
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
                    content: json!(text),
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
                    content: json!(text),
                    tool_call_id: None,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                });
            }
            crate::agent::context::ContextMessage::Tool {
                tool_call_id,
                content,
                is_error,
                images,
            } => {
                // The tool-role message itself is text-only — OpenAI's
                // spec doesn't allow multimodal here. When there are
                // images AND the model supports vision, we push a
                // follow-up user message right after carrying the
                // actual pixels as `image_url` data-URL blocks. This
                // is the same trick most OpenAI-compatible gateways
                // use to route vision to the underlying model.
                let has_images = !images.is_empty();
                let vision_ok = crate::agent::thinking::supports_vision(model);
                let payload = if !has_images {
                    content.clone()
                } else if vision_ok {
                    format!("{} (image(s) attached in the next message)", content)
                } else {
                    format!(
                        "{} ({} image(s) omitted — model does not support vision)",
                        content,
                        images.len()
                    )
                };
                let _ = is_error; // is_error is content-only, no wire field on tool role
                messages.push(WireMessage {
                    role: "tool".into(),
                    content: json!(payload),
                    tool_call_id: Some(tool_call_id.clone()),
                    tool_calls: None,
                });
                if has_images && vision_ok {
                    let mut blocks: Vec<Value> = Vec::new();
                    blocks.push(json!({
                        "type": "text",
                        "text": format!(
                            "(image content from tool_call {})",
                            tool_call_id
                        ),
                    }));
                    for img in images {
                        blocks.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!(
                                    "data:{};base64,{}",
                                    img.media_type, img.data_base64
                                )
                            }
                        }));
                    }
                    messages.push(WireMessage {
                        role: "user".into(),
                        content: Value::Array(blocks),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
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
        let body_wire = build_request(ctx, &self.model);
        // v0.9.3: serialize to Value so a vendor can add
        // reasoning_effort / thinking / enable_thinking fields.
        let mut body = serde_json::to_value(&body_wire).unwrap_or(serde_json::Value::Null);
        if let (Some(vendor), Some(level)) = (self.vendor.as_deref(), ctx.thinking) {
            if vendor.supports_thinking(&self.model) {
                vendor.write_thinking(&mut body, level, &self.model);
            }
        }

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
            // Reqwest's client has no built-in send timeout. If the
            // gateway accepts the TCP connection but never sends the
            // response headers, `.send()` blocks forever — the retry
            // loop can't advance because it never sees a result to
            // classify. Wrap in tokio::time::timeout so a slow/hung
            // gateway triggers a retryable "send timeout" error
            // instead of hanging the whole session.
            //
            // 60 seconds covers realistic queued-gateway waits; longer
            // hangs are almost always a dead connection. We don't put
            // a timeout on the body stream itself, since streaming
            // completions legitimately last minutes.
            let send_fut = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send();
            let send_res =
                match tokio::time::timeout(std::time::Duration::from_secs(60), send_fut).await {
                    Ok(res) => res,
                    Err(_elapsed) => {
                        // Synthesize a retryable transport error so the
                        // existing retry/report code path handles it.
                        if attempt >= retry.max_attempts {
                            return Err(OpenAiError::Api(
                                "send timeout: no response headers after 60s".into(),
                            ));
                        }
                        let delay =
                            crate::provider::retry::compute_delay(attempt, &retry, None, rand01());
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
                    let hint = crate::provider::retry::parse_retry_after(resp.headers());
                    let body_text = resp.text().await.unwrap_or_default();
                    let clean =
                        extract_error_message(&body_text).unwrap_or_else(|| body_text.clone());
                    let retryable = crate::provider::retry::is_retryable_status(status.as_u16())
                        || crate::provider::retry::is_retryable_message(&clean);
                    if attempt >= retry.max_attempts || !retryable {
                        // Flatten only on the way out — `retryable` above
                        // is decided on the full text so truncation can't
                        // hide a retry keyword.
                        return Err(OpenAiError::Status {
                            status: status.as_u16(),
                            body: crate::provider::retry::flatten_error_body(&clean, 300),
                        });
                    }
                    let delay =
                        crate::provider::retry::compute_delay(attempt, &retry, hint, rand01());
                    eprintln!(
                        "[retrying ({}/{}) after {:.1}s: HTTP {} {}]",
                        attempt + 1,
                        retry.max_attempts,
                        delay.as_secs_f64(),
                        status.as_u16(),
                        crate::provider::retry::flatten_error_body(&clean, 100),
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
                    let delay =
                        crate::provider::retry::compute_delay(attempt, &retry, None, rand01());
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

            let byte_stream = resp.bytes_stream().map_err(|e| OpenAiError::Http(e));
            let sse = SseStream::new(byte_stream);

            let mut pending: std::collections::HashMap<u32, PendingToolCall> = Default::default();
            let mut emitted_call_ids: std::collections::HashSet<String> = Default::default();
            let mut started = false;
            let mut usage_local = Usage::default();
            let mut splitter = InlineThinkSplitter::new();

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
                        let delay =
                            crate::provider::retry::compute_delay(attempt, &retry, None, rand01());
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
                            if self.split_inline_think {
                                for seg in splitter.push(t) {
                                    let ev = match seg {
                                        Segment::Text(text) => AgentEvent::TextDelta {
                                            content_index: choice.index,
                                            text,
                                        },
                                        Segment::Think(text) => AgentEvent::ThinkingDelta {
                                            content_index: choice.index,
                                            text,
                                        },
                                    };
                                    let _ = tx.send(ev).await;
                                }
                            } else {
                                let _ = tx
                                    .send(AgentEvent::TextDelta {
                                        content_index: choice.index,
                                        text: t.clone(),
                                    })
                                    .await;
                            }
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
                        let entry = pending.entry(idx).or_default();
                        entry.apply_delta(tc);
                    }
                    // Flush completed tool calls on finish_reason.
                    if let Some(fr) = &choice.finish_reason {
                        flush_pending_tool_calls(
                            &mut pending,
                            &mut emitted_call_ids,
                            &tx,
                            choice.index,
                        )
                        .await;
                        // Drain any buffered splitter state BEFORE Done —
                        // an event arriving after Done is a reordering
                        // bug. finish() resets state, so a second drain
                        // after the loop (stream closed without an
                        // explicit finish_reason) is harmless.
                        if self.split_inline_think {
                            for seg in splitter.finish() {
                                let ev = match seg {
                                    Segment::Text(text) => AgentEvent::TextDelta {
                                        content_index: choice.index,
                                        text,
                                    },
                                    Segment::Think(text) => AgentEvent::ThinkingDelta {
                                        content_index: choice.index,
                                        text,
                                    },
                                };
                                let _ = tx.send(ev).await;
                            }
                        }
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
            // Drain any splitter state still buffered (unclosed <think>,
            // partial delimiter prefix) so nothing is lost. Harmless if
            // the finish_reason branch above already drained it.
            if self.split_inline_think {
                for seg in splitter.finish() {
                    let ev = match seg {
                        Segment::Text(text) => AgentEvent::TextDelta {
                            content_index: 0,
                            text,
                        },
                        Segment::Think(text) => AgentEvent::ThinkingDelta {
                            content_index: 0,
                            text,
                        },
                    };
                    let _ = tx.send(ev).await;
                }
            }
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

impl PendingToolCall {
    /// Merge one streaming `tool_calls[]` delta into this pending call.
    /// Two subtleties worth calling out:
    ///
    /// * Some gateways (observed: minimax M3 through its OpenAI-compat
    ///   shim) stream a later delta carrying `function: { name: "" }`
    ///   as a continuation marker after the real name was set in the
    ///   first delta. Overwriting would leave the flushed call with an
    ///   empty name, which the router rejects as `"unknown tool: "` and
    ///   the model then retries verbatim in a tight loop. So an empty
    ///   `name` never clobbers an existing non-empty one.
    /// * Arguments can arrive either directly on the tool_call
    ///   (DeepSeek and some proxies) or nested under `function`
    ///   (OpenAI proper). Both paths append to `args_buf`.
    fn apply_delta(&mut self, tc: &WireToolCall) {
        if let Some(id) = &tc.id {
            self.id = Some(id.clone());
        }
        if let Some(args) = &tc.arguments {
            self.args_buf.push_str(args);
        }
        if let Some(func) = &tc.function {
            if let Some(name) = &func.name {
                if !name.is_empty() || self.name.is_none() {
                    self.name = Some(name.clone());
                }
            }
            if let Some(args) = &func.arguments {
                self.args_buf.push_str(args);
            }
        }
    }
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
        // `""` and `None` are treated the same — a nameless tool_call
        // from the gateway can't be dispatched, so surface it under the
        // literal "unknown" so the resulting error reads
        // `"unknown tool: unknown"` instead of `"unknown tool: "`.
        let raw = p
            .name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "unknown".into());
        let name = normalize_mangled_tool_name(&raw);
        let _ = tx
            .send(AgentEvent::ToolCall {
                content_index,
                call: ToolCall {
                    id,
                    name,
                    arguments: args,
                },
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::{ContextMessage, ToolSpec};
    use crate::event::AgentEvent;

    #[test]
    fn extract_error_from_plain_json() {
        let body = r#"{"error":{"message":"boom","type":"internal"}}"#;
        assert_eq!(extract_error_message(body).as_deref(), Some("boom"));
    }

    #[test]
    fn extract_error_from_flat_string() {
        let body = r#"{"error":"OAuth expired"}"#;
        assert_eq!(
            extract_error_message(body).as_deref(),
            Some("OAuth expired")
        );
    }

    #[test]
    fn extract_error_from_sse_stream() {
        let body = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"queue_timeout\",\"message\":\"queue full, retry later\"}}\n\ndata: [DONE]\n";
        assert_eq!(
            extract_error_message(body).as_deref(),
            Some("queue full, retry later")
        );
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

    /// When a tool returns an image AND the model supports vision,
    /// the OpenAI adapter emits the tool-role message as text-only
    /// (per spec) followed by a synthetic user-role message with the
    /// image encoded as an `image_url` data-URL block. That's how
    /// OpenAI-compatible gateways surface tool-supplied images to
    /// the underlying vision model.
    #[test]
    fn build_request_forwards_tool_images_via_follow_up_user_message() {
        let mut ctx = Context::default();
        ctx.push_tool_result_with_images(
            "call_A",
            "Read image file [image/png]",
            false,
            vec![crate::tool::ImageAttachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
        );
        let req = build_request(&ctx, "gpt-4o");
        // Two messages: the tool result + the follow-up user with image_url.
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "tool");
        assert_eq!(req.messages[0].tool_call_id.as_deref(), Some("call_A"));

        // Follow-up must be a user message with an array content
        // holding a text block + an image_url block.
        assert_eq!(req.messages[1].role, "user");
        let blocks = req.messages[1]
            .content
            .as_array()
            .expect("follow-up user should use array content for multimodal");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image_url");
        let url = blocks[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(url.ends_with("AAAA"));
    }

    /// If the model doesn't do vision, we don't emit the follow-up
    /// user message — just a text note on the tool result.
    #[test]
    fn build_request_skips_follow_up_for_text_only_model() {
        let mut ctx = Context::default();
        ctx.push_tool_result_with_images(
            "call_A",
            "Read image file [image/png]",
            false,
            vec![crate::tool::ImageAttachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
        );
        let req = build_request(&ctx, "some-text-only-model");
        assert_eq!(
            req.messages.len(),
            1,
            "no follow-up user for non-vision model"
        );
        assert_eq!(req.messages[0].role, "tool");
        let text = req.messages[0].content.as_str().unwrap();
        assert!(text.contains("image(s) omitted"), "got {text:?}");
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

    /// End-to-end wire-format regression for the `--continue` bug:
    ///
    ///   load_session (drops tools) → hydrate_resumed (must repopulate)
    ///   → build_request (must emit non-empty `tools`)
    ///
    /// Under the bug, `hydrate_resumed` left `ctx.tools` empty, so
    /// `WireRequest.tools` was `[]` — and because the field is marked
    /// `skip_serializing_if = Vec::is_empty`, the `tools` key was
    /// omitted from the outgoing JSON entirely. That's what made
    /// minimax-M3 leak its native tool-call sentinel tokens
    /// (`<]minimax[>`) into `content` as visible text after resume.
    #[test]
    fn resumed_session_outgoing_request_has_tools() {
        use crate::agent::build::SkillLoadPolicy;
        use crate::agent::loop_::{Agent, HooksConfig};
        use crate::agent::permission::PermissionGate;
        use crate::session::SessionEntry;
        use crate::tool::ToolRegistry;
        use crate::util::{time, uuid};

        let _g = crate::TEST_LOCK.lock().unwrap();
        let prev_home = std::env::var_os("NANOPI_HOME");
        let home = std::env::temp_dir().join(format!("nanopi-resume-tools-{}", uuid::v7()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("NANOPI_HOME", &home);

        let cwd = std::env::temp_dir().join(format!("nanopi-resume-cwd-{}", uuid::v7()));
        std::fs::create_dir_all(&cwd).unwrap();

        // Mint a session that already exercised a tool — this mirrors the
        // real 3.jsonl repro: prior turn made a tool_call, tool_result
        // returned, then the user asks a new question and hits --continue.
        let (path, _hdr) =
            crate::session::new_session(&cwd, "minimax-M3", "https://api.minimaxi.com/v1")
                .expect("new session");
        crate::session::append_entry(
            &path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "list this folder".into(),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path,
            &SessionEntry::ToolCall {
                id: "call_prior".into(),
                timestamp: time::now_iso8601(),
                tool_name: "ls".into(),
                arguments: json!({"path": "."}),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path,
            &SessionEntry::ToolResult {
                tool_call_id: "call_prior".into(),
                timestamp: time::now_iso8601(),
                content: "".into(),
                is_error: false,
                images: Vec::new(),
            },
        )
        .unwrap();

        // Full --continue pipeline: load + hydrate.
        let mut agent = Agent::load_session(&path, &cwd).expect("load");
        let registry = ToolRegistry::standard();
        let expected_tools = registry.all_specs().len();
        let _ = agent.hydrate_resumed(
            Box::new(OpenAiProvider::new(
                "https://api.minimaxi.com/v1",
                "",
                "minimax-M3",
            )),
            registry,
            PermissionGate::from_cli(false, None),
            HooksConfig::default(),
            "minimax-M3".into(),
            "https://api.minimaxi.com/v1".into(),
            "".into(),
            SkillLoadPolicy::default(),
            true,
            crate::agent::prompt_override::PromptOverrides::default(),
            &[],
        );

        // Wire body — serialize to JSON to actually exercise the
        // `skip_serializing_if` path that hid the bug.
        let req = build_request(&agent.context, "minimax-M3");
        let body = serde_json::to_value(&req).expect("serialize wire body");
        let tools = body.get("tools").expect(
            "outgoing request after --continue must include a `tools` key \
             (empty Vec is dropped by skip_serializing_if, which caused \
             minimax-M3 to leak sentinel tokens into content)",
        );
        let arr = tools.as_array().expect("`tools` must be a JSON array");
        assert_eq!(arr.len(), expected_tools);
        for t in arr {
            assert_eq!(t.get("type").and_then(|v| v.as_str()), Some("function"));
            assert!(t
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .is_some());
        }

        if let Some(p) = prev_home {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn build_request_includes_tool_messages() {
        let mut ctx = Context::default();
        ctx.push_user_text("do it");
        ctx.messages.push(ContextMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "ok".into(),
            is_error: false,
            images: Vec::new(),
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
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"comma")
        );
    }

    /// Regression: Xiaomi MiMo's OpenAI-compat endpoint sends an
    /// explicit `"tool_calls": null` (and `"content": null`) on chunks
    /// that carry only reasoning text. That used to kill the stream with
    /// `JSON parse error: invalid type: null, expected a sequence`.
    #[test]
    fn wire_chunk_tolerates_explicit_null_tool_calls() {
        let json = r#"{"id":"x","choices":[{"delta":{"content":null,"role":null,"tool_calls":null,"reasoning_content":"Hmm"},"finish_reason":null,"index":0}]}"#;
        let chunk: WireChunk = serde_json::from_str(json).unwrap();
        let delta = &chunk.choices[0].delta;
        assert!(delta.tool_calls.is_empty());
        assert_eq!(delta.content, None);
        assert_eq!(delta.reasoning_content.as_deref(), Some("Hmm"));

        // An absent field must still work (the `default` path).
        let chunk: WireChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"hi"},"index":0}]}"#).unwrap();
        assert!(chunk.choices[0].delta.tool_calls.is_empty());
    }

    /// Regression: a gateway that streams the tool name only in the
    /// first delta, then keeps sending `function.arguments` chunks
    /// (some also carrying `function: { name: "" }` as a continuation
    /// marker), must not have its final name clobbered to "" and end
    /// up as `"unknown tool: "` at the router. Repro of the
    /// minimax-M3 empty-tool-name retry loop observed in the wild.
    #[test]
    fn pending_tool_call_preserves_name_across_deltas() {
        let mut p = PendingToolCall::default();

        // Chunk 1: id + name + start of args
        let d1: WireToolCall = serde_json::from_str(
            r#"{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"co"}}"#,
        )
        .unwrap();
        p.apply_delta(&d1);

        // Chunk 2: continuation delta — name reappears as empty string
        // (this is what triggered the bug). Must NOT overwrite "bash".
        let d2: WireToolCall = serde_json::from_str(
            r#"{"index":0,"function":{"name":"","arguments":"mmand"}}"#,
        )
        .unwrap();
        p.apply_delta(&d2);

        // Chunk 3: more args, no name field at all
        let d3: WireToolCall =
            serde_json::from_str(r#"{"index":0,"function":{"arguments":"\":\"ls\"}"}}"#).unwrap();
        p.apply_delta(&d3);

        assert_eq!(p.name.as_deref(), Some("bash"));
        assert_eq!(p.id.as_deref(), Some("call_1"));
        assert_eq!(p.args_buf, r#"{"command":"ls"}"#);
    }

    /// If the gateway never sends a non-empty name AT ALL (also
    /// observed on minimax when args-only tool_calls arrive without a
    /// leading name delta), the flush path must not emit an empty
    /// name — replace it with "unknown" so the downstream error is
    /// legible as `"unknown tool: unknown"` and downstream loop-cap
    /// logic has a real name to fingerprint on.
    #[tokio::test]
    async fn flush_replaces_empty_name_with_unknown() {
        let mut pending: std::collections::HashMap<u32, PendingToolCall> = Default::default();
        pending.insert(
            0,
            PendingToolCall {
                id: Some("call_x".into()),
                name: Some(String::new()),
                args_buf: r#"{"command":"ls"}"#.into(),
            },
        );
        let mut emitted: std::collections::HashSet<String> = Default::default();
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(4);
        flush_pending_tool_calls(&mut pending, &mut emitted, &tx, 0).await;
        drop(tx);
        let ev = rx.recv().await.expect("expected a ToolCall event");
        match ev {
            AgentEvent::ToolCall { call, .. } => {
                assert_eq!(call.name, "unknown");
                assert_eq!(call.id, "call_x");
                assert_eq!(call.arguments, serde_json::json!({"command": "ls"}));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
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
        AgentEvent::Start {
            message_id: "x".into(),
        }
    }
}

//! Provider-agnostic context types.
//!
//! The agent loop builds a `Context` and hands it to a `Provider`. The
//! provider is responsible for translating to its wire format (OpenAI
//! messages, Anthropic messages, etc.).
//!
//! See `docs/v0.5-research.md` §1.5 for the design.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A complete conversation context (system + messages + tools).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    /// Optional system prompt. Anthropic uses a top-level field; OpenAI
    /// uses an initial `{role:"system"}` message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    #[serde(default)]
    pub messages: Vec<ContextMessage>,

    #[serde(default)]
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ContextMessage {
    User {
        #[serde(default)]
        content: Vec<ContentBlock>,
    },
    Assistant {
        #[serde(default)]
        content: Vec<AssistantBlock>,
    },
    /// OpenAI tool result (separate role).
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text { text: String },
    Thinking { text: String },
    ToolCall { call: ToolCallBlock },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallBlock {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Tool definition in provider-agnostic form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub parameters: Value,
}

impl Context {
    /// Append a user text message. Convenience for the common case.
    pub fn push_user_text(&mut self, text: impl Into<String>) {
        self.messages.push(ContextMessage::User {
            content: vec![ContentBlock::Text { text: text.into() }],
        });
    }

    /// Append an assistant text message.
    pub fn push_assistant_text(&mut self, text: impl Into<String>) {
        self.messages.push(ContextMessage::Assistant {
            content: vec![AssistantBlock::Text { text: text.into() }],
        });
    }

    /// Append a tool result message (OpenAI-style: separate role).
    ///
    /// Guard: if the context already contains a Tool message with the
    /// same `tool_call_id`, skip. Anthropic's API rejects requests where
    /// a single `tool_use_id` has multiple `tool_result` blocks, so we
    /// enforce that invariant at the source. This can happen if a
    /// gateway/proxy replays a tool_use id across turns or if our
    /// streaming parser ever misses a dedup.
    pub fn push_tool_result(
        &mut self,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) {
        let id_str = tool_call_id.into();
        if self.messages.iter().any(|m| {
            matches!(m, ContextMessage::Tool { tool_call_id: existing, .. } if existing == &id_str)
        }) {
            // Duplicate — silently drop. Emitting a warn! would spam
            // stderr on every reused id (some proxies do this per turn).
            return;
        }
        self.messages.push(ContextMessage::Tool {
            tool_call_id: id_str,
            content: content.into(),
            is_error,
        });
    }

    /// Rough size of the context in characters. Used as a proxy for
    /// tokens (chars/4 ≈ tokens for English/code) by the compaction
    /// trigger.
    pub fn estimate_chars(&self) -> usize {
        let mut n = self.system.as_deref().map(|s| s.len()).unwrap_or(0);
        for m in &self.messages {
            match m {
                ContextMessage::User { content } => {
                    for b in content {
                        if let ContentBlock::Text { text } = b {
                            n += text.len();
                        }
                    }
                }
                ContextMessage::Assistant { content } => {
                    for b in content {
                        match b {
                            AssistantBlock::Text { text } | AssistantBlock::Thinking { text } => {
                                n += text.len();
                            }
                            AssistantBlock::ToolCall { call } => {
                                n += call.name.len();
                                n += call.arguments.to_string().len();
                            }
                        }
                    }
                }
                ContextMessage::Tool { content, .. } => n += content.len(),
            }
        }
        n
    }

    /// Flatten a message range to a plain-text transcript for the
    /// summarization prompt. Roles are labeled; tool_calls and
    /// tool_results are rendered inline.
    pub fn flatten_range(&self, start: usize, end: usize) -> String {
        let mut out = String::new();
        for m in &self.messages[start..end] {
            match m {
                ContextMessage::User { content } => {
                    out.push_str("USER: ");
                    for b in content {
                        if let ContentBlock::Text { text } = b {
                            out.push_str(text);
                        }
                    }
                    out.push('\n');
                }
                ContextMessage::Assistant { content } => {
                    out.push_str("ASSISTANT: ");
                    for b in content {
                        match b {
                            AssistantBlock::Text { text } => out.push_str(text),
                            AssistantBlock::Thinking { .. } => {}
                            AssistantBlock::ToolCall { call } => {
                                out.push_str(&format!(
                                    "[tool_call {}({})]",
                                    call.name,
                                    call.arguments
                                ));
                            }
                        }
                    }
                    out.push('\n');
                }
                ContextMessage::Tool { content, is_error, .. } => {
                    out.push_str(if *is_error { "TOOL_ERROR: " } else { "TOOL: " });
                    out.push_str(content);
                    out.push('\n');
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_user_text_creates_correct_message() {
        let mut ctx = Context::default();
        ctx.push_user_text("hello");
        assert_eq!(ctx.messages.len(), 1);
        match &ctx.messages[0] {
            ContextMessage::User { content } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "hello"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected user message"),
        }
    }

    /// Guard rail: repeated `push_tool_result` with the same
    /// tool_call_id must not duplicate the message. Anthropic API
    /// rejects requests containing multiple tool_result blocks with the
    /// same tool_use_id.
    #[test]
    fn push_tool_result_drops_duplicate_ids() {
        let mut ctx = Context::default();
        ctx.push_tool_result("call_A", "first", false);
        ctx.push_tool_result("call_A", "second (should be dropped)", false);
        ctx.push_tool_result("call_B", "different id, kept", false);

        let tool_msgs: Vec<_> = ctx
            .messages
            .iter()
            .filter_map(|m| match m {
                ContextMessage::Tool { tool_call_id, content, .. } => {
                    Some((tool_call_id.clone(), content.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(tool_msgs.len(), 2, "expected exactly 2 Tool messages");
        assert_eq!(tool_msgs[0].0, "call_A");
        assert_eq!(tool_msgs[0].1, "first", "first push wins");
        assert_eq!(tool_msgs[1].0, "call_B");
    }

    #[test]
    fn serialize_roundtrips() {
        let mut ctx = Context::default();
        ctx.system = Some("You are helpful.".into());
        ctx.push_user_text("hi");
        ctx.push_assistant_text("hello!");
        ctx.push_tool_result("call_1", "ok", false);

        let s = serde_json::to_string(&ctx).unwrap();
        let back: Context = serde_json::from_str(&s).unwrap();
        assert_eq!(back.system.as_deref(), Some("You are helpful."));
        assert_eq!(back.messages.len(), 3);
    }

    #[test]
    fn estimate_chars_sums_all_content() {
        let mut ctx = Context::default();
        ctx.system = Some("hi".into());          // 2
        ctx.push_user_text("hello");             // 5
        ctx.push_assistant_text("world");        // 5
        ctx.push_tool_result("id", "output", false); // 6
        assert_eq!(ctx.estimate_chars(), 2 + 5 + 5 + 6);
    }

    #[test]
    fn flatten_range_labels_roles() {
        let mut ctx = Context::default();
        ctx.push_user_text("hi");
        ctx.push_assistant_text("hello");
        ctx.push_tool_result("id", "output", false);
        let s = ctx.flatten_range(0, 3);
        assert!(s.contains("USER: hi"));
        assert!(s.contains("ASSISTANT: hello"));
        assert!(s.contains("TOOL: output"));
    }

    #[test]
    fn tool_spec_serializes_with_parameters() {
        let spec = ToolSpec {
            name: "bash".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }),
        };
        let s = serde_json::to_string(&spec).unwrap();
        assert!(s.contains("\"name\":\"bash\""));
        assert!(s.contains("\"parameters\""));
    }
}
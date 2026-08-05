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
    pub fn push_tool_result(
        &mut self,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) {
        self.messages.push(ContextMessage::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
            is_error,
        });
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
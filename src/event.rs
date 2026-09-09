//! Unified agent event stream.
//!
//! Both OpenAI-compatible and (future) Anthropic-compatible providers
//! produce a stream of `AgentEvent`s. The agent loop, renderers, and
//! session persistence all consume this single type.
//!
//! See `docs/v0.5-research.md` §1.3 for the design rationale.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One event in the agent's stream-of-consciousness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Stream started; carries the empty assistant message skeleton id.
    Start { message_id: String },

    /// Incremental text content (the model's words).
    TextDelta { content_index: u32, text: String },

    /// Incremental thinking content (Anthropic-specific; ignored by OpenAI).
    ThinkingDelta { content_index: u32, text: String },

    /// A complete tool call. Provider adapters buffer partial JSON until
    /// the call block is closed, then emit one `ToolCall` with parsed args.
    ToolCall { content_index: u32, call: ToolCall },

    /// A `tool_execution_start` hook rewrote a call's arguments.
    ///
    /// `ToolCall` above is forwarded straight off the provider stream,
    /// so it necessarily carries what the MODEL asked for — the hook
    /// has not run yet. Without this event the card would keep showing
    /// `echo hello` while `echo REWRITTEN` actually ran, which is
    /// exactly what happened in manual testing: the model saw its own
    /// request come back with a different answer and invented a
    /// sandbox to explain it, then spent a turn investigating.
    ///
    /// Emitted only when the arguments actually changed, so the common
    /// case costs nothing.
    ToolCallRewritten {
        call_id: String,
        tool_name: String,
        arguments: Value,
    },

    /// A tool call finished. Emitted by `Agent::execute_tool_calls`
    /// after the local tool ran, NOT by any provider. Carries the
    /// output content, error flag, and wall-clock duration so the
    /// renderer can draw a single card with command + output + timing
    /// (PI-style — see docs/img/PI_talk02.jpg).
    ToolResult {
        call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
        elapsed_ms: u64,
    },

    /// Stream finished; carries final usage.
    Done {
        finish_reason: FinishReason,
        usage: Usage,
    },

    /// Stream errored.
    Error { error: String },

    /// Auto-compaction started because the context grew past the
    /// model's window minus reserve. Emitted by Agent::compact_now
    /// when called from maybe_compact. Manual /compact does NOT emit
    /// these (the palette handler drives its own UI feedback).
    CompactionStart {
        /// Why it fired: "threshold" (approaching window) or "manual".
        reason: String,
    },

    /// Auto-compaction finished. Renderer draws a scrollback marker.
    CompactionEnd {
        /// How many messages were folded into the summary.
        replaced_count: usize,
        /// True if the LLM actually summarized; false = placeholder fallback.
        used_llm: bool,
    },

    /// User invoked a skill via `/skill:name [args]`. Emitted after
    /// expansion, before the user message hits the model. Mirrors PI's
    /// `SkillInvocation` display path (see
    /// `pi/packages/coding-agent/src/modes/interactive/components/
    /// skill-invocation-message.ts`). The TUI renders this as a
    /// collapsible card; other modes may ignore it.
    SkillInvocation {
        name: String,
        location: String,
        base_dir: String,
        /// SKILL.md body with frontmatter stripped, trimmed.
        body: String,
        /// Optional extra user text following the command.
        user_message: Option<String>,
    },
}

/// A message injected into the agent loop mid-turn (Pi parity:
///
/// - `SteeringMessages` get pushed into the conversation as a fresh
///   user turn as soon as the current iteration ends, so the LLM sees
///   them in its next call.
/// - `FollowUpMessages` are queued and run AFTER the current turn
///   returns — they don't interrupt the model mid-response, but they
///   fire as a follow-on turn instead of letting the agent idle.
///
/// `tui.rs` owns the receiving end of these (`mpsc::Receiver`); the
/// TUI's input handler sends `SteerMessage` whenever the user types
/// while the agent is working. Non-interactive modes (`-p`, scripts)
/// pass `None` and ignore this entirely — their loop is single-turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SteerMessage {
    /// Push this text as a new user message at the next iteration
    /// boundary. Matches Pi's `getSteeringMessages()`.
    Steering { text: String },
    /// Queue this text for execution AFTER the current turn ends.
    /// Matches Pi's `getFollowUpMessages()`.
    FollowUp { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural end of message.
    Stop,
    /// Model wants to call tool(s); agent loop continues.
    ToolCalls,
    /// Hit max_tokens.
    Length,
    /// Anthropic-only: model refused.
    Refusal,
    /// Anything else.
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// OpenAI cached + Anthropic cache_read.
    pub cache_read_tokens: u32,
    /// Anthropic cache_creation.
    pub cache_write_tokens: u32,
}

impl AgentEvent {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_uses_tagged_format() {
        let e = AgentEvent::TextDelta {
            content_index: 0,
            text: "hi".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"type":"text_delta","content_index":0,"text":"hi"}"#);
    }

    #[test]
    fn deserialize_roundtrip_all_variants() {
        let variants = vec![
            AgentEvent::Start {
                message_id: "msg_1".into(),
            },
            AgentEvent::TextDelta {
                content_index: 0,
                text: "hello".into(),
            },
            AgentEvent::ThinkingDelta {
                content_index: 0,
                text: "hmm".into(),
            },
            AgentEvent::ToolCall {
                content_index: 0,
                call: ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                },
            },
            AgentEvent::Done {
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            },
            AgentEvent::Error {
                error: "timeout".into(),
            },
        ];
        for v in &variants {
            let s = serde_json::to_string(v).unwrap();
            let back: AgentEvent = serde_json::from_str(&s).unwrap();
            // Compare as Values, not strings: a roundtrip may reorder
            // keys, which is not a difference we care about.
            assert_eq!(
                serde_json::to_value(v).unwrap(),
                serde_json::to_value(&back).unwrap(),
                "roundtrip failed for {s}"
            );
        }
    }

    #[test]
    fn finish_reason_serializes_as_snake_case() {
        let r = FinishReason::ToolCalls;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, "\"tool_calls\"");
    }

    #[test]
    fn usage_default_is_zero() {
        let u = Usage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
    }
}

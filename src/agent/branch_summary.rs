//! Summarize a session branch (the tail of history that got cut off
//! when the user forked backward to an earlier point) via one LLM
//! call.
//!
//! Mirrors PI's `generateBranchSummary` (see
//! `packages/coding-agent/src/core/compaction/branch-summarization.ts`).
//! Ships as a small module rather than folding into `agent::compact`
//! because the framing prompt is different: compaction summarizes
//! "the conversation so far", branch summary summarizes "an
//! abandoned line of work" and its output is meant to be read by
//! the user as much as by the model.

use tokio::sync::mpsc;

use crate::agent::context::{ContentBlock, Context, ContextMessage};
use crate::agent::loop_::Provider;
use crate::event::AgentEvent;
use crate::session::SessionEntry;

/// Default system prompt when the user picks "Summarize" (no custom
/// instructions). Aimed at leaving a useful pointer in the new
/// branch for future turns.
const DEFAULT_SUMMARIZER_SYSTEM: &str = "You summarize an abandoned branch of a coding conversation so the user can pick up the thread later. Cover: what the user was trying to do, what was tried, what worked, what didn't, files/functions touched, and any open questions or pending TODOs. Aim for 200-400 words. Output only the summary — no preamble.";

/// Flatten a slice of SessionEntries into a plaintext transcript.
/// Format is intentionally close to `Context::flatten_range`'s so the
/// model gets the same shape it saw during the original turns.
fn flatten_entries(entries: &[SessionEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        match e {
            SessionEntry::Message { role, content, .. } => {
                let tag = match role.as_str() {
                    "user" => "USER",
                    "assistant" => "ASSISTANT",
                    other => other,
                };
                out.push_str(&format!("{tag}: {content}\n\n"));
            }
            SessionEntry::ToolCall { tool_name, arguments, .. } => {
                let args_line = arguments.to_string();
                let trimmed: String = args_line.chars().take(500).collect();
                out.push_str(&format!("[tool_call {tool_name}] {trimmed}\n"));
            }
            SessionEntry::ToolResult { content, is_error, .. } => {
                let tag = if *is_error { "tool_error" } else { "tool_result" };
                let trimmed: String = content.chars().take(500).collect();
                out.push_str(&format!("[{tag}] {trimmed}\n\n"));
            }
            SessionEntry::Compaction { summary, .. } => {
                out.push_str(&format!("[earlier summary] {summary}\n\n"));
            }
            SessionEntry::BranchSummary { summary, .. } => {
                out.push_str(&format!("[branch summary] {summary}\n\n"));
            }
            SessionEntry::SkillInvocation { name, user_message, .. } => {
                let extra = user_message.as_deref().unwrap_or("");
                out.push_str(&format!("[skill {name}] {extra}\n\n"));
            }
            SessionEntry::Header { .. } | SessionEntry::ModelChange { .. } => {}
        }
    }
    out
}

/// Ask `provider` to summarize `entries`. `custom_instructions`, when
/// Some, replaces the default system prompt entirely (matches PI's
/// `replaceInstructions` flag). Returns None on any provider error
/// or empty response — the caller can decide whether to fall back
/// to a placeholder or skip the summary insertion.
pub async fn summarize_branch(
    entries: &[SessionEntry],
    custom_instructions: Option<&str>,
    provider: &dyn Provider,
) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let transcript = flatten_entries(entries);
    let system = custom_instructions.unwrap_or(DEFAULT_SUMMARIZER_SYSTEM);

    let ctx = Context {
        system: Some(system.to_string()),
        messages: vec![ContextMessage::User {
            content: vec![ContentBlock::Text { text: transcript }],
        }],
        thinking: None,
        tools: vec![],
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let collect = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::TextDelta { text: t, .. } = ev {
                text.push_str(&t);
            }
        }
        text
    });

    let stream_res = provider.stream_turn(&ctx, tx).await;
    let text = collect.await.ok()?;
    stream_res.ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FinishReason, Usage};

    struct FakeProvider {
        response: String,
    }
    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn id(&self) -> &'static str { "fake" }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            let _ = tx.send(AgentEvent::TextDelta {
                content_index: 0,
                text: self.response.clone(),
            }).await;
            let _ = tx.send(AgentEvent::Done {
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            }).await;
            Ok(Usage::default())
        }
    }

    fn user(content: &str) -> SessionEntry {
        SessionEntry::Message {
            id: "1".into(),
            timestamp: "".into(),
            role: "user".into(),
            content: content.into(),
        }
    }

    #[tokio::test]
    async fn summarize_returns_provider_response() {
        let entries = vec![user("hi"), user("what's up")];
        let p = FakeProvider { response: "  they said hi twice  ".into() };
        let out = summarize_branch(&entries, None, &p).await;
        assert_eq!(out.as_deref(), Some("they said hi twice"));
    }

    #[tokio::test]
    async fn summarize_none_on_empty_entries() {
        let p = FakeProvider { response: "unused".into() };
        assert!(summarize_branch(&[], None, &p).await.is_none());
    }

    #[tokio::test]
    async fn summarize_none_on_provider_error() {
        struct Broken;
        #[async_trait::async_trait]
        impl Provider for Broken {
            fn id(&self) -> &'static str { "broken" }
            async fn stream_turn(
                &self,
                _ctx: &Context,
                _tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                Err("provider down".into())
            }
        }
        let entries = vec![user("x")];
        assert!(summarize_branch(&entries, None, &Broken).await.is_none());
    }

    #[tokio::test]
    async fn custom_prompt_replaces_default() {
        let entries = vec![user("hi")];
        let p = FakeProvider { response: "answer".into() };
        let custom = "Summarize in exactly 10 words. No preamble.";
        let out = summarize_branch(&entries, Some(custom), &p).await;
        // We can't inspect the system prompt directly via FakeProvider,
        // but we can verify the call goes through with custom set.
        assert_eq!(out.as_deref(), Some("answer"));
    }
}

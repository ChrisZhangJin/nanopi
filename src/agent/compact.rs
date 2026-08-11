//! Context compaction — condense old messages into a summary so long
//! conversations stay under the model's context window.
//!
//! Trigger: `Context::estimate_chars() > MAX_CONTEXT_CHARS`.
//! Strategy: keep the last N messages verbatim, summarize everything
//! before that via a one-shot LLM call, replace the summarized range
//! with a single "[Prior conversation summary]" user message.
//!
//! Fallback: if the LLM call fails or returns empty, replace the range
//! with a plain "[N earlier messages truncated]" placeholder. The
//! conversation continues; nothing errors up.

use tokio::sync::mpsc;

use crate::agent::context::{ContentBlock, Context, ContextMessage};
use crate::agent::loop_::Provider;
use crate::event::AgentEvent;

/// Fallback char-count threshold used when we don't know the model's
/// context window (unknown model id). Roughly 25k tokens. Prefer the
/// token-based check in `should_auto_compact` when a window is known.
pub const MAX_CONTEXT_CHARS: usize = 100_000;

/// Space (in tokens) to keep between "current context" and the model's
/// hard window when auto-compacting. Matches PI (see
/// `packages/coding-agent/src/core/compaction/compaction.ts:134`).
pub const RESERVE_TOKENS: u32 = 16_384;

/// Rough chars-per-token used to convert a char estimate into tokens
/// when we haven't yet seen a real Usage number for this session.
/// PI's tokenizer is more accurate; 4 chars ≈ 1 token is the industry
/// rule of thumb and lands within ~30% for English + code.
pub const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

/// Keep at least this many trailing messages verbatim after compaction.
/// Roughly 4 turn pairs.
pub const KEEP_LAST_MESSAGES: usize = 8;

/// Decide whether an auto-compact should fire.
///
/// - When `context_window` is `Some(w)`: fire if
///   `estimated_tokens + RESERVE_TOKENS > w`. This matches PI.
/// - When it's `None` (unknown model): fall back to
///   `estimated_chars >= MAX_CONTEXT_CHARS`.
pub fn should_auto_compact(estimated_chars: usize, context_window: Option<u32>) -> bool {
    match context_window {
        Some(window) => {
            let est_tokens = (estimated_chars / CHARS_PER_TOKEN_ESTIMATE) as u32;
            est_tokens.saturating_add(RESERVE_TOKENS) > window
        }
        None => estimated_chars >= MAX_CONTEXT_CHARS,
    }
}

/// System prompt used to drive the summarization LLM call.
///
/// Ported from PI's SUMMARIZATION_PROMPT
/// (`packages/coding-agent/src/core/compaction/compaction.ts:467-498`).
/// The structured sections encourage the model to preserve the specific
/// dimensions that matter for continuing work: what the user wants,
/// what's been done, what's next, what to keep verbatim. A free-form
/// "write a summary" prompt tends to lose exact file paths and task
/// state on long conversations.
const SUMMARIZER_SYSTEM: &str = "You compress conversations. \
The messages provided are a conversation to summarize. Create a \
structured context checkpoint summary that another LLM will use to \
continue the work.\n\n\
Use this EXACT format:\n\n\
## Goal\n\
[What is the user trying to accomplish? Can be multiple items if the \
session covers different tasks.]\n\n\
## Constraints & Preferences\n\
- [Any constraints, preferences, or requirements mentioned by user]\n\
- [Or \"(none)\" if none were mentioned]\n\n\
## Progress\n\
### Done\n\
- [x] [Completed tasks/changes]\n\n\
### In Progress\n\
- [ ] [Current work]\n\n\
### Blocked\n\
- [Issues preventing progress, if any]\n\n\
## Key Decisions\n\
- **[Decision]**: [Brief rationale]\n\n\
## Next Steps\n\
1. [Ordered list of what should happen next]\n\n\
## Critical Context\n\
- [Any data, examples, or references needed to continue]\n\
- [Or \"(none)\" if not applicable]\n\n\
Keep each section concise. Preserve exact file paths, function \
names, and error messages. Output only the summary — no preamble, no \
meta-commentary.";

/// System prompt for INCREMENTAL compaction — when the context already
/// contains a prior summary from an earlier compact pass, merge the
/// new range into it instead of overwriting. Ported from PI's
/// `UPDATE_SUMMARIZATION_PROMPT` (`compaction.ts:500-537`). The caller
/// supplies the previous summary in a `<previous-summary>...</previous-
/// summary>` block appended to the transcript.
const SUMMARIZER_UPDATE_SYSTEM: &str = "You compress conversations. \
The messages provided are NEW conversation messages to incorporate \
into the existing summary provided in <previous-summary> tags.\n\n\
Update the existing structured summary with new information. RULES:\n\
- PRESERVE all existing information from the previous summary\n\
- ADD new progress, decisions, and context from the new messages\n\
- UPDATE the Progress section: move items from \"In Progress\" to \
\"Done\" when completed\n\
- UPDATE \"Next Steps\" based on what was accomplished\n\
- PRESERVE exact file paths, function names, and error messages\n\
- If something is no longer relevant, you may remove it\n\n\
Use this EXACT format:\n\n\
## Goal\n\
[Preserve existing goals, add new ones if the task expanded]\n\n\
## Constraints & Preferences\n\
- [Preserve existing, add new ones discovered]\n\n\
## Progress\n\
### Done\n\
- [x] [Include previously done items AND newly completed items]\n\n\
### In Progress\n\
- [ ] [Current work - update based on progress]\n\n\
### Blocked\n\
- [Current blockers - remove if resolved]\n\n\
## Key Decisions\n\
- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n\
## Next Steps\n\
1. [Update based on current state]\n\n\
## Critical Context\n\
- [Preserve important context, add new if needed]\n\n\
Keep each section concise. Preserve exact file paths, function \
names, and error messages. Output only the updated summary.";

/// Result of one successful compaction pass.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The generated summary text.
    pub summary: String,
    /// How many messages the summary replaced.
    pub replaced_count: usize,
    /// True if the LLM was used; false if we fell back to a placeholder.
    pub used_llm: bool,
}

/// Find the boundary index where compaction should split messages.
/// Returns `Some(i)` such that `messages[0..i]` should be summarized and
/// `messages[i..]` kept. Returns `None` if no clean boundary is
/// available (too few messages, or no user message near the tail).
///
/// A "clean" boundary is a User message — starting the kept segment on a
/// tool result or an orphaned assistant tool_call would confuse the
/// provider's message-role validation.
pub fn find_compact_boundary(messages: &[ContextMessage], keep_last_n: usize) -> Option<usize> {
    if messages.len() <= keep_last_n + 1 {
        // Not enough to bother — need at least one message to actually
        // compact plus the trailing keep-set.
        return None;
    }
    let start_hint = messages.len().saturating_sub(keep_last_n);
    for i in start_hint..messages.len() {
        if matches!(messages[i], ContextMessage::User { .. }) {
            if i == 0 {
                return None;
            }
            return Some(i);
        }
    }
    None
}

/// Ask `provider` to summarize `transcript`. Returns None on any failure
/// (network, empty response) so the caller can fall back to a
/// placeholder without erroring the turn.
pub async fn summarize_via_provider(
    provider: &dyn Provider,
    transcript: &str,
    previous_summary: Option<&str>,
) -> Option<String> {
    // Pick the fresh vs. update prompt based on whether we've compacted
    // before this session. On update, append the prior summary as a
    // <previous-summary> block so the model has explicit access to
    // what it must preserve — matches PI's shape (see UPDATE_SUMMARIZATION_PROMPT
    // in `compaction.ts:500`).
    let (system, user_text) = match previous_summary {
        Some(prev) => (
            SUMMARIZER_UPDATE_SYSTEM,
            format!("{transcript}\n\n<previous-summary>\n{prev}\n</previous-summary>"),
        ),
        None => (SUMMARIZER_SYSTEM, transcript.to_string()),
    };

    let ctx = Context {
        system: Some(system.into()),
        messages: vec![ContextMessage::User {
            content: vec![ContentBlock::Text { text: user_text }],
        }],
        tools: vec![],
        thinking: None,
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
    // tx dropped inside stream_turn; rx.recv returns None; collect completes.
    let text = collect.await.ok()?;
    stream_res.ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Marker prefix that identifies a compaction-generated summary
/// message inserted at the head of the context. `compact()` uses this
/// to detect a prior summary on second and later passes and feed it
/// into the UPDATE prompt for incremental compaction.
pub(crate) const PRIOR_SUMMARY_PREFIX: &str = "[Prior conversation summary]\n\n";

/// If the first context message is a compaction-inserted summary,
/// return its body (without the marker prefix). Returns None otherwise.
fn extract_prior_summary(messages: &[ContextMessage]) -> Option<String> {
    let ContextMessage::User { content, .. } = messages.first()? else {
        return None;
    };
    let ContentBlock::Text { text } = content.first()? else {
        return None;
    };
    text.strip_prefix(PRIOR_SUMMARY_PREFIX).map(str::to_string)
}

/// Compact `ctx` in place: summarize the head, keep the tail.
/// Returns `Some(CompactionResult)` if anything was compacted, `None`
/// if no boundary was found (in which case ctx is unchanged).
///
/// On the first pass, the whole head range is summarized fresh. On
/// subsequent passes, the prior "[Prior conversation summary]" message
/// is detected and passed as `previous_summary` so the LLM MERGES the
/// new range into the existing summary rather than starting over.
/// Matches PI's `runSessionCompaction` update path.
pub async fn compact(ctx: &mut Context, provider: &dyn Provider) -> Option<CompactionResult> {
    let cut = find_compact_boundary(&ctx.messages, KEEP_LAST_MESSAGES)?;

    // Detect a prior summary at position 0 so we can drive incremental
    // merge. If present, the messages being summarized are 1..cut
    // (skipping the summary itself); the prior text is fed as context
    // rather than re-summarized.
    let prior = extract_prior_summary(&ctx.messages);
    let (summarize_start, replaced_count) = if prior.is_some() {
        (1usize, cut)
    } else {
        (0usize, cut)
    };
    let transcript = ctx.flatten_range(summarize_start, cut);

    let (summary, used_llm) =
        match summarize_via_provider(provider, &transcript, prior.as_deref()).await {
            Some(s) => (s, true),
            None => (
                format!(
                    "[{} earlier messages truncated to save tokens]",
                    replaced_count
                ),
                false,
            ),
        };

    let kept: Vec<ContextMessage> = ctx.messages.drain(cut..).collect();
    ctx.messages.clear();
    ctx.messages.push(ContextMessage::User {
        content: vec![ContentBlock::Text {
            text: format!("{PRIOR_SUMMARY_PREFIX}{summary}"),
        }],
    });
    ctx.messages.extend(kept);

    Some(CompactionResult {
        summary,
        replaced_count,
        used_llm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::{AssistantBlock, ToolCallBlock};
    use crate::event::{FinishReason, Usage};

    fn user(text: &str) -> ContextMessage {
        ContextMessage::User {
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
    fn assistant(text: &str) -> ContextMessage {
        ContextMessage::Assistant {
            content: vec![AssistantBlock::Text { text: text.into() }],
        }
    }
    fn assistant_tool_call(id: &str, name: &str) -> ContextMessage {
        ContextMessage::Assistant {
            content: vec![AssistantBlock::ToolCall {
                call: ToolCallBlock {
                    id: id.into(),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                },
            }],
        }
    }
    fn tool_result(id: &str, content: &str) -> ContextMessage {
        ContextMessage::Tool {
            tool_call_id: id.into(),
            content: content.into(),
            is_error: false,
            images: Vec::new(),
        }
    }

    #[test]
    fn should_auto_compact_fires_when_close_to_window() {
        // Claude Opus window ≈ 200_000 tokens. Estimated 190k tokens ≈
        // 760_000 chars. With 16k reserve, 190k + 16k = 206k > 200k → fire.
        assert!(should_auto_compact(760_000, Some(200_000)));
    }

    #[test]
    fn should_auto_compact_holds_when_below_reserve() {
        // 150k tokens = 600k chars. 150k + 16k = 166k < 200k → hold.
        assert!(!should_auto_compact(600_000, Some(200_000)));
    }

    #[test]
    fn should_auto_compact_falls_back_to_char_check_for_unknown_model() {
        assert!(!should_auto_compact(99_000, None));
        assert!(should_auto_compact(100_001, None));
    }

    #[test]
    fn boundary_none_when_too_few_messages() {
        let msgs = vec![user("a"), assistant("b")];
        assert_eq!(find_compact_boundary(&msgs, 8), None);
    }

    #[test]
    fn boundary_lands_on_user_message() {
        // 10 messages, keep_last_n = 4. hint = 6.
        // We should find a User at index 6+ (or later).
        let msgs = vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            user("u3"),
            assistant("a3"),
            user("u4"), // idx 6 — clean boundary
            assistant("a4"),
            user("u5"),
            assistant("a5"),
        ];
        let cut = find_compact_boundary(&msgs, 4).unwrap();
        assert!(matches!(&msgs[cut], ContextMessage::User { .. }));
        assert!(cut >= 6);
    }

    #[test]
    fn boundary_walks_past_orphan_tool_result() {
        // The natural "keep last 4" starts at index 3, which is a tool
        // result. Boundary should walk forward until the next user.
        let msgs = vec![
            user("u1"),
            assistant_tool_call("t1", "bash"),
            tool_result("t1", "ok"),
            assistant("done"),
            user("u2"), // idx 4 — first user after hint
            assistant("bye"),
        ];
        let cut = find_compact_boundary(&msgs, 4).unwrap();
        assert_eq!(cut, 4);
    }

    #[tokio::test]
    async fn compact_replaces_with_summary_and_preserves_tail() {
        // Use a fake provider that always returns "SUMMARY".
        struct Fake;
        #[async_trait::async_trait]
        impl Provider for Fake {
            fn id(&self) -> &'static str {
                "fake"
            }
            async fn stream_turn(
                &self,
                _ctx: &Context,
                tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                let _ = tx
                    .send(AgentEvent::TextDelta {
                        content_index: 0,
                        text: "SUMMARY".into(),
                    })
                    .await;
                let _ = tx
                    .send(AgentEvent::Done {
                        finish_reason: FinishReason::Stop,
                        usage: Usage::default(),
                    })
                    .await;
                Ok(Usage::default())
            }
        }
        let mut ctx = Context::default();
        for i in 1..=10 {
            ctx.push_user_text(format!("u{i}"));
            ctx.push_assistant_text(format!("a{i}"));
        }
        // 20 messages. keep_last_n=8 → boundary somewhere near idx 12.
        let res = compact(&mut ctx, &Fake).await;
        let res = res.expect("compaction should occur");
        assert!(res.used_llm);
        assert_eq!(res.summary, "SUMMARY");
        // First message is the summary user.
        match &ctx.messages[0] {
            ContextMessage::User { content } => match &content[0] {
                ContentBlock::Text { text } => {
                    assert!(text.contains("[Prior conversation summary]"));
                    assert!(text.contains("SUMMARY"));
                }
                _ => panic!("expected text"),
            },
            _ => panic!("expected summary user message"),
        }
        // Total messages: 1 (summary) + kept tail. Must be < 20.
        assert!(ctx.messages.len() < 20);
        // And the tail must include the very last assistant message.
        match ctx.messages.last().unwrap() {
            ContextMessage::Assistant { content } => match &content[0] {
                AssistantBlock::Text { text } => assert_eq!(text, "a10"),
                _ => panic!("expected assistant text"),
            },
            _ => panic!("expected assistant"),
        }
    }

    #[tokio::test]
    async fn compact_falls_back_to_placeholder_on_provider_error() {
        struct Broken;
        #[async_trait::async_trait]
        impl Provider for Broken {
            fn id(&self) -> &'static str {
                "broken"
            }
            async fn stream_turn(
                &self,
                _ctx: &Context,
                _tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                Err("network down".into())
            }
        }
        let mut ctx = Context::default();
        for i in 1..=10 {
            ctx.push_user_text(format!("u{i}"));
            ctx.push_assistant_text(format!("a{i}"));
        }
        let res = compact(&mut ctx, &Broken)
            .await
            .expect("compaction should still occur with fallback");
        assert!(!res.used_llm);
        assert!(res.summary.contains("earlier messages truncated"));
    }
}

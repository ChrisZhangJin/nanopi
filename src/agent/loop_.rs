//! The Agent turn loop — the heart of nanopi.
//!
//! See `docs/v0.5-research.md` §2 for the state machine. v0.5 executes
//! tool calls sequentially (one tool at a time, in LLM-returned order).
//! v0.6 will add parallel tool execution (Pi parity).

use std::path::{Path, PathBuf};


use futures_util::future::join_all;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::agent::hook::{run_hooks, run_session_hooks, HookConfig, HookEvent, HookOutcome};
use crate::agent::permission::PermissionGate;
use crate::event::{AgentEvent, FinishReason, SteerMessage, ToolCall, Usage};
use crate::provider::openai::OpenAiProvider;
use crate::session::{self, SessionEntry};

/// Provider trait — abstracts the LLM backend so tests can inject fakes
/// without HTTP. `OpenAiProvider` implements it; v0.6 will add
/// `AnthropicProvider`.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    async fn stream_turn(
        &self,
        ctx: &Context,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<Usage, String>;
}
use crate::tool::{ToolContext, ToolRegistry};
use crate::util::{time, uuid};

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session error: {0}")]
    Session(String),
}

impl From<session::SessionError> for AgentError {
    fn from(e: session::SessionError) -> Self {
        AgentError::Session(e.to_string())
    }
}

/// Hook configuration for an Agent.
#[derive(Debug, Clone, Default)]
pub struct HooksConfig {
    pub pre_tool_use: Vec<HookConfig>,
    pub post_tool_use: Vec<HookConfig>,
    pub user_prompt_submit: Vec<HookConfig>,
    pub session_start: Vec<HookConfig>,
    pub session_end: Vec<HookConfig>,
    /// NEW v0.11.0 lifecycle hooks (see docs/pi-vs-nanopi.md §4.3).
    /// BeforeAgentStart is the only new variant that supports Block /
    /// Transform; the other three are advisory only.
    pub before_agent_start: Vec<HookConfig>,
    pub turn_start: Vec<HookConfig>,
    pub turn_end: Vec<HookConfig>,
    pub message_end: Vec<HookConfig>,
    /// v0.11.0: compaction lifecycle hooks (mirrors Pi's
    /// `session_before_compact` / `session_compact`).
    pub session_before_compact: Vec<HookConfig>,
    pub session_compact: Vec<HookConfig>,
}

/// The agent — owns context, provider, tool registry, session, permissions.
pub struct Agent {
    pub context: Context,
    pub provider: Box<dyn Provider>,
    pub registry: ToolRegistry,
    pub session_path: PathBuf,
    /// Session identifier from the header. Free-form String, not a
    /// `Uuid`, because `--session-id` lets callers pick their own.
    pub session_id: String,
    pub cwd: PathBuf,
    pub permission: PermissionGate,
    pub hooks: HooksConfig,
    /// The model id string, cached here so status-line renderers can
    /// read it without going through the Provider trait (which has
    /// no `model()` method today).
    pub model: String,
    /// Base URL + api key stashed so slash-command handlers (`/model`)
    /// can rebuild `provider` with a different model without needing
    /// to plumb config through every callsite. Not sensitive — same
    /// values the user already put in config.toml or env vars.
    pub base_url: String,
    pub api_key: String,
    /// Cumulative token usage over the whole session — summed across
    /// every LLM turn (including compaction summarization). Read by
    /// the status bar; never resets except on `Agent::load_session`
    /// (fresh Agent starts at zero).
    pub usage_total: Usage,
    /// Turn counter, incremented at the start of every `run_turn`.
    pub turn_count: u32,
    /// Skills loaded at Agent build time (via `Agent::build_fresh` or
    /// `Agent::hydrate_resumed`). Consulted by `/skill:name` expansion
    /// in `run_turn`. Empty when discovery is disabled and no --skill
    /// paths were passed.
    pub skills: Vec<crate::resources::Skill>,
    /// v0.11.0: text pending from a `SteerMessage::FollowUp`. When
    /// non-empty, the TUI's turn-completed handler auto-starts a new
    /// turn with this text (Pi's `getFollowUpMessages()` semantic).
    /// Reset to `None` when consumed.
    pub pending_follow_up: Option<String>,
    /// v0.11.0: tool execution mode (parallel by default; user can
    /// configure sequential via `tool_exec_mode` in config.toml).
    /// Set at build time and reused on every turn.
    pub tool_exec_mode: crate::config::ToolExecMode,
    /// When true, AGENTS.md / CLAUDE.md discovery is skipped entirely
    /// (CLI `--no-context-files` / `-nc`). Stashed here so `/reload` can
    /// rebuild the system prompt with the same policy. Mirrors PI's
    /// `noContextFiles`.
    pub no_context_files: bool,
    /// The `--system-prompt` / `--append-system-prompt` policy, stored
    /// UNRESOLVED (not the resolved text). `/reload` and the TUI's
    /// `/new`, `/fork`, `/model`, `/resume` rebuilds recompose the
    /// system prompt via `compose_system_prompt`, and reusing this same
    /// policy — rather than caching resolved text — is what lets
    /// `/reload` re-read an edited `SYSTEM.md` from disk, which is the
    /// whole point of `/reload`.
    pub prompt_overrides: crate::agent::prompt_override::PromptOverrides,
}

impl Agent {
    /// Reconstruct an Agent from an existing session JSONL file.
    /// Replays message entries into the Context so a new turn can build
    /// on prior history. Used by `--continue` and the v0.6 multi-turn
    /// TUI.
    pub fn load_session(session_path: &Path, cwd: &Path) -> Result<Self, AgentError> {
        use crate::agent::context::{
            AssistantBlock, ContentBlock, ContextMessage, ToolCallBlock,
        };
        let (header, entries) = crate::session::read_session(session_path)?;
        let mut context = Context::default();

        // Assistant turns are written to the JSONL in split form:
        //   Message(assistant, text)? then ToolCall* then ToolResult*
        // Anthropic requires a single Assistant message per turn that
        // holds the text AND the tool_use blocks together, so on replay
        // we hold the assistant "open" and merge ToolCall entries into
        // it before flushing. Anything else (User / Tool result /
        // Compaction / BranchSummary) closes the pending block.
        let mut pending: Option<Vec<AssistantBlock>> = None;
        let flush = |pending: &mut Option<Vec<AssistantBlock>>, context: &mut Context| {
            if let Some(blocks) = pending.take() {
                if !blocks.is_empty() {
                    context
                        .messages
                        .push(ContextMessage::Assistant { content: blocks });
                }
            }
        };

        for entry in entries {
            match entry {
                SessionEntry::Message { role, content, .. } => match role.as_str() {
                    "user" => {
                        flush(&mut pending, &mut context);
                        context.push_user_text(content);
                    }
                    "assistant" => {
                        flush(&mut pending, &mut context);
                        pending = Some(vec![AssistantBlock::Text { text: content }]);
                    }
                    _ => {}
                },
                SessionEntry::ToolCall {
                    id,
                    tool_name,
                    arguments,
                    ..
                } => {
                    let blocks = pending.get_or_insert_with(Vec::new);
                    blocks.push(AssistantBlock::ToolCall {
                        call: ToolCallBlock {
                            id,
                            name: tool_name,
                            arguments,
                        },
                    });
                }
                SessionEntry::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                    images,
                    ..
                } => {
                    flush(&mut pending, &mut context);
                    context.push_tool_result_with_images(
                        tool_call_id,
                        content,
                        is_error,
                        images,
                    );
                }
                SessionEntry::Compaction {
                    summary,
                    replaced_count,
                    ..
                } => {
                    flush(&mut pending, &mut context);
                    // On replay: the first N messages we've pushed so far
                    // are the ones that were summarized. Drop them and
                    // insert the summary at index 0 so the surviving tail
                    // stays in order.
                    let n = replaced_count.min(context.messages.len());
                    context.messages.drain(0..n);
                    let summary_msg = ContextMessage::User {
                        content: vec![ContentBlock::Text {
                            text: format!("[Prior conversation summary]\n\n{summary}"),
                        }],
                    };
                    context.messages.insert(0, summary_msg);
                }
                SessionEntry::BranchSummary { summary, .. } => {
                    flush(&mut pending, &mut context);
                    // Written by the fork picker's "Summarize branch"
                    // action to carry over what happened on the branch
                    // we abandoned. Replayed at its position (in order),
                    // NOT dropped/inserted like Compaction — it belongs
                    // where the user placed it in the new branch.
                    context.push_user_text(format!(
                        "[Branch summary from the abandoned line of \
                        history]\n\n{summary}"
                    ));
                }
                _ => {}
            }
        }
        flush(&mut pending, &mut context);
        Ok(Self {
            context,
            provider: Box::new(OpenAiProvider::new("", "", "")),
            registry: ToolRegistry::standard(),
            session_path: session_path.to_path_buf(),
            session_id: header.id,
            cwd: cwd.to_path_buf(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        })
    }

    /// Fire all `session_start` hooks. Advisory — outcome is not enforced.
    /// Call once, after Agent construction, before the first turn.
    ///
    /// v0.9.1 fix: honors `--no-hooks` — previously session lifecycle
    /// hooks leaked through the emergency switch because only the
    /// tool-facing sites gated on `hooks_active()`.
    pub async fn fire_session_start(&self) {
        if !self.permission.hooks_active() {
            return;
        }
        run_session_hooks(
            &self.hooks.session_start,
            HookEvent::SessionStart,
            &self.session_id.to_string(),
            &self.cwd,
        )
        .await;
    }

    /// Fire all `session_end` hooks. Advisory. Call before the process
    /// exits (or before Agent is dropped in the interactive loop).
    /// See `fire_session_start` for the `--no-hooks` note.
    pub async fn fire_session_end(&self) {
        if !self.permission.hooks_active() {
            return;
        }
        run_session_hooks(
            &self.hooks.session_end,
            HookEvent::SessionEnd,
            &self.session_id.to_string(),
            &self.cwd,
        )
        .await;
    }

    /// Force a compaction pass regardless of threshold. Bound to `/compact`
    /// in interactive mode. Best-effort — a provider error triggers a
    /// placeholder fallback, no error propagates up. Records a
    /// `Compaction` SessionEntry on success.
    ///
    /// When `tx` is Some, emits `CompactionStart` + `CompactionEnd`
    /// events so the UI can draw a scrollback marker. The manual
    /// `/compact` path passes None and drives its own feedback via
    /// the palette; the auto-trigger passes Some(&turn_tx).
    pub async fn compact_now(&mut self, tx: Option<&mpsc::Sender<AgentEvent>>, reason: &str) {
        use crate::agent::compact::compact;

        // ── SessionBeforeCompact hook (v0.11.0) ──────────────────
        // Advisory only — fires before compaction runs. matches
        // against the compaction reason string ("threshold" or
        // "manual").
        if self.permission.hooks_active()
            && !self.hooks.session_before_compact.is_empty()
        {
            run_session_hooks(
                &self.hooks.session_before_compact,
                HookEvent::SessionBeforeCompact,
                reason,
                &self.cwd,
            )
            .await;
        }

        if let Some(tx) = tx {
            let _ = tx
                .send(AgentEvent::CompactionStart {
                    reason: reason.to_string(),
                })
                .await;
        }
        let Some(result) = compact(&mut self.context, self.provider.as_ref()).await else {
            return;
        };

        if let Some(tx) = tx {
            let _ = tx
                .send(AgentEvent::CompactionEnd {
                    replaced_count: result.replaced_count,
                    used_llm: result.used_llm,
                })
                .await;
        }

        // ── SessionCompact hook (v0.11.0) ──────────────────────────
        // Fires after compaction completes. matcher is the
        // compaction reason.
        if self.permission.hooks_active()
            && !self.hooks.session_compact.is_empty()
        {
            run_session_hooks(
                &self.hooks.session_compact,
                HookEvent::SessionCompact,
                reason,
                &self.cwd,
            )
            .await;
        }

        let _ = session::append_entry(
            &self.session_path,
            &SessionEntry::Compaction {
                timestamp: time::now_iso8601(),
                summary: result.summary,
                replaced_count: result.replaced_count,
            },
        );
    }

    /// Threshold-gated version of `compact_now`. Called at the top of
    /// each turn AND after each response completes (matching PI). Uses
    /// the model's known context window when available, otherwise a
    /// fixed char threshold. Returns true if a pass was run.
    pub async fn maybe_compact(&mut self, tx: &mpsc::Sender<AgentEvent>) -> bool {
        use crate::agent::compact::should_auto_compact;
        let est_chars = self.context.estimate_chars();
        let window = crate::models::context_window(&self.model);
        if !should_auto_compact(est_chars, window) {
            return false;
        }
        self.compact_now(Some(tx), "threshold").await;
        true
    }

    /// Run a single user turn to completion. Streams events to `tx` and
    /// persists all messages, tool calls, and tool results to the session
    /// JSONL file. Returns the final concatenated assistant text.
    ///
    /// Safety: caps at 16 tool-iteration rounds to prevent infinite loops.
    ///
    /// Honors `cancel`: if the token is cancelled, the turn returns early
    /// with whatever was streamed so far. The session file is left in a
    /// consistent state (last completed turns persisted; the cancelled
    /// turn's partial assistant text is dropped).
    pub async fn run_turn(
        &mut self,
        user_msg: &str,
        tx: &mpsc::Sender<AgentEvent>,
        cancel: Option<tokio_util::sync::CancellationToken>,
        steer_rx: Option<mpsc::Receiver<SteerMessage>>,
    ) -> Result<String, AgentError> {
        // If the accumulated context is too big, compact it before adding
        // the new user message so the new message survives intact.
        self.maybe_compact(tx).await;
        self.turn_count = self.turn_count.saturating_add(1);

        // ── BeforeAgentStart hook (v0.11.0; mirrors PI's before_agent_start) ─
        // Fires once per turn, after compaction + turn-count bump but
        // BEFORE the user message enters context. The only new lifecycle
        // hook that supports Block (early return) and Transform (rewrite
        // the prompt); the others are advisory.
        // matcher applies to the turn_count string so users can scope
        // audit hooks (e.g. "^1$" to log only the first turn).
        if self.permission.hooks_active() && !self.hooks.before_agent_start.is_empty() {
            let turn_label = self.turn_count.to_string();
            let (outcome, _new_args) = run_hooks(
                &self.hooks.before_agent_start,
                HookEvent::BeforeAgentStart,
                &turn_label,
                serde_json::json!({
                    "turn_count": self.turn_count,
                    "prompt": user_msg,
                }),
                &self.cwd,
                Some(&self.session_id.to_string()),
            )
            .await;
            match outcome {
                HookOutcome::Block { reason } => {
                    let marker =
                        format!("[BeforeAgentStart hook blocked the turn: {reason}]");
                    let _ = tx
                        .send(AgentEvent::Error {
                            error: marker.clone(),
                        })
                        .await;
                    return Ok(marker);
                }
                HookOutcome::Transform { .. } => {
                    // Rewrite effective_msg via the same fallback as
                    // UserPromptSubmit — both Block and Transform work
                    // the same way. If the hook didn't change the
                    // prompt, leave it as `user_msg`.
                }
                HookOutcome::Allow => {}
            }
        }

        // ── UserPromptSubmit hook (mirrors PI's beforeUserMessage) ────
        // Allow user hooks to inspect / transform the raw prompt before
        // any skill-command expansion, and to block outright. Same
        // Allow/Block/Transform semantics as pre_tool_use hooks; Block
        // aborts the turn with a synthetic assistant marker so the user
        // sees why. Transform mutates the prompt in place.
        let mut effective_msg = user_msg.to_string();
        if self.permission.hooks_active() && !self.hooks.user_prompt_submit.is_empty() {
            let (outcome, new_args) = run_hooks(
                &self.hooks.user_prompt_submit,
                HookEvent::UserPromptSubmit,
                "", // no tool name for prompt submit
                serde_json::json!({ "prompt": effective_msg }),
                &self.cwd,
                Some(&self.session_id.to_string()),
            )
            .await;
            match outcome {
                HookOutcome::Block { reason } => {
                    let marker = format!("[UserPromptSubmit hook blocked the prompt: {reason}]");
                    let _ = tx
                        .send(AgentEvent::Error {
                            error: marker.clone(),
                        })
                        .await;
                    return Ok(marker);
                }
                HookOutcome::Transform { new_arguments } => {
                    if let Some(v) = new_arguments.get("prompt").and_then(|v| v.as_str()) {
                        effective_msg = v.to_string();
                    }
                }
                HookOutcome::Allow => {
                    if let Some(v) = new_args
                        .as_ref()
                        .and_then(|a| a.get("prompt"))
                        .and_then(|v| v.as_str())
                    {
                        if v != effective_msg {
                            effective_msg = v.to_string();
                        }
                    }
                }
            }
        }

        // ── /skill:name expansion (mirrors PI's _expandSkillCommand) ──
        // Runs AFTER the UserPromptSubmit hook so a hook that rewrites
        // the prompt into a /skill: call still triggers expansion.
        // Emits SkillInvocation so the TUI can render its own card.
        if let Some(expansion) =
            crate::resources::expand_skill_command(&effective_msg, &self.skills)
        {
            let _ = tx
                .send(AgentEvent::SkillInvocation {
                    name: expansion.name.clone(),
                    location: expansion.location.display().to_string(),
                    base_dir: expansion.base_dir.display().to_string(),
                    body: expansion.body.clone(),
                    user_message: expansion.user_args.clone(),
                })
                .await;
            // Persist a SkillInvocation entry alongside the expanded
            // user message so `--continue` and TUI replay reproduce
            // the card. Written BEFORE the user message so replay
            // order matches original.
            session::append_entry(
                &self.session_path,
                &SessionEntry::SkillInvocation {
                    id: uuid::v7().to_string(),
                    timestamp: time::now_iso8601(),
                    name: expansion.name,
                    location: expansion.location.display().to_string(),
                    base_dir: expansion.base_dir.display().to_string(),
                    body: expansion.body,
                    user_message: expansion.user_args,
                },
            )?;
            effective_msg = expansion.expanded_text;
        }

        // Append user message to context + session.
        let user_id = uuid::v7().to_string();
        self.context.push_user_text(effective_msg.clone());
        session::append_entry(
            &self.session_path,
            &SessionEntry::Message {
                id: user_id,
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: effective_msg,
            },
        )?;

        let mut final_text = String::new();
        // Safety belt against runaway tool loops. PI has no hard cap and
        // relies on `finish_reason=stop`; nanopi keeps a cap because a
        // broken provider or gateway that never sends `Done` would burn
        // tokens forever. 16 was too tight — research-style prompts
        // ("investigate X across the codebase") legitimately need dozens
        // of `read`/`grep` rounds. 50 fits the observed p99 without
        // giving up the safety belt. Bumping this alone is not a fix
        // for a stuck-looking session; see the fix below to
        // SessionEntry::Message content that also went out in v0.9.1.
        const MAX_ITERATIONS: u32 = 50;

        // Tripwire for "stuck retrying the same failing tool_call" —
        // observed in the wild with minimax-M3 through an OpenAI-compat
        // gateway that streamed tool_calls with an empty `name` field:
        // every iteration got an "unknown tool: unknown" tool_result,
        // the model responded with the identical empty-name call, and
        // MAX_ITERATIONS would let it burn ~50 rounds before quitting.
        // Break out after `STUCK_LIMIT` consecutive rounds where the
        // tool_call fingerprints match AND every call errored. Args
        // are stringified so `{"command":"ls"}` compares byte-for-byte
        // regardless of internal Value ordering.
        const STUCK_LIMIT: u32 = 3;
        let mut stuck_streak: u32 = 0;
        let mut last_error_sig: Option<Vec<(String, String)>> = None;

        // ── Steer / follow-up (v0.11.0) ────────────────────────────────
        // Mutably held so the loop body can `try_recv()` at each
        // iteration boundary. `take()` happens once; after that the
        // Option is `None` (the receiver is moved into the loop
        // scope).
        let mut steer_rx = steer_rx;
        let mut follow_up_queue: Vec<String> = Vec::new();

        for iteration_idx in 0..MAX_ITERATIONS {
            // If a cancel token was provided, bail before starting a new
            // LLM turn. The user's accumulated context is preserved.
            if let Some(ct) = cancel.as_ref() {
                if ct.is_cancelled() {
                    return Ok(final_text);
                }
            }

            // ── Steer pump (v0.11.0) ───────────────────────────────
            // Drain any pending `SteerMessage::Steering` messages
            // from the channel and push them as fresh user messages
            // into context so the next LLM call sees them. `FollowUp`
            // messages are queued — they fire after the turn ends.
            // `try_recv` is non-blocking so the agent loop never
            // stalls waiting for user input.
            if let Some(rx) = steer_rx.as_mut() {
                loop {
                    match rx.try_recv() {
                        Ok(SteerMessage::Steering { text }) => {
                            self.context.push_user_text(text.clone());
                            let _ = session::append_entry(
                                &self.session_path,
                                &SessionEntry::Message {
                                    id: uuid::v7().to_string(),
                                    timestamp: time::now_iso8601(),
                                    role: "user".into(),
                                    content: text,
                                },
                            );
                        }
                        Ok(SteerMessage::FollowUp { text }) => {
                            follow_up_queue.push(text);
                        }
                        // Empty or disconnected — nothing pending.
                        _ => break,
                    }
                }
            }

            // ── TurnStart hook (v0.11.0) ────────────────────────────────
            // Advisory only — Block is logged but does not abort the
            // iteration. matcher applied to turn_count (as string).
            if self.permission.hooks_active() && !self.hooks.turn_start.is_empty() {
                let turn_label = self.turn_count.to_string();
                run_hooks(
                    &self.hooks.turn_start,
                    HookEvent::TurnStart,
                    &turn_label,
                    serde_json::json!({
                        "turn_count": self.turn_count,
                        "iteration": iteration_idx,
                    }),
                    &self.cwd,
                    Some(&self.session_id.to_string()),
                )
                .await;
            }

            // Set up a forward channel: provider pushes to `forward_tx`,
            // we both observe (to drive the loop) and forward to `tx`.
            let (forward_tx, mut forward_rx) = mpsc::channel::<AgentEvent>(64);
            let collector_tx = tx.clone();
            let collect_task = tokio::spawn(async move {
                let mut calls: Vec<ToolCall> = Vec::new();
                let mut done: Option<(FinishReason, Usage)> = None;
                let mut assistant_text = String::new();
                while let Some(ev) = forward_rx.recv().await {
                    match &ev {
                        AgentEvent::TextDelta { text, .. } => {
                            assistant_text.push_str(text);
                        }
                        AgentEvent::ToolCall { call, .. } => {
                            calls.push(call.clone());
                        }
                        AgentEvent::Done {
                            finish_reason,
                            usage,
                        } => {
                            done = Some((*finish_reason, usage.clone()));
                        }
                        _ => {}
                    }
                    let _ = collector_tx.send(ev).await;
                }
                (calls, done, assistant_text)
            });

            // Run one streaming assistant turn. To make Esc interrupt
            // immediately (PI's app.interrupt), race the HTTP stream
            // against the cancel token via tokio::select!. If cancel
            // fires first, the select drops the stream future — that
            // drops `forward_tx` and closes the reqwest connection,
            // which in turn ends `collect_task` (it sees forward_rx
            // return None). Without this the .await would block until
            // the model naturally finished, which is what the user
            // saw as "Esc waits a while".
            //
            // `biased` gives priority to the cancel branch when both
            // are ready — otherwise tokio picks randomly and Esc can
            // still get outraced by a single-chunk fast reply.
            let (cancelled, stream_result) = if let Some(ct) = cancel.as_ref() {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => (true, Ok(())),
                    r = self.provider.stream_turn(&self.context, forward_tx) => {
                        (false, r.map(|_| ()).map_err(|e| e))
                    }
                }
            } else {
                (
                    false,
                    self.provider
                        .stream_turn(&self.context, forward_tx)
                        .await
                        .map(|_| ()),
                )
            };

            // Collect whatever the streaming parser managed to
            // accumulate before the connection dropped. Because the
            // forward_tx has been dropped by now (either the stream
            // completed or its future was dropped by cancel), the
            // channel is closed and collect_task returns promptly.
            let (mut calls, done, assistant_text) = collect_task
                .await
                .map_err(|e| AgentError::Provider(format!("collect task: {e}")))?;
            if cancelled {
                // Push a short directive-only marker so the NEXT
                // turn's LLM sees the turn was aborted and answers
                // the user's new question on its own terms.
                //
                // Deliberately DOES NOT embed the accumulated
                // `assistant_text`. Including the half-written
                // response made the next turn's model "continue"
                // that response instead of answering the new user
                // message — a user hit this after aborting a long
                // markdown reply about capitalism and asking "2+2"
                // in a fresh fork, only to see the capitalism
                // response resumed from scratch.
                //
                // The partial content is not lost: it was already
                // streamed to the user's terminal via TextDelta
                // events. The transcript keeps the visual record;
                // the model context stays clean.
                //
                // Discards `assistant_text` and any pending tool
                // calls — an aborted turn has no completed calls
                // to reason about.
                let _ = assistant_text;
                let _ = calls;
                let marker = "[Previous response was aborted by the user \
                              before completion. Ignore this turn and \
                              respond only to the next user message.]"
                    .to_string();
                session::append_entry(
                    &self.session_path,
                    &SessionEntry::Message {
                        id: uuid::v7().to_string(),
                        timestamp: time::now_iso8601(),
                        role: "assistant".into(),
                        content: marker.clone(),
                    },
                )?;
                self.context.push_assistant_text(marker);
                return Ok(final_text);
            }

            if let Err(e) = stream_result {
                return Err(AgentError::Provider(e.to_string()));
            }

            // Normalize gateway-mangled tool names (e.g. `Bash_tool` → `bash`)
            // BEFORE anything downstream sees them. Otherwise:
            //   - the assistant's own tool_call history in context ends up
            //     with a name that doesn't match `tools`, so the next LLM
            //     turn thinks it made a typo and retries in a loop;
            //   - session JSONL persists the mangled name.
            for c in calls.iter_mut() {
                if let Some(canonical) = self.registry.canonical_name(&c.name) {
                    c.name = canonical;
                }
            }

            // Handle duplicate tool_use ids from misbehaving gateways:
            //
            // (1) Within THIS response: dedup by id, keep first. The
            //     provider parsers already dedup, but belt-and-suspenders.
            // (2) Across turns: if the same id already appears in a
            //     prior Assistant's ToolCall blocks, rewrite this one to
            //     a fresh uuid. If we didn't, Anthropic would reject the
            //     next request ("each tool_use must have a single
            //     result") because two tool_result blocks would share
            //     the same tool_use_id. Rewriting keeps the model happy
            //     — its next-turn view will see the fresh id in both
            //     tool_use and tool_result, coherently paired.
            {
                let mut seen: std::collections::HashSet<String> = Default::default();
                calls.retain(|c| seen.insert(c.id.clone()));
            }
            for c in calls.iter_mut() {
                if self.context.has_assistant_tool_call_id(&c.id) {
                    c.id = format!("call_{}", uuid::v7());
                }
            }

            // Persist assistant text + push assistant message to context.
            // CRITICAL: must include ToolCall blocks so the provider knows
            // the following Tool messages are responses.
            //
            // v0.9.1 fix: previously wrote `final_text` (cumulative across
            // ALL tool-loop iterations of this turn) into every session
            // entry — so N iterations produced N assistant messages with
            // ever-growing content, and `--continue` / `/resume` replayed
            // them all, poisoning context. The session entry must carry
            // only THIS iteration's fresh text; the tool_call / tool_result
            // entries interleave in JSONL order.
            //
            // Guard also tightened: skip the Message entry entirely when
            // there's no new text (pure tool-call iteration). Tool calls
            // are already logged as their own SessionEntry::ToolCall by
            // execute_tool_calls below, so a text-less Message would be
            // redundant noise on replay.
            if !assistant_text.is_empty() || !calls.is_empty() {
                final_text.push_str(&assistant_text);
                let mut assistant_blocks = Vec::new();
                if !assistant_text.is_empty() {
                    assistant_blocks.push(crate::agent::context::AssistantBlock::Text {
                        text: assistant_text.clone(),
                    });
                }
                for c in &calls {
                    assistant_blocks.push(crate::agent::context::AssistantBlock::ToolCall {
                        call: crate::agent::context::ToolCallBlock {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            arguments: c.arguments.clone(),
                        },
                    });
                }
                if !assistant_text.is_empty() {
                    session::append_entry(
                        &self.session_path,
                        &SessionEntry::Message {
                            id: uuid::v7().to_string(),
                            timestamp: time::now_iso8601(),
                            role: "assistant".into(),
                            content: assistant_text.clone(),
                        },
                    )?;
                }
                self.context
                    .messages
                    .push(crate::agent::context::ContextMessage::Assistant {
                        content: assistant_blocks,
                    });
            }

            let Some((finish_reason, usage)) = done else {
                // Stream ended without Done. If we accumulated ANY
                // text or completed tool calls this iteration, treat
                // that as an implicit Stop and return what we have.
                // But if the whole turn produced nothing at all —
                // no text, no tool calls, no error — that's a silent
                // failure (v0.9.1: observed with a gateway that
                // returned HTTP 200 + one non-conforming SSE error
                // event that got silently skipped). Emit a real Error
                // event and surface it so the user sees something
                // instead of the "nothing was printed" bug.
                if final_text.is_empty() && assistant_text.is_empty() && calls.is_empty() {
                    let msg = "empty response — provider ended the stream with no text, tool call, or Done event (likely gateway silently dropped an error)".to_string();
                    let _ = tx.send(AgentEvent::Error { error: msg.clone() }).await;
                    return Err(AgentError::Provider(msg));
                }
                return Ok(final_text);
            };

            // Accumulate tokens across every LLM iteration in the session.
            self.usage_total.input_tokens = self
                .usage_total
                .input_tokens
                .saturating_add(usage.input_tokens);
            self.usage_total.output_tokens = self
                .usage_total
                .output_tokens
                .saturating_add(usage.output_tokens);
            self.usage_total.cache_read_tokens = self
                .usage_total
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
            self.usage_total.cache_write_tokens = self
                .usage_total
                .cache_write_tokens
                .saturating_add(usage.cache_write_tokens);

            // Snapshot had_tool_calls before the match — `calls` is moved
            // inside the ToolCalls arm, so the TurnEnd hook below needs
            // a value captured before the move.
            let had_tool_calls = !calls.is_empty();

            match finish_reason {
                FinishReason::Stop | FinishReason::Length | FinishReason::Refusal => {
                    break;
                }
                FinishReason::ToolCalls => {
                    if calls.is_empty() {
                        // LLM said "tool calls" but emitted none — done.
                        break;
                    }
                    // Fingerprint BEFORE `calls` is moved into
                    // `execute_tool_calls`, so we can compare against
                    // the previous iteration once it returns.
                    let sig: Vec<(String, String)> = calls
                        .iter()
                        .map(|c| (c.name.clone(), c.arguments.to_string()))
                        .collect();

                    // Pass the cancel token so Esc can kill an in-flight
                    // bash command instead of waiting for it to finish.
                    let is_errors = self
                        .execute_tool_calls(calls, tx, cancel.clone())
                        .await?;

                    let all_error = !is_errors.is_empty()
                        && is_errors.iter().all(|e| *e);
                    if all_error && last_error_sig.as_ref() == Some(&sig) {
                        stuck_streak += 1;
                    } else {
                        stuck_streak = if all_error { 1 } else { 0 };
                        last_error_sig = if all_error { Some(sig.clone()) } else { None };
                    }
                    if stuck_streak >= STUCK_LIMIT {
                        let (name, args) = sig
                            .first()
                            .map(|(n, a)| (n.as_str(), a.as_str()))
                            .unwrap_or(("<none>", "{}"));
                        let msg = format!(
                            "aborting turn: assistant made the same failing \
                             tool_call {STUCK_LIMIT} times in a row (tool={name}, \
                             args={args}). This usually indicates the upstream \
                             provider/gateway is emitting malformed tool_calls \
                             (e.g. empty tool name). Rather than burn the \
                             iteration budget, ending the turn so you can \
                             intervene."
                        );
                        let _ = tx.send(AgentEvent::Error { error: msg.clone() }).await;
                        return Err(AgentError::Provider(msg));
                    }
                    // Loop back to the next LLM turn.
                }
                FinishReason::Unknown => {
                    break;
                }
            }

            // ── TurnEnd hook (v0.11.0) ──────────────────────────────────
            // Advisory only — fired at the bottom of each iteration.
            if self.permission.hooks_active() && !self.hooks.turn_end.is_empty() {
                let turn_label = self.turn_count.to_string();
                run_hooks(
                    &self.hooks.turn_end,
                    HookEvent::TurnEnd,
                    &turn_label,
                    serde_json::json!({
                        "turn_count": self.turn_count,
                        "iteration": iteration_idx,
                        "had_tool_calls": had_tool_calls,
                    }),
                    &self.cwd,
                    Some(&self.session_id.to_string()),
                )
                .await;
            }
        }
        // ── MessageEnd hook (v0.11.0) ───────────────────────────────────
        // Fires once after the for-loop completes (all tool rounds done),
        // just before post-turn compaction. Advisory only — Block is
        // logged but does not abort the turn (which has already ended).
        if self.permission.hooks_active() && !self.hooks.message_end.is_empty() {
            let turn_label = self.turn_count.to_string();
            run_hooks(
                &self.hooks.message_end,
                HookEvent::MessageEnd,
                &turn_label,
                serde_json::json!({
                    "turn_count": self.turn_count,
                    "response_length": final_text.len(),
                }),
                &self.cwd,
                Some(&self.session_id.to_string()),
            )
            .await;
        }
        // Post-turn compaction check. Matches PI (`agent-session.ts`
        // `_handlePostAgentRun`). Firing here means the user sees the
        // compaction event bundled with the just-finished response
        // instead of at the start of the next turn.
        self.maybe_compact(tx).await;

        // v0.11.0: surface the first `FollowUp` message (if any)
        // onto the Agent so the caller can auto-trigger another
        // turn. Matches Pi's `getFollowUpMessages()` semantic.
        if !follow_up_queue.is_empty() {
            self.pending_follow_up = Some(follow_up_queue.remove(0));
        }
        Ok(final_text)
    }

    /// Execute a list of tool calls sequentially. Runs hooks, executes the
    /// tool, persists results to session, pushes ToolResult messages into
    /// context. Streams human-readable output to `tx` for the renderer.
    pub async fn execute_tool_calls(
        &mut self,
        calls: Vec<ToolCall>,
        tx: &mpsc::Sender<AgentEvent>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<bool>, AgentError> {
        // Phase 1: Run tool executions per `tool_exec_mode`.
        // - Parallel (default): `tokio::join_all` — all calls concurrently.
        // - Sequential: each call awaited in order.
        //
        // In sequential mode we still wrap cancel race around the whole
        // batch so Esc can short-circuit. Cancellation drops in-flight
        // bash children via `kill_on_drop`.
        let cwd = self.cwd.clone();
        let registry = self.registry.clone();
        let session_path = self.session_path.clone();
        let session_id = self.session_id.clone();
        let permission = self.permission.clone();
        let hooks = self.hooks.clone();

        // Keep id/name copies so we can synthesize cancelled results if
        // the whole batch gets dropped mid-flight (the calls Vec itself
        // is moved into the futures).
        let call_meta: Vec<(String, String)> = calls
            .iter()
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();

        let futs: Vec<_> = calls
            .into_iter()
            .map(|call| {
                let cwd = cwd.clone();
                let registry = registry.clone();
                let session_path = session_path.clone();
                let session_id = session_id.clone();
                let permission = permission.clone();
                let hooks = hooks.clone();
                let tx = tx.clone();
                async move {
                    let outcome = run_one_tool(
                        call,
                        registry,
                        session_path,
                        session_id,
                        cwd,
                        permission,
                        hooks,
                        tx,
                    )
                    .await;
                    outcome
                }
            })
            .collect();

        // Race the whole batch against the cancel token. `biased` gives
        // priority to cancel so a fast-arriving Esc doesn't lose the
        // race to a fast-completing tool. When cancel wins, `futs`
        // (and everything they own, including any live tokio::process
        // Child handles) is dropped; kill_on_drop = true sends SIGKILL
        // to running bash children. We then synthesize cancelled
        // tool_result entries so the assistant's tool_use blocks stay
        // paired with tool_result blocks — otherwise the next request
        // to Anthropic would 400 on unmatched tool_use ids.
        let (results, cancelled) = match self.tool_exec_mode {
            crate::config::ToolExecMode::Parallel => match cancel.as_ref() {
                Some(ct) => tokio::select! {
                    biased;
                    _ = ct.cancelled() => (Vec::new(), true),
                    r = join_all(futs) => (r, false),
                },
                None => (join_all(futs).await, false),
            },
            crate::config::ToolExecMode::Sequential => {
                // Sequentially await each tool; cancel can interrupt
                // mid-batch by dropping the iterator. The returned
                // `Vec<ToolCallOutcome>` has the same shape as
                // `join_all` for the post-phase.
                let mut results = Vec::with_capacity(futs.len());
                let mut cancelled = false;
                let mut iter = futs.into_iter();
                loop {
                    if let Some(ct) = cancel.as_ref() {
                        if ct.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                    }
                    match iter.next() {
                        Some(fut) => {
                            results.push(fut.await);
                        }
                        None => break,
                    }
                }
                (results, cancelled)
            }
        };

        let results = if cancelled {
            let mut synth = Vec::with_capacity(call_meta.len());
            for (id, _name) in call_meta {
                let content = "[cancelled by user before tool completed]".to_string();
                let _ = session::append_entry(
                    &self.session_path,
                    &SessionEntry::ToolResult {
                        tool_call_id: id.clone(),
                        timestamp: time::now_iso8601(),
                        content: content.clone(),
                        is_error: true,
                        images: Vec::new(),
                    },
                );
                synth.push(ToolCallOutcome {
                    call_id: id,
                    content,
                    is_error: true,
                    images: Vec::new(),
                });
            }
            synth
        } else {
            results
        };

        // Phase 2: Push ToolResult messages into context in call order
        // so the next LLM turn sees them. Persistence already happened
        // during phase 1 (in run_one_tool).
        let mut errors = Vec::with_capacity(results.len());
        for outcome in results {
            errors.push(outcome.is_error);
            self.context.push_tool_result_with_images(
                outcome.call_id,
                outcome.content,
                outcome.is_error,
                outcome.images,
            );
        }

        Ok(errors)
    }
}

/// Result of running one tool — what `run_one_tool` returns to the
/// caller's join_all. Only what the caller actually needs to push a
/// tool result into context; persistence already happened inside
/// `run_one_tool`.
struct ToolCallOutcome {
    call_id: String,
    content: String,
    is_error: bool,
    /// Multimodal image attachments from the tool (empty for text-only
    /// tools). Forwarded into context so the next request to a vision
    /// model can carry the image blocks.
    images: Vec<crate::tool::ImageAttachment>,
}

/// Run a single tool call: hooks → execute → persist → render. Pure
/// function over its arguments; safe to call concurrently from
/// `execute_tool_calls`.
async fn run_one_tool(
    call: ToolCall,
    registry: ToolRegistry,
    session_path: PathBuf,
    session_id: String,
    cwd: PathBuf,
    permission: PermissionGate,
    hooks: HooksConfig,
    tx: mpsc::Sender<AgentEvent>,
) -> ToolCallOutcome {
    // PreToolUse hooks.
    let mut effective_args = call.arguments.clone();
    if permission.hooks_active() && !hooks.pre_tool_use.is_empty() {
        let (outcome, transformed) = run_hooks(
            &hooks.pre_tool_use,
            HookEvent::PreToolUse,
            &call.name,
            call.arguments.clone(),
            &cwd,
            Some(&session_id.to_string()),
        )
        .await;
        effective_args = transformed.unwrap_or(call.arguments.clone());
        if let HookOutcome::Block { reason } = outcome {
            if permission.should_honor_pretooluse_block() {
                let result_text = format!("blocked by hook: {reason}");
                let _ = session::append_entry(
                    &session_path,
                    &SessionEntry::ToolResult {
                        tool_call_id: call.id.clone(),
                        timestamp: time::now_iso8601(),
                        content: result_text.clone(),
                        is_error: true,
                        images: Vec::new(),
                    },
                );
                let _ = tx
                    .send(AgentEvent::TextDelta {
                        content_index: 0,
                        text: format!("\n[{} blocked: {}]\n", call.name, reason),
                    })
                    .await;
                return ToolCallOutcome {
                    call_id: call.id,
                    content: result_text,
                    is_error: true,
                    images: Vec::new(),
                };
            }
        }
    }

    // Persist tool call.
    let _ = session::append_entry(
        &session_path,
        &SessionEntry::ToolCall {
            id: call.id.clone(),
            timestamp: time::now_iso8601(),
            tool_name: call.name.clone(),
            arguments: effective_args.clone(),
        },
    );

    // Resolve and execute. Wall-clock timed so the TUI can show
    // "Took 0.3s" next to the marker.
    let started = std::time::Instant::now();
    let (mut content, mut is_error, images) = match registry.get(&call.name) {
        Some(tool) => {
            let ctx = ToolContext { cwd: cwd.clone() };
            match tool.execute(effective_args.clone(), &ctx).await {
                Ok(o) => (o.content, o.is_error, o.images),
                Err(e) => (format!("tool error: {e}"), true, Vec::new()),
            }
        }
        None => (format!("unknown tool: {}", call.name), true, Vec::new()),
    };
    let elapsed = started.elapsed();

    // Persist result.
    let _ = session::append_entry(
        &session_path,
        &SessionEntry::ToolResult {
            tool_call_id: call.id.clone(),
            timestamp: time::now_iso8601(),
            content: content.clone(),
            is_error,
            images: images.clone(),
        },
    );

    // PostToolUse hooks. Payload mirrors Claude Code's PostToolUse
    // wire schema so hooks can inspect what actually happened —
    // `tool_input` (final args after any PreToolUse transform) and
    // `tool_response` (content + is_error + duration_ms).
    //
    // v0.9.1 fix: previously passed `Value::Object(Default::default())`
    // (empty `{}`), so hooks could see tool_name and session_id but
    // nothing about the call itself — no way to log outputs, react
    // to failures, or scrape secrets from stdout. Everything the
    // hook actually needs to be useful was missing.
    //
    // P1 ordering: hooks fire BEFORE the ToolResult event is emitted,
    // so the post-hook content/is_error is what the renderer sees
    // (and what the next LLM turn sees in the context).
    if permission.hooks_active() && !hooks.post_tool_use.is_empty() {
        let post_payload = serde_json::json!({
            "tool_input": effective_args,
            "tool_response": {
                "content": content,
                "is_error": is_error,
                "duration_ms": elapsed.as_millis() as u64,
            },
        });
        let (_outcome, new_args) = run_hooks(
            &hooks.post_tool_use,
            HookEvent::PostToolUse,
            &call.name,
            post_payload,
            &cwd,
            Some(&session_id.to_string()),
        )
        .await;
        // P1: post_tool_use Transform replaces the tool result content.
        //
        // `run_hooks` always returns `HookOutcome::Allow` — transforms
        // are accumulated into `current_args` which becomes `new_args`.
        // So we just look at whether `new_args` was rewritten, not at
        // the outcome enum.
        //
        // Hook emits
        //   {"decision":"allow","updated_input":{"content":"...","is_error":true}}
        // → `run_hooks` replaces the whole `current_args` with
        //   `{"content":"...","is_error":true}` → `new_args` holds that.
        if let Some(v) = new_args.as_ref() {
            // Only apply if the rewrite is structurally different from
            // the original payload — it must have a `content` key at
            // the top level (not nested under `tool_response`).
            if let Some(new_content) = v.get("content").and_then(|v| v.as_str()) {
                content = new_content.to_string();
            }
            if let Some(new_is_error) = v.get("is_error").and_then(|v| v.as_bool()) {
                is_error = new_is_error;
            }
        }
    }

    // Stream a structured ToolResult so the TUI can render one green
    // (or red) card containing command + output preview + timing.
    // Rustyline mode picks the same event apart and prints a compact
    // marker line instead.
    let _ = tx
        .send(AgentEvent::ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: content.clone(),
            is_error,
            elapsed_ms: elapsed.as_millis() as u64,
        })
        .await;

    ToolCallOutcome {
        call_id: call.id,
        content,
        is_error,
        images,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::{ContentBlock, ContextMessage, ToolSpec};
    use serde_json::json;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-agent-{}", uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fake_provider() -> OpenAiProvider {
        // Provider URL/key/model don't matter — tests don't make HTTP calls.
        OpenAiProvider::new("http://localhost:0", "fake-key", "fake-model")
    }

    #[tokio::test]
    async fn execute_tool_calls_blocked_by_hook_returns_error_result() {
        let dir = tmp();
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap(); // touch

        // Pre-tool hook that always blocks.
        let pre_hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: "echo blocked 1>&2; exit 2".into(),
            timeout: 2000,
        };
        let hooks = HooksConfig {
            pre_tool_use: vec![pre_hook],
            ..Default::default()
        };
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(fake_provider()),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks,
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(16);
        agent
            .execute_tool_calls(
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}),
                }],
                &tx,
                None,
            )
            .await
            .unwrap();
        drop(tx);

        // Drain channel.
        let mut count = 0;
        while rx.recv().await.is_some() {
            count += 1;
        }

        // Context should now have a tool result with is_error=true.
        let last = agent.context.messages.last().expect("a message");
        match last {
            crate::agent::context::ContextMessage::Tool {
                content, is_error, ..
            } => {
                assert!(is_error);
                assert!(content.contains("blocked"));
            }
            _ => panic!("expected Tool message, got {last:?}"),
        }
        assert!(count >= 1, "expected at least one rendered event");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: v0.9.1 PostToolUse used to be called with
    /// `Value::Object(Default::default())` (empty `{}`) — hooks
    /// couldn't see what the tool actually did. Fix populates the
    /// payload with `tool_input` (final args) and `tool_response`
    /// (content / is_error / duration_ms). This test writes the
    /// hook stdin JSON to disk and asserts every field is present.
    #[tokio::test]
    async fn post_tool_use_hook_receives_input_and_response() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp();
        let session_path = dir.join("s.jsonl");
        std::fs::write(&session_path, "").unwrap();

        // Post-tool hook that dumps its stdin JSON to a known path.
        let stdin_dump = dir.join("post.json");
        let hook_script = dir.join("post.sh");
        std::fs::write(
            &hook_script,
            format!(
                "#!/usr/bin/env bash\ncat > {}\nexit 0\n",
                stdin_dump.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let post_hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: hook_script.display().to_string(),
            timeout: 3000,
        };
        let hooks = HooksConfig {
            post_tool_use: vec![post_hook],
            ..Default::default()
        };
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(fake_provider()),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks,
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(16);
        agent
            .execute_tool_calls(
                vec![ToolCall {
                    id: "call_pt".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "echo hi"}),
                }],
                &tx,
                None,
            )
            .await
            .unwrap();
        drop(tx);
        while rx.recv().await.is_some() {}

        let dumped =
            std::fs::read_to_string(&stdin_dump).expect("hook must have written its stdin JSON");
        let v: serde_json::Value = serde_json::from_str(&dumped).expect("stdin was JSON");

        assert_eq!(v["event"], "post_tool_use");
        assert_eq!(v["tool_name"], "bash");
        // tool_input carries the (post-transform) tool arguments.
        assert_eq!(v["arguments"]["tool_input"]["command"], "echo hi");
        // tool_response fields must all be present so hooks can react
        // to failures / scrape output / measure durations.
        assert!(
            v["arguments"]["tool_response"]["content"]
                .as_str()
                .map(|s| s.contains("hi"))
                .unwrap_or(false),
            "content missing/wrong: {v}"
        );
        assert_eq!(v["arguments"]["tool_response"]["is_error"], false);
        assert!(v["arguments"]["tool_response"]["duration_ms"].is_u64());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1 regression: post_tool_use hook can rewrite the tool's output
    /// content via `{"updated_input":{"content":"..."}}`. Previously
    /// the hook's return was discarded (`let _ = run_hooks(...)`),
    /// so redact / log-scrubbing / result-truncation plugins were
    /// inert. Fix: capture the outcome and apply Transform to the
    /// result `content` and `is_error` (image attachments keep their
    /// original handling).
    #[tokio::test]
    async fn post_tool_use_hook_can_transform_result() {
        let dir = tmp();
        let session_path = dir.join("s.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let post_hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            // Replace content with a redacted marker. Mirror Claude
            // Code's protocol — `decision:"allow"` + `updated_input`.
            command: r#"echo '{"decision":"allow","updated_input":{"content":"[REDACTED]","is_error":true}}'"#.into(),
            timeout: 2000,
        };
        let hooks = HooksConfig {
            post_tool_use: vec![post_hook],
            ..Default::default()
        };
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(fake_provider()),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks,
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(16);
        agent
            .execute_tool_calls(
                vec![ToolCall {
                    id: "call_xform".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "echo SECRET=abc123"}),
                }],
                &tx,
                None,
            )
            .await
            .unwrap();
        drop(tx);

        let last = agent.context.messages.last().expect("message in context");
        match last {
            crate::agent::context::ContextMessage::Tool {
                content,
                is_error,
                ..
            } => {
                assert!(
                    content.contains("[REDACTED]"),
                    "expected hook to rewrite content, got {content:?}"
                );
                assert!(
                    !content.contains("SECRET"),
                    "raw tool output leaked into context: {content:?}"
                );
                assert!(
                    *is_error,
                    "hook should be able to flip is_error via updated_input"
                );
            }
            other => panic!("expected Tool message, got {other:?}"),
        }

        // Drain channel — the renderer also gets the rewritten content.
        let mut found_redacted = false;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::ToolResult { content, is_error, .. } = &ev {
                if content.contains("[REDACTED]") {
                    found_redacted = true;
                    assert!(is_error, "renderer event should reflect is_error flip");
                }
            }
        }
        assert!(
            found_redacted,
            "renderer must receive the post-hook rewritten content"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn execute_tool_calls_unknown_tool_yields_error_result() {
        let dir = tmp();
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(fake_provider()),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };
        let (tx, _rx) = mpsc::channel::<AgentEvent>(16);
        agent
            .execute_tool_calls(
                vec![ToolCall {
                    id: "c1".into(),
                    name: "nosuchtool".into(),
                    arguments: json!({}),
                }],
                &tx,
                None,
            )
            .await
            .unwrap();

        let last = agent.context.messages.last().unwrap();
        match last {
            crate::agent::context::ContextMessage::Tool {
                content, is_error, ..
            } => {
                assert!(is_error);
                assert!(content.contains("unknown tool"));
            }
            _ => panic!("expected Tool message"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Esc-during-streaming (PI's app.interrupt): the cancel token
    /// fires while stream_turn is still running. run_turn must:
    ///   1) drop the stream future promptly (not wait for it to
    ///      complete naturally),
    ///   2) preserve the aborted turn in context by pushing a
    ///      synthetic "(interrupted by user…)" assistant message,
    ///      so the next turn's request doesn't leave the LLM staring
    ///      at two consecutive user messages.
    #[tokio::test]
    async fn run_turn_cancel_drops_stream_and_marks_context_aborted() {
        // Provider that hangs forever — simulates a real HTTP stream
        // that hasn't finished yet when the user hits Esc.
        struct HangingProvider;
        #[async_trait::async_trait]
        impl Provider for HangingProvider {
            fn id(&self) -> &'static str {
                "hanging"
            }
            async fn stream_turn(
                &self,
                _ctx: &Context,
                _tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        let dir = tmp();
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(HangingProvider),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, _rx) = mpsc::channel::<AgentEvent>(16);
        let ct = tokio_util::sync::CancellationToken::new();
        let ct_clone = ct.clone();

        // Trigger cancel after a short delay — long enough for run_turn
        // to enter the streaming select but short enough to keep the
        // test fast.
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ct_clone.cancel();
        });

        let start = std::time::Instant::now();
        let out = agent.run_turn("do something forever", &tx, Some(ct), None).await;
        canceller.await.ok();
        let elapsed = start.elapsed();

        assert!(out.is_ok(), "run_turn should return Ok on cancel");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "cancel should be prompt; took {:?}",
            elapsed
        );

        // Last message in context must be a SHORT directive-only
        // marker. In particular it must NOT include any partial
        // response text (that would distract the next turn's LLM
        // into "continuing" the aborted answer instead of responding
        // to the new user message).
        let last = agent.context.messages.last().expect("context has messages");
        match last {
            ContextMessage::Assistant { content } => {
                let marker_text = content
                    .iter()
                    .filter_map(|b| match b {
                        crate::agent::context::AssistantBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>();
                assert!(
                    marker_text.contains("aborted by the user"),
                    "got {marker_text:?}"
                );
                assert!(
                    marker_text.contains("Ignore this turn"),
                    "got {marker_text:?}"
                );
                assert!(
                    marker_text.len() < 300,
                    "marker should be short; got {} chars",
                    marker_text.len()
                );
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_has_tools_field() {
        // Smoke: make sure we can add tools to a Context.
        let mut ctx = Context::default();
        ctx.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            parameters: json!({"type":"object"}),
        });
        assert_eq!(ctx.tools.len(), 1);
    }

    /// Build a minimal Agent for compaction tests. Uses FakeProvider so
    /// the summarizer call inside compact() returns synthesized text
    /// instead of hitting the network.
    fn agent_for_compact_test(response: &str) -> (Agent, PathBuf) {
        let dir = tmp();
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();
        let agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: response.into(),
            }),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };
        (agent, dir)
    }

    #[tokio::test]
    async fn compact_now_emits_start_and_end_when_tx_provided() {
        let (mut agent, dir) = agent_for_compact_test("SUMMARY");
        // Populate enough messages that find_compact_boundary succeeds
        // — need > KEEP_RECENT_TOKENS worth of tail so the algorithm
        // actually cuts. Each message is ~10k tokens.
        let big = "x".repeat(10_000 * crate::agent::compact::CHARS_PER_TOKEN_ESTIMATE);
        for i in 1..=10 {
            agent.context.push_user_text(format!("u{i}-{big}"));
            agent.context.push_assistant_text(format!("a{i}-{big}"));
        }

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(16);
        agent.compact_now(Some(&tx), "threshold").await;
        drop(tx);

        let mut got_start_reason = None;
        let mut got_end = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::CompactionStart { reason } => got_start_reason = Some(reason),
                AgentEvent::CompactionEnd {
                    used_llm,
                    replaced_count,
                } => {
                    assert!(used_llm, "expected used_llm=true from FakeProvider");
                    assert!(replaced_count > 0);
                    got_end = true;
                }
                _ => {}
            }
        }
        assert_eq!(got_start_reason.as_deref(), Some("threshold"));
        assert!(got_end, "expected CompactionEnd event");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn maybe_compact_returns_false_and_stays_silent_below_threshold() {
        let (mut agent, dir) = agent_for_compact_test("SUMMARY");
        agent.context.push_user_text("hello");
        agent.context.push_assistant_text("hi");

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(16);
        let fired = agent.maybe_compact(&tx).await;
        drop(tx);
        assert!(!fired);
        // Channel should be empty and closed → recv returns None.
        assert!(rx.recv().await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-only Provider that emits a fixed assistant message and
    /// `Done(Stop)`. No HTTP, no LLM.
    struct FakeProvider {
        response: String,
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn id(&self) -> &'static str {
            "fake"
        }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            let _ = tx
                .send(AgentEvent::Start {
                    message_id: "m".into(),
                })
                .await;
            let _ = tx
                .send(AgentEvent::TextDelta {
                    content_index: 0,
                    text: self.response.clone(),
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

    /// Regression: v0.9.1 discovered nanopi could return silently
    /// when the provider stream ended without any TextDelta, ToolCall,
    /// or Done event — the exact shape seen when a corporate gateway
    /// returned HTTP 200 + an unrecognized SSE `event: error` that
    /// the Anthropic parser silently skipped. Fix: `run_turn` now
    /// surfaces this as a Provider error instead of returning
    /// `Ok("")`. That way the user always sees SOMETHING.
    #[tokio::test]
    async fn empty_stream_without_done_surfaces_error() {
        struct SilentProvider;
        #[async_trait::async_trait]
        impl Provider for SilentProvider {
            fn id(&self) -> &'static str {
                "silent"
            }
            async fn stream_turn(
                &self,
                _ctx: &Context,
                tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                // Not even a Start event. Just drop the channel.
                drop(tx);
                Ok(Usage::default())
            }
        }

        let dir = tmp();
        let session_path = dir.join("empty.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":2,\"id\":\"019fe000-0000-7000-8000-000000000000\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"silent\",\"base_url\":\"\"}\n",
        ).unwrap();

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(SilentProvider),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: "silent".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(16);
        let drain = tokio::spawn(async move {
            let mut got_error = false;
            while let Some(ev) = rx.recv().await {
                if matches!(ev, AgentEvent::Error { .. }) {
                    got_error = true;
                }
            }
            got_error
        });

        let r = agent.run_turn("hi", &tx, None, None).await;
        drop(tx);
        let got_error_event = drain.await.unwrap();

        // Must be an error, not Ok("").
        assert!(
            matches!(r, Err(AgentError::Provider(_))),
            "silent stream must surface as Provider error, got {r:?}"
        );
        // And an Error event must have been streamed for the renderer.
        assert!(got_error_event, "renderer must receive an Error event");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: v0.9.1 discovered `--no-hooks` didn't gate the
    /// session lifecycle hooks — SessionStart and SessionEnd fired
    /// regardless. Fix guarded both on `permission.hooks_active()`.
    /// This test writes a marker file from a session_start hook and
    /// asserts the marker never appears when `--no-hooks` is set.
    #[tokio::test]
    async fn no_hooks_disables_session_start_and_end() {
        let dir = tmp();
        let marker = dir.join("session_start_fired");
        let hook_script = dir.join("hook.sh");
        std::fs::write(
            &hook_script,
            format!("#!/usr/bin/env bash\ntouch {}\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let session_path = dir.join("noh.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":2,\"id\":\"019fe000-0000-7000-8000-000000000000\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"m\",\"base_url\":\"\"}\n",
        )
        .unwrap();

        // Same hook wired for both session_start and session_end.
        let hook_cfg = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: hook_script.display().to_string(),
            timeout: 3000,
        };

        // --no-hooks → permission.hooks_active()==false.
        let agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: "ok".into(),
            }),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(true /*no_hooks*/, None),
            hooks: HooksConfig {
                session_start: vec![hook_cfg.clone()],
                session_end: vec![hook_cfg],
                ..Default::default()
            },
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };
        agent.fire_session_start().await;
        agent.fire_session_end().await;

        assert!(
            !marker.exists(),
            "--no-hooks must disable both session_start and session_end"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────── v0.6: --continue / active_session ───────

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// No active session registered for this cwd → returns None.
    #[test]
    fn active_session_returns_none_when_no_history() {
        let _g = lock();
        let home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);

        let cwd = tmp();
        let got = crate::session::active_session(&cwd);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
        assert!(got.is_none(), "expected None, got {got:?}");
    }

    /// After creating a session and registering it as active, returns
    /// that session's path.
    #[test]
    fn active_session_returns_path_after_use() {
        let _g = lock();
        let home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);

        let cwd = tmp();
        let (path, _header) = crate::session::new_session(&cwd, "m", "http://x").expect("new");
        crate::session::set_active_session(&cwd, &path).expect("active");

        let got = crate::session::active_session(&cwd).expect("some");
        assert_eq!(got, path);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // ─────── v0.6+: compaction replay in load_session ───────

    /// A Compaction entry in the JSONL should collapse the earlier messages
    /// into a single summary user message on load. Tail is preserved.
    #[test]
    fn load_session_replays_compaction() {
        let _g = lock();
        let home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = tmp();

        let (path, _hdr) = crate::session::new_session(&cwd, "m", "http://x").expect("new session");
        // 4 messages that WILL be compacted, then a Compaction entry, then a
        // trailing message that must survive.
        for i in 1..=4 {
            crate::session::append_entry(
                &path,
                &SessionEntry::Message {
                    id: uuid::v7().to_string(),
                    timestamp: time::now_iso8601(),
                    role: (if i % 2 == 0 { "assistant" } else { "user" }).into(),
                    content: format!("m{i}"),
                },
            )
            .unwrap();
        }
        crate::session::append_entry(
            &path,
            &SessionEntry::Compaction {
                timestamp: time::now_iso8601(),
                summary: "the summary".into(),
                replaced_count: 4,
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "after".into(),
            },
        )
        .unwrap();

        let agent = Agent::load_session(&path, &cwd).expect("load");
        // Expect: [summary_user, "after"] — 2 messages total.
        assert_eq!(agent.context.messages.len(), 2);
        match &agent.context.messages[0] {
            crate::agent::context::ContextMessage::User { content } => {
                let text = match &content[0] {
                    crate::agent::context::ContentBlock::Text { text } => text,
                    _ => panic!("expected text"),
                };
                assert!(text.contains("Prior conversation summary"));
                assert!(text.contains("the summary"));
            }
            _ => panic!("expected summary user"),
        }
        match &agent.context.messages[1] {
            crate::agent::context::ContextMessage::User { content } => {
                let text = match &content[0] {
                    crate::agent::context::ContentBlock::Text { text } => text,
                    _ => panic!("expected text"),
                };
                assert_eq!(text, "after");
            }
            _ => panic!("expected trailing user"),
        }

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// Regression: `--continue` used to drop every ToolCall and ToolResult
    /// entry from the reloaded Context (they hit the `_ => {}` arm), so
    /// the resumed model saw a text-only chat and stopped calling tools
    /// — narrating shell commands as prose instead. This pins that a
    /// text+tool_call assistant turn round-trips through load_session as
    /// one Assistant message holding both blocks, followed by the Tool
    /// result and the next assistant text.
    #[test]
    fn load_session_replays_tool_calls() {
        use crate::agent::context::{AssistantBlock, ContextMessage};
        let _g = lock();
        let home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = tmp();

        let (path, _hdr) = crate::session::new_session(&cwd, "m", "http://x").expect("new session");

        // User asks → assistant emits text + a bash tool_call → tool result → assistant follow-up text.
        crate::session::append_entry(
            &path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "count folders in /home".into(),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "assistant".into(),
                content: "checking".into(),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path,
            &SessionEntry::ToolCall {
                id: "call_1".into(),
                timestamp: time::now_iso8601(),
                tool_name: "bash".into(),
                arguments: serde_json::json!({"command": "find /home -maxdepth 1 -type d | wc -l"}),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path,
            &SessionEntry::ToolResult {
                tool_call_id: "call_1".into(),
                timestamp: time::now_iso8601(),
                content: "44".into(),
                is_error: false,
                images: Vec::new(),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "assistant".into(),
                content: "44 folders".into(),
            },
        )
        .unwrap();

        let agent = Agent::load_session(&path, &cwd).expect("load");

        // Expected: [User, Assistant(Text+ToolCall), Tool(result), Assistant(Text)]
        assert_eq!(agent.context.messages.len(), 4, "messages: {:#?}", agent.context.messages);

        match &agent.context.messages[1] {
            ContextMessage::Assistant { content } => {
                assert_eq!(content.len(), 2, "text + tool_call must be merged");
                match &content[0] {
                    AssistantBlock::Text { text } => assert_eq!(text, "checking"),
                    b => panic!("expected Text first, got {b:?}"),
                }
                match &content[1] {
                    AssistantBlock::ToolCall { call } => {
                        assert_eq!(call.id, "call_1");
                        assert_eq!(call.name, "bash");
                    }
                    b => panic!("expected ToolCall second, got {b:?}"),
                }
            }
            m => panic!("expected Assistant, got {m:?}"),
        }

        match &agent.context.messages[2] {
            ContextMessage::Tool {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(content, "44");
                assert!(!is_error);
            }
            m => panic!("expected Tool result, got {m:?}"),
        }

        // A pure tool-call iteration (no assistant text) still replays as
        // an Assistant message carrying just the ToolCall block.
        let (path2, _hdr2) = crate::session::new_session(&cwd, "m", "http://x").expect("new session");
        crate::session::append_entry(
            &path2,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "run it".into(),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path2,
            &SessionEntry::ToolCall {
                id: "call_2".into(),
                timestamp: time::now_iso8601(),
                tool_name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        )
        .unwrap();
        crate::session::append_entry(
            &path2,
            &SessionEntry::ToolResult {
                tool_call_id: "call_2".into(),
                timestamp: time::now_iso8601(),
                content: "a\nb".into(),
                is_error: false,
                images: Vec::new(),
            },
        )
        .unwrap();

        let agent2 = Agent::load_session(&path2, &cwd).expect("load2");
        assert_eq!(agent2.context.messages.len(), 3);
        match &agent2.context.messages[1] {
            ContextMessage::Assistant { content } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(content[0], AssistantBlock::ToolCall { .. }));
            }
            m => panic!("expected Assistant with lone ToolCall, got {m:?}"),
        }

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // ─────── v0.6: parallel tool execution ───────

    /// Two bash calls that each sleep 1s — they MUST run in parallel
    /// (total wall time < 1.8s), not sequentially (<2s). v0.5 was
    /// sequential; this test pins the new parallel contract.
    /// Regression: v0.9 shipped with an accumulation bug where
    /// SessionEntry::Message stored the cumulative `final_text` across
    /// every tool-loop iteration, so a 3-iteration turn wrote 3
    /// messages with growing prefixes ("A", "AB", "ABC"). This test
    /// pins the fix: each iteration's session entry must carry only
    /// its own fresh text, and iterations that produce only tool
    /// calls (no text) must not emit a Message entry at all.
    #[tokio::test]
    async fn session_assistant_messages_are_per_iteration_not_cumulative() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct SteppedProvider {
            step: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl Provider for SteppedProvider {
            fn id(&self) -> &'static str {
                "stepped"
            }
            async fn stream_turn(
                &self,
                _ctx: &Context,
                tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                let step = self.step.fetch_add(1, Ordering::SeqCst);
                let _ = tx
                    .send(AgentEvent::Start {
                        message_id: "m".into(),
                    })
                    .await;
                match step {
                    0 => {
                        // Iteration 1: text + a tool call → provider says
                        // "call tools then come back to me".
                        let _ = tx
                            .send(AgentEvent::TextDelta {
                                content_index: 0,
                                text: "first".into(),
                            })
                            .await;
                        let _ = tx
                            .send(AgentEvent::ToolCall {
                                content_index: 0,
                                call: ToolCall {
                                    id: "c1".into(),
                                    name: "bash".into(),
                                    arguments: json!({"command": "echo hi"}),
                                },
                            })
                            .await;
                        let _ = tx
                            .send(AgentEvent::Done {
                                finish_reason: FinishReason::ToolCalls,
                                usage: Usage::default(),
                            })
                            .await;
                    }
                    1 => {
                        // Iteration 2: fresh text only, then stop. No
                        // cumulative "firstsecond" should appear.
                        let _ = tx
                            .send(AgentEvent::TextDelta {
                                content_index: 0,
                                text: "second".into(),
                            })
                            .await;
                        let _ = tx
                            .send(AgentEvent::Done {
                                finish_reason: FinishReason::Stop,
                                usage: Usage::default(),
                            })
                            .await;
                    }
                    _ => unreachable!("provider called too many times"),
                }
                Ok(Usage::default())
            }
        }

        let dir = tmp();
        let session_path = dir.join("accum.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":2,\"id\":\"019fe000-0000-7000-8000-000000000000\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"stepped\",\"base_url\":\"\"}\n",
        ).unwrap();

        let step = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(SteppedProvider { step: step.clone() }),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: "stepped".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        // Drain events so the sender doesn't block.
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let final_text = agent.run_turn("go", &tx, None, None).await.expect("turn");
        drop(tx);
        drain.await.unwrap();

        // final_text still concatenates for the caller — that's the
        // whole turn's assistant output, useful for -p mode.
        assert_eq!(final_text, "firstsecond");

        // Read the session file and inspect the assistant messages.
        let content = std::fs::read_to_string(&session_path).unwrap();
        let assistant_contents: Vec<String> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("message"))
            .filter(|v| v.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .filter_map(|v| {
                v.get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| s.to_string())
            })
            .collect();

        assert_eq!(
            assistant_contents,
            vec!["first".to_string(), "second".to_string()],
            "each iteration's session entry must carry only its own fresh text"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn execute_tool_calls_runs_in_parallel_not_sequence() {
        use std::time::Instant;

        let dir = tmp();
        let session_path = dir.join("p.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: "ok".into(),
            }),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let start = Instant::now();
        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "sleep 1; echo a"}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "sleep 1; echo b"}),
                    },
                ],
                &tx,
                None,
            )
            .await
            .expect("execute");
        // Drain events.
        while rx.try_recv().is_ok() {}
        let elapsed = start.elapsed();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            elapsed < std::time::Duration::from_millis(1800),
            "expected parallel execution (<1.8s), got {elapsed:?}"
        );
    }

    /// Regression: the minimax-M3 empty-tool-name bug trapped nanopi
    /// in a ~10-round retry loop where the
    /// gateway streamed tool_calls with `function.name = ""`, the
    /// router rejected each as "unknown tool: ", and the model
    /// re-emitted the identical call every turn until the user hit
    /// Esc. The parser fix (in `provider::openai::PendingToolCall`)
    /// stops the empty name from being emitted downstream, but as a
    /// belt-and-suspenders defense the agent loop also detects the
    /// "same failing tool_call K rounds in a row" pattern and aborts
    /// the turn with a visible error — otherwise a future upstream
    /// glitch could produce a similar loop and burn 50 iterations
    /// (`MAX_ITERATIONS`) before self-terminating.
    #[tokio::test]
    async fn identical_failing_tool_calls_break_the_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        /// Provider that emits the same unknown-tool call every turn.
        struct StuckProvider {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl Provider for StuckProvider {
            fn id(&self) -> &'static str {
                "stuck"
            }
            async fn stream_turn(
                &self,
                _ctx: &Context,
                tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _ = tx
                    .send(AgentEvent::Start {
                        message_id: "m".into(),
                    })
                    .await;
                let _ = tx
                    .send(AgentEvent::ToolCall {
                        content_index: 0,
                        call: ToolCall {
                            id: format!("call_{}", uuid::v7()),
                            name: "nosuchtool".into(),
                            arguments: json!({"command": "ls"}),
                        },
                    })
                    .await;
                let _ = tx
                    .send(AgentEvent::Done {
                        finish_reason: FinishReason::ToolCalls,
                        usage: Usage::default(),
                    })
                    .await;
                Ok(Usage::default())
            }
        }

        let dir = tmp();
        let session_path = dir.join("stuck.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":2,\"id\":\"019fe000-0000-7000-8000-000000000000\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"stuck\",\"base_url\":\"\"}\n",
        ).unwrap();

        let call_counter = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(StuckProvider {
                calls: call_counter.clone(),
            }),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: "stuck".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let drain = tokio::spawn(async move {
            let mut got_error = false;
            while let Some(ev) = rx.recv().await {
                if matches!(ev, AgentEvent::Error { .. }) {
                    got_error = true;
                }
            }
            got_error
        });

        let r = agent.run_turn("hi", &tx, None, None).await;
        drop(tx);
        let got_error_event = drain.await.unwrap();

        // Must abort with a Provider error — not silently exhaust
        // MAX_ITERATIONS (50).
        assert!(
            matches!(r, Err(AgentError::Provider(_))),
            "expected stuck-loop tripwire to fire, got {r:?}"
        );
        // And the renderer must have seen the Error event.
        assert!(got_error_event, "renderer must receive an Error event");
        // Streak trips at 3 identical rounds — must fire well before
        // MAX_ITERATIONS. Give a little slack for future tweaks: 5.
        let calls = call_counter.load(Ordering::SeqCst);
        assert!(
            calls <= 5,
            "tripwire should fire in ≤5 rounds, got {calls}"
        );
        assert!(
            calls >= 3,
            "tripwire should require ≥3 identical rounds before firing, got {calls}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P3 regression: `SteerMessage::Steering` sent mid-turn must be
    /// picked up at the next iteration boundary and injected as a
    /// fresh user message. Two-iteration SteppedProvider: first
    /// iteration emits text + tool_call, second iteration emits
    /// final text. The test sends `Steering { text: "hi steer" }`
    /// before the second iteration starts.
    #[tokio::test]
    async fn steer_message_injected_as_user_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct SteppedProvider {
            step: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl Provider for SteppedProvider {
            fn id(&self) -> &'static str {
                "stepped"
            }
            async fn stream_turn(
                &self,
                ctx: &Context,
                tx: mpsc::Sender<AgentEvent>,
            ) -> Result<Usage, String> {
                let step = self.step.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
                match step {
                    0 => {
                        let _ = tx.send(AgentEvent::TextDelta {
                            content_index: 0,
                            text: "first".into(),
                        }).await;
                        let _ = tx.send(AgentEvent::ToolCall {
                            content_index: 0,
                            call: ToolCall {
                                id: "c1".into(),
                                name: "bash".into(),
                                arguments: json!({"command": "echo hi"}),
                            },
                        }).await;
                        let _ = tx.send(AgentEvent::Done {
                            finish_reason: FinishReason::ToolCalls,
                            usage: Usage::default(),
                        }).await;
                    }
                    1 => {
                        // The steer message should now be in context
                        // as a user turn. Verify by checking the
                        // context's last user message.
                        let steer_present = ctx.messages.iter().any(|m| match m {
                            ContextMessage::User { content } => content.iter().any(|b| match b {
                                ContentBlock::Text { text } => text.contains("hi steer"),
                                _ => false,
                            }),
                            _ => false,
                        });
                        assert!(
                            steer_present,
                            "steer message must be in context by iteration 2"
                        );
                        let _ = tx.send(AgentEvent::TextDelta {
                            content_index: 0,
                            text: "done".into(),
                        }).await;
                        let _ = tx.send(AgentEvent::Done {
                            finish_reason: FinishReason::Stop,
                            usage: Usage::default(),
                        }).await;
                    }
                    _ => unreachable!(),
                }
                Ok(Usage::default())
            }
        }

        let dir = tmp();
        let session_path = dir.join("steer.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":2,\"id\":\"019fe000-0000-7000-8000-000000000000\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"stepped\",\"base_url\":\"\"}\n",
        ).unwrap();

        let step = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(SteppedProvider { step: step.clone() }),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: "stepped".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
        };

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);

        // Fire the steer AFTER the first iteration's tool_call is
        // processed but BEFORE the second iteration's stream_turn.
        // The real TUI sends it while the user types mid-turn; here
        // we just enqueue it before calling run_turn and let the
        // try_recv drain it on the second iteration boundary.
        let _ = steer_tx.send(SteerMessage::Steering {
            text: "hi steer".into(),
        }).await;

        let final_text = agent.run_turn("go", &tx, None, Some(steer_rx)).await.expect("turn");
        assert_eq!(final_text, "firstdone");

        // Drain channel.
        drop(tx);
        while rx.recv().await.is_some() {}

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Follow-up messages: a FollowUp steer message queues text for
    /// after the turn ends. `pending_follow_up` must be populated.
    #[tokio::test]
    async fn follow_up_message_populates_pending() {
        let dir = tmp();
        let session_path = dir.join("fu.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let (tx, _rx) = mpsc::channel::<AgentEvent>(16);
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);

        // Enqueue a follow-up message before calling run_turn.
        let _ = steer_tx.send(SteerMessage::FollowUp {
            text: "next question".into(),
        }).await;

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider { response: "ok".into() }),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
            pending_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
        };

        agent.run_turn("go", &tx, None, Some(steer_rx)).await.unwrap();

        assert_eq!(
            agent.pending_follow_up.as_deref(),
            Some("next question"),
            "FollowUp message must surface on agent.pending_follow_up"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

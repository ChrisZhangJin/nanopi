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
use crate::agent::hook::{
    event_payload_json, run_hooks, run_session_hooks, HookConfig, HookEvent, HookOutcome,
};
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
    pub tool_execution_start: Vec<HookConfig>,
    pub tool_execution_end: Vec<HookConfig>,
    pub input: Vec<HookConfig>,
    pub session_start: Vec<HookConfig>,
    pub session_shutdown: Vec<HookConfig>,
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
    /// v0.11.0: messages waiting to become their own turn. The TUI's
    /// turn-completed handler pops the front and auto-starts a turn
    /// with it (Pi's `getFollowUpMessages()` semantic).
    ///
    /// A queue rather than one slot: a `SteerMessage::FollowUp` can
    /// arrive more than once per turn, and cancelling a turn converts
    /// every still-pending steer into a follow-up at once. Keeping only
    /// the first silently dropped the rest.
    pub pending_follow_ups: std::collections::VecDeque<String>,
    /// v0.11.0: tool execution mode (parallel by default; user can
    /// configure sequential via `tool_exec_mode` in config.toml).
    /// Set at build time and reused on every turn.
    pub tool_exec_mode: crate::config::ToolExecMode,
    /// v0.11.0: slash commands registered by WASM plugins, already
    /// filtered for collisions. Held here for the same reason `skills`
    /// is: the TUI snapshots it after every rebuild rather than
    /// reaching into the plugin layer, which keeps `mode::tui` free of
    /// any `cfg(feature = "wasm")`. Empty in a build without the
    /// feature, and in print mode, which has no command palette.
    pub plugin_commands: Vec<crate::command::PluginCommand>,
    /// Lifecycle-event subscribers registered by WASM plugins — the
    /// granted ∩ requested intersection computed at load time. Held
    /// here for the same non-gated reason as `plugin_commands`: keeps
    /// `mode::tui` free of `cfg(feature = "wasm")`. Empty in a build
    /// without the feature.
    pub event_subscribers: crate::subscriber::EventSubscribers,
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            // Populated by `hydrate_resumed`, which is what loads the
            // plugins — `load_session` only replays JSONL and knows
            // nothing about config.
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        })
    }

    /// Fire all `session_start` hooks. Advisory — outcome is not enforced.
    /// Call once, after Agent construction, before the first turn.
    ///
    /// `reason` is the session-start vocabulary: `startup|new|resume|fork|
    /// import`. Diverges from PI (which has no `import`) because nanopi
    /// supports importing a session from an external source; matches PI's
    /// `startup|new|resume|fork` otherwise.
    ///
    /// v0.9.1 fix: honors `--no-hooks` — previously session lifecycle
    /// hooks leaked through the emergency switch because only the
    /// tool-facing sites gated on `hooks_active()`.
    pub async fn fire_session_start(&self, reason: &str) {
        if !self.permission.hooks_active() {
            return;
        }
        let arguments = serde_json::json!({"reason": reason});
        run_session_hooks(
            &self.hooks.session_start,
            HookEvent::SessionStart,
            arguments.clone(),
            &self.session_id.to_string(),
            &self.session_id.to_string(),
            &self.cwd,
        )
        .await;
        let session_id = self.session_id.to_string();
        let cwd = self.cwd.clone();
        self.event_subscribers.deliver_with(HookEvent::SessionStart, || {
                event_payload_json(
                    HookEvent::SessionStart,
                    None,
                    None,
                    &arguments,
                    &cwd,
                    Some(&session_id),
                )
            })
            .await;
    }

    /// Fire all `session_shutdown` hooks. Advisory. Call before the process
    /// exits (or before Agent is dropped in the interactive loop).
    ///
    /// `reason` is the session-shutdown vocabulary: `quit|new|resume|fork|
    /// import`. Diverges from PI (which has no `reload` and no `import`)
    /// — nanopi has no `reload` reason, and adds `import` for the same
    /// reason as `fire_session_start`.
    ///
    /// See `fire_session_start` for the `--no-hooks` note.
    pub async fn fire_session_shutdown(&self, reason: &str) {
        if !self.permission.hooks_active() {
            return;
        }
        let arguments = serde_json::json!({"reason": reason});
        run_session_hooks(
            &self.hooks.session_shutdown,
            HookEvent::SessionShutdown,
            arguments.clone(),
            &self.session_id.to_string(),
            &self.session_id.to_string(),
            &self.cwd,
        )
        .await;
        let session_id = self.session_id.to_string();
        let cwd = self.cwd.clone();
        self.event_subscribers.deliver_with(HookEvent::SessionShutdown, || {
                event_payload_json(
                    HookEvent::SessionShutdown,
                    None,
                    None,
                    &arguments,
                    &cwd,
                    Some(&session_id),
                )
            })
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
    /// Returns true if a pass actually ran. False means
    /// `find_compact_boundary` found nothing to do — the whole context
    /// already fits inside the verbatim tail budget — and the context is
    /// byte-for-byte unchanged.
    ///
    /// The caller needs this to describe what happened: measuring
    /// `estimate_chars()` either side can't tell "compacted, and the
    /// summary happened to be the same length" from "never ran", and
    /// `/compact` on a short session reported the first while doing the
    /// second.
    pub async fn compact_now(
        &mut self,
        tx: Option<&mpsc::Sender<AgentEvent>>,
        reason: &str,
    ) -> bool {
        use crate::agent::compact::compact;

        // Decide whether there is anything to do BEFORE announcing
        // anything. `compact()` makes this same call internally and
        // returns None when it comes back empty, but by then the
        // `session_before_compact` hook has already fired — leaving a
        // `before` with no matching `after` in every hook log, and no
        // way for a hook author to tell that pair apart from a crash
        // mid-compaction. The duplicated boundary scan is a walk over
        // the message list; the honesty is worth it.
        if crate::agent::compact::find_compact_boundary(
            &self.context.messages,
            crate::agent::compact::KEEP_RECENT_TOKENS,
        )
        .is_none()
        {
            return false;
        }

        // ── SessionBeforeCompact hook (v0.11.0) ──────────────────
        // Advisory only — fires before compaction runs. `subject`
        // (matcher target) is still the compaction reason string
        // ("threshold" or "manual"); `session_id` is the real session
        // id, carried honestly in the payload (v0.12.0 fix — this used
        // to be the reason string, not the actual session id).
        if self.permission.hooks_active() {
            let arguments = serde_json::json!({"reason": reason});
            if !self.hooks.session_before_compact.is_empty() {
                run_session_hooks(
                    &self.hooks.session_before_compact,
                    HookEvent::SessionBeforeCompact,
                    arguments.clone(),
                    reason,
                    &self.session_id.to_string(),
                    &self.cwd,
                )
                .await;
            }
            let session_id = self.session_id.to_string();
            let cwd = self.cwd.clone();
            self.event_subscribers.deliver_with(HookEvent::SessionBeforeCompact, || {
                    event_payload_json(
                        HookEvent::SessionBeforeCompact,
                        None,
                        None,
                        &arguments,
                        &cwd,
                        Some(&session_id),
                    )
                })
                .await;
        }

        if let Some(tx) = tx {
            let _ = tx
                .send(AgentEvent::CompactionStart {
                    reason: reason.to_string(),
                })
                .await;
        }
        // The boundary was there a moment ago and nothing has touched
        // the context since, so this is not expected to be None.
        let Some(result) = compact(&mut self.context, self.provider.as_ref()).await else {
            return false;
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
        // Fires after compaction completes. `subject` (matcher target)
        // is the compaction reason; `session_id` is the real session
        // id, carried honestly in the payload.
        if self.permission.hooks_active() {
            let arguments = serde_json::json!({"reason": reason});
            if !self.hooks.session_compact.is_empty() {
                run_session_hooks(
                    &self.hooks.session_compact,
                    HookEvent::SessionCompact,
                    arguments.clone(),
                    reason,
                    &self.session_id.to_string(),
                    &self.cwd,
                )
                .await;
            }
            let session_id = self.session_id.to_string();
            let cwd = self.cwd.clone();
            self.event_subscribers.deliver_with(HookEvent::SessionCompact, || {
                    event_payload_json(
                        HookEvent::SessionCompact,
                        None,
                        None,
                        &arguments,
                        &cwd,
                        Some(&session_id),
                    )
                })
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
        true
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
        // Over threshold but no boundary (a single message larger than
        // the tail budget) means no pass ran — say so rather than
        // reporting one.
        self.compact_now(Some(tx), "threshold").await
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
    /// Move anything still sitting in the steer channel onto the
    /// follow-up queue.
    ///
    /// Called on every early return from `run_turn`. A steer that the
    /// channel accepted but the pump never reached has already been
    /// echoed to the user as landed; letting it die with the receiver
    /// makes their text vanish without a trace — the same failure
    /// c15c8a9 fixed on the send side and missed here. On cancellation
    /// a `Steering` message becomes a follow-up, since there is no
    /// longer a turn for it to steer.
    fn drain_steer_to_follow_ups(
        &mut self,
        steer_rx: &mut Option<mpsc::Receiver<SteerMessage>>,
        queue: &mut Vec<String>,
    ) {
        if let Some(rx) = steer_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(SteerMessage::Steering { text })
                    | Ok(SteerMessage::FollowUp { text }) => queue.push(text),
                    Err(mpsc::error::TryRecvError::Empty)
                    | Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
        }
        self.pending_follow_ups.extend(queue.drain(..));
    }

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
        //
        // A rewritten prompt lands in `pre_start_msg` rather than being
        // applied directly: `effective_msg` is only born below, and
        // seeding it from here is what makes the two prompt hooks chain
        // — BeforeAgentStart's output is Input's input.
        let mut pre_start_msg: Option<String> = None;
        if self.permission.hooks_active() {
            let turn_label = self.turn_count.to_string();
            let arguments = serde_json::json!({
                "turn_count": self.turn_count,
                "prompt": user_msg,
            });
            // A Block is stashed rather than returned from inside the
            // match, so the WASM delivery below still runs. Observe-only
            // subscribers must not have a blind spot exactly where a
            // shell hook refuses something — an audit plugin cares most
            // about the refused turns. Matches what the
            // ToolExecutionStart site does with its own block.
            let mut blocked: Option<String> = None;
            if !self.hooks.before_agent_start.is_empty() {
                let (outcome, new_args) = run_hooks(
                    &self.hooks.before_agent_start,
                    HookEvent::BeforeAgentStart,
                    &turn_label,
                    None,
                    arguments.clone(),
                    &self.cwd,
                    Some(&self.session_id.to_string()),
                )
                .await;
                match outcome {
                    HookOutcome::Block { reason } => {
                        blocked = Some(reason);
                    }
                    HookOutcome::Transform { new_arguments } => {
                        // Only the `prompt` key is honored — the payload
                        // also carries `turn_count`, which is ours, not the
                        // hook's, to rewrite.
                        if let Some(v) = new_arguments.get("prompt").and_then(|v| v.as_str()) {
                            pre_start_msg = Some(v.to_string());
                        }
                    }
                    HookOutcome::Allow => {
                        // `run_hooks` folds transforms into its accumulated
                        // args and still reports Allow when no hook blocked,
                        // so a rewrite usually arrives here rather than in
                        // the Transform arm. Same fallback the Input hook
                        // uses; compare against `user_msg` so an unchanged
                        // echo doesn't count as a rewrite.
                        if let Some(v) = new_args
                            .as_ref()
                            .and_then(|a| a.get("prompt"))
                            .and_then(|v| v.as_str())
                        {
                            if v != user_msg {
                                pre_start_msg = Some(v.to_string());
                            }
                        }
                    }
                }
            }
            let session_id = self.session_id.to_string();
            let cwd = self.cwd.clone();
            self.event_subscribers.deliver_with(HookEvent::BeforeAgentStart, || {
                    event_payload_json(
                        HookEvent::BeforeAgentStart,
                        Some(&turn_label),
                        None,
                        &arguments,
                        &cwd,
                        Some(&session_id),
                    )
                })
                .await;
            if let Some(reason) = blocked {
                let marker = format!("[BeforeAgentStart hook blocked the turn: {reason}]");
                let _ = tx
                    .send(AgentEvent::Error {
                        error: marker.clone(),
                    })
                    .await;
                return Ok(marker);
            }
        }

        // ── Input hook (mirrors PI's beforeUserMessage) ────
        // Allow user hooks to inspect / transform the raw prompt before
        // any skill-command expansion, and to block outright. Same
        // Allow/Block/Transform semantics as tool_execution_start hooks;
        // Block aborts the turn with a synthetic assistant marker so the
        // user sees why. Transform mutates the prompt in place.
        let mut effective_msg = pre_start_msg.unwrap_or_else(|| user_msg.to_string());
        if self.permission.hooks_active() {
            // Pre-transform arguments — shared byte-for-byte by the
            // shell-hook call and the WASM delivery below, per §4.1.
            let arguments = serde_json::json!({ "prompt": effective_msg });
            // Stashed, not returned from inside the match — same
            // reasoning as the BeforeAgentStart site above: an
            // observe-only subscriber must still see a prompt that a
            // shell hook refused.
            let mut blocked: Option<String> = None;
            if !self.hooks.input.is_empty() {
                let (outcome, new_args) = run_hooks(
                    &self.hooks.input,
                    HookEvent::Input,
                    // No tool name here, so `matcher` is tested against "" —
                    // only `*` (or an omitted matcher) can ever match. Any
                    // real regex silently never fires.
                    "",
                    None,
                    arguments.clone(),
                    &self.cwd,
                    Some(&self.session_id.to_string()),
                )
                .await;
                match outcome {
                    HookOutcome::Block { reason } => {
                        blocked = Some(reason);
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
            let session_id = self.session_id.to_string();
            let cwd = self.cwd.clone();
            self.event_subscribers.deliver_with(HookEvent::Input, || {
                    event_payload_json(
                        HookEvent::Input,
                        Some(""),
                        None,
                        &arguments,
                        &cwd,
                        Some(&session_id),
                    )
                })
                .await;
            if let Some(reason) = blocked {
                let marker = format!("[Input hook blocked the prompt: {reason}]");
                let _ = tx
                    .send(AgentEvent::Error {
                        error: marker.clone(),
                    })
                    .await;
                return Ok(marker);
            }
        }

        // ── /skill:name expansion (mirrors PI's _expandSkillCommand) ──
        // Runs AFTER the Input hook so a hook that rewrites
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
                    self.drain_steer_to_follow_ups(&mut steer_rx, &mut follow_up_queue);
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
                        // Spelled out rather than `_` so adding a
                        // `SteerMessage` variant is a compile error
                        // here instead of silently falling into this
                        // arm and aborting the drain with the rest of
                        // the queue still buffered.
                        Err(mpsc::error::TryRecvError::Empty)
                        | Err(mpsc::error::TryRecvError::Disconnected) => break,
                    }
                }
            }

            // ── TurnStart hook (v0.11.0) ────────────────────────────────
            // Advisory only — a Block is reported on stderr but does not
            // abort the iteration. matcher applied to turn_count (as
            // string).
            if self.permission.hooks_active() {
                let turn_label = self.turn_count.to_string();
                let arguments = serde_json::json!({
                    "turn_count": self.turn_count,
                    "iteration": iteration_idx,
                });
                if !self.hooks.turn_start.is_empty() {
                    let (outcome, _) = run_hooks(
                        &self.hooks.turn_start,
                        HookEvent::TurnStart,
                        &turn_label,
                        None,
                        arguments.clone(),
                        &self.cwd,
                        Some(&self.session_id.to_string()),
                    )
                    .await;
                    crate::agent::hook::report_advisory_outcome(
                        HookEvent::TurnStart,
                        outcome,
                    );
                }
                let session_id = self.session_id.to_string();
                let cwd = self.cwd.clone();
                self.event_subscribers.deliver_with(HookEvent::TurnStart, || {
                        event_payload_json(
                            HookEvent::TurnStart,
                            Some(&turn_label),
                            None,
                            &arguments,
                            &cwd,
                            Some(&session_id),
                        )
                    })
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
                self.drain_steer_to_follow_ups(&mut steer_rx, &mut follow_up_queue);
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
            // Advisory only — fired at the bottom of each iteration. A
            // Block is reported on stderr and otherwise ignored.
            if self.permission.hooks_active() {
                let turn_label = self.turn_count.to_string();
                let arguments = serde_json::json!({
                    "turn_count": self.turn_count,
                    "iteration": iteration_idx,
                    "had_tool_calls": had_tool_calls,
                });
                if !self.hooks.turn_end.is_empty() {
                    let (outcome, _) = run_hooks(
                        &self.hooks.turn_end,
                        HookEvent::TurnEnd,
                        &turn_label,
                        None,
                        arguments.clone(),
                        &self.cwd,
                        Some(&self.session_id.to_string()),
                    )
                    .await;
                    crate::agent::hook::report_advisory_outcome(
                        HookEvent::TurnEnd,
                        outcome,
                    );
                }
                let session_id = self.session_id.to_string();
                let cwd = self.cwd.clone();
                self.event_subscribers.deliver_with(HookEvent::TurnEnd, || {
                        event_payload_json(
                            HookEvent::TurnEnd,
                            Some(&turn_label),
                            None,
                            &arguments,
                            &cwd,
                            Some(&session_id),
                        )
                    })
                    .await;
            }
        }
        // ── MessageEnd hook (v0.11.0) ───────────────────────────────────
        // Fires once after the for-loop completes (all tool rounds done),
        // just before post-turn compaction. Advisory only — a Block is
        // reported on stderr but does not abort the turn (which has
        // already ended anyway).
        if self.permission.hooks_active() {
            let turn_label = self.turn_count.to_string();
            let arguments = serde_json::json!({
                "turn_count": self.turn_count,
                "response_length": final_text.len(),
            });
            if !self.hooks.message_end.is_empty() {
                let (outcome, _) = run_hooks(
                    &self.hooks.message_end,
                    HookEvent::MessageEnd,
                    &turn_label,
                    None,
                    arguments.clone(),
                    &self.cwd,
                    Some(&self.session_id.to_string()),
                )
                .await;
                crate::agent::hook::report_advisory_outcome(
                    HookEvent::MessageEnd,
                    outcome,
                );
            }
            let session_id = self.session_id.to_string();
            let cwd = self.cwd.clone();
            self.event_subscribers.deliver_with(HookEvent::MessageEnd, || {
                    event_payload_json(
                        HookEvent::MessageEnd,
                        Some(&turn_label),
                        None,
                        &arguments,
                        &cwd,
                        Some(&session_id),
                    )
                })
                .await;
        }
        // Post-turn compaction check. Matches PI (`agent-session.ts`
        // `_handlePostAgentRun`). Firing here means the user sees the
        // compaction event bundled with the just-finished response
        // instead of at the start of the next turn.
        self.maybe_compact(tx).await;

        // Anything still sitting in the steer channel arrived too late
        // for the steer pump, which only runs at the top of an
        // iteration. A turn that ends without tool calls — the common
        // case — has no next iteration, so a message typed during the
        // stream was left in the channel: never pushed into context,
        // never persisted, never seen by the model. The TUI had already
        // drawn its `[steer]` bar, so the user was told it landed.
        //
        // Demote them to follow-ups, same as the cancel and
        // interrupted-marker paths already do, and the same contract
        // the WIT docs state for `send_user_message`: steer the running
        // turn, or queue as a follow-up if it arrives too late.
        self.drain_steer_to_follow_ups(&mut steer_rx, &mut follow_up_queue);

        // v0.11.0: surface the first `FollowUp` message (if any)
        // onto the Agent so the caller can auto-trigger another
        // turn. Matches Pi's `getFollowUpMessages()` semantic.
        self.pending_follow_ups.extend(follow_up_queue.drain(..));
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
        let subscribers = self.event_subscribers.clone();

        // Keep id/name copies so we can synthesize cancelled results if
        // the whole batch gets dropped mid-flight (the calls Vec itself
        // is moved into the futures).
        let call_meta: Vec<(String, String)> = calls
            .iter()
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();

        // Outcomes land here as each tool finishes, so a cancel that
        // interrupts the batch still sees what already completed. Plain
        // `std::sync::Mutex`: the lock is taken and released inside one
        // statement, never across an await.
        let completed: std::sync::Arc<std::sync::Mutex<Vec<ToolCallOutcome>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        // Per-path serialization. The batch is split into groups; the
        // groups run concurrently, the calls inside one group run one
        // after another in the order the model emitted them. A group is
        // "the calls that mutate the same file" (see
        // `tool::mutation_key`); every non-mutating call is its own
        // one-element group, so it still runs fully in parallel.
        //
        // Sequential mode is NOT grouped, and that is load-bearing
        // rather than an optimization: grouping reorders a batch (all
        // calls for one key move to the position of the first of them),
        // and Sequential's contract is "one at a time, in the order the
        // model emitted them". One call per group reproduces today's
        // behaviour exactly — including the per-call cancel checks
        // below, which straddle group boundaries.
        let groups: Vec<Vec<ToolCall>> = match self.tool_exec_mode {
            crate::config::ToolExecMode::Sequential => calls.into_iter().map(|c| vec![c]).collect(),
            crate::config::ToolExecMode::Parallel => group_by_mutation_key(&registry, &cwd, calls),
        };

        let futs: Vec<_> = groups
            .into_iter()
            .map(|group| {
                let cwd = cwd.clone();
                let registry = registry.clone();
                let session_path = session_path.clone();
                let session_id = session_id.clone();
                let permission = permission.clone();
                let hooks = hooks.clone();
                let subscribers = subscribers.clone();
                let tx = tx.clone();
                let done = completed.clone();
                async move {
                    // Serial within the group. Awaiting each call before
                    // starting the next is the whole guarantee: the
                    // second `edit` of a file cannot snapshot it until
                    // the first has written.
                    for call in group {
                        let outcome = run_one_tool(
                            call,
                            registry.clone(),
                            session_path.clone(),
                            session_id.clone(),
                            cwd.clone(),
                            permission.clone(),
                            hooks.clone(),
                            subscribers.clone(),
                            tx.clone(),
                        )
                        .await;
                        // Recorded through a shared handle rather than
                        // only returned, because `join_all`'s output is
                        // all-or-nothing: when cancel wins the race
                        // below, its `Vec` is empty even for tools that
                        // already finished — and already wrote their
                        // real ToolResult to the session file. Losing
                        // them there is what made the cancel path append
                        // a SECOND, contradictory entry for the same
                        // tool_call_id.
                        //
                        // Pushing per call (not per group) keeps that
                        // true at group granularity too: a cancel that
                        // drops a group mid-way still leaves the calls
                        // it already finished recorded here, so the
                        // synthesis pass below only invents results for
                        // calls that genuinely never ran.
                        done.lock().unwrap_or_else(|e| e.into_inner()).push(outcome);
                    }
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
        let cancelled = match self.tool_exec_mode {
            crate::config::ToolExecMode::Parallel => match cancel.as_ref() {
                Some(ct) => tokio::select! {
                    biased;
                    _ = ct.cancelled() => true,
                    _ = join_all(futs) => false,
                },
                None => {
                    join_all(futs).await;
                    false
                }
            },
            crate::config::ToolExecMode::Sequential => {
                // Sequentially await each tool. Cancel is raced against
                // the *in-flight* future, not just checked between
                // tools: a between-tools-only check meant a 30s bash
                // ran to completion after Esc, since nothing was
                // polling the token while it was awaited. Dropping the
                // future here has the same effect as the Parallel arm —
                // kill_on_drop SIGKILLs any live bash child.
                //
                // The returned `Vec<ToolCallOutcome>` has the same
                // shape as `join_all` for the post-phase, except it may
                // be short when cancel won.
                let mut cancelled = false;
                let mut iter = futs.into_iter();
                loop {
                    // Pre-check so an already-cancelled token doesn't
                    // pay for spawning the next tool at all.
                    if let Some(ct) = cancel.as_ref() {
                        if ct.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                    }
                    match iter.next() {
                        Some(fut) => match cancel.as_ref() {
                            // `biased` matches the Parallel arm: cancel
                            // is polled first so a fast Esc can't lose
                            // the race to a fast-finishing tool.
                            Some(ct) => tokio::select! {
                                biased;
                                _ = ct.cancelled() => {
                                    cancelled = true;
                                    break;
                                }
                                _ = fut => {}
                            },
                            None => fut.await,
                        },
                        None => break,
                    }
                }
                cancelled
            }
        };

        // Both arms record into `completed`; neither returns outcomes
        // directly any more, so the two modes now behave identically
        // under cancellation.
        let results = std::sync::Arc::try_unwrap(completed)
            .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
            .unwrap_or_else(|arc| {
                // A future still holds a reference — only reachable if
                // cancel dropped the batch mid-poll. Clone what landed.
                arc.lock().unwrap_or_else(|e| e.into_inner()).clone()
            });

        let results = if cancelled {
            // Cancel can land mid-batch in either mode, so some tools
            // already ran — and already appended their real ToolResult
            // to the session JSONL from inside `run_one_tool`. Keeping
            // those and synthesizing only for the calls that never ran
            // is what stops a second, contradictory entry being written
            // for the same tool_call_id.
            //
            // That duplicate was live in Parallel until `completed`
            // existed: `join_all` returns an empty Vec when cancel wins
            // the race, so finished-and-persisted tools looked un-run
            // here and got a cancel marker appended on top of their real
            // result. On resume `load_session` replayed both, and two
            // tool_result blocks sharing one tool_use_id is a permanent
            // 400 from Anthropic — the session could never be resumed
            // again.
            //
            // Iterating `call_meta` rather than `results` keeps
            // tool_result order matching tool_use order; Anthropic also
            // 400s on an unmatched or misordered pair.
            let mut done: std::collections::HashMap<String, ToolCallOutcome> = results
                .into_iter()
                .map(|o| (o.call_id.clone(), o))
                .collect();
            let mut synth = Vec::with_capacity(call_meta.len());
            for (id, _name) in call_meta {
                if let Some(finished) = done.remove(&id) {
                    synth.push(finished);
                    continue;
                }
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
#[derive(Clone)]
struct ToolCallOutcome {
    call_id: String,
    content: String,
    is_error: bool,
    /// Multimodal image attachments from the tool (empty for text-only
    /// tools). Forwarded into context so the next request to a vision
    /// model can carry the image blocks.
    images: Vec<crate::tool::ImageAttachment>,
}

/// Split a parallel batch into groups that must not run concurrently
/// with each other.
///
/// Calls sharing a `tool::mutation_key` land in one group and are run
/// serially by the caller; calls with no key (`bash`, `read`, `grep`,
/// `find`, `ls`, WASM plugin tools — see `tool::mutation_key` for why)
/// each get their own group and stay fully parallel.
///
/// Two ordering properties the caller depends on:
///
///   - within a group, model order is preserved (calls are pushed in
///     iteration order);
///   - groups appear in order of their first member, so the batch's
///     overall shape stays as close to model order as grouping allows.
///
/// Names are canonicalized first. `run_turn` already normalizes
/// gateway-mangled names (`Edit_tool` → `edit`) before calling
/// `execute_tool_calls`, but `execute_tool_calls` is `pub` and reachable
/// with raw names, and a missed normalization here would silently
/// degrade to "no serialization" rather than fail loudly.
fn group_by_mutation_key(
    registry: &ToolRegistry,
    cwd: &Path,
    calls: Vec<ToolCall>,
) -> Vec<Vec<ToolCall>> {
    let mut groups: Vec<Vec<ToolCall>> = Vec::new();
    let mut index: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for call in calls {
        let name = registry
            .canonical_name(&call.name)
            .unwrap_or_else(|| call.name.clone());
        match crate::tool::mutation_key(cwd, &name, &call.arguments) {
            Some(key) => match index.get(&key) {
                Some(&i) => groups[i].push(call),
                None => {
                    index.insert(key, groups.len());
                    groups.push(vec![call]);
                }
            },
            None => groups.push(vec![call]),
        }
    }
    groups
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
    subscribers: crate::subscriber::EventSubscribers,
    tx: mpsc::Sender<AgentEvent>,
) -> ToolCallOutcome {
    // ToolExecutionStart hooks. `hook::PER_DELTA_EVENTS` (message_update,
    // tool_execution_update) are the two per-delta events that will never
    // be plugin-deliverable (§5.2) — this is the nearest per-tool-call
    // site to anchor that comment against.
    let mut effective_args = call.arguments.clone();
    if permission.hooks_active() {
        let mut block: Option<String> = None;
        if !hooks.tool_execution_start.is_empty() {
            let (outcome, transformed) = run_hooks(
                &hooks.tool_execution_start,
                HookEvent::ToolExecutionStart,
                &call.name,
                Some(call.id.as_str()),
                call.arguments.clone(),
                &cwd,
                Some(&session_id.to_string()),
            )
            .await;
            effective_args = transformed.unwrap_or(call.arguments.clone());
            if let HookOutcome::Block { reason } = outcome {
                if permission.should_honor_tool_execution_start_block() {
                    block = Some(reason);
                }
            }
        }
        // Tell the renderers (and, below, the model) that the call
        // that actually runs is not the one the model asked for.
        // `AgentEvent::ToolCall` was forwarded off the provider stream
        // before this hook ran, so without this the card shows the
        // pre-transform command forever.
        if effective_args != call.arguments {
            let _ = tx
                .send(AgentEvent::ToolCallRewritten {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: effective_args.clone(),
                })
                .await;
        }
        let session_id_str = session_id.to_string();
        subscribers.deliver_with(HookEvent::ToolExecutionStart, || {
                event_payload_json(
                    HookEvent::ToolExecutionStart,
                    Some(&call.name),
                    Some(call.id.as_str()),
                    &call.arguments,
                    &cwd,
                    Some(&session_id_str),
                )
            })
            .await;
        if let Some(reason) = block {
            // Self-explanatory on purpose. The system prompt never
            // mentions that hooks exist, so `blocked by hook: X` left
            // the model guessing what a "hook" was — in manual testing
            // it twice concluded a sandbox was intercepting bash, and
            // once went looking for `.claude/settings.json`, i.e. a
            // different product's config. Naming the mechanism and
            // ruling out the wrong hypothesis costs one line and saves
            // a turn of investigation.
            //
            // The hook's own reason is preserved verbatim and last, so
            // a hook author's message stays the most visible part.
            let result_text = format!(
                "blocked by a user-configured `tool_execution_start` hook — this is a \
                 policy refusal from the user's nanopi configuration, not a sandbox or \
                 environment failure. Hook's reason: {reason}"
            );
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

    // Tell the model a hook rewrote its arguments.
    //
    // Not cosmetic: without it the model gets an answer to a question
    // it did not ask, with no way to find out why. In manual testing
    // it concluded a sandbox was replacing bash output with a
    // constant, then spent a turn devising an experiment to confirm
    // that theory. Hiding the rewrite by silently replacing the
    // model's own recorded arguments would remove the contradiction
    // but also the information — and a coding agent whose commands are
    // being normalized should know, so it can stop re-sending the form
    // that gets rewritten.
    //
    // Only on an actual rewrite, so no ordinary tool result changes
    // shape. Prefixed rather than appended: a long result would push a
    // trailing note out of the model's attention, and out of the
    // 6-line card preview entirely.
    if effective_args != call.arguments {
        let args_line = serde_json::to_string(&effective_args)
            .unwrap_or_else(|_| effective_args.to_string());
        content = format!(
            "[note: a tool_execution_start hook rewrote the arguments of this \
             call to {args_line} — the output below is from those arguments, \
             not the ones you sent]\n{content}"
        );
    }

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

    // ToolExecutionEnd hooks. Payload mirrors Claude Code's post-tool-use
    // wire schema so hooks can inspect what actually happened —
    // `tool_input` (final args after any ToolExecutionStart transform) and
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
    if permission.hooks_active() {
        let post_payload = serde_json::json!({
            "tool_input": effective_args,
            "tool_response": {
                "content": content,
                "is_error": is_error,
                "duration_ms": elapsed.as_millis() as u64,
            },
        });
        if !hooks.tool_execution_end.is_empty() {
            let (_outcome, new_args) = run_hooks(
                &hooks.tool_execution_end,
                HookEvent::ToolExecutionEnd,
                &call.name,
                Some(call.id.as_str()),
                post_payload.clone(),
                &cwd,
                Some(&session_id.to_string()),
            )
            .await;
            // P1: tool_execution_end Transform replaces the tool result content.
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
        let session_id_str = session_id.to_string();
        subscribers.deliver_with(HookEvent::ToolExecutionEnd, || {
                event_payload_json(
                    HookEvent::ToolExecutionEnd,
                    Some(&call.name),
                    Some(call.id.as_str()),
                    &post_payload,
                    &cwd,
                    Some(&session_id_str),
                )
            })
            .await;
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
            tool_execution_start: vec![pre_hook],
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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

    /// Regression: v0.9.1 ToolExecutionEnd used to be called with
    /// `Value::Object(Default::default())` (empty `{}`) — hooks
    /// couldn't see what the tool actually did. Fix populates the
    /// payload with `tool_input` (final args) and `tool_response`
    /// (content / is_error / duration_ms). This test writes the
    /// hook stdin JSON to disk and asserts every field is present.
    #[tokio::test]
    async fn tool_execution_end_hook_receives_input_and_response() {
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
            tool_execution_end: vec![post_hook],
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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

        assert_eq!(v["event"], "tool_execution_end");
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
        // v0.11.0: was hardcoded `None` at every call site, so this
        // field was permanently null and a tool_execution_end hook had no
        // way to pair a result with its tool_execution_start.
        assert_eq!(v["tool_call_id"], "call_pt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1 regression: tool_execution_end hook can rewrite the tool's output
    /// content via `{"updated_input":{"content":"..."}}`. Previously
    /// the hook's return was discarded (`let _ = run_hooks(...)`),
    /// so redact / log-scrubbing / result-truncation plugins were
    /// inert. Fix: capture the outcome and apply Transform to the
    /// result `content` and `is_error` (image attachments keep their
    /// original handling).
    #[tokio::test]
    async fn tool_execution_end_hook_can_transform_result() {
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
            tool_execution_end: vec![post_hook],
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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

    /// Regression: the BeforeAgentStart `Transform` arm was empty and
    /// `run_hooks`' rewritten args were dropped on the floor, so a hook
    /// returning `updated_input.prompt` was silently inert even though
    /// the payload ships `prompt` specifically for it. Fix mirrors
    /// Input and seeds `effective_msg` from the result.
    /// A shell hook that BLOCKS must still deliver the event to
    /// observe-only WASM subscribers.
    ///
    /// Regression guard for an inconsistency: `tool_execution_start`
    /// stashed its block and delivered first, while
    /// `before_agent_start` and `input` returned from inside the match
    /// and skipped delivery. Two behaviours for one situation, and the
    /// missing half is the security-relevant one — an audit plugin
    /// cares most about the turns something refused, and a dropped
    /// event is invisible to it.
    ///
    /// Delivery cannot change the outcome (observe-only is in the
    /// `EventHandler` signature), so delivering costs nothing but a
    /// blind spot is real.
    /// Every emit site must actually deliver.
    ///
    /// The `deliver_with` mechanism is unit-tested in `subscriber`, and
    /// two sites are pinned by the blocking-hook test below — but
    /// nothing asserted that each of the eleven `run_hooks` sites has a
    /// matching delivery. A missing `deliver_with` at one site is
    /// completely silent: the plugin just never hears about that event
    /// and there is no error anywhere to notice.
    ///
    /// So run a real turn that goes through a tool round and assert the
    /// exact set of events a turn CAN produce. The two compaction
    /// events are not reachable from `run_turn` without crossing the
    /// threshold, so they are asserted separately below.
    #[tokio::test]
    async fn a_turn_delivers_every_event_it_can_reach() {
        use crate::subscriber::{EventHandler, EventSubscribers, Subscriber};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        #[derive(Default)]
        struct Recorder {
            seen: std::sync::Mutex<Vec<String>>,
        }
        impl EventHandler for Recorder {
            fn handle_event(&self, event: &str, _payload: &str) {
                self.seen.lock().unwrap().push(event.to_string());
            }
        }

        let recorder = Arc::new(Recorder::default());
        let subs = EventSubscribers::from_subscribers(vec![Subscriber {
            plugin_name: Arc::from("watcher"),
            events: crate::agent::hook::EVENT_NAMES.to_vec(),
            handler: recorder.clone(),
        }]);

        let dir = tmp();
        let session_path = dir.join("all-events.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(SteppedProviderForEvents {
                step: Arc::new(AtomicUsize::new(0)),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: subs,
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        agent.fire_session_start("startup").await;
        agent.run_turn("go", &tx, None, None).await.unwrap();
        agent.fire_session_shutdown("quit").await;

        let seen = recorder.seen.lock().unwrap().clone();
        for expected in [
            "session_start",
            "before_agent_start",
            "input",
            "turn_start",
            "tool_execution_start",
            "tool_execution_end",
            "turn_end",
            "message_end",
            "session_shutdown",
        ] {
            assert!(
                seen.iter().any(|e| e == expected),
                "{expected} was never delivered; a turn produced {seen:?}"
            );
        }

        // Ordering that a subscriber can rely on: the turn is bracketed,
        // and a tool's start precedes its end.
        let pos = |name: &str| seen.iter().position(|e| e == name).unwrap();
        assert!(pos("session_start") < pos("before_agent_start"));
        assert!(pos("before_agent_start") < pos("input"));
        assert!(pos("input") < pos("turn_start"));
        assert!(pos("tool_execution_start") < pos("tool_execution_end"));
        assert!(pos("message_end") < pos("session_shutdown"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two events the turn test cannot reach. Together with it,
    /// all eleven are covered.
    #[tokio::test]
    async fn compaction_delivers_both_of_its_events() {
        use crate::subscriber::{EventHandler, EventSubscribers, Subscriber};
        use std::sync::Arc;

        #[derive(Default)]
        struct Recorder {
            seen: std::sync::Mutex<Vec<String>>,
        }
        impl EventHandler for Recorder {
            fn handle_event(&self, event: &str, payload: &str) {
                self.seen
                    .lock()
                    .unwrap()
                    .push(format!("{event}:{payload}"));
            }
        }

        let recorder = Arc::new(Recorder::default());
        let subs = EventSubscribers::from_subscribers(vec![Subscriber {
            plugin_name: Arc::from("watcher"),
            events: crate::agent::hook::EVENT_NAMES.to_vec(),
            handler: recorder.clone(),
        }]);

        let dir = tmp();
        let session_path = dir.join("compaction-events.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: "summary".into(),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: subs,
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };
        // Enough tail that find_compact_boundary actually cuts —
        // otherwise `compact` returns None, the function returns before
        // the SessionCompact site, and the test would be asserting the
        // wrong thing. Same sizing as compact_now_emits_start_and_end.
        let big = "x".repeat(10_000 * crate::agent::compact::CHARS_PER_TOKEN_ESTIMATE);
        for i in 1..=10 {
            agent.context.push_user_text(format!("u{i}-{big}"));
            agent.context.push_assistant_text(format!("a{i}-{big}"));
        }

        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        agent.compact_now(Some(&tx), "manual").await;

        let seen = recorder.seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|e| e.starts_with("session_before_compact:")),
            "session_before_compact not delivered; got {seen:?}"
        );
        assert!(
            seen.iter().any(|e| e.starts_with("session_compact:")),
            "session_compact not delivered; got {seen:?}"
        );
        // The reason rides in arguments, and session_id is the real id —
        // the payload-honesty fix, asserted through the plugin surface
        // rather than only the shell one.
        assert!(
            seen.iter().any(|e| e.contains(r#""reason":"manual""#)),
            "reason missing from the compaction payload: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| e.contains(r#""session_id":"manual""#)),
            "session_id is carrying the reason again: {seen:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two-step provider for `a_turn_delivers_every_event_it_can_reach`:
    /// a tool round, then a plain answer.
    struct SteppedProviderForEvents {
        step: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Provider for SteppedProviderForEvents {
        fn id(&self) -> &'static str {
            "stepped-events"
        }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            let step = self
                .step
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = tx
                .send(AgentEvent::Start {
                    message_id: "m".into(),
                })
                .await;
            if step == 0 {
                let _ = tx
                    .send(AgentEvent::ToolCall {
                        content_index: 0,
                        call: ToolCall {
                            id: "c1".into(),
                            name: "ls".into(),
                            arguments: json!({}),
                        },
                    })
                    .await;
                let _ = tx
                    .send(AgentEvent::Done {
                        finish_reason: FinishReason::ToolCalls,
                        usage: Usage::default(),
                    })
                    .await;
            } else {
                let _ = tx
                    .send(AgentEvent::TextDelta {
                        content_index: 0,
                        text: "done".into(),
                    })
                    .await;
                let _ = tx
                    .send(AgentEvent::Done {
                        finish_reason: FinishReason::Stop,
                        usage: Usage::default(),
                    })
                    .await;
            }
            Ok(Usage::default())
        }
    }

    /// A blocked tool call must tell the model WHAT blocked it.
    ///
    /// The system prompt never mentions hooks, so `blocked by hook: X`
    /// left the model to guess. In manual testing it twice decided a
    /// sandbox was intercepting bash, and once went hunting for
    /// `.claude/settings.json` — another product's config file. The
    /// message now names the mechanism and rules out the environment
    /// hypothesis, while keeping the hook author's own reason last and
    /// verbatim.
    #[tokio::test]
    async fn a_blocked_call_explains_itself_to_the_model() {
        let dir = tmp();
        let session_path = dir.join("blocked-msg.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let hooks = HooksConfig {
            tool_execution_start: vec![HookConfig {
                matcher: "*".into(),
                kind: "command".into(),
                command: "sh -c 'cat >/dev/null; echo policy-says-no >&2; exit 2'".into(),
                timeout: 4000,
            }],
            ..Default::default()
        };

        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let outcome = run_one_tool(
            ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            },
            ToolRegistry::standard(),
            session_path,
            uuid::v7().to_string(),
            dir.clone(),
            PermissionGate::from_cli(false, None),
            hooks,
            Default::default(),
            tx,
        )
        .await;

        assert!(outcome.is_error, "a blocked call is an error result");
        let c = &outcome.content;
        // The hook's own words survive, and stay the tail of the line.
        assert!(c.contains("policy-says-no"), "{c}");
        // The mechanism is named, so "hook" is not an unexplained noun.
        assert!(c.contains("tool_execution_start"), "{c}");
        assert!(c.contains("user-configured"), "{c}");
        // And the wrong hypothesis is pre-empted.
        assert!(
            c.contains("not a sandbox or environment failure"),
            "the message must rule out the environment theory: {c}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_blocking_hook_still_delivers_the_event_to_subscribers() {
        use crate::subscriber::{EventHandler, EventSubscribers, Subscriber};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Recorder {
            events: std::sync::Mutex<Vec<String>>,
            calls: Arc<AtomicUsize>,
        }
        impl EventHandler for Recorder {
            fn handle_event(&self, event: &str, _payload_json: &str) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.events.lock().unwrap().push(event.to_string());
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let recorder = Arc::new(Recorder {
            events: std::sync::Mutex::new(Vec::new()),
            calls: calls.clone(),
        });

        let dir = tmp();
        let session_path = dir.join("blocked.jsonl");
        std::fs::write(&session_path, "").unwrap();

        // exit 2 is the hook protocol's "block".
        let blocking = |event: &str| HooksConfig {
            before_agent_start: if event == "before_agent_start" {
                vec![HookConfig {
                    matcher: "*".into(),
                    kind: "command".into(),
                    command: "exit 2".into(),
                    timeout: 2000,
                }]
            } else {
                Vec::new()
            },
            input: if event == "input" {
                vec![HookConfig {
                    matcher: "*".into(),
                    kind: "command".into(),
                    command: "exit 2".into(),
                    timeout: 2000,
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        };

        for event in ["before_agent_start", "input"] {
            calls.store(0, Ordering::SeqCst);
            recorder.events.lock().unwrap().clear();

            let subs = EventSubscribers::from_subscribers(vec![Subscriber {
                plugin_name: Arc::from("test-watcher"),
                events: crate::agent::hook::EVENT_NAMES.to_vec(),
                handler: recorder.clone(),
            }]);

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
                hooks: blocking(event),
                model: String::new(),
                base_url: String::new(),
                api_key: String::new(),
                usage_total: Usage::default(),
                turn_count: 0,
                skills: Vec::new(),
                no_context_files: false,
                pending_follow_ups: Default::default(),
                tool_exec_mode: crate::config::ToolExecMode::default(),
                plugin_commands: Vec::new(),
                event_subscribers: subs,
                prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
            };

            let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
            let out = agent.run_turn("a prompt", &tx, None, None).await.unwrap();

            // The block still takes effect — this is not a regression
            // in the veto, only in what observers see.
            assert!(
                out.contains("blocked"),
                "{event}: the hook should still have blocked the turn, got {out:?}"
            );
            let seen = recorder.events.lock().unwrap().clone();
            assert!(
                seen.iter().any(|e| e == event),
                "{event}: subscriber never saw it, only saw {seen:?}"
            );
        }
    }

    #[tokio::test]
    async fn before_agent_start_hook_can_transform_prompt() {
        let dir = tmp();
        let session_path = dir.join("bas.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let hooks = HooksConfig {
            before_agent_start: vec![HookConfig {
                matcher: "*".into(),
                kind: "command".into(),
                command:
                    r#"echo '{"decision":"allow","updated_input":{"prompt":"REWRITTEN BY HOOK"}}'"#
                        .into(),
                timeout: 2000,
            }],
            ..Default::default()
        };
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: "ok".into(),
            }),
            registry: ToolRegistry::standard(),
            session_path,
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        agent.run_turn("original prompt", &tx, None, None).await.unwrap();

        let user_text = agent
            .context
            .messages
            .iter()
            .find_map(|m| match m {
                ContextMessage::User { content } => Some(
                    content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .expect("user message in context");
        assert_eq!(
            user_text, "REWRITTEN BY HOOK",
            "BeforeAgentStart transform must reach context, got {user_text:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two prompt hooks must chain: BeforeAgentStart runs first and
    /// its rewritten text is what Input receives. The second
    /// hook here only emits a rewrite when it sees the first hook's
    /// output, so a passing assert proves the ordering, not just that
    /// each hook fired.
    #[tokio::test]
    async fn before_agent_start_output_feeds_input() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp();
        let session_path = dir.join("chain.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let ups_script = dir.join("ups.sh");
        std::fs::write(
            &ups_script,
            "#!/usr/bin/env bash\n\
             payload=$(cat)\n\
             if grep -q FROM_BAS <<< \"$payload\"; then\n\
             \x20 echo '{\"decision\":\"allow\",\"updated_input\":{\"prompt\":\"CHAINED\"}}'\n\
             fi\n\
             exit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&ups_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hooks = HooksConfig {
            before_agent_start: vec![HookConfig {
                matcher: "*".into(),
                kind: "command".into(),
                command: r#"echo '{"decision":"allow","updated_input":{"prompt":"FROM_BAS"}}'"#
                    .into(),
                timeout: 2000,
            }],
            input: vec![HookConfig {
                matcher: "*".into(),
                kind: "command".into(),
                command: ups_script.display().to_string(),
                timeout: 3000,
            }],
            ..Default::default()
        };
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: "ok".into(),
            }),
            registry: ToolRegistry::standard(),
            session_path,
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        agent.run_turn("original", &tx, None, None).await.unwrap();

        let user_text = agent
            .context
            .messages
            .iter()
            .find_map(|m| match m {
                ContextMessage::User { content } => Some(
                    content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .expect("user message in context");
        assert_eq!(
            user_text, "CHAINED",
            "Input must see BeforeAgentStart's rewrite, got {user_text:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: in Sequential mode cancel was only checked *between*
    /// tools, so Esc during a long-running bash waited for it to finish;
    /// and the cancel path then replaced every result — including tools
    /// that had already run and already written their real ToolResult to
    /// the session JSONL — with a synthetic marker.
    ///
    /// Asserts all three properties at once: the batch aborts long
    /// before the 30s sleep would end, the completed tool keeps its real
    /// output, and the un-run tool gets the marker in call order.
    #[tokio::test]
    async fn sequential_cancel_interrupts_and_keeps_completed_results() {
        let dir = tmp();
        let session_path = dir.join("seqcancel.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(fake_provider()),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::Sequential,
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        let ct = tokio_util::sync::CancellationToken::new();
        let ct_clone = ct.clone();
        // Long enough for the `echo` to finish and the `sleep` to be the
        // in-flight future; far shorter than the sleep itself.
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            ct_clone.cancel();
        });

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let start = std::time::Instant::now();
        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "call_fast".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "echo FIRST_DONE"}),
                    },
                    ToolCall {
                        id: "call_slow".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "sleep 30"}),
                    },
                    ToolCall {
                        id: "call_never".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "echo NEVER_RAN"}),
                    },
                ],
                &tx,
                Some(ct),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        drop(tx);
        canceller.await.ok();
        drain.await.ok();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "cancel must interrupt the in-flight tool, not wait it out; took {elapsed:?}"
        );

        // Every tool_use needs a paired tool_result, in the original
        // call order — Anthropic 400s otherwise.
        let tools: Vec<(&str, &str)> = agent
            .context
            .messages
            .iter()
            .filter_map(|m| match m {
                ContextMessage::Tool {
                    tool_call_id,
                    content,
                    ..
                } => Some((tool_call_id.as_str(), content.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            tools.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec!["call_fast", "call_slow", "call_never"],
            "tool_results must stay paired and in call order"
        );
        assert!(
            tools[0].1.contains("FIRST_DONE"),
            "completed tool must keep its real output, got {:?}",
            tools[0].1
        );
        assert!(
            tools[1].1.contains("cancelled by user"),
            "interrupted tool must get the cancel marker, got {:?}",
            tools[1].1
        );
        assert!(
            tools[2].1.contains("cancelled by user"),
            "never-started tool must get the cancel marker, got {:?}",
            tools[2].1
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: cancelling a Parallel batch wrote TWO `tool_result`
    /// entries for a tool that had already finished — its real one from
    /// `run_one_tool`, then a synthetic cancel marker on top, because
    /// `join_all` reports an empty Vec when cancel wins and the
    /// finished work looked un-run.
    ///
    /// The assertion is on the session file, not on context, because
    /// that is where the damage was permanent: `load_session` replays
    /// every entry, and two `tool_result` blocks sharing one
    /// `tool_use_id` is a 400 from Anthropic on every future resume,
    /// fork, or `--continue` of that session.
    ///
    /// Parallel is the default mode, so this was the reachable one.
    #[tokio::test]
    async fn cancelled_parallel_batch_writes_one_result_per_call() {
        for mode in [
            crate::config::ToolExecMode::Parallel,
            crate::config::ToolExecMode::Sequential,
        ] {
            let dir = tmp();
            let session_path = dir.join("s.jsonl");
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
                pending_follow_ups: Default::default(),
                tool_exec_mode: mode,
                plugin_commands: Vec::new(),
                event_subscribers: Default::default(),
                prompt_overrides:
                    crate::agent::prompt_override::PromptOverrides::default(),
            };

            let ct = tokio_util::sync::CancellationToken::new();
            let ct2 = ct.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                ct2.cancel();
            });
            let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

            agent
                .execute_tool_calls(
                    vec![
                        ToolCall {
                            id: "c_fast".into(),
                            name: "bash".into(),
                            arguments: json!({"command": "echo FIRST_DONE"}),
                        },
                        ToolCall {
                            id: "c_slow".into(),
                            name: "bash".into(),
                            arguments: json!({"command": "sleep 30"}),
                        },
                    ],
                    &tx,
                    Some(ct),
                )
                .await
                .unwrap();
            drop(tx);
            drain.await.ok();

            let raw = std::fs::read_to_string(&session_path).unwrap();
            let ids: Vec<String> = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .filter_map(|v| {
                    v.get("tool_call_id")
                        .and_then(|i| i.as_str())
                        .map(str::to_string)
                })
                .collect();

            let fast = ids.iter().filter(|i| *i == "c_fast").count();
            let slow = ids.iter().filter(|i| *i == "c_slow").count();
            assert_eq!(
                fast, 1,
                "{mode:?}: c_fast got {fast} tool_result entries in the session file; \
                 a resumed session would 400"
            );
            assert_eq!(slow, 1, "{mode:?}: c_slow got {slow} tool_result entries");

            // The finished tool must also keep its real output rather
            // than being overwritten by the cancel marker.
            assert!(
                raw.contains("FIRST_DONE"),
                "{mode:?}: completed work was discarded"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Regression: a steer that the channel ACCEPTED could still be
    /// lost. c15c8a9 handled the send failing; it did not handle the
    /// turn being cancelled after a successful send but before the pump
    /// reached the message. The user had already seen `[steer] …`
    /// echoed, and the text died with the receiver — no context entry,
    /// no session entry, no follow-up.
    ///
    /// On cancel a pending steer becomes a follow-up: there is no turn
    /// left to steer, but the text is still something the user typed.
    #[tokio::test]
    async fn cancelled_turn_keeps_pending_steers_as_follow_ups() {
        let dir = tmp();
        let session_path = dir.join("s.jsonl");
        std::fs::write(&session_path, "").unwrap();
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(fake_provider()),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };

        // Two messages buffered and a token already cancelled, so
        // `run_turn` takes its very first early return.
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(8);
        steer_tx
            .send(SteerMessage::Steering { text: "first".into() })
            .await
            .unwrap();
        steer_tx
            .send(SteerMessage::Steering { text: "second".into() })
            .await
            .unwrap();
        let ct = tokio_util::sync::CancellationToken::new();
        ct.cancel();

        let (tx, _rx) = mpsc::channel::<AgentEvent>(16);
        agent
            .run_turn("go", &tx, Some(ct), Some(steer_rx))
            .await
            .unwrap();

        let queued: Vec<&str> = agent
            .pending_follow_ups
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(
            queued,
            vec!["first", "second"],
            "pending steers must survive cancellation, in order and all of them"
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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
    /// session lifecycle hooks — SessionStart and SessionShutdown fired
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

        // Same hook wired for both session_start and session_shutdown.
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
                session_shutdown: vec![hook_cfg],
                ..Default::default()
            },
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };
        agent.fire_session_start("startup").await;
        agent.fire_session_shutdown("quit").await;

        assert!(
            !marker.exists(),
            "--no-hooks must disable both session_start and session_shutdown"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.12.0: session_start payload must carry an honest `reason` in
    /// `arguments` and the real session id in `session_id`.
    #[tokio::test]
    async fn session_start_payload_carries_reason_and_real_session_id() {
        let dir = tmp();
        let out = dir.join("payload.json");
        let hook_script = dir.join("hook.sh");
        std::fs::write(
            &hook_script,
            format!("#!/usr/bin/env bash\ncat > {}\n", out.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let session_path = dir.join("s.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":2,\"id\":\"019fe000-0000-7000-8000-000000000000\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"m\",\"base_url\":\"\"}\n",
        )
        .unwrap();

        let hook_cfg = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: hook_script.display().to_string(),
            timeout: 3000,
        };
        let session_id = uuid::v7().to_string();

        let agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: "ok".into(),
            }),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: session_id.clone(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig {
                session_start: vec![hook_cfg],
                ..Default::default()
            },
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        };
        agent.fire_session_start("startup").await;

        let text = std::fs::read_to_string(&out).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["arguments"]["reason"], "startup");
        assert_eq!(payload["session_id"], session_id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.12.0 regression: compaction hook payloads must carry the real
    /// session id, not the compaction reason string, in `session_id`.
    /// Previously `session_id` held `"threshold"`/`"manual"` — a lie.
    #[tokio::test]
    async fn compact_now_session_hooks_carry_real_session_id_not_reason() {
        let (mut agent, dir) = agent_for_compact_test("SUMMARY");
        let out = dir.join("compact_payload.json");
        let hook_script = dir.join("hook.sh");
        std::fs::write(
            &hook_script,
            format!("#!/usr/bin/env bash\ncat > {}\n", out.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hook_cfg = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: hook_script.display().to_string(),
            timeout: 3000,
        };
        agent.hooks.session_before_compact = vec![hook_cfg];
        let session_id = agent.session_id.clone();

        let big = "x".repeat(10_000 * crate::agent::compact::CHARS_PER_TOKEN_ESTIMATE);
        for i in 1..=10 {
            agent.context.push_user_text(format!("u{i}-{big}"));
            agent.context.push_assistant_text(format!("a{i}-{big}"));
        }

        agent.compact_now(None, "threshold").await;

        let text = std::fs::read_to_string(&out).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["arguments"]["reason"], "threshold");
        assert_eq!(
            payload["session_id"], session_id,
            "session_id must be the real session id, not the reason string"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A context small enough to fit entirely in the verbatim tail has
    /// no head to summarize, so no pass runs — and `compact_now` must
    /// say so. `/compact` on a short session printed
    /// `[compacted: 2158 → 2158 chars]`: the no-op was correct, the
    /// claim was not.
    #[tokio::test]
    async fn compact_now_reports_false_when_there_is_nothing_to_compact() {
        let (mut agent, dir) = agent_for_compact_test("SUMMARY");
        agent.context.push_user_text("hi".to_string());
        agent.context.push_assistant_text("hello".to_string());
        let before = agent.context.estimate_chars();

        let ran = agent.compact_now(None, "manual").await;

        assert!(!ran, "nothing to compact must not report a pass");
        assert_eq!(
            agent.context.estimate_chars(),
            before,
            "a no-op pass must leave the context untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And the inverse, so the flag isn't just wired to `false`.
    #[tokio::test]
    async fn compact_now_reports_true_when_a_pass_actually_runs() {
        let (mut agent, dir) = agent_for_compact_test("SUMMARY");
        let big = "x".repeat(10_000 * crate::agent::compact::CHARS_PER_TOKEN_ESTIMATE);
        for i in 1..=10 {
            agent.context.push_user_text(format!("u{i}-{big}"));
            agent.context.push_assistant_text(format!("a{i}-{big}"));
        }
        let before = agent.context.estimate_chars();

        let ran = agent.compact_now(None, "manual").await;

        assert!(ran, "a real pass must report itself");
        assert!(
            agent.context.estimate_chars() < before,
            "a real pass must shrink the context"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hook pair must be balanced. A `session_before_compact` with
    /// no matching `session_compact` is indistinguishable, from a hook
    /// author's side, from nanopi crashing mid-compaction — and that is
    /// exactly what a no-op `/compact` used to write into the log.
    #[tokio::test]
    async fn a_no_op_compaction_fires_neither_hook() {
        let (mut agent, dir) = agent_for_compact_test("SUMMARY");
        let out = dir.join("hooks.jsonl");
        let hook_script = dir.join("hook.sh");
        std::fs::write(
            &hook_script,
            format!("#!/usr/bin/env bash\ncat >> {}\n", out.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hook_cfg = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: hook_script.display().to_string(),
            timeout: 3000,
        };
        agent.hooks.session_before_compact = vec![hook_cfg.clone()];
        agent.hooks.session_compact = vec![hook_cfg];

        agent.context.push_user_text("hi".to_string());
        agent.context.push_assistant_text("hello".to_string());
        let ran = agent.compact_now(None, "manual").await;

        assert!(!ran);
        assert!(
            !out.exists(),
            "a compaction that never ran must not announce one: {:?}",
            std::fs::read_to_string(&out).unwrap_or_default()
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
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

    /// A steer that arrives during a turn which ends without tool calls
    /// must not vanish.
    ///
    /// The steer pump only runs at the top of an iteration, so a turn
    /// finishing on `Stop` — the common single-shot case — has no next
    /// iteration to drain it. The message stayed in the channel: never
    /// pushed into context, never persisted, never seen by the model.
    /// The TUI had already drawn `[steer] …`, so the user was told it
    /// landed, and on resume the session showed one question and an
    /// answer that had never been asked.
    ///
    /// Observed with `who are you` + a mid-stream `who mai`: the saved
    /// session held only the first, and the reply addressed only the
    /// first.
    #[tokio::test]
    async fn a_steer_that_misses_its_turn_becomes_a_follow_up() {
        let dir = tmp();
        let session_path = dir.join("late_steer.jsonl");
        std::fs::write(&session_path, "").unwrap();

        // Single-shot provider: text, then Stop. No tool calls, so
        // `run_turn` never comes back around to the steer pump.
        //
        // The steer is sent from INSIDE `stream_turn`, which is the
        // only way to land in the window that matters: the pump at the
        // top of iteration 1 has already run, and there is no
        // iteration 2. Enqueuing before `run_turn` instead exercises
        // the pump, which always worked.
        struct OneShot {
            steer_tx: mpsc::Sender<SteerMessage>,
        }
        #[async_trait::async_trait]
        impl Provider for OneShot {
            fn id(&self) -> &'static str {
                "oneshot"
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
                // The user types mid-stream. The send SUCCEEDS — the
                // receiver is alive — which is what distinguishes this
                // from the TUI's dropped-receiver fallback.
                self.steer_tx
                    .send(SteerMessage::Steering {
                        text: "who am i".into(),
                    })
                    .await
                    .expect("steer channel must still be open mid-stream");
                let _ = tx
                    .send(AgentEvent::TextDelta {
                        content_index: 0,
                        text: "I'm a model.".into(),
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

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(OneShot { steer_tx }),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7().to_string(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: "oneshot".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
        };

        let _ = agent
            .run_turn("who are you", &tx, None, Some(steer_rx))
            .await
            .expect("turn");

        assert_eq!(
            agent.pending_follow_ups.pop_front().as_deref(),
            Some("who am i"),
            "a steer that missed its turn must survive as a follow-up, \
             not be dropped after the TUI has already echoed it"
        );

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
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
        };

        agent.run_turn("go", &tx, None, Some(steer_rx)).await.unwrap();

        assert_eq!(
            agent.pending_follow_ups.front().map(String::as_str),
            Some("next question"),
            "FollowUp message must surface on agent.pending_follow_ups"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────── concurrent file mutation under Parallel tool exec ───────
    //
    // `execute_tool_calls` groups a parallel batch by
    // `tool::mutation_key` and runs same-key calls serially, so `edit`
    // and `write` against one file are serialized. `bash` has no key
    // (its command string is not analyzable) and stays fully parallel,
    // so it can still lose an update. The tests below pin down both
    // halves of that split, with the variable being *which tool*
    // performs the concurrent mutation:
    //
    //   edit + edit  → safe, guaranteed by the per-path grouping
    //   bash + bash  → genuinely races (no key, so no serialization)
    //   Sequential   → the bash race disappears too, confirming the
    //                  config knob is the real mitigation for bash

    /// Builds an Agent wired to `dir` with the given exec mode.
    fn concurrency_agent(dir: &std::path::Path, mode: crate::config::ToolExecMode) -> Agent {
        let session_path = dir.join("p.jsonl");
        std::fs::write(&session_path, "").unwrap();
        Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider {
                response: "ok".into(),
            }),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: uuid::v7().to_string(),
            cwd: dir.to_path_buf(),
            permission: PermissionGate::from_cli(false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_ups: Default::default(),
            tool_exec_mode: mode,
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        }
    }

    /// Two `edit` calls hitting the same file in one parallel batch,
    /// each replacing a different line. Both changes must survive.
    ///
    /// What guarantees this now is the per-path pipeline: both calls
    /// resolve to the same `tool::mutation_key`, so
    /// `execute_tool_calls` puts them in one group and awaits the first
    /// to completion before starting the second. The second edit reads
    /// a file that already contains the first edit's write.
    ///
    /// It is worth being precise about what changed, because the test
    /// body did not. Before the pipeline this test also passed, but by
    /// accident: `EditTool::execute` contains no `.await` between
    /// `read_to_string` and `fs::write`, and `join_all` polls every
    /// tool future on ONE task, so nothing could preempt an edit
    /// mid-read-modify-write. That property still holds, and it must
    /// still not be relied on — it is one keystroke from evaporating
    /// (switching `edit` to `tokio::fs::…().await` reads like a
    /// harmless async cleanup), and a second mechanism quietly
    /// disappearing is exactly the kind of thing that goes unnoticed
    /// while a test stays green for the wrong reason. The grouping is
    /// the designed guarantee; the no-await property is an accident
    /// that happens to agree with it.
    ///
    /// So this test now asserts the pipeline works. If it goes red,
    /// suspect the grouping in `execute_tool_calls` or the key in
    /// `tool::mutation_key`, not `edit.rs`.
    #[tokio::test]
    async fn parallel_edits_to_one_file_keep_both_changes() {
        let dir = tmp();
        let target = dir.join("foo.txt");
        std::fs::write(&target, "alpha\nbravo\n").unwrap();

        let mut agent = concurrency_agent(&dir, crate::config::ToolExecMode::Parallel);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "e1".into(),
                        name: "edit".into(),
                        arguments: json!({
                            "path": "foo.txt",
                            "oldText": "alpha",
                            "newText": "ALPHA"
                        }),
                    },
                    ToolCall {
                        id: "e2".into(),
                        name: "edit".into(),
                        arguments: json!({
                            "path": "foo.txt",
                            "oldText": "bravo",
                            "newText": "BRAVO"
                        }),
                    },
                ],
                &tx,
                None,
            )
            .await
            .expect("execute");
        while rx.try_recv().is_ok() {}

        let got = std::fs::read_to_string(&target).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            got, "ALPHA\nBRAVO\n",
            "both edits must survive; a lost update here means the \
             read-modify-write in edit.rs gained a yield point"
        );
    }

    /// The batch shape that actually loses data: two `bash` calls
    /// mutating one file.
    ///
    /// `bash` awaits, so unlike `edit` it has no atomic critical
    /// section. Both children are spawned before either yields control
    /// back, both snapshot the file at t≈spawn, both write their own
    /// snapshot back at t≈spawn+300ms — so whichever writes second
    /// silently reverts the other. Each command is a read-modify-write
    /// straddling a sleep, which is the shape of any real `sed -i`,
    /// formatter, or codemod the model might reach for.
    ///
    /// Note this is NOT reachable by pairing `bash` with `edit`: a
    /// same-batch `edit` finishes its whole read-modify-write in
    /// microseconds, while bash needs milliseconds just to fork/exec,
    /// so bash always snapshots the post-edit file. The exposure is
    /// specifically bash-against-bash, where the two sides are
    /// symmetric and neither has an atomic section.
    ///
    /// Expected today: FAILS, and which change survives is genuinely
    /// nondeterministic. Both tools report success — nothing surfaces
    /// the loss to the model. Ignored so CI stays green; run with
    /// `cargo test -- --ignored` to demonstrate the bug.
    #[tokio::test]
    #[ignore = "known bug: no per-path mutation queue, concurrent bash loses updates"]
    async fn parallel_bash_calls_on_one_file_lose_an_update() {
        let dir = tmp();
        let target = dir.join("foo.txt");
        std::fs::write(&target, "alpha\nbravo\n").unwrap();

        let mut agent = concurrency_agent(&dir, crate::config::ToolExecMode::Parallel);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "b1".into(),
                        name: "bash".into(),
                        arguments: json!({
                            "command":
                                "snap=$(cat foo.txt); sleep 0.3; \
                                 printf '%s' \"${snap/alpha/ALPHA}\" > foo.txt"
                        }),
                    },
                    ToolCall {
                        id: "b2".into(),
                        name: "bash".into(),
                        arguments: json!({
                            "command":
                                "snap=$(cat foo.txt); sleep 0.3; \
                                 printf '%s' \"${snap/bravo/BRAVO}\" > foo.txt"
                        }),
                    },
                ],
                &tx,
                None,
            )
            .await
            .expect("execute");
        while rx.try_recv().is_ok() {}

        let got = std::fs::read_to_string(&target).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            got.contains("ALPHA") && got.contains("BRAVO"),
            "both mutations must survive, got {got:?} — one bash wrote \
             back a snapshot taken before the other committed"
        );
    }

    // ─────────── the per-path pipeline itself (v0.11.0) ───────────

    /// A stand-in for `write` that yields for `delay_ms` before touching
    /// the file, so the grouping becomes observable in wall-clock time.
    ///
    /// The real `write` finishes its whole read-modify-write without an
    /// `.await`, which is precisely why timing cannot distinguish
    /// parallel from serial with it. This tool has the yield point the
    /// real one lacks — i.e. it is the shape the real one might become —
    /// so it measures the pipeline rather than an implementation
    /// accident of `write.rs`.
    struct SlowWriteTool {
        delay_ms: u64,
        /// Tags in the order calls actually *finished*. Some effects of
        /// (mis)grouping are invisible in the resulting file contents
        /// but plain here — notably a reorder, where the same writes
        /// land in the same places, just not in the order the model
        /// asked for.
        journal: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        /// How many calls are inside `execute` right now.
        in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// High-water mark of `in_flight`. This, not wall clock, is what
        /// the grouping tests assert on: "2 calls took less than 750ms"
        /// infers overlap from a stopwatch, and a loaded machine running
        /// the rest of the suite in parallel can blow that budget while
        /// the calls really were concurrent. Watching the counter
        /// answers the actual question — did two of these run at once —
        /// with no timing assumption at all.
        peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl crate::tool::Tool for SlowWriteTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "write".into(),
                description: "slow write stand-in".into(),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }
        }
        async fn execute(
            &self,
            args: serde_json::Value,
            ctx: &crate::tool::ToolContext,
        ) -> Result<crate::tool::ToolOutput, crate::tool::ToolError> {
            use std::sync::atomic::Ordering;
            let path = args["path"].as_str().unwrap_or_default().to_string();
            let abs = crate::tool::resolve_in_cwd(&ctx.cwd, &path)
                .map_err(crate::tool::ToolError::Execution)?;
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            // Read-modify-write straddling a yield: appends its own tag
            // to whatever it saw. Two of these racing lose a tag.
            let before = std::fs::read_to_string(&abs).unwrap_or_default();
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            let tag = args["content"].as_str().unwrap_or("?");
            std::fs::write(&abs, format!("{before}{tag}")).unwrap();
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.journal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(tag.to_string());
            Ok(crate::tool::ToolOutput {
                content: "ok".into(),
                is_error: false,
                metadata: None,
                images: Vec::new(),
            })
        }
    }

    /// Registry whose `write` is the slow stand-in above, plus the
    /// completion journal and the concurrency watermark it writes into.
    /// Built from `new()` rather than `standard()` so `register_external`
    /// does not hit the anti-shadowing refusal.
    #[allow(clippy::type_complexity)]
    fn slow_write_registry(
        delay_ms: u64,
    ) -> (
        ToolRegistry,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let journal = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut r = ToolRegistry::new();
        r.register_external(std::sync::Arc::new(SlowWriteTool {
            delay_ms,
            journal: journal.clone(),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: peak.clone(),
        }))
        .expect("fresh registry has no `write` to shadow");
        (r, journal, peak)
    }

    /// The over-serialization guard. Two `write` calls to DIFFERENT
    /// files get different mutation keys, so they must land in different
    /// groups and still run concurrently.
    ///
    /// This is the failure mode a naive "just take one global file lock"
    /// fix would have: correct, and quietly half the throughput on every
    /// multi-file batch the model emits (which is most of them).
    #[tokio::test]
    async fn parallel_writes_to_different_paths_stay_parallel() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();

        let mut agent = concurrency_agent(&dir, crate::config::ToolExecMode::Parallel);
        let (registry, _journal, peak) = slow_write_registry(100);
        agent.registry = registry;
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "w1".into(),
                        name: "write".into(),
                        arguments: json!({"path": "a.txt", "content": "A"}),
                    },
                    ToolCall {
                        id: "w2".into(),
                        name: "write".into(),
                        arguments: json!({"path": "b.txt", "content": "B"}),
                    },
                ],
                &tx,
                None,
            )
            .await
            .expect("execute");
        while rx.try_recv().is_ok() {}

        let a = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        let b = std::fs::read_to_string(dir.join("b.txt")).unwrap();
        let peak = peak.load(std::sync::atomic::Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            (a.as_str(), b.as_str()),
            ("A", "B"),
            "both writes must land"
        );
        assert_eq!(
            peak, 2,
            "two writes to different paths must be in flight at once; peak \
             concurrency was {peak} — the mutation key is over-matching and \
             grouping unrelated files together"
        );
    }

    /// The other side of the same coin: two `write` calls to the SAME
    /// file share a key, so the pipeline must serialize them.
    ///
    /// Both assertions matter. The concurrency watermark proves they
    /// did not overlap; the content one proves the serialization
    /// actually prevented the lost update, and that the calls ran in
    /// the order the model emitted them rather than whatever order the
    /// executor found convenient.
    #[tokio::test]
    async fn parallel_writes_to_one_path_are_serialized_in_order() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "").unwrap();

        let mut agent = concurrency_agent(&dir, crate::config::ToolExecMode::Parallel);
        let (registry, _journal, peak) = slow_write_registry(100);
        agent.registry = registry;
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "w1".into(),
                        name: "write".into(),
                        arguments: json!({"path": "a.txt", "content": "A"}),
                    },
                    // Same file, spelled differently on purpose: the key
                    // must collapse `./a.txt` onto `a.txt`.
                    ToolCall {
                        id: "w2".into(),
                        name: "write".into(),
                        arguments: json!({"path": "./a.txt", "content": "B"}),
                    },
                ],
                &tx,
                None,
            )
            .await
            .expect("execute");
        while rx.try_recv().is_ok() {}

        let got = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        let peak = peak.load(std::sync::atomic::Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            got, "AB",
            "same-file writes must serialize in model order; {got:?} means \
             the second call snapshotted the file before the first wrote"
        );
        assert_eq!(
            peak, 1,
            "the two spellings must share a key and never overlap; peak \
             concurrency was {peak}"
        );
    }

    /// The grouping contract, tested directly on the pure function so
    /// the ordering properties `execute_tool_calls` relies on are pinned
    /// without timing.
    #[test]
    fn group_by_mutation_key_shapes_the_batch() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "x").unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let registry = ToolRegistry::standard();

        let call = |id: &str, name: &str, args: serde_json::Value| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        };

        let groups = group_by_mutation_key(
            &registry,
            &dir,
            vec![
                call(
                    "1",
                    "edit",
                    json!({"path": "a.txt", "oldText": "x", "newText": "y"}),
                ),
                call("2", "bash", json!({"command": "ls"})),
                call("3", "write", json!({"path": "./a.txt", "content": "z"})),
                call("4", "read", json!({"path": "a.txt"})),
                call("5", "write", json!({"path": "b.txt", "content": "z"})),
                call(
                    "6",
                    "edit",
                    json!({"path": "a.txt", "oldText": "x", "newText": "w"}),
                ),
            ],
        );

        let ids: Vec<Vec<&str>> = groups
            .iter()
            .map(|g| g.iter().map(|c| c.id.as_str()).collect())
            .collect();

        assert_eq!(
            ids,
            vec![
                // a.txt: all three spellings collapse, in model order,
                // and the group sits where its FIRST member was.
                vec!["1", "3", "6"],
                // bash and read are unserialized, each its own group.
                vec!["2"],
                vec!["4"],
                // b.txt is a separate file, so a separate group.
                vec!["5"],
            ],
            "grouping must collapse same-file mutations in model order \
             while leaving everything else parallel"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gateway-mangled names must still group. If `Write_tool` failed to
    /// canonicalize to `write` here, the key would come back `None` and
    /// the serialization would silently not happen — a failure that is
    /// invisible except as a rare lost update.
    #[test]
    fn group_by_mutation_key_canonicalizes_mangled_tool_names() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();

        let groups = group_by_mutation_key(
            &ToolRegistry::standard(),
            &dir,
            vec![
                ToolCall {
                    id: "1".into(),
                    name: "Write_tool".into(),
                    arguments: json!({"path": "a.txt", "content": "z"}),
                },
                ToolCall {
                    id: "2".into(),
                    name: "EDIT_TOOL".into(),
                    arguments: json!({"path": "a.txt", "oldText": "x", "newText": "y"}),
                },
            ],
        );

        assert_eq!(groups.len(), 1, "mangled names must canonicalize and group");
        assert_eq!(groups[0].len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sequential mode must NOT be regrouped: its contract is "every
    /// call, one at a time, in the order the model emitted them", and
    /// grouping reorders a batch — same-key calls migrate to the
    /// position of the first of them. In the batch below, grouping would
    /// hoist `C` (a.txt) ahead of `B` (b.txt).
    ///
    /// The assertion is on completion ORDER, not on timing or file
    /// contents, because those two do not distinguish the cases: a
    /// grouped Sequential run still awaits every group serially, so it
    /// still takes 3×200ms, and `a.txt` still ends up "AC" and `b.txt`
    /// still "B" either way. The reorder is the entire observable
    /// difference, and for a `write`-then-`bash`-then-`write` chain —
    /// exactly what `sequential` exists to serve — a reorder is a
    /// correctness bug.
    #[tokio::test]
    async fn sequential_mode_is_not_regrouped() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();

        let mut agent = concurrency_agent(&dir, crate::config::ToolExecMode::Sequential);
        let (registry, journal, _peak) = slow_write_registry(50);
        agent.registry = registry;
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "w1".into(),
                        name: "write".into(),
                        arguments: json!({"path": "a.txt", "content": "A"}),
                    },
                    ToolCall {
                        id: "w2".into(),
                        name: "write".into(),
                        arguments: json!({"path": "b.txt", "content": "B"}),
                    },
                    // Same key as w1. Grouping would pull this call up
                    // next to it, ahead of w2.
                    ToolCall {
                        id: "w3".into(),
                        name: "write".into(),
                        arguments: json!({"path": "a.txt", "content": "C"}),
                    },
                ],
                &tx,
                None,
            )
            .await
            .expect("execute");
        while rx.try_recv().is_ok() {}

        let order = journal.lock().unwrap().clone();
        let a = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        let b = std::fs::read_to_string(dir.join("b.txt")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            order,
            vec!["A", "B", "C"],
            "sequential mode must run the batch in model order; \
             [\"A\", \"C\", \"B\"] means the per-path grouping leaked into \
             the Sequential arm and reordered the batch"
        );
        assert_eq!((a.as_str(), b.as_str()), ("AC", "B"));
    }

    /// The same batch as the test above under
    /// `tool_exec_mode = "sequential"`. The first bash is awaited to
    /// completion before the second is spawned at all, so the second
    /// snapshots the first's output instead of racing it and both
    /// mutations land.
    ///
    /// This is what makes `sequential` a genuine mitigation rather than
    /// a placebo — at the cost of serialising every tool in the batch.
    #[tokio::test]
    async fn sequential_mode_prevents_the_concurrent_bash_race() {
        let dir = tmp();
        let target = dir.join("foo.txt");
        std::fs::write(&target, "alpha\nbravo\n").unwrap();

        let mut agent = concurrency_agent(&dir, crate::config::ToolExecMode::Sequential);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

        agent
            .execute_tool_calls(
                vec![
                    ToolCall {
                        id: "b1".into(),
                        name: "bash".into(),
                        arguments: json!({
                            "command":
                                "snap=$(cat foo.txt); sleep 0.3; \
                                 printf '%s' \"${snap/alpha/ALPHA}\" > foo.txt"
                        }),
                    },
                    ToolCall {
                        id: "b2".into(),
                        name: "bash".into(),
                        arguments: json!({
                            "command":
                                "snap=$(cat foo.txt); sleep 0.3; \
                                 printf '%s' \"${snap/bravo/BRAVO}\" > foo.txt"
                        }),
                    },
                ],
                &tx,
                None,
            )
            .await
            .expect("execute");
        while rx.try_recv().is_ok() {}

        let got = std::fs::read_to_string(&target).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            got.contains("ALPHA") && got.contains("BRAVO"),
            "sequential mode must serialise the batch, got {got:?}"
        );
    }
}

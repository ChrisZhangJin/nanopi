//! The Agent turn loop — the heart of nanopi.
//!
//! See `docs/v0.5-research.md` §2 for the state machine. v0.5 executes
//! tool calls sequentially (one tool at a time, in LLM-returned order).
//! v0.6 will add parallel tool execution (Pi parity).

use std::path::{Path, PathBuf};

use serde_json::Value;

use futures_util::future::join_all;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::agent::hook::{HookConfig, HookEvent, HookOutcome, run_hooks, run_session_hooks};
use crate::agent::permission::PermissionGate;
use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
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
}

/// The agent — owns context, provider, tool registry, session, permissions.
pub struct Agent {
    pub context: Context,
    pub provider: Box<dyn Provider>,
    pub registry: ToolRegistry,
    pub session_path: PathBuf,
    pub session_id: uuid::Uuid,
    pub cwd: PathBuf,
    pub permission: PermissionGate,
    pub hooks: HooksConfig,
    /// The model id string, cached here so status-line renderers can
    /// read it without going through the Provider trait (which has
    /// no `model()` method today).
    pub model: String,
    /// Cumulative token usage over the whole session — summed across
    /// every LLM turn (including compaction summarization). Read by
    /// the status bar; never resets except on `Agent::load_session`
    /// (fresh Agent starts at zero).
    pub usage_total: Usage,
    /// Turn counter, incremented at the start of every `run_turn`.
    pub turn_count: u32,
}

impl Agent {
    /// Reconstruct an Agent from an existing session JSONL file.
    /// Replays message entries into the Context so a new turn can build
    /// on prior history. Used by `--continue` and the v0.6 multi-turn
    /// TUI.
    pub fn load_session(
        session_path: &Path,
        cwd: &Path,
    ) -> Result<Self, AgentError> {
        use crate::agent::context::{ContentBlock, ContextMessage};
        let (header, entries) = crate::session::read_session(session_path)?;
        let mut context = Context::default();
        for entry in entries {
            match entry {
                SessionEntry::Message { role, content, .. } => {
                    match role.as_str() {
                        "user" => context.push_user_text(content),
                        "assistant" => context.push_assistant_text(content),
                        _ => {}
                    }
                }
                SessionEntry::Compaction { summary, replaced_count, .. } => {
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
                _ => {}
            }
        }
        Ok(Self {
            context,
            provider: Box::new(OpenAiProvider::new("", "", "")),
            registry: ToolRegistry::standard(),
            session_path: session_path.to_path_buf(),
            session_id: header.id,
            cwd: cwd.to_path_buf(),
            permission: PermissionGate::from_cli(false, false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
        })
    }

    /// Find the most recently used session path for the given cwd.
    /// Returns None if no session has ever been recorded for it. Used
    /// by `--continue` and the multi-turn TUI to resume.
    pub fn continue_last_session(cwd: &Path) -> Option<PathBuf> {
        crate::session::active_session(cwd)
    }

    /// Fire all `session_start` hooks. Advisory — outcome is not enforced.
    /// Call once, after Agent construction, before the first turn.
    pub async fn fire_session_start(&self) {
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
    pub async fn fire_session_end(&self) {
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
    pub async fn compact_now(&mut self) {
        use crate::agent::compact::compact;
        let Some(result) = compact(&mut self.context, self.provider.as_ref()).await else {
            return;
        };
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
    /// each turn so long conversations don't blow the model's context
    /// window.
    pub async fn maybe_compact(&mut self) {
        use crate::agent::compact::MAX_CONTEXT_CHARS;
        if self.context.estimate_chars() < MAX_CONTEXT_CHARS {
            return;
        }
        self.compact_now().await;
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
    ) -> Result<String, AgentError> {
        // If the accumulated context is too big, compact it before adding
        // the new user message so the new message survives intact.
        self.maybe_compact().await;
        self.turn_count = self.turn_count.saturating_add(1);

        // Append user message to context + session.
        let user_id = uuid::v7().to_string();
        self.context.push_user_text(user_msg.to_string());
        session::append_entry(
            &self.session_path,
            &SessionEntry::Message {
                id: user_id,
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: user_msg.into(),
            },
        )?;

        let mut final_text = String::new();
        const MAX_ITERATIONS: u32 = 16;

        for _ in 0..MAX_ITERATIONS {
            // If a cancel token was provided, bail before starting a new
            // LLM turn. The user's accumulated context is preserved.
            if let Some(ct) = cancel.as_ref() {
                if ct.is_cancelled() {
                    return Ok(final_text);
                }
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
                        AgentEvent::Done { finish_reason, usage } => {
                            done = Some((*finish_reason, usage.clone()));
                        }
                        _ => {}
                    }
                    let _ = collector_tx.send(ev).await;
                }
                (calls, done, assistant_text)
            });

            // Run one streaming assistant turn.
            if let Err(e) = self.provider.stream_turn(&self.context, forward_tx).await {
                return Err(AgentError::Provider(e.to_string()));
            }

            let (mut calls, done, assistant_text) = if let Some(ct) = cancel.as_ref() {
                tokio::select! {
                    _ = ct.cancelled() => {
                        // Cancellation: drop any partial result and return
                        // the text we have so far. The user can send a new
                        // message and the context is preserved.
                        return Ok(final_text);
                    }
                    res = collect_task => res.map_err(|e| AgentError::Provider(format!("collect task: {e}")))?,
                }
            } else {
                collect_task.await.map_err(|e| AgentError::Provider(format!("collect task: {e}")))?
            };

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

            // Persist assistant text + push assistant message to context.
            // CRITICAL: must include ToolCall blocks so the provider knows
            // the following Tool messages are responses.
            if !assistant_text.is_empty() || !calls.is_empty() {
                final_text.push_str(&assistant_text);
                let mut assistant_blocks = Vec::new();
                if !assistant_text.is_empty() {
                    assistant_blocks.push(
                        crate::agent::context::AssistantBlock::Text {
                            text: assistant_text.clone(),
                        },
                    );
                }
                for c in &calls {
                    assistant_blocks.push(
                        crate::agent::context::AssistantBlock::ToolCall {
                            call: crate::agent::context::ToolCallBlock {
                                id: c.id.clone(),
                                name: c.name.clone(),
                                arguments: c.arguments.clone(),
                            },
                        },
                    );
                }
                session::append_entry(
                    &self.session_path,
                    &SessionEntry::Message {
                        id: uuid::v7().to_string(),
                        timestamp: time::now_iso8601(),
                        role: "assistant".into(),
                        content: final_text.clone(),
                    },
                )?;
                self.context.messages.push(
                    crate::agent::context::ContextMessage::Assistant {
                        content: assistant_blocks,
                    },
                );
            }

            let Some((finish_reason, usage)) = done else {
                // Stream ended without Done. Treat as Stop.
                return Ok(final_text);
            };

            // Accumulate tokens across every LLM iteration in the session.
            self.usage_total.input_tokens = self.usage_total.input_tokens.saturating_add(usage.input_tokens);
            self.usage_total.output_tokens = self.usage_total.output_tokens.saturating_add(usage.output_tokens);
            self.usage_total.cache_read_tokens = self.usage_total.cache_read_tokens.saturating_add(usage.cache_read_tokens);
            self.usage_total.cache_write_tokens = self.usage_total.cache_write_tokens.saturating_add(usage.cache_write_tokens);

            match finish_reason {
                FinishReason::Stop | FinishReason::Length | FinishReason::Refusal => {
                    return Ok(final_text);
                }
                FinishReason::ToolCalls => {
                    if calls.is_empty() {
                        // LLM said "tool calls" but emitted none — done.
                        return Ok(final_text);
                    }
                    // Execute tools sequentially (v0.5).
                    self.execute_tool_calls(calls, tx).await?;
                    // Loop back to the next LLM turn.
                }
                FinishReason::Unknown => {
                    return Ok(final_text);
                }
            }
        }
        // Hit MAX_ITERATIONS — give up gracefully.
        Ok(final_text)
    }

    /// Execute a list of tool calls sequentially. Runs hooks, executes the
    /// tool, persists results to session, pushes ToolResult messages into
    /// context. Streams human-readable output to `tx` for the renderer.
    pub async fn execute_tool_calls(
        &mut self,
        calls: Vec<ToolCall>,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<(), AgentError> {
        // Phase 1: Run all tool executions CONCURRENTLY via join_all.
        // Each future resolves to (ToolCall, Result<ToolOutput, ToolError>).
        // Hooks, persistence, and context mutation happen in phase 2.
        let cwd = self.cwd.clone();
        let registry = self.registry.clone();
        let session_path = self.session_path.clone();
        let session_id = self.session_id;
        let permission = self.permission.clone();
        let hooks = self.hooks.clone();

        let futs: Vec<_> = calls
            .into_iter()
            .map(|call| {
                let cwd = cwd.clone();
                let registry = registry.clone();
                let session_path = session_path.clone();
                let session_id = session_id;
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

let results = join_all(futs).await;

        // Phase 2: Push ToolResult messages into context in call order
        // so the next LLM turn sees them. Persistence already happened
        // during phase 1 (in run_one_tool).
        for outcome in results {
            self.context.push_tool_result(
                outcome.call_id,
                outcome.content,
                outcome.is_error,
            );
        }

        Ok(())
    }
}

/// Result of running one tool — what `run_one_tool` returns to the
/// caller's join_all. We carry enough info so the caller can persist
/// and update context in any order (in our case, in call order).
struct ToolCallOutcome {
    call_id: String,
    tool_name: String,
    args: Value,
    content: String,
    is_error: bool,
}

/// Run a single tool call: hooks → execute → persist → render. Pure
/// function over its arguments; safe to call concurrently from
/// `execute_tool_calls`.
async fn run_one_tool(
    call: ToolCall,
    registry: ToolRegistry,
    session_path: PathBuf,
    session_id: uuid::Uuid,
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
                    tool_name: call.name,
                    args: effective_args,
                    content: result_text,
                    is_error: true,
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
    let (content, is_error) = match registry.get(&call.name) {
        Some(tool) => {
            let ctx = ToolContext { cwd: cwd.clone() };
            match tool.execute(effective_args.clone(), &ctx).await {
                Ok(o) => (o.content, o.is_error),
                Err(e) => (format!("tool error: {e}"), true),
            }
        }
        None => (format!("unknown tool: {}", call.name), true),
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
        },
    );

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

    // PostToolUse hooks.
    if permission.hooks_active() && !hooks.post_tool_use.is_empty() {
        let _ = run_hooks(
            &hooks.post_tool_use,
            HookEvent::PostToolUse,
            &call.name,
            Value::Object(Default::default()),
            &cwd,
            Some(&session_id.to_string()),
        )
        .await;
    }

    ToolCallOutcome {
        call_id: call.id,
        tool_name: call.name,
        args: effective_args,
        content,
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::ToolSpec;
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
            post_tool_use: vec![],
            user_prompt_submit: vec![],
            session_start: vec![],
            session_end: vec![],
        };
        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(fake_provider()),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, false, None),
            hooks,
            model: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
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
            crate::agent::context::ContextMessage::Tool { content, is_error, .. } => {
                assert!(is_error);
                assert!(content.contains("blocked"));
            }
            _ => panic!("expected Tool message, got {last:?}"),
        }
        assert!(count >= 1, "expected at least one rendered event");
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
            session_id: uuid::v7(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
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
            )
            .await
            .unwrap();

        let last = agent.context.messages.last().unwrap();
        match last {
            crate::agent::context::ContextMessage::Tool { content, is_error, .. } => {
                assert!(is_error);
                assert!(content.contains("unknown tool"));
            }
            _ => panic!("expected Tool message"),
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
                .send(AgentEvent::Start { message_id: "m".into() })
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

    // ─────── v0.6: --continue / continue_last_session ───────

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// No active session registered for this cwd → returns None.
    #[test]
    fn continue_last_session_returns_none_when_no_history() {
        let _g = lock();
        let home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);

        let cwd = tmp();
        let got = Agent::continue_last_session(&cwd);

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
    fn continue_last_session_returns_active_path_after_use() {
        let _g = lock();
        let home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);

        let cwd = tmp();
        let (path, _header) =
            crate::session::new_session(&cwd, "m", "http://x").expect("new");
        crate::session::set_active_session(&cwd, &path).expect("active");

        let got = Agent::continue_last_session(&cwd).expect("some");
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

        let (path, _hdr) =
            crate::session::new_session(&cwd, "m", "http://x").expect("new session");
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

    // ─────── v0.6: parallel tool execution ───────

    /// Two bash calls that each sleep 1s — they MUST run in parallel
    /// (total wall time < 1.8s), not sequentially (<2s). v0.5 was
    /// sequential; this test pins the new parallel contract.
    #[tokio::test]
    async fn execute_tool_calls_runs_in_parallel_not_sequence() {
        use std::time::Instant;

        let dir = tmp();
        let session_path = dir.join("p.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let mut agent = Agent {
            context: Context::default(),
            provider: Box::new(FakeProvider { response: "ok".into() }),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, false, None),
            hooks: HooksConfig::default(),
            model: String::new(),
            usage_total: Usage::default(),
            turn_count: 0,
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
}
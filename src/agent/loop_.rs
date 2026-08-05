//! The Agent turn loop — the heart of nanopi.
//!
//! See `docs/v0.5-research.md` §2 for the state machine. v0.5 executes
//! tool calls sequentially (one tool at a time, in LLM-returned order).
//! v0.6 will add parallel tool execution (Pi parity).

use std::path::PathBuf;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::agent::hook::{HookConfig, HookEvent, HookOutcome, run_hooks};
use crate::agent::permission::PermissionGate;
use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
use crate::provider::openai::OpenAiProvider;
use crate::session::{self, SessionEntry};
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
}

/// The agent — owns context, provider, tool registry, session, permissions.
pub struct Agent {
    pub context: Context,
    pub provider: OpenAiProvider,
    pub registry: ToolRegistry,
    pub session_path: PathBuf,
    pub session_id: uuid::Uuid,
    pub cwd: PathBuf,
    pub permission: PermissionGate,
    pub hooks: HooksConfig,
}

impl Agent {
    /// Run a single user turn to completion. Streams events to `tx` and
    /// persists all messages, tool calls, and tool results to the session
    /// JSONL file. Returns the final concatenated assistant text.
    ///
    /// Safety: caps at 16 tool-iteration rounds to prevent infinite loops.
    pub async fn run_turn(
        &mut self,
        user_msg: &str,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<String, AgentError> {
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

            let (calls, done, assistant_text) = collect_task
                .await
                .map_err(|e| AgentError::Provider(format!("collect task: {e}")))?;

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

            let Some((finish_reason, _usage)) = done else {
                // Stream ended without Done. Treat as Stop.
                return Ok(final_text);
            };

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
        for call in calls {
            // PreToolUse hooks (if enabled).
            let mut effective_args = call.arguments.clone();
            if self.permission.hooks_active() && !self.hooks.pre_tool_use.is_empty() {
                let (outcome, transformed) = run_hooks(
                    &self.hooks.pre_tool_use,
                    HookEvent::PreToolUse,
                    &call.name,
                    call.arguments.clone(),
                    &self.cwd,
                    Some(&self.session_id.to_string()),
                )
                .await;
                effective_args = transformed.unwrap_or(call.arguments.clone());
                match outcome {
                    HookOutcome::Block { reason } if self.permission.should_honor_pretooluse_block() => {
                        let result_text = format!("blocked by hook: {reason}");
                        session::append_entry(
                            &self.session_path,
                            &SessionEntry::ToolResult {
                                tool_call_id: call.id.clone(),
                                timestamp: time::now_iso8601(),
                                content: result_text.clone(),
                                is_error: true,
                            },
                        )?;
                        self.context.push_tool_result(call.id.clone(), result_text.clone(), true);
                        let _ = tx
                            .send(AgentEvent::TextDelta {
                                content_index: 0,
                                text: format!("\n[{} blocked: {}]\n", call.name, reason),
                            })
                            .await;
                        continue;
                    }
                    _ => {} // Allow, Transform, or Block-in-yolo (logged but not honored)
                }
            }

            // Resolve tool.
            let Some(tool) = self.registry.get(&call.name) else {
                let msg = format!("unknown tool: {}", call.name);
                session::append_entry(
                    &self.session_path,
                    &SessionEntry::ToolResult {
                        tool_call_id: call.id.clone(),
                        timestamp: time::now_iso8601(),
                        content: msg.clone(),
                        is_error: true,
                    },
                )?;
                self.context.push_tool_result(call.id.clone(), msg, true);
                continue;
            };

            // Persist tool call.
            session::append_entry(
                &self.session_path,
                &SessionEntry::ToolCall {
                    id: call.id.clone(),
                    timestamp: time::now_iso8601(),
                    tool_name: call.name.clone(),
                    arguments: effective_args.clone(),
                },
            )?;

            // Execute.
            let ctx = ToolContext { cwd: self.cwd.clone() };
            let exec_result = tool.execute(effective_args, &ctx).await;
            let (content, is_error) = match exec_result {
                Ok(o) => (o.content, o.is_error),
                Err(e) => (format!("tool error: {e}"), true),
            };

            // Persist result.
            session::append_entry(
                &self.session_path,
                &SessionEntry::ToolResult {
                    tool_call_id: call.id.clone(),
                    timestamp: time::now_iso8601(),
                    content: content.clone(),
                    is_error,
                },
            )?;
            self.context.push_tool_result(call.id.clone(), content.clone(), is_error);

            // Stream to renderer (so user sees tool output live).
            let _ = tx
                .send(AgentEvent::TextDelta {
                    content_index: 0,
                    text: format!("\n[{} → {} bytes]\n", call.name, content.len()),
                })
                .await;

            // PostToolUse hooks (informational; v0.5 ignores their output).
            if self.permission.hooks_active() && !self.hooks.post_tool_use.is_empty() {
                let _ = run_hooks(
                    &self.hooks.post_tool_use,
                    HookEvent::PostToolUse,
                    &call.name,
                    Value::Object(Default::default()),
                    &self.cwd,
                    Some(&self.session_id.to_string()),
                )
                .await;
            }
        }
        Ok(())
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
        };
        let mut agent = Agent {
            context: Context::default(),
            provider: fake_provider(),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, false, None),
            hooks,
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
            provider: fake_provider(),
            registry: ToolRegistry::standard(),
            session_path: session_path.clone(),
            session_id: uuid::v7(),
            cwd: dir.clone(),
            permission: PermissionGate::from_cli(false, false, None),
            hooks: HooksConfig::default(),
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
}
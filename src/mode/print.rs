//! Print mode (`-p`) — non-interactive, output to stdout, exit on completion.
//!
//! See `docs/v0.5-research.md` §5 for the design.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::agent::loop_::{Agent, HooksConfig};
use crate::agent::permission::PermissionGate;
use crate::event::AgentEvent;
use crate::provider::openai::OpenAiProvider;
use crate::render::stdout::StdoutRenderer;
use crate::session::{self, SessionEntry};
use crate::settings;
use crate::tool::ToolRegistry;
use crate::util::time;

/// What to print on stdout in `-p` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

/// JSON envelope returned at the end of `-p --output json` mode.
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonEnvelope {
    pub session_id: String,
    pub model: String,
    pub finish_reason: String,
    pub duration_ms: u64,
    pub usage: Value,
    pub messages: Vec<Value>,
}

pub async fn run_print_mode(
    base_url: &str,
    model: &str,
    api_key: &str,
    message: &str,
    output: OutputFormat,
    cwd: PathBuf,
    yolo: bool,
    no_hooks: bool,
    approve: Option<bool>,
    continue_session: bool,
    session_id: Option<String>,
    fork_id: Option<String>,
) -> Result<i32> {
    let started = std::time::Instant::now();

    // Resolve which session to use: --fork > --session > --continue > new.
    let choice = session::resolve_session(
        &cwd,
        continue_session,
        session_id.as_deref(),
        fork_id.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("resolve session: {e}"))?;

let (session_path, header) = match &choice {
        session::SessionChoice::Resume(p) => {
            // Reuse the existing session. We trust its recorded model /
            // base_url; if those are wrong the user can pass them again
            // via flags and the next turn will pick them up.
            let (h, _entries) = session::read_session(p)
                .map_err(|e| anyhow::anyhow!("read resumed session: {e}"))?;
            (p.clone(), h)
        }
        session::SessionChoice::New => session::new_session(&cwd, model, base_url)
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?,
    };

    // Register this cwd's active session pointer (used by next --continue).
    let _ = session::set_active_session(&cwd, &session_path);

    // Build the agent.
    let provider = OpenAiProvider::new(base_url, api_key, model);
    let permission = PermissionGate::from_cli(yolo, no_hooks, approve);

    // For v0.5: no hooks loaded yet (settings.toml loader is a separate
    // concern). YOLO mode means trust is implicit.
    let hooks = match settings::load_settings(&cwd) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("warning: failed to load settings: {e}");
            HooksConfig::default()
        }
    };

    let registry = ToolRegistry::standard();

// If we resumed an existing session, hydrate the Agent with its
    // history (so the model sees prior turns). Otherwise start fresh.
    let permission_for_resume = permission.clone();
    let mut agent = if let session::SessionChoice::Resume(_) = &choice {
        let mut a = Agent::load_session(&session_path, &cwd)
            .map_err(|e| anyhow::anyhow!("load session: {e}"))?;
        a.provider = Box::new(provider);
        a.registry = registry;
        a.permission = permission_for_resume;
        a.hooks = hooks;
        a.model = model.to_string();
        if a.context.system.is_none() {
            a.context.system = Some(crate::agent::system_prompt::build(&cwd, &a.registry.names()));
        }
        a
    } else {
        Agent {
            context: Context {
                system: Some(crate::agent::system_prompt::build(&cwd, &registry.names())),
                messages: Vec::new(),
                tools: registry.all_specs(),
            },
            provider: Box::new(provider),
            registry,
            session_path: session_path.clone(),
            session_id: header.id,
            cwd: cwd.clone(),
            permission,
            hooks,
            model: model.to_string(),
            usage_total: crate::event::Usage::default(),
            turn_count: 0,
        }
    };

    // Fire session_start hooks before the first turn.
    agent.fire_session_start().await;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let message_owned = message.to_string();
    let agent_task = {
        let mut agent = agent; // move
        tokio::spawn(async move {
            let r = agent.run_turn(message_owned.as_str(), &tx, None).await;
            // Fire session_end regardless of turn outcome so cleanup
            // hooks (e.g. flush metrics) always run.
            agent.fire_session_end().await;
            r
        })
    };

    let mut renderer = StdoutRenderer::new();
    // Spinner only in text mode. JSON mode buffers everything and dumps at
    // the end, so no user-facing terminal chatter to keep alive.
    let mut spinner = if output == OutputFormat::Text {
        Some(crate::render::spinner::Spinner::start("thinking"))
    } else {
        None
    };
    while let Some(ev) = rx.recv().await {
        if output == OutputFormat::Text {
            if let Some(mut s) = spinner.take() {
                if matches!(
                    ev,
                    AgentEvent::TextDelta { .. }
                        | AgentEvent::ToolCall { .. }
                        | AgentEvent::Error { .. }
                ) {
                    s.stop().await;
                } else {
                    spinner = Some(s);
                }
            }
            let _ = renderer.render(&ev);
        }
        // JSON mode: ignore events (we'll build envelope at the end).
    }
    if let Some(mut s) = spinner.take() {
        s.stop().await;
    }
    let final_text = agent_task.await??;

    let duration_ms = started.elapsed().as_millis() as u64;

    match output {
        OutputFormat::Text => {
            eprintln!(
                "\n✓ session {} saved to {}",
                header.id,
                session_path.display()
            );
            Ok(0)
        }
        OutputFormat::Json => {
            let envelope = JsonEnvelope {
                session_id: header.id.to_string(),
                model: model.to_string(),
                finish_reason: "stop".into(),
                duration_ms,
                usage: json!({}),
                messages: collect_messages(&session_path)?,
            };
            let s = serde_json::to_string(&envelope)?;
            println!("{s}");
            Ok(0)
        }
    }
    .map(|code| {
        let _ = final_text; // silence unused warning if json branch
        code
    })
}

/// Read back all SessionEntries from a session file and present the
/// user/assistant messages in the JSON envelope.
fn collect_messages(session_path: &std::path::Path) -> Result<Vec<Value>> {
    let (_header, entries) = session::read_session(session_path)
        .map_err(|e| anyhow::anyhow!("read session: {e}"))?;
    let mut out = Vec::new();
    for e in entries {
        match e {
            SessionEntry::Message { role, content, .. } => {
                out.push(json!({"role": role, "content": content}));
            }
            SessionEntry::ToolCall { tool_name, arguments, .. } => {
                out.push(json!({"role": "assistant_tool_call", "tool": tool_name, "arguments": arguments}));
            }
            SessionEntry::ToolResult { tool_call_id, content, is_error, .. } => {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                    "is_error": is_error,
                }));
            }
            _ => {}
        }
    }
    Ok(out)
}

// Arc import used by future extensions (TUI mode).
#[allow(dead_code)]
fn _arc_unused(_: Arc<()>) {}

// time import retained for future usage.
#[allow(dead_code)]
fn _time_marker() -> String { time::now_iso8601() }
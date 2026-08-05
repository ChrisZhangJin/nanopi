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
) -> Result<i32> {
    let started = std::time::Instant::now();

    // Create session.
    let (session_path, header) = session::new_session(&cwd, model, base_url)
        .map_err(|e| anyhow::anyhow!("create session: {e}"))?;

    // Build the agent.
    let provider = OpenAiProvider::new(base_url, api_key, model);
    let permission = PermissionGate::from_cli(yolo, no_hooks, approve);

    // For v0.5: no hooks loaded yet (settings.toml loader is a separate
    // concern). YOLO mode means trust is implicit.
    let hooks = HooksConfig::default();

    let registry = ToolRegistry::standard();

    let mut agent = Agent {
        context: Context {
            system: None,
            messages: Vec::new(),
            tools: registry.all_specs(),
        },
        provider,
        registry,
        session_path: session_path.clone(),
        session_id: header.id,
        cwd: cwd.clone(),
        permission,
        hooks,
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let message_owned = message.to_string();
    let agent_task = {
        let mut agent = agent; // move
        tokio::spawn(async move { agent.run_turn(message_owned.as_str(), &tx).await })
    };

    let mut renderer = StdoutRenderer::new();
    while let Some(ev) = rx.recv().await {
        if output == OutputFormat::Text {
            let _ = renderer.render(&ev);
        }
        // JSON mode: ignore events (we'll build envelope at the end).
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
//! Interactive mode — v0.5 uses a single-question-then-exit flow.
//!
//! Full multi-turn TUI (with input editing, history, alt-screen) is a
//! v0.6+ concern. v0.5 interactive mode reads a single message from
//! stdin (or accepts `--message`), runs the agent, and prints the result.
//! This matches the v0.1 demo's UX while exercising the v0.5 stack.

use std::path::PathBuf;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::agent::loop_::{Agent, HooksConfig};
use crate::agent::permission::PermissionGate;
use crate::event::AgentEvent;
use crate::provider::openai::OpenAiProvider;
use crate::render::stdout::StdoutRenderer;
use crate::session;
use crate::settings;
use crate::tool::ToolRegistry;

pub async fn run_interactive_mode(
    base_url: &str,
    model: &str,
    api_key: &str,
    message: Option<String>,
    cwd: PathBuf,
    yolo: bool,
    no_hooks: bool,
    approve: Option<bool>,
) -> Result<i32> {
    let msg = match message {
        Some(m) => m,
        None => {
            use std::io::BufRead;
            eprintln!("(enter your message; Ctrl-D to finish)");
            let mut buf = String::new();
            let stdin = std::io::stdin();
            stdin.lock().read_line(&mut buf)?;
            if buf.trim().is_empty() {
                anyhow::bail!("no message provided");
            }
            buf.trim().to_string()
        }
    };

    // Create session.
    let (session_path, header) = session::new_session(&cwd, model, base_url)
        .map_err(|e| anyhow::anyhow!("create session: {e}"))?;

    let provider = OpenAiProvider::new(base_url, api_key, model);
    let permission = PermissionGate::from_cli(yolo, no_hooks, approve);

    let registry = ToolRegistry::standard();

    let hooks = match settings::load_settings(&cwd) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("warning: failed to load settings: {e}");
            HooksConfig::default()
        }
    };

    let mut agent = Agent {
        context: Context {
            system: None,
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
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let agent_task = {
        let mut agent = agent;
        tokio::spawn(async move { agent.run_turn(msg.as_str(), &tx).await })
    };

    let mut renderer = StdoutRenderer::new();
    while let Some(ev) = rx.recv().await {
        let _ = renderer.render(&ev);
    }
    agent_task.await??;

    eprintln!(
        "\n✓ session {} saved to {}",
        header.id,
        session_path.display()
    );
    Ok(0)
}
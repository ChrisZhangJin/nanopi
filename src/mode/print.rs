//! Print mode (`-p`) — non-interactive, output to stdout, exit on completion.
//!
//! See `docs/v0.5-research.md` §5 for the design.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::agent::loop_::{Agent, HooksConfig};
use crate::agent::permission::PermissionGate;
use crate::event::AgentEvent;
use crate::render::stdout::StdoutRenderer;
use crate::session::{self, SessionEntry};
use crate::settings;
use crate::tool::ToolRegistry;

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
    // `None` = no explicit `api_kind`; the vendor picks the transport.
    api_kind: Option<crate::provider::ApiKind>,
    // `config.provider` — explicit vendor id overriding the
    // base_url/model sniff.
    cfg_provider: Option<String>,
    base_url: &str,
    model: &str,
    api_key: &str,
    message: &str,
    output: OutputFormat,
    cwd: PathBuf,
    no_hooks: bool,
    approve: Option<bool>,
    continue_session: bool,
    session_id: Option<String>,
    fork_id: Option<String>,
    // `--session-id`: use this exact session, creating it if missing.
    exact_session_id: Option<String>,
    skill_load: crate::agent::build::SkillLoadPolicy,
    no_context_files: bool,
    prompt_overrides: crate::agent::prompt_override::PromptOverrides,
) -> Result<i32> {
    let started = std::time::Instant::now();

    // Resolve which session to use: --fork > --session > --continue > new.
    let choice = session::resolve_session(
        &cwd,
        continue_session,
        session_id.as_deref(),
        fork_id.as_deref(),
        exact_session_id.as_deref(),
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
        session::SessionChoice::NewWithId(id) => {
            // PI warns here too — a typo'd --session-id silently starting
            // a fresh conversation instead of resuming is worth a line on
            // stderr (main.ts:390-399).
            eprintln!(
                "nanopi: no session found with id '{id}' for this directory; \
                 creating a new one with that id"
            );
            session::new_session_with_id(&cwd, model, base_url, Some(id))
                .map_err(|e| anyhow::anyhow!("create session: {e}"))?
        }
    };

    // Register this cwd's active session pointer (used by next --continue).
    let _ = session::set_active_session(&cwd, &session_path);

    // Build the agent.
    let provider = crate::provider::build(
        api_kind,
        base_url,
        api_key,
        model,
        Some(crate::vendor::pick_vendor(
            cfg_provider.as_deref(),
            Some(base_url),
            model,
        )),
    );
    let permission = PermissionGate::from_cli(no_hooks, approve);

    // For v0.5: no hooks loaded yet (settings.toml loader is a separate
    // concern).
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
    use crate::agent::build::{print_skill_diagnostics, AgentBuildInputs};
    let agent = if let session::SessionChoice::Resume(_) = &choice {
        let mut a = Agent::load_session(&session_path, &cwd)
            .map_err(|e| anyhow::anyhow!("load session: {e}"))?;
        let diags = a.hydrate_resumed(
            provider,
            registry,
            permission,
            hooks,
            model.to_string(),
            base_url.to_string(),
            api_key.to_string(),
            skill_load,
            no_context_files,
            prompt_overrides,
        );
        print_skill_diagnostics(&diags);
        a
    } else {
        let (a, diags) = Agent::build_fresh(AgentBuildInputs {
            cwd: cwd.clone(),
            registry,
            provider,
            session_path: session_path.clone(),
            session_id: header.id.clone(),
            permission,
            hooks,
            model: model.to_string(),
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            skill_load,
            no_context_files,
            prompt_overrides,
            initial_follow_up: None,
            tool_exec_mode: crate::config::ToolExecMode::default(),
        });
        print_skill_diagnostics(&diags);
        a
    };

    // Fire session_start hooks before the first turn.
    agent.fire_session_start().await;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let message_owned = message.to_string();
    let agent_task = {
        let mut agent = agent; // move
        tokio::spawn(async move {
            let r = agent.run_turn(message_owned.as_str(), &tx, None, None).await;
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
                session_id: header.id.clone(),
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
    let (_header, entries) =
        session::read_session(session_path).map_err(|e| anyhow::anyhow!("read session: {e}"))?;
    let mut out = Vec::new();
    for e in entries {
        match e {
            SessionEntry::Message { role, content, .. } => {
                out.push(json!({"role": role, "content": content}));
            }
            SessionEntry::ToolCall {
                tool_name,
                arguments,
                ..
            } => {
                out.push(json!({"role": "assistant_tool_call", "tool": tool_name, "arguments": arguments}));
            }
            SessionEntry::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
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

// time import retained for future usage.

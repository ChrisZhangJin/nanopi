//! Interactive mode — v0.6+ adds a multi-turn event loop on top of v0.5's
//! single-question flow. Reads user messages from stdin, one per line.
//! Submit by Enter, exit by EOF (Ctrl-D) or empty line. Same Agent
//! instance is reused across turns so context accumulates.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::agent::context::Context;
use crate::agent::loop_::{Agent, HooksConfig};
use crate::agent::permission::PermissionGate;
use crate::event::AgentEvent;
use crate::provider::openai::OpenAiProvider;
use crate::render::stdout::StdoutRenderer;
use crate::session::{self, SessionChoice};
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
    continue_session: bool,
    session_id: Option<String>,
    fork_id: Option<String>,
) -> Result<i32> {
    let permission = PermissionGate::from_cli(yolo, no_hooks, approve);

    // ── Resolve which session file to use ─────────────────────────────────
    let choice = session::resolve_session(
        &cwd,
        continue_session,
        session_id.as_deref(),
        fork_id.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("resolve session: {e}"))?;

    let (session_path, header) = match &choice {
        SessionChoice::Resume(p) => {
            let (h, _entries) = session::read_session(p)
                .map_err(|e| anyhow::anyhow!("read resumed session: {e}"))?;
            (p.clone(), h)
        }
        SessionChoice::New => session::new_session(&cwd, model, base_url)
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?,
    };
    // Remember this cwd's active session for next --continue.
    let _ = session::set_active_session(&cwd, &session_path);

    let provider = OpenAiProvider::new(base_url, api_key, model);
    let registry = ToolRegistry::standard();

    let hooks = match settings::load_settings(&cwd) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("warning: failed to load settings: {e}");
            HooksConfig::default()
        }
    };

    // ── Build the Agent (resume or fresh) ───────────────────────────────
    let permission_for_resume = permission.clone();
    let agent: Option<Agent> = Some({
        if let SessionChoice::Resume(_) = &choice {
            let mut a = Agent::load_session(&session_path, &cwd)
                .map_err(|e| anyhow::anyhow!("load session: {e}"))?;
            a.provider = Box::new(provider);
            a.registry = registry;
            a.permission = permission_for_resume;
            a.hooks = hooks;
            a
        } else {
            Agent {
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
            }
        }
    });

    // ── If `--message` is set, run one turn and exit (v0.5 compat) ─────
    let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(agent));

    // Fire session_start hooks once, before any turn.
    fire_lifecycle_hook(&agent_slot, true).await;

    if let Some(first_msg) = message {
        let rc = run_one_turn(Arc::clone(&agent_slot), &first_msg, None).await;
        fire_lifecycle_hook(&agent_slot, false).await;
        return rc;
    }

    // ── Multi-turn event loop ───────────────────────────────────────────
    let initial_session_id = {
        let g = agent_slot.lock().await;
        g.as_ref().map(|a| a.session_id).unwrap()
    };
    eprintln!(
        "nanopi v0.6 interactive — session {}\n\
         (empty line or Ctrl-D to exit)\n",
        initial_session_id
    );

    use tokio_util::sync::CancellationToken;
    let cancel = CancellationToken::new();

    // Watch for Ctrl-C in a side task. It cancels the current turn
    // (not the whole event loop) on each press.
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_ok() {
                    cancel.cancel();
                }
            }
        });
    }

    // v0.6+: switch to a real line editor with history. Editor lives
    // in a Mutex so each spawn_blocking read can borrow it briefly.
    use rustyline::Editor;
    use rustyline::history::FileHistory;

    let history_path = dirs::home_dir()
        .map(|h| h.join(".nanopi").join("history.txt"));
    let editor: Arc<StdMutex<Editor<(), FileHistory>>> = Arc::new(StdMutex::new({
        let mut e: Editor<(), FileHistory> = Editor::new()
            .expect("init editor");
        if let Some(p) = &history_path {
            let _ = e.load_history(p);
        }
        e
    }));

    loop {
        // Block on stdin via rustyline (handles ↑↓, Ctrl-A/E/K/U, etc).
        // We do the read AND the history save inside the same blocking
        // task so we never have two simultaneous `&mut editor` borrows.
        let history_path_for_task = history_path.clone();
        let editor_for_task = Arc::clone(&editor);
        let readline = tokio::task::spawn_blocking(move || -> Result<String, std::io::Error> {
            let mut editor = editor_for_task
                .lock()
                .expect("editor lock poisoned");
            let prompt = "> ";
            let result = editor.readline(&prompt);
            // Re-save history after every read so ↑↓ works across crashes.
            if let Some(p) = &history_path_for_task {
                let _ = editor.append_history(p);
            }
            result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e}")))
        })
        .await;
        let line = match readline {
            Ok(Ok(line)) => line,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Ctrl-C during readline: ignore the line, loop again.
                continue;
            }
            Ok(Err(e)) => {
                eprintln!("\n[input error: {e}]");
                continue;
            }
            Err(join_err) => {
                eprintln!("\n[internal: {join_err}]");
                continue;
            }
        };
        let msg = line.trim();
        if msg.is_empty() {
            // Empty line == exit (v0.6+ explicit; Ctrl-D also exits).
            break;
        }
        // Slash commands are handled locally, never sent to the LLM.
        if msg == "/compact" {
            let mut guard = agent_slot.lock().await;
            if let Some(a) = guard.as_mut() {
                let before = a.context.estimate_chars();
                a.compact_now().await;
                let after = a.context.estimate_chars();
                eprintln!("[compacted: {before} → {after} chars]");
            }
            continue;
        }
        if msg == "/quit" || msg == "/exit" {
            break;
        }
        // Per-turn cancel token, distinct from the global one.
        let turn_cancel = CancellationToken::new();
        let cancel_link = cancel.clone();
        let link_task = tokio::spawn({
            let turn_cancel = turn_cancel.clone();
            async move {
                cancel_link.cancelled().await;
                turn_cancel.cancel();
            }
        });
        run_one_turn(Arc::clone(&agent_slot), msg, Some(turn_cancel.clone())).await?;
        link_task.abort();
        if turn_cancel.is_cancelled() {
            eprintln!("\n[turn cancelled — type next message to continue]");
        }
    }

    eprintln!(
        "\n✓ session {} saved to {}",
        initial_session_id,
        {
            let g = agent_slot.lock().await;
            g.as_ref().map(|a| a.session_path.display().to_string()).unwrap_or_default()
        }
    );

    // Fire session_end hooks before returning.
    fire_lifecycle_hook(&agent_slot, false).await;

    Ok(0)
}

/// Fire session_start (start=true) or session_end (start=false) hooks
/// on the currently-parked Agent. No-op if the Agent slot is empty
/// (e.g. a turn is in flight — shouldn't happen at start/end).
async fn fire_lifecycle_hook(
    agent: &Arc<Mutex<Option<Agent>>>,
    start: bool,
) {
    let guard = agent.lock().await;
    if let Some(a) = guard.as_ref() {
        if start {
            a.fire_session_start().await;
        } else {
            a.fire_session_end().await;
        }
    }
}

/// Run one user turn, printing events to stdout via StdoutRenderer.
/// Same Agent is reused across turns, so context accumulates.
///
/// Implementation: the Agent is parked inside a `Mutex<Option<Agent>>`
/// shared with the spawned turn task. We `take()` the Option, run the
/// turn in the background, and on completion `replace()` the
/// (potentially-mutated) agent back. The outer caller sees the
/// updated `agent: &mut Agent` after each call.
async fn run_one_turn(
    agent: Arc<Mutex<Option<Agent>>>,
    msg: &str,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<i32> {
    use tokio::sync::Mutex;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let tx_for_task = tx.clone();
    let slot_for_task = Arc::clone(&agent);
    let msg_owned = msg.to_string();
    let task = tokio::spawn(async move {
        let mut guard = slot_for_task.lock().await;
        let mut a = guard.take().expect("agent was just put here");
        drop(guard);
        let result = a.run_turn(&msg_owned, &tx_for_task, cancel).await;
        // tx_for_task drops here, closing the channel once the task ends
        let mut guard = slot_for_task.lock().await;
        *guard = Some(a);
        result
    });
    drop(tx); // close the main sender; task has its own clone

    let mut renderer = StdoutRenderer::new();
    while let Some(ev) = rx.recv().await {
        let _ = renderer.render(&ev);
    }
    let result = task.await?;
    result.map(|_| 0).map_err(|e| anyhow::anyhow!(e))
}
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
        // ReadOutcome preserves EOF vs Interrupted vs Other so we can exit
        // cleanly on piped stdin (rustyline returns Eof immediately when
        // stdin isn't a TTY).
        enum ReadOutcome {
            Line(String),
            Eof,
            Interrupted,
            Other(String),
        }
        let readline = tokio::task::spawn_blocking(move || -> ReadOutcome {
            let mut editor = editor_for_task
                .lock()
                .expect("editor lock poisoned");
            let result = editor.readline("> ");
            if let Some(p) = &history_path_for_task {
                let _ = editor.append_history(p);
            }
            match result {
                Ok(s) => ReadOutcome::Line(s),
                Err(rustyline::error::ReadlineError::Eof) => ReadOutcome::Eof,
                Err(rustyline::error::ReadlineError::Interrupted) => ReadOutcome::Interrupted,
                Err(e) => ReadOutcome::Other(format!("{e}")),
            }
        })
        .await;
        let line = match readline {
            Ok(ReadOutcome::Line(s)) => s,
            Ok(ReadOutcome::Eof) => {
                // Ctrl-D or piped-stdin exhausted → clean exit.
                break;
            }
            Ok(ReadOutcome::Interrupted) => {
                // Ctrl-C at the prompt: exit cleanly. (Ctrl-C during a
                // running turn is handled elsewhere — the tokio signal
                // handler cancels that turn and control returns here.)
                eprintln!();
                break;
            }
            Ok(ReadOutcome::Other(e)) => {
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
        // Pre-turn dim status line: model · tokens · cost · cwd · branch.
        print_pre_turn_status(&agent_slot).await;
        let turn_started = std::time::Instant::now();
        match run_one_turn(Arc::clone(&agent_slot), msg, Some(turn_cancel.clone())).await {
            Ok(_) => {}
            Err(e) => {
                // Provider errors (rate limit, timeout, 5xx) should NOT
                // kill the whole REPL. Print and prompt again — the
                // agent slot is already restored by run_one_turn.
                eprintln!("\n[error: {e:#}]");
            }
        }
        link_task.abort();
        if turn_cancel.is_cancelled() {
            eprintln!("\n[turn cancelled — type next message to continue]");
        }
        // Post-turn dim footer: ↑input ↓output · elapsed · turn #N.
        print_post_turn_status(&agent_slot, turn_started.elapsed()).await;
    }

    let sid = initial_session_id.to_string();
    let sid_short: String = sid.chars().take(8).collect();
    eprintln!(
        "\n\x1b[2m✓ session {} saved\x1b[0m",
        sid_short,
    );
    eprintln!(
        "\x1b[2mTo resume:  nanopi --continue    or    nanopi --session {}\x1b[0m",
        sid,
    );

    // Fire session_end hooks before returning.
    fire_lifecycle_hook(&agent_slot, false).await;

    Ok(0)
}

/// Print a dim-gray status line BEFORE the assistant starts streaming,
/// so the user sees what state they're about to spend on. Format:
///   `model · ↑1.2k ↓340 · $0.05 · ~/dir · branch`
/// Missing pieces (unknown cost, no branch) are dropped. Best-effort:
/// silently skips if the agent slot is locked/empty.
async fn print_pre_turn_status(agent: &Arc<Mutex<Option<Agent>>>) {
    let g = agent.lock().await;
    if let Some(a) = g.as_ref() {
        let line = crate::render::status_line::classic_status_line(
            &a.model,
            &a.usage_total,
            &a.cwd,
        );
        // ANSI dim (2) + reset (0) — matches other stderr chatter.
        eprintln!("\x1b[2m{line}\x1b[0m");
    }
}

/// Print a dim-gray delta after a turn completes: token delta this
/// turn (approximated as diff between now and pre-turn snapshot is
/// tricky without extra state; we just show cumulative + turn count
/// + wall time).
async fn print_post_turn_status(
    agent: &Arc<Mutex<Option<Agent>>>,
    elapsed: std::time::Duration,
) {
    let g = agent.lock().await;
    if let Some(a) = g.as_ref() {
        eprintln!(
            "\x1b[2m↑{} ↓{} · {:.1}s · turn #{}\x1b[0m",
            crate::pricing::fmt_tokens(a.usage_total.input_tokens),
            crate::pricing::fmt_tokens(a.usage_total.output_tokens),
            elapsed.as_secs_f64(),
            a.turn_count,
        );
    }
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
    // Spinner lives for the full turn, with labels that follow the
    // state machine:
    //   idle           → "thinking"          (waiting for first token)
    //   assistant text → hidden              (real output flowing)
    //   tool_call      → "running <name>"    (tool about to execute)
    //   tool result    → "thinking"          (waiting for next LLM iter)
    // Done events do NOT stop the spinner because Done(ToolCalls) is
    // followed by silent tool execution + another stream_turn; the
    // spinner should carry through. Final teardown happens after the
    // outer channel closes.
    let mut spinner: Option<crate::render::spinner::Spinner> =
        Some(crate::render::spinner::Spinner::start("thinking"));
    while let Some(ev) = rx.recv().await {
        match &ev {
            AgentEvent::TextDelta { .. } => {
                // Real model text arriving — stop the spinner.
                if let Some(mut s) = spinner.take() {
                    s.stop().await;
                }
            }
            AgentEvent::ToolCall { call, .. } => {
                if let Some(mut s) = spinner.take() {
                    s.stop().await;
                }
                let label = tool_spinner_label(&call.name, &call.arguments);
                spinner = Some(crate::render::spinner::Spinner::start(label));
            }
            AgentEvent::ToolResult { .. } => {
                // Tool done → next iteration is LLM waiting again.
                if let Some(mut s) = spinner.take() {
                    s.stop().await;
                }
                spinner = Some(crate::render::spinner::Spinner::start("thinking"));
            }
            AgentEvent::Error { .. } => {
                if let Some(mut s) = spinner.take() {
                    s.stop().await;
                }
            }
            // Done / Start / ThinkingDelta: leave spinner alone.
            _ => {}
        }
        let _ = renderer.render(&ev);
    }
    if let Some(mut s) = spinner.take() {
        s.stop().await;
    }
    let result = task.await?;
    result.map(|_| 0).map_err(|e| anyhow::anyhow!(e))
}


/// Build a spinner label for a specific tool call, so the user sees
/// what's actually running. Falls back to `running <tool>` for
/// unknown tools. Normalizes gateway-mangled names (`Bash_tool` →
/// `bash`) so labels stay canonical even when the upstream is buggy.
fn tool_spinner_label(name: &str, args: &serde_json::Value) -> String {
    let lower = name.to_ascii_lowercase();
    let canonical = lower.strip_suffix("_tool").unwrap_or(&lower);
    let arg_preview = match canonical {
        "bash" => args.get("command").and_then(|v| v.as_str()).map(|s| s.to_string()),
        "read" | "write" | "edit" | "ls" => args.get("path").and_then(|v| v.as_str()).map(|s| s.to_string()),
        "grep" | "find" => args.get("pattern").and_then(|v| v.as_str()).map(|s| s.to_string()),
        _ => None,
    };
    let verb = match canonical {
        "bash" => "running bash",
        "read" => "reading",
        "write" => "writing",
        "edit" => "editing",
        "grep" => "grepping for",
        "find" => "finding",
        "ls" => "listing",
        other => return format!("running {other}"),
    };
    match arg_preview {
        Some(s) => {
            let clipped: String = s.chars().take(60).collect();
            let ellip = if s.chars().count() > 60 { "…" } else { "" };
            format!("{verb} {clipped}{ellip}")
        }
        None => format!("running {canonical}"),
    }
}

/// (Kept for backward-compat with earlier code paths; no longer used
/// by the primary spinner loop.) An event that produces terminal
/// output the user can see.
#[allow(dead_code)]
fn is_visible_event(ev: &AgentEvent) -> bool {
    matches!(
        ev,
        AgentEvent::TextDelta { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::Error { .. }
    )
}
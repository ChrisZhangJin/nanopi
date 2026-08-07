//! PI-style inline TUI (opt-in via `--tui`, default when stdin is TTY).
//!
//! Not alt-screen. Terminal scrollback is preserved. Layout:
//!
//!   [startup one-shot printed to stdout]
//!     nanopi v0.6+  claude-opus-4-7  session abc12345
//!     escape · ctrl+c/d exit · / commands · ! bash
//!
//!   [conversation history — printed to stdout via insert_before]
//!     > user turn 1
//!     assistant reply 1
//!     [tool_call: bash …]
//!     [bash → 97 bytes]
//!     assistant reply 2
//!     …
//!
//!   [docked inline region — 5 rows, always at bottom]
//!     ┌───────────────────────────────────────┐   ← input box top border
//!     │ > user typing here_                   │
//!     └───────────────────────────────────────┘   ← input box bottom border
//!     ~/nanopi (main)                              ← cwd + branch
//!     42% context (auto) · ↑1.2k ↓340 · $0.05    ← metrics
//!
//! Keys:
//!   Enter      submit
//!   Backspace  delete char
//!   Ctrl-C     cancel turn (if streaming) / exit (if idle)
//!   Ctrl-D     exit
//!   /quit      exit
//!   /compact   force context compaction
//!
//! MVP: single-line input; multi-line + Emacs keys come in v0.7 S2.

use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::context::Context;
use crate::agent::loop_::{Agent, HooksConfig};
use crate::agent::permission::PermissionGate;
use crate::event::AgentEvent;
use crate::provider::openai::OpenAiProvider;
use crate::session::{self, SessionChoice};
use crate::settings;
use crate::tool::ToolRegistry;
use crate::render::menu::{MenuAction, MenuItem, MenuState};
use crate::render::text_buffer::{Action as TbAction, TextBuffer};

/// Slash commands available in the palette. Keep the payload
/// `&'static str` so it's copy-able and hashable if we ever key on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashCmd {
    Compact,
    Quit,
    // Future: Model, Login, Settings, Export, Import.
}

fn slash_items() -> Vec<MenuItem<SlashCmd>> {
    vec![
        MenuItem::new("/compact", "Force context compaction", SlashCmd::Compact),
        MenuItem::new("/quit",    "Exit the session",         SlashCmd::Quit),
        MenuItem::new("/exit",    "Exit the session",         SlashCmd::Quit),
    ]
}

const DOCK_HEIGHT: u16 = 10; // palette(5) + input(3) + footer(2)

// ─────────────────────────────────────────────────────────────────────
// Entry
// ─────────────────────────────────────────────────────────────────────

pub async fn run_tui_mode(
    base_url: &str,
    model: &str,
    api_key: &str,
    cwd: PathBuf,
    yolo: bool,
    no_hooks: bool,
    approve: Option<bool>,
    continue_session: bool,
    session_id: Option<String>,
    fork_id: Option<String>,
) -> Result<i32> {
    let permission = PermissionGate::from_cli(yolo, no_hooks, approve);

    let choice = session::resolve_session(
        &cwd,
        continue_session,
        session_id.as_deref(),
        fork_id.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("resolve session: {e}"))?;
    let (session_path, header) = match &choice {
        SessionChoice::Resume(p) => {
            let (h, _e) = session::read_session(p)
                .map_err(|e| anyhow::anyhow!("read resumed session: {e}"))?;
            (p.clone(), h)
        }
        SessionChoice::New => session::new_session(&cwd, model, base_url)
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?,
    };
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

    let agent: Agent = if let SessionChoice::Resume(_) = &choice {
        let mut a = Agent::load_session(&session_path, &cwd)
            .map_err(|e| anyhow::anyhow!("load session: {e}"))?;
        a.provider = Box::new(provider);
        a.registry = registry;
        a.permission = permission;
        a.hooks = hooks;
        a.model = model.to_string();
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
            model: model.to_string(),
            usage_total: crate::event::Usage::default(),
            turn_count: 0,
        }
    };

    let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(Some(agent)));

    // Session-start hook fires once.
    {
        let g = agent_slot.lock().await;
        if let Some(a) = g.as_ref() {
            a.fire_session_start().await;
        }
    }

    // ── Startup banner (one-shot to stdout, scrolls up like normal
    // output). Printed BEFORE we enter raw mode so println! behaves.
    print_startup_banner(model, &header.id.to_string());

    let mut terminal = setup_terminal()?;
    let mut app = App::new(header.id.to_string(), model.to_string(), cwd.clone());
    let result = run_app(&mut terminal, &mut app, agent_slot.clone()).await;
    let _ = teardown_terminal(&mut terminal);

    // Session-end hook.
    {
        let g = agent_slot.lock().await;
        if let Some(a) = g.as_ref() {
            a.fire_session_end().await;
        }
    }

    // Closing lines: `✓ session saved` + resume hint so the user knows
    // how to pick up where they left off.
    let sid = header.id.to_string();
    let sid_short = &sid[..8.min(sid.len())];
    println!("\n\x1b[2m✓ session {sid_short} saved\x1b[0m");
    println!("\x1b[2mTo resume:  nanopi --continue    or    nanopi --session {sid}\x1b[0m");

    result
}

fn print_startup_banner(model: &str, session_id: &str) {
    let sid_short = &session_id[..8.min(session_id.len())];
    // Title line: bold nanopi + dim details.
    println!(
        "\x1b[1mnanopi\x1b[0m \x1b[2mv0.6+  {}  session {}\x1b[0m",
        model, sid_short
    );
    // Hint line, dim.
    println!(
        "\x1b[2mCtrl-C interrupt · Ctrl-D exit · / commands · Enter submit\x1b[0m"
    );
    println!();
}

// ─────────────────────────────────────────────────────────────────────
// Terminal setup / teardown  (NO alt-screen — inline mode preserves
// scrollback the way PI does)
// ─────────────────────────────────────────────────────────────────────

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(DOCK_HEIGHT),
        },
    )?;
    Ok(terminal)
}

fn teardown_terminal(term: &mut Term) -> Result<()> {
    // Clear the dock area before restoring so the user's shell prompt
    // lands on a clean line.
    let _ = term.clear();
    disable_raw_mode()?;
    crossterm::execute!(term.backend_mut(), DisableBracketedPaste)?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// App state (dock only — scrollback content is stdout, no buffer)
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    Streaming,
}

struct App {
    input: TextBuffer,
    /// When Some, the slash-command palette is open and consumes keys
    /// (except letters, which the TextBuffer still gets for filtering).
    palette: Option<MenuState<SlashCmd>>,
    status: Status,
    session_id: String,
    model: String,
    should_exit: bool,
    cwd: PathBuf,
    usage: crate::event::Usage,
    context_chars: usize,
    turn_count: u32,
    /// Partial assistant text still being streamed on the "current
    /// line". Flushed to scrollback via insert_before on newline / turn
    /// end.
    stream_buf: String,
    /// Same, for `ThinkingDelta` events (Anthropic reasoning traces).
    /// Rendered dim italic to distinguish from the model's actual reply.
    thinking_buf: String,
    /// A tool_call event just arrived; its bar is not yet drawn. On the
    /// matching tool-result marker we colour it green (success) or red
    /// (failure) using the marker's separator (`→` vs `✗`).
    pending_tool_call: Option<PendingBar>,
    /// Optional short status shown to the right of the input box
    /// (e.g. "cancelling…", "compacting…").
    status_note: Option<String>,
}

impl App {
    fn new(session_id: String, model: String, cwd: PathBuf) -> Self {
        Self {
            input: TextBuffer::new(),
            palette: None,
            status: Status::Idle,
            session_id,
            model,
            should_exit: false,
            cwd,
            usage: crate::event::Usage::default(),
            context_chars: 0,
            turn_count: 0,
            stream_buf: String::new(),
            thinking_buf: String::new(),
            pending_tool_call: None,
            status_note: None,
        }
    }
}

/// A ToolCall event we've received but not yet drawn to scrollback.
/// We wait until the tool_result marker so we can pick the bar color
/// (green success / red failure).
#[derive(Debug, Clone)]
struct PendingBar {
    leading: String,
    body: String,
}

// ─────────────────────────────────────────────────────────────────────
// Key → action
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum KeyAction {
    Nothing,
    StartTurn(String),
    CancelTurn,
    Exit,
    Compact,
}

/// Dispatch one key event. Palette (if open) claims navigation keys
/// first; everything else flows through TextBuffer. After the key is
/// handled, the palette's open/closed state is re-synced against the
/// input buffer (open ⇔ first line starts with `/`).
fn interpret_key(app: &mut App, k: KeyEvent) -> KeyAction {
    // ── Palette owns navigation keys when open. ─────────────────────
    if app.palette.is_some() {
        let claims_nav = matches!(
            k.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Esc | KeyCode::Tab
        ) || (k.code == KeyCode::Enter && !k.modifiers.contains(KeyModifiers::SHIFT));
        if claims_nav {
            let m = app.palette.as_mut().unwrap();
            match m.handle_key(k) {
                MenuAction::Chosen(cmd) => {
                    app.palette = None;
                    app.input.clear();
                    return dispatch_slash(cmd);
                }
                MenuAction::Cancel => {
                    app.palette = None;
                    app.input.clear();
                    return KeyAction::Nothing;
                }
                MenuAction::Nothing => {
                    return KeyAction::Nothing;
                }
            }
        }
    }

    // ── Otherwise route through the text buffer. ────────────────────
    let action = app.input.handle_key(k);
    let out = match action {
        TbAction::Nothing => KeyAction::Nothing,
        TbAction::SlashChanged(_) => KeyAction::Nothing, // sync below opens palette
        TbAction::Cancel => {
            if app.palette.is_some() {
                app.palette = None;
                app.input.clear();
                KeyAction::Nothing
            } else if app.status == Status::Streaming {
                KeyAction::CancelTurn
            } else {
                KeyAction::Exit
            }
        }
        TbAction::Exit => KeyAction::Exit,
        TbAction::Submit(text) => {
            if app.status == Status::Streaming {
                // Ignore submits mid-turn.
                KeyAction::Nothing
            } else {
                let t = text.trim();
                if t.is_empty() {
                    KeyAction::Nothing
                } else if t == "/quit" || t == "/exit" {
                    KeyAction::Exit
                } else if t == "/compact" {
                    KeyAction::Compact
                } else {
                    KeyAction::StartTurn(t.to_string())
                }
            }
        }
    };
    sync_palette(app);
    out
}

/// Sync palette state with the input buffer: open if first line
/// starts with `/`, else close. Also refreshes the filter query.
fn sync_palette(app: &mut App) {
    let full = app.input.as_string();
    let first_line = full.lines().next().unwrap_or("");
    if first_line.starts_with('/') {
        if app.palette.is_none() {
            app.palette = Some(MenuState::new(slash_items()));
        }
        if let Some(m) = app.palette.as_mut() {
            let filter = first_line.strip_prefix('/').unwrap_or("").to_string();
            m.set_filter(filter);
        }
    } else if app.palette.is_some() {
        app.palette = None;
    }
}

fn dispatch_slash(cmd: SlashCmd) -> KeyAction {
    match cmd {
        SlashCmd::Compact => KeyAction::Compact,
        SlashCmd::Quit => KeyAction::Exit,
    }
}

// ─────────────────────────────────────────────────────────────────────
// Main event loop
// ─────────────────────────────────────────────────────────────────────

async fn run_app(
    term: &mut Term,
    app: &mut App,
    agent_slot: Arc<Mutex<Option<Agent>>>,
) -> Result<i32> {
    let mut key_events = EventStream::new();
    let mut ag_rx: Option<mpsc::Receiver<AgentEvent>> = None;
    let mut turn_task: Option<tokio::task::JoinHandle<Result<String, String>>> = None;
    let mut cancel: Option<CancellationToken> = None;

    // Initial dock draw.
    term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;

    loop {
        if app.should_exit {
            return Ok(0);
        }

        tokio::select! {
            key = key_events.next() => {
                match key {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        let action = interpret_key(app, k);
                        handle_action(action, app, term, &agent_slot,
                                      &mut ag_rx, &mut cancel, &mut turn_task).await?;
                    }
                    Some(Ok(Event::Paste(s))) => {
                        app.input.insert_str(&s);
                        sync_palette(app);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {}
                    None => return Ok(0),
                }
                term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;
            }
            maybe_ev = recv_optional(&mut ag_rx) => {
                match maybe_ev {
                    Some(ev) => {
                        on_agent_event(term, app, ev)?;
                        term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;
                    }
                    None => {
                        // Turn done. Flush buffers + any orphan pending bar
                        // (tool that didn't get to emit its result marker,
                        // e.g. turn was cancelled).
                        flush_stream_buf(term, app)?;
                        flush_thinking_buf(term, app)?;
                        if let Some(pending) = app.pending_tool_call.take() {
                            let yellow = Style::default()
                                .bg(Color::Yellow)
                                .fg(Color::Black)
                                .add_modifier(Modifier::BOLD);
                            let dim = Style::default()
                                .bg(Color::Yellow)
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC);
                            insert_line_bg(term, Line::from(vec![
                                Span::styled(pending.leading, yellow),
                                Span::styled(pending.body, yellow),
                                Span::styled("  (interrupted)", dim),
                            ]), Some(yellow))?;
                        }
                        ag_rx = None;
                        cancel = None;
                        app.status = Status::Idle;
                        app.status_note = None;
                        refresh_status(app, &agent_slot).await;
                        if let Some(t) = turn_task.take() {
                            match t.await {
                                Ok(Ok(_)) => {}
                                Ok(Err(e)) => {
                                    insert_line(term, Line::from(vec![
                                        Span::styled(format!("[error: {e}]"),
                                            Style::default().fg(Color::Red)),
                                    ]))?;
                                }
                                Err(join_err) => {
                                    insert_line(term, Line::from(vec![
                                        Span::styled(format!("[turn panic: {join_err}]"),
                                            Style::default().fg(Color::Red)),
                                    ]))?;
                                }
                            }
                        }
                        // Blank separator between turns.
                        insert_line(term, Line::from(""))?;
                        term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;
                    }
                }
            }
        }
    }
}

async fn handle_action(
    action: KeyAction,
    app: &mut App,
    term: &mut Term,
    agent_slot: &Arc<Mutex<Option<Agent>>>,
    ag_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    cancel: &mut Option<CancellationToken>,
    turn_task: &mut Option<tokio::task::JoinHandle<Result<String, String>>>,
) -> Result<()> {
    match action {
        KeyAction::Nothing => {}
        KeyAction::Exit => {
            app.should_exit = true;
        }
        KeyAction::CancelTurn => {
            if let Some(ct) = cancel.as_ref() {
                ct.cancel();
                app.status_note = Some("cancelling…".into());
            }
        }
        KeyAction::Compact => {
            app.status_note = Some("compacting…".into());
            term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;
            let mut g = agent_slot.lock().await;
            if let Some(a) = g.as_mut() {
                let before = a.context.estimate_chars();
                a.compact_now().await;
                let after = a.context.estimate_chars();
                insert_line(term, Line::from(vec![
                    Span::styled(
                        format!("[compacted: {before} → {after} chars]"),
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                    ),
                ]))?;
                app.context_chars = after;
                app.usage = a.usage_total.clone();
                app.turn_count = a.turn_count;
            }
            app.status_note = None;
        }
        KeyAction::StartTurn(msg) => {
            if app.status == Status::Streaming {
                return Ok(());
            }
            // Echo user message into scrollback, PI-style: blank line +
            // full-width gray "card" (3 rows: pad, content, pad) + blank.
            // Use 256-color palette (Indexed 238) — visible on every
            // terminal including tmux without truecolor passthrough.
            // Truecolor (RGB) fails silently on tmux-256color, which is
            // the default in most setups.
            let user_bg = Style::default()
                .bg(Color::Indexed(238))
                .fg(Color::Indexed(255));
            insert_line(term, Line::from(""))?;
            insert_line_bg(term, Line::from(vec![Span::styled("", user_bg)]), Some(user_bg))?;
            for line in msg.lines() {
                insert_line_bg(term, Line::from(vec![
                    Span::styled("  ", user_bg),
                    Span::styled(line.to_string(), user_bg),
                ]), Some(user_bg))?;
            }
            insert_line_bg(term, Line::from(vec![Span::styled("", user_bg)]), Some(user_bg))?;
            insert_line(term, Line::from(""))?;
            let (tx, rx) = mpsc::channel::<AgentEvent>(64);
            let ct = CancellationToken::new();
            let ct_task = ct.clone();
            let agent_task_slot = agent_slot.clone();
            let task = tokio::spawn(async move {
                let mut guard = agent_task_slot.lock().await;
                let mut a = guard.take().ok_or_else(|| "agent slot empty".to_string())?;
                drop(guard);
                let result = a.run_turn(&msg, &tx, Some(ct_task)).await;
                let mut guard = agent_task_slot.lock().await;
                *guard = Some(a);
                result.map_err(|e| e.to_string())
            });
            *ag_rx = Some(rx);
            *cancel = Some(ct);
            *turn_task = Some(task);
            app.status = Status::Streaming;
        }
    }
    Ok(())
}

async fn recv_optional(rx: &mut Option<mpsc::Receiver<AgentEvent>>) -> Option<AgentEvent> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn refresh_status(app: &mut App, agent: &Arc<Mutex<Option<Agent>>>) {
    let g = agent.lock().await;
    if let Some(a) = g.as_ref() {
        app.usage = a.usage_total.clone();
        app.context_chars = a.context.estimate_chars();
        app.turn_count = a.turn_count;
        app.cwd = a.cwd.clone();
    }
}

// ─────────────────────────────────────────────────────────────────────
// Agent event → scrollback insertion
// ─────────────────────────────────────────────────────────────────────

fn on_agent_event(term: &mut Term, app: &mut App, ev: AgentEvent) -> Result<()> {
    match ev {
        AgentEvent::TextDelta { text, .. } => {
            // Detect the synthetic tool-result marker: `\n[tool → N bytes]\n`.
            // Render collapsed (first N lines only) + dim italic PI-style.
            if is_tool_result_marker(&text) {
                flush_stream_buf(term, app)?;
                render_tool_result_marker(term, app, &text)?;
                return Ok(());
            }
            // Real assistant text → flush any pending thinking, then
            // accumulate + emit full lines.
            flush_thinking_buf(term, app)?;
            app.stream_buf.push_str(&text);
            while let Some(nl) = app.stream_buf.find('\n') {
                let line: String = app.stream_buf.drain(..=nl).collect();
                let trimmed = line.trim_end_matches('\n');
                insert_line(term, Line::from(vec![
                    Span::styled(trimmed.to_string(), Style::default().fg(Color::White)),
                ]))?;
            }
        }
        AgentEvent::ThinkingDelta { text, .. } => {
            // Buffer thinking similarly to text — flush on newlines.
            app.thinking_buf.push_str(&text);
            while let Some(nl) = app.thinking_buf.find('\n') {
                let line: String = app.thinking_buf.drain(..=nl).collect();
                let trimmed = line.trim_end_matches('\n');
                if !trimmed.is_empty() {
                    insert_line(term, Line::from(vec![
                        Span::styled(
                            trimmed.to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]))?;
                }
            }
        }
        AgentEvent::ToolCall { call, .. } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            // Don't draw the bar yet — we don't know if it succeeded.
            // Stash it; the tool-result marker will trigger the draw
            // with a green (success) or red (failure) bg.
            let (leading, body) = tool_call_bar_text(&call.name, &call.arguments);
            app.pending_tool_call = Some(PendingBar { leading, body });
        }
        AgentEvent::Error { error } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            insert_line(term, Line::from(vec![
                Span::styled(format!("[error: {error}]"), Style::default().fg(Color::Red)),
            ]))?;
        }
        AgentEvent::Done { .. } | AgentEvent::Start { .. } => {}
    }
    Ok(())
}

/// Recognize `\n[<tool> <sep> <N> bytes  Took <t>]\n` markers
/// emitted by `Agent::execute_tool_calls`.
fn is_tool_result_marker(text: &str) -> bool {
    text.starts_with("\n[")
        && (text.contains(" → ") || text.contains(" ✗ "))
        && text.contains(" bytes")
        && text.ends_with("]\n")
}

/// True when the marker uses the `✗` separator (error path).
fn tool_marker_is_error(text: &str) -> bool {
    text.contains(" ✗ ")
}

/// If any partial (no trailing newline) assistant text sits in the
/// stream buffer, insert it as a line and clear.
fn flush_stream_buf(term: &mut Term, app: &mut App) -> Result<()> {
    if !app.stream_buf.is_empty() {
        let text = std::mem::take(&mut app.stream_buf);
        insert_line(term, Line::from(vec![
            Span::styled(text, Style::default().fg(Color::White)),
        ]))?;
    }
    Ok(())
}

/// Flush any pending thinking-buffer content as italic dim lines.
fn flush_thinking_buf(term: &mut Term, app: &mut App) -> Result<()> {
    if !app.thinking_buf.is_empty() {
        let text = std::mem::take(&mut app.thinking_buf);
        insert_line(term, Line::from(vec![
            Span::styled(
                text,
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ),
        ]))?;
    }
    Ok(())
}

/// Build the two parts of the tool-call bar:
///   leading (fixed short chip like ` $ ` or ` read `)
///   body    (the meat: command / path / pattern)
///
/// bash mimics a shell prompt: ` $ ls -la /tmp`.
/// Other tools show ` <name> <arg>`: ` read /etc/hostname`.
fn tool_call_bar_text(name: &str, args: &serde_json::Value) -> (String, String) {
    let name_lc = name.to_ascii_lowercase();
    match name_lc.as_str() {
        "bash" => {
            let cmd = args
                .get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(String::new);
            (" $ ".to_string(), truncate_bar_body(&cmd))
        }
        "read" | "write" | "edit" | "ls" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (format!(" {} ", name_lc), truncate_bar_body(path))
        }
        "grep" | "find" => {
            let pat = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            (format!(" {} ", name_lc), truncate_bar_body(&format!("'{pat}'")))
        }
        _ => {
            let s = args.to_string();
            (format!(" {} ", name_lc), truncate_bar_body(&s))
        }
    }
}

fn truncate_bar_body(s: &str) -> String {
    const MAX: usize = 120;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let taken: String = s.chars().take(MAX).collect();
        format!("{taken}…")
    }
}

/// Emit the whole tool block: blank line, colored bar, summary line
/// (dim italic with duration), blank line. Bg is green on success,
/// red on error. Matches PI's tool card layout (see img/PI_talk02.jpg).
fn render_tool_result_marker(term: &mut Term, app: &mut App, text: &str) -> Result<()> {
    let is_err = tool_marker_is_error(text);
    let (bar_bg, bar_fg) = if is_err {
        (Color::Red, Color::White)
    } else {
        (Color::Green, Color::Indexed(255))
    };
    let bar_style = Style::default().bg(bar_bg).fg(bar_fg).add_modifier(Modifier::BOLD);
    let dim_bar_style = Style::default()
        .bg(bar_bg)
        .fg(Color::Indexed(250))
        .add_modifier(Modifier::ITALIC);

    // Breathing room above.
    insert_line(term, Line::from(""))?;

    // Draw the bar now that we know the outcome.
    if let Some(pending) = app.pending_tool_call.take() {
        insert_line_bg(term, Line::from(vec![
            Span::styled(pending.leading, bar_style),
            Span::styled(pending.body, bar_style),
            Span::styled("  (ctrl+o to expand)", dim_bar_style),
        ]), Some(bar_style))?;
    }

    // Then the result summary line (dim italic, or dim red for errors).
    let cleaned = text.trim();
    let summary_style = if is_err {
        Style::default().fg(Color::Red).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
    };
    insert_line(term, Line::from(vec![
        Span::styled(cleaned.to_string(), summary_style),
    ]))?;

    // Breathing room below.
    insert_line(term, Line::from(""))?;
    Ok(())
}

/// Insert one line above the viewport into scrollback.
fn insert_line(term: &mut Term, line: Line<'_>) -> Result<()> {
    insert_line_bg(term, line, None)
}

/// Insert one line with an optional full-row background style. If
/// `bg_style` is Some, unset cells beyond the text get that style
/// (PI's "user echo" gray bar effect).
fn insert_line_bg(term: &mut Term, line: Line<'_>, bg_style: Option<Style>) -> Result<()> {
    let spans: Vec<(String, Style)> = line
        .spans
        .iter()
        .map(|s| (s.content.to_string(), s.style))
        .collect();
    term.insert_before(1, |buf: &mut Buffer| {
        let mut col: u16 = 0;
        let row: u16 = 0;
        for (text, style) in &spans {
            if col >= buf.area.width {
                break;
            }
            let remaining = buf.area.width.saturating_sub(col);
            let take = text.chars().take(remaining as usize).collect::<String>();
            for (i, ch) in take.chars().enumerate() {
                let x = col + i as u16;
                if x < buf.area.width {
                    if let Some(cell) = buf.cell_mut((x, row)) {
                        cell.set_char(ch).set_style(*style);
                    }
                }
            }
            col += take.chars().count() as u16;
        }
        // Fill the rest of the row with bg_style (space char + style).
        if let Some(bg) = bg_style {
            for x in col..buf.area.width {
                if let Some(cell) = buf.cell_mut((x, row)) {
                    cell.set_char(' ').set_style(bg);
                }
            }
        }
    })?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Dock rendering (5-row region at bottom)
// ─────────────────────────────────────────────────────────────────────

fn draw_dock(buf: &mut Buffer, area: Rect, app: &App) {
    // Layout: 5 rows palette + 3 rows input + 2 rows footer = 10 total.
    // When palette closed, the top 5 rows are blank.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // palette area (blank when closed)
            Constraint::Length(3), // input box (top border + content + bottom border)
            Constraint::Length(1), // cwd + branch
            Constraint::Length(1), // stats
        ])
        .split(area);

    // ── Palette (dropdown menu) ──────────────────────────────────
    if let Some(m) = &app.palette {
        draw_palette(buf, chunks[0], m);
    }

    // ── Input box ────────────────────────────────────────────────
    // TextBuffer can be multi-line but for MVP we render only the row
    // containing the cursor + trim overflow. The bordered block is 3
    // rows total (top ─, content, bottom ─).
    let cursor_row = app.input.cursor().0;
    let cursor_col = app.input.cursor().1;
    let display_row = app.input.lines().get(cursor_row).cloned().unwrap_or_default();
    // Render prefix + text-so-far + cursor block + text-after.
    let (pre, post) = split_at_col(&display_row, cursor_col);
    let mut input_spans: Vec<Span> = vec![
        Span::styled("> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(pre.to_string()),
        // Reverse-video block as cursor; either the char under it or space.
        {
            let cursor_char: String = post.chars().next().map(|c| c.to_string()).unwrap_or_else(|| " ".into());
            Span::styled(cursor_char, Style::default().add_modifier(Modifier::REVERSED))
        },
        Span::raw(post.chars().skip(1).collect::<String>()),
    ];
    if let Some(note) = &app.status_note {
        input_spans.push(Span::raw("  "));
        input_spans.push(Span::styled(
            format!("({note})"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
        ));
    }
    // If multi-line, show "(+N more lines)" hint on the right.
    if app.input.row_count() > 1 {
        input_spans.push(Span::raw("  "));
        input_spans.push(Span::styled(
            format!("(line {}/{})", cursor_row + 1, app.input.row_count()),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ));
    }
    let input_para = Paragraph::new(Line::from(input_spans))
        .block(Block::default().borders(Borders::TOP | Borders::BOTTOM));
    input_para.render(chunks[1], buf);

    // Line 1 of footer: cwd + branch + session
    let cwd_str = crate::render::status_line::cwd_display(&app.cwd);
    let branch = crate::render::status_line::git_branch(&app.cwd);
    let sid_short = &app.session_id[..8.min(app.session_id.len())];
    let mut l1 = vec![Span::styled(cwd_str, Style::default().fg(Color::Cyan))];
    if !branch.is_empty() {
        l1.push(Span::raw(" ("));
        l1.push(Span::styled(branch, Style::default().fg(Color::Magenta)));
        l1.push(Span::raw(")"));
    }
    l1.push(Span::styled(
        format!("  · session {}", sid_short),
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(Line::from(l1)).render(chunks[2], buf);

    // Line 2: context% + tokens + cost + model  (or streaming note)
    let mut l2: Vec<Span> = Vec::new();
    if let Some(pct) = crate::render::status_line::context_percent(&app.model, app.context_chars) {
        let color = match crate::render::status_line::context_color(pct) {
            "red" => Color::Red,
            "yellow" => Color::Yellow,
            _ => Color::Green,
        };
        l2.push(Span::styled(format!("{pct}% context"), Style::default().fg(color)));
        l2.push(Span::raw(" · "));
    }
    l2.push(Span::styled(
        crate::render::status_line::tokens_summary(&app.usage),
        Style::default().fg(Color::White),
    ));
    let cost = crate::render::status_line::cost_string(&app.model, &app.usage);
    if !cost.is_empty() {
        l2.push(Span::raw(" · "));
        l2.push(Span::styled(cost, Style::default().fg(Color::Green)));
    }
    l2.push(Span::raw(" · "));
    l2.push(Span::styled(app.model.clone(), Style::default().fg(Color::LightBlue)));
    if app.status == Status::Streaming {
        l2.push(Span::raw("  · "));
        l2.push(Span::styled(
            "streaming",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
        ));
    }
    Paragraph::new(Line::from(l2)).render(chunks[3], buf);
}

/// Split a string at the given byte position, returning (pre, post).
fn split_at_col(s: &str, col: usize) -> (&str, &str) {
    let clamped = col.min(s.len());
    s.split_at(clamped)
}

/// Render the slash-command palette in the given rect. Selected item
/// gets green `→` + bold label; others get grey.
fn draw_palette(buf: &mut Buffer, area: Rect, m: &MenuState<SlashCmd>) {
    let vis = m.visible();
    let sel = m.cursor();
    if vis.is_empty() {
        let msg = Line::from(vec![Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )]);
        Paragraph::new(msg).render(area, buf);
        return;
    }
    let max_rows = area.height as usize;
    // Center the window around the cursor if items overflow.
    let start = if vis.len() <= max_rows {
        0
    } else if sel < max_rows / 2 {
        0
    } else if sel >= vis.len() - (max_rows - max_rows / 2) {
        vis.len().saturating_sub(max_rows)
    } else {
        sel - max_rows / 2
    };
    let end = (start + max_rows).min(vis.len());
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in vis[start..end].iter().enumerate() {
        let absolute = start + i;
        let is_sel = absolute == sel;
        let arrow = if is_sel { "→ " } else { "  " };
        let label_style = if is_sel {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let desc_style = Style::default().fg(Color::DarkGray);
        lines.push(Line::from(vec![
            Span::styled(arrow, label_style),
            Span::styled(format!("{:<12}", item.label), label_style),
            Span::styled(item.description.clone(), desc_style),
        ]));
    }
    // Overflow indicator on last row if more items exist below.
    if end < vis.len() {
        let count = vis.len() - end;
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled(
                format!("   (+{count} more)"),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ));
        }
    }
    Paragraph::new(lines).render(area, buf);
}

// ─────────────────────────────────────────────────────────────────────
// Tests (unit tests for the pure key mapping)
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mkapp() -> App {
        App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"))
    }

    fn seed_input(app: &mut App, text: &str) {
        app.input.insert_str(text);
        sync_palette(app);
    }

    #[test]
    fn typing_appends() {
        let mut app = mkapp();
        assert!(matches!(
            interpret_key(&mut app, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            KeyAction::Nothing
        ));
        assert_eq!(app.input.as_string(), "a");
    }

    #[test]
    fn backspace_pops() {
        let mut app = mkapp();
        seed_input(&mut app, "abc");
        interpret_key(&mut app, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input.as_string(), "ab");
    }

    #[test]
    fn enter_submits() {
        let mut app = mkapp();
        seed_input(&mut app, "hi");
        match interpret_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            KeyAction::StartTurn(s) => assert_eq!(s, "hi"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn enter_empty_is_noop() {
        let mut app = mkapp();
        assert!(matches!(
            interpret_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::Nothing
        ));
    }

    #[test]
    fn slash_quit_opens_palette_then_enter_exits() {
        let mut app = mkapp();
        seed_input(&mut app, "/quit");
        assert!(app.palette.is_some(), "palette should open on /");
        // Enter picks the selected item; "quit" matches only /quit
        // so it's the selected item.
        assert!(matches!(
            interpret_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::Exit
        ));
    }

    #[test]
    fn slash_compact_picks_command() {
        let mut app = mkapp();
        seed_input(&mut app, "/compact");
        assert!(matches!(
            interpret_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::Compact
        ));
    }

    #[test]
    fn palette_esc_closes_and_clears_input() {
        let mut app = mkapp();
        seed_input(&mut app, "/co");
        assert!(app.palette.is_some());
        interpret_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.palette.is_none());
        assert!(app.input.is_empty());
    }

    #[test]
    fn palette_filters_by_typing() {
        let mut app = mkapp();
        seed_input(&mut app, "/co");
        let m = app.palette.as_ref().unwrap();
        assert!(m.visible().iter().any(|it| it.label == "/compact"));
    }

    #[test]
    fn ctrl_d_exits() {
        let mut app = mkapp();
        assert!(matches!(
            interpret_key(&mut app, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            KeyAction::Exit
        ));
    }

    #[test]
    fn ctrl_c_streaming_cancels() {
        let mut app = mkapp();
        app.status = Status::Streaming;
        assert!(matches!(
            interpret_key(&mut app, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::CancelTurn
        ));
    }

    #[test]
    fn ctrl_c_idle_exits() {
        let mut app = mkapp();
        assert!(matches!(
            interpret_key(&mut app, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::Exit
        ));
    }

    #[test]
    fn tool_result_marker_detection() {
        assert!(is_tool_result_marker("\n[bash → 97 bytes  Took 0.3s]\n"));
        assert!(is_tool_result_marker("\n[read → 12345 bytes  Took 45ms]\n"));
        assert!(is_tool_result_marker("\n[bash ✗ 32 bytes  Took 12ms]\n"));
        assert!(!is_tool_result_marker("bash output"));
        assert!(!is_tool_result_marker("\n[bash starting]"));
    }

    #[test]
    fn error_marker_flagged() {
        assert!(tool_marker_is_error("\n[bash ✗ 32 bytes  Took 12ms]\n"));
        assert!(!tool_marker_is_error("\n[bash → 32 bytes  Took 12ms]\n"));
    }
}

//! Full ratatui-based interactive TUI (opt-in via `--tui`).
//!
//! Layout (top → bottom): title / body (scrollable) / status / input.
//! Streams `AgentEvent`s into rendered lines; tool calls render as
//! one-line panels.
//!
//! Keys:
//!   Enter     — submit input
//!   Backspace — delete char
//!   Ctrl-C    — cancel the streaming turn (context preserved)
//!   Ctrl-D    — exit
//!   PgUp/PgDn — scroll conversation
//!   /quit     — exit
//!   /compact  — force context compaction
//!
//! v0.6+ MVP: no expanded tool-panel view, no syntax highlighting, no
//! multi-line input. Those are v0.7 polish.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
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

/// Entry point for `--tui` mode.
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

    // Fire session_start once.
    {
        let g = agent_slot.lock().await;
        if let Some(a) = g.as_ref() {
            a.fire_session_start().await;
        }
    }

    let mut terminal = setup_terminal()?;
    let mut app = App::new(header.id.to_string(), model.to_string(), cwd.clone());
    let result = run_app(&mut terminal, &mut app, agent_slot.clone()).await;

    // Restore terminal even on error.
    let _ = teardown_terminal(&mut terminal);

    // Fire session_end.
    {
        let g = agent_slot.lock().await;
        if let Some(a) = g.as_ref() {
            a.fire_session_end().await;
        }
    }

    result
}

// ─────────────────────────────────────────────────────────────────────
// Terminal setup / teardown
// ─────────────────────────────────────────────────────────────────────

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide,
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn teardown_terminal(term: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        cursor::Show,
    )?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// App state (visual only — turn plumbing lives as locals in run_app
// to keep borrow-checker happy across tokio::select! arms)
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    Streaming,
}

struct App {
    /// Rendered scrollback. Each `Line` is one logical output line.
    lines: Vec<Line<'static>>,
    /// True if the last line was assistant text and should accept
    /// subsequent TextDelta events as inline appends.
    last_line_is_assistant: bool,
    scroll: u16,
    input: String,
    status: Status,
    session_id: String,
    model: String,
    should_exit: bool,
    /// Snapshot of the Agent's cwd (used by the status footer for
    /// the pwd + git-branch line). Updated whenever we can lock the
    /// slot.
    cwd: PathBuf,
    /// Cumulative usage across all turns in this session.
    usage: crate::event::Usage,
    /// Current context size in characters (rough proxy for tokens).
    context_chars: usize,
    /// How many turns have completed. Displayed in the status line.
    turn_count: u32,
}

impl App {
    fn new(session_id: String, model: String, cwd: PathBuf) -> Self {
        let mut app = Self {
            lines: Vec::new(),
            last_line_is_assistant: false,
            scroll: 0,
            input: String::new(),
            status: Status::Idle,
            session_id,
            model,
            should_exit: false,
            cwd,
            usage: crate::event::Usage::default(),
            context_chars: 0,
            turn_count: 0,
        };
        app.push_system_line("nanopi TUI. Enter to submit, Ctrl-C cancels a turn, Ctrl-D exits.");
        app
    }

    fn push_system_line(&mut self, s: impl Into<String>) {
        let s: String = s.into();
        self.lines.push(Line::from(Span::styled(
            s,
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )));
        self.last_line_is_assistant = false;
    }

    fn push_user_line(&mut self, s: &str) {
        let line = Line::from(vec![
            Span::styled(
                "> ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(s.to_string()),
        ]);
        self.lines.push(line);
        self.last_line_is_assistant = false;
    }

    fn push_assistant_delta(&mut self, s: &str) {
        if self.last_line_is_assistant {
            if let Some(last) = self.lines.last_mut() {
                last.spans.push(Span::raw(s.to_string()));
                return;
            }
        }
        self.lines
            .push(Line::from(vec![Span::raw(s.to_string())]));
        self.last_line_is_assistant = true;
    }

    fn push_tool_line(&mut self, name: &str, args_preview: &str, state: &str) {
        let style = match state {
            "done" => Style::default().fg(Color::Green),
            "err" => Style::default().fg(Color::Red),
            _ => Style::default().fg(Color::Yellow),
        };
        let line = Line::from(vec![
            Span::styled(format!("[{state}] "), style),
            Span::styled(
                name.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                args_preview.to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        self.lines.push(line);
        self.last_line_is_assistant = false;
    }

    fn end_turn(&mut self) {
        self.lines.push(Line::from(""));
        self.last_line_is_assistant = false;
    }
}

// ─────────────────────────────────────────────────────────────────────
// Actions that handle_key can request from the run_app loop
// ─────────────────────────────────────────────────────────────────────

enum KeyAction {
    Nothing,
    StartTurn(String),
    CancelTurn,
    Exit,
    Compact,
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

    loop {
        term.draw(|f| ui(f, app))?;
        if app.should_exit {
            return Ok(0);
        }

        tokio::select! {
            key = key_events.next() => {
                match key {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        let action = interpret_key(app, k);
                        match action {
                            KeyAction::Nothing => {}
                            KeyAction::Exit => {
                                app.should_exit = true;
                            }
                            KeyAction::CancelTurn => {
                                if let Some(ct) = cancel.as_ref() {
                                    ct.cancel();
                                    app.push_system_line("[cancelling…]");
                                }
                            }
                            KeyAction::Compact => {
                                let mut g = agent_slot.lock().await;
                                if let Some(a) = g.as_mut() {
                                    let before = a.context.estimate_chars();
                                    a.compact_now().await;
                                    let after = a.context.estimate_chars();
                                    app.push_system_line(format!(
                                        "[compacted: {before} → {after} chars]"
                                    ));
                                    app.context_chars = after;
                                    app.usage = a.usage_total.clone();
                                    app.turn_count = a.turn_count;
                                }
                            }
                            KeyAction::StartTurn(msg) => {
                                if app.status == Status::Streaming {
                                    // Shouldn't happen (interpret_key gates it)
                                    // but be defensive.
                                    continue;
                                }
                                app.push_user_line(&msg);
                                let (tx, rx) = mpsc::channel::<AgentEvent>(64);
                                let ct = CancellationToken::new();
                                let ct_task = ct.clone();
                                let agent_task_slot = agent_slot.clone();
                                let task = tokio::spawn(async move {
                                    let mut guard = agent_task_slot.lock().await;
                                    let mut a = guard
                                        .take()
                                        .ok_or_else(|| "agent slot empty".to_string())?;
                                    drop(guard);
                                    let result = a.run_turn(&msg, &tx, Some(ct_task)).await;
                                    let mut guard = agent_task_slot.lock().await;
                                    *guard = Some(a);
                                    result.map_err(|e| e.to_string())
                                });
                                ag_rx = Some(rx);
                                cancel = Some(ct);
                                turn_task = Some(task);
                                app.status = Status::Streaming;
                            }
                        }
                    }
                    Some(Ok(Event::Paste(s))) => {
                        app.input.push_str(&s);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        app.push_system_line(format!("[event error: {e}]"));
                    }
                    None => return Ok(0),
                }
            }
            maybe_ev = recv_optional(&mut ag_rx) => {
                match maybe_ev {
                    Some(ev) => on_agent_event(app, ev),
                    None => {
                        // Channel closed → turn task is done.
                        ag_rx = None;
                        cancel = None;
                        app.status = Status::Idle;
                        // Snapshot fresh Agent metrics into App for status
                        // footer rendering.
                        refresh_status(app, &agent_slot).await;
                        if let Some(t) = turn_task.take() {
                            match t.await {
                                Ok(Ok(_final_text)) => {}
                                Ok(Err(e)) => {
                                    app.push_system_line(format!("[turn error: {e}]"))
                                }
                                Err(join_err) => {
                                    app.push_system_line(format!("[turn panic: {join_err}]"))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Await the next event on `rx` if present, else pend forever. Keeps
/// the `select!` arm's borrow scoped to this async block.
async fn recv_optional(rx: &mut Option<mpsc::Receiver<AgentEvent>>) -> Option<AgentEvent> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Copy the Agent's live metrics into App fields so the status footer
/// can render without holding the agent lock during draw.
async fn refresh_status(app: &mut App, agent: &Arc<Mutex<Option<Agent>>>) {
    let g = agent.lock().await;
    if let Some(a) = g.as_ref() {
        app.usage = a.usage_total.clone();
        app.context_chars = a.context.estimate_chars();
        app.turn_count = a.turn_count;
        app.cwd = a.cwd.clone();
    }
}

/// Pure key → action mapping. Also mutates the input buffer for
/// backspace/typing. Returns a top-level action for the loop to enact.
fn interpret_key(app: &mut App, k: KeyEvent) -> KeyAction {
    // Ctrl-C: cancel current turn if one is running; otherwise exit.
    // (Matches the default interactive-mode semantics.)
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        return if app.status == Status::Streaming {
            KeyAction::CancelTurn
        } else {
            KeyAction::Exit
        };
    }
    // Ctrl-D: exit.
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('d') {
        return KeyAction::Exit;
    }
    match k.code {
        KeyCode::Enter => {
            if app.status == Status::Streaming {
                return KeyAction::Nothing;
            }
            let msg = std::mem::take(&mut app.input);
            let trimmed = msg.trim().to_string();
            if trimmed.is_empty() {
                return KeyAction::Nothing;
            }
            if trimmed == "/quit" || trimmed == "/exit" {
                return KeyAction::Exit;
            }
            if trimmed == "/compact" {
                return KeyAction::Compact;
            }
            KeyAction::StartTurn(trimmed)
        }
        KeyCode::Backspace => {
            app.input.pop();
            KeyAction::Nothing
        }
        KeyCode::PageUp => {
            app.scroll = app.scroll.saturating_sub(10);
            KeyAction::Nothing
        }
        KeyCode::PageDown => {
            app.scroll = app.scroll.saturating_add(10);
            KeyAction::Nothing
        }
        KeyCode::Home => {
            app.scroll = 0;
            KeyAction::Nothing
        }
        KeyCode::End => {
            app.scroll = u16::MAX;
            KeyAction::Nothing
        }
        KeyCode::Char(c) => {
            app.input.push(c);
            KeyAction::Nothing
        }
        _ => KeyAction::Nothing,
    }
}

fn on_agent_event(app: &mut App, ev: AgentEvent) {
    match ev {
        AgentEvent::TextDelta { text, .. } => app.push_assistant_delta(&text),
        AgentEvent::ToolCall { call, .. } => {
            let args_str = call.arguments.to_string();
            let preview = if args_str.len() > 60 {
                format!("{}…", &args_str[..60])
            } else {
                args_str
            };
            // Tool execution and its result are internal to Agent — the
            // event stream doesn't include a ToolResult. So we can only
            // show that a tool was requested; not its outcome.
            app.push_tool_line(&call.name, &preview, "run");
        }
        AgentEvent::Error { error } => {
            app.push_system_line(format!("[error: {error}]"));
        }
        AgentEvent::Done { .. } => app.end_turn(),
        _ => {}
    }
}

// ─────────────────────────────────────────────────────────────────────
// Rendering
// ─────────────────────────────────────────────────────────────────────

fn ui(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Min(3),    // body (grows)
            Constraint::Length(3), // status footer (3 lines, PI-style)
            Constraint::Length(3), // input (bordered)
        ])
        .split(f.area());

    // Title — session id + turn count.
    let sid_short = &app.session_id[..8.min(app.session_id.len())];
    let title = Paragraph::new(Line::from(vec![
        Span::styled("nanopi ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "session {} · turn {}",
            sid_short, app.turn_count
        )),
    ]))
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(title, chunks[0]);

    // Body.
    let body_lines: Vec<Line> = app.lines.iter().cloned().collect();
    let body = Paragraph::new(body_lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0));
    f.render_widget(body, chunks[1]);

    // ── Status footer (3 lines, PI-style) ──────────────────────────
    let footer_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 3])
        .split(chunks[2]);
    let footer_style = Style::default().bg(Color::Reset).fg(Color::DarkGray);

    // Line 1: cwd + git branch + short session id (right-aligned)
    let cwd_str = crate::render::status_line::cwd_display(&app.cwd);
    let branch = crate::render::status_line::git_branch(&app.cwd);
    let mut left = vec![Span::styled(cwd_str, Style::default().fg(Color::Cyan))];
    if !branch.is_empty() {
        left.push(Span::raw(" ("));
        left.push(Span::styled(branch, Style::default().fg(Color::Magenta)));
        left.push(Span::raw(")"));
    }
    let l1 = Paragraph::new(Line::from(left)).style(footer_style);
    f.render_widget(l1, footer_chunks[0]);

    // Line 2: tokens + cost + model
    let tokens_str = crate::render::status_line::tokens_summary(&app.usage);
    let cost_str = crate::render::status_line::cost_string(&app.model, &app.usage);
    let mut mid = vec![
        Span::styled(tokens_str, Style::default().fg(Color::White)),
    ];
    if !cost_str.is_empty() {
        mid.push(Span::raw("  "));
        mid.push(Span::styled(cost_str, Style::default().fg(Color::Green)));
    }
    mid.push(Span::raw("  "));
    mid.push(Span::styled(
        app.model.clone(),
        Style::default().fg(Color::LightBlue),
    ));
    let l2 = Paragraph::new(Line::from(mid)).style(footer_style);
    f.render_widget(l2, footer_chunks[1]);

    // Line 3: context% + status hint
    let mut bottom = Vec::new();
    if let Some(pct) = crate::render::status_line::context_percent(
        &app.model,
        app.context_chars,
    ) {
        let color = match crate::render::status_line::context_color(pct) {
            "red" => Color::Red,
            "yellow" => Color::Yellow,
            _ => Color::Green,
        };
        bottom.push(Span::styled(
            format!("{pct}% context"),
            Style::default().fg(color),
        ));
        bottom.push(Span::raw("  "));
    }
    let hint = match &app.status {
        Status::Idle => "idle · Ctrl-C exit · PgUp/PgDn scroll · /compact",
        Status::Streaming => "streaming · Ctrl-C cancel",
    };
    bottom.push(Span::raw(hint));
    let l3 = Paragraph::new(Line::from(bottom)).style(footer_style);
    f.render_widget(l3, footer_chunks[2]);

    // Input.
    let input_text = format!("> {}", app.input);
    let input = Paragraph::new(input_text)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(input, chunks[3]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_key_typing_appends() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        let k = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::Nothing));
        assert_eq!(app.input, "a");
    }

    #[test]
    fn interpret_key_backspace_pops() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        app.input = "abc".into();
        let k = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::Nothing));
        assert_eq!(app.input, "ab");
    }

    #[test]
    fn interpret_key_enter_starts_turn() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        app.input = "hello".into();
        let k = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match interpret_key(&mut app, k) {
            KeyAction::StartTurn(msg) => assert_eq!(msg, "hello"),
            _ => panic!("expected StartTurn"),
        }
        assert_eq!(app.input, "");
    }

    #[test]
    fn interpret_key_enter_empty_is_noop() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        let k = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::Nothing));
    }

    #[test]
    fn interpret_key_slash_quit_exits() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        app.input = "/quit".into();
        let k = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::Exit));
    }

    #[test]
    fn interpret_key_slash_compact_returns_compact() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        app.input = "/compact".into();
        let k = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::Compact));
    }

    #[test]
    fn interpret_key_ctrl_d_exits() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        let k = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::Exit));
    }

    #[test]
    fn interpret_key_ctrl_c_while_streaming_cancels() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        app.status = Status::Streaming;
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::CancelTurn));
    }

    #[test]
    fn interpret_key_ctrl_c_while_idle_exits() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(interpret_key(&mut app, k), KeyAction::Exit));
    }

    #[test]
    fn push_assistant_delta_appends_to_prev() {
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        app.push_assistant_delta("hello ");
        app.push_assistant_delta("world");
        assert_eq!(app.lines.last().unwrap().spans.len(), 2);
    }

    #[test]
    fn on_agent_event_tool_call_appends_line() {
        use crate::event::ToolCall;
        let mut app = App::new("s".into(), "m".into(), std::path::PathBuf::from("/tmp"));
        let before = app.lines.len();
        on_agent_event(&mut app, AgentEvent::ToolCall {
            content_index: 0,
            call: ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        });
        assert_eq!(app.lines.len(), before + 1);
        let first = &app.lines.last().unwrap().spans[0];
        assert!(first.content.starts_with("[run]"), "got {:?}", first.content);
    }
}

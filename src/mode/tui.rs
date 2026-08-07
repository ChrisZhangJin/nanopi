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
use std::path::{Path, PathBuf};
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

/// Slash commands available in the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashCmd {
    Compact,
    Quit,
    Model,
    // Future: Login, Settings, Export, Import.
}

fn slash_items() -> Vec<MenuItem<SlashCmd>> {
    vec![
        MenuItem::new("/model",   "Switch to a different model", SlashCmd::Model),
        MenuItem::new("/compact", "Force context compaction",    SlashCmd::Compact),
        MenuItem::new("/quit",    "Exit the session",            SlashCmd::Quit),
        MenuItem::new("/exit",    "Exit the session",            SlashCmd::Quit),
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
        a.base_url = base_url.to_string();
        a.api_key = api_key.to_string();
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
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
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
    /// When Some, the model picker is open — takes priority over the
    /// slash palette. Payload is the target model id.
    model_picker: Option<MenuState<String>>,
    /// When Some, the fork picker (Esc double-tap) is open. Payload is
    /// `(source_session_path, entry_index_in_that_session)` — used to
    /// call `session::fork_session_at` on Enter. The source path may
    /// point at the current session OR any ancestor along the
    /// `parent_id` chain, so the user can revive tails that were cut
    /// off by earlier forks.
    fork_picker: Option<MenuState<(PathBuf, usize)>>,
    /// After picking a fork target: modal asking whether to summarize
    /// the cut-off branch. Three options match PI's UX:
    /// No summary / Summarize (default prompt) / Summarize with custom
    /// prompt. On Chosen, dispatch RunSummary.
    summary_prompt: Option<MenuState<SummaryChoice>>,
    /// A fork the user picked that is waiting on a summary decision.
    /// Carries the fork target and the cut-off entries so the summary
    /// LLM call doesn't need to re-read the session file.
    pending_fork: Option<PendingFork>,
    /// True after user picks "Summarize with custom prompt" — the next
    /// Enter on the input box will submit the typed text as the
    /// custom summarize instructions, not as a chat turn.
    capture_custom_prompt: bool,
    /// While a branch summary is being generated, hold the join handle
    /// so the main loop can poll for completion via `is_finished`.
    summarize_task: Option<tokio::task::JoinHandle<SummarizeOutcome>>,
    /// Timestamp of the last Esc keypress. Second Esc within 500ms on
    /// an empty editor opens the fork picker (PI's `doubleEscapeAction`
    /// = "fork"; see packages/coding-agent/src/modes/interactive/
    /// interactive-mode.ts:2644).
    last_esc_at: Option<std::time::Instant>,
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
    /// Markdown parser state (mostly: are we inside a ``` fenced block?).
    /// Kept across TextDelta lines within a single turn.
    md_state: crate::render::markdown::MdState,
    /// Most recent tool result — content, is_error flag (so the
    /// expanded rows land in the matching green/red zone), and an
    /// `expanded` bool that flips true on Ctrl+O so a second press
    /// doesn't duplicate the dump.
    last_tool_output: Option<LastTool>,
    /// A tool_call event just arrived; its bar is not yet drawn. On the
    /// matching tool-result marker we colour it green (success) or red
    /// (failure) using the marker's separator (`→` vs `✗`).
    pending_tool_call: Option<PendingBar>,
    /// When Some, a tool is currently executing — show a live BLUE
    /// "$ command  Elapsed X.Xs" strip inside the dock (PI-style
    /// working state, see img/PI_work_status.jpg). Cleared when
    /// ToolResult arrives.
    tool_started_at: Option<std::time::Instant>,
    /// When the assistant is streaming (or awaiting first token), show
    /// a `⣷ thinking (X.Xs)` strip in the dock. `None` = idle.
    turn_started_at: Option<std::time::Instant>,
    /// Optional short status shown to the right of the input box
    /// (e.g. "cancelling…", "compacting…").
    status_note: Option<String>,
}

impl App {
    fn new(session_id: String, model: String, cwd: PathBuf) -> Self {
        Self {
            input: TextBuffer::new(),
            palette: None,
            model_picker: None,
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
            tool_started_at: None,
            turn_started_at: None,
            status_note: None,
            md_state: crate::render::markdown::MdState::default(),
            last_tool_output: None,
            fork_picker: None,
            last_esc_at: None,
            summary_prompt: None,
            pending_fork: None,
            capture_custom_prompt: false,
            summarize_task: None,
        }
    }
}

/// Three options in the post-fork "Summarize branch?" modal — mirrors
/// PI's showBranchSummarySelector at
/// `packages/coding-agent/src/modes/interactive/interactive-mode.ts:4736-4779`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SummaryChoice {
    /// Fork immediately, no summary marker inserted.
    NoSummary,
    /// Ask the model to summarize the cut-off tail with the default
    /// system prompt (see `agent::branch_summary::DEFAULT_SUMMARIZER_SYSTEM`).
    DefaultSummary,
    /// Prompt user for custom instructions, then summarize with them.
    CustomSummary,
}

/// A fork target the user picked and confirmed but hasn't yet committed
/// because the summary modal is open. When summary decision arrives
/// (or capture-mode Enter fires), this is consumed by the fork
/// execution path.
#[derive(Debug, Clone)]
struct PendingFork {
    /// Which session file the picker rows point into (may be current
    /// session or an ancestor).
    source_path: PathBuf,
    /// Entry index within source_path at which to fork.
    target_entry_idx: usize,
    /// If target was a user Message, its text (for editor prefill).
    prefill: Option<String>,
    /// The tail from the CURRENT session that's about to be abandoned.
    /// Passed to `summarize_branch` when the user picks a summary
    /// option. Empty when a summary doesn't make sense (target is on
    /// an ancestor session — the whole current session becomes moot).
    cut_off: Vec<crate::session::SessionEntry>,
}

/// Result the async summarize task hands back to the main loop.
#[derive(Debug)]
enum SummarizeOutcome {
    /// Summarization succeeded → carry the summary text into the fork.
    Ok { summary: Option<String>, fork: PendingFork },
    /// Something went wrong assembling the request. Reported to user
    /// but fork proceeds without a summary.
    Err { error: String, fork: PendingFork },
}

/// A ToolCall event we've received but not yet drawn to scrollback.
/// We wait until the tool_result marker so we can pick the bar color
/// (green success / red failure).
#[derive(Debug, Clone)]
struct PendingBar {
    leading: String,
    body: String,
}

#[derive(Debug, Clone)]
struct LastTool {
    content: String,
    is_error: bool,
    expanded: bool,
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
    OpenModelPicker,
    SwapModel(String),
    ExpandLastTool,
    /// User double-tapped Esc on an empty editor — build the
    /// user-message picker from the current session.
    OpenForkPicker,
    /// User selected a fork target in the picker — but the fork isn't
    /// executed yet. If the target sits inside the current session and
    /// isn't the very last entry, we open the "Summarize branch?"
    /// modal first (PendingFork stashed on the App). Otherwise the
    /// fork runs immediately as if the user picked "No summary".
    ForkChosen(PathBuf, usize),
    /// User picked one of the 3 summary options. `Custom` transitions
    /// into capture mode, waiting for the user to type instructions.
    SummaryChosen(SummaryChoice),
    /// While capture_custom_prompt is on, the input box's Enter fires
    /// this instead of a chat turn: the typed text is the custom
    /// summarization prompt.
    RunCustomSummary(String),
    /// A queued summarize task finished; commit its output to a fork.
    /// Dispatched by the main loop's tick when `summarize_task.
    /// is_finished()`.
    SummaryFinished(SummarizeOutcome),
    /// User pressed Esc in the summary modal — abandon the pending
    /// fork, don't switch sessions.
    CancelPendingFork,
}

/// Dispatch one key event. Palette (if open) claims navigation keys
/// first; everything else flows through TextBuffer. After the key is
/// handled, the palette's open/closed state is re-synced against the
/// input buffer (open ⇔ first line starts with `/`).
fn interpret_key(app: &mut App, k: KeyEvent) -> KeyAction {
    // Ctrl+O — expand the last tool output (PI's `app.tools.expand`).
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('o') {
        return KeyAction::ExpandLastTool;
    }

    // ── Summary modal (highest priority once open) ──────────────────
    if app.summary_prompt.is_some() {
        let m = app.summary_prompt.as_mut().unwrap();
        match m.handle_key(k) {
            MenuAction::Chosen(choice) => {
                app.summary_prompt = None;
                return KeyAction::SummaryChosen(choice);
            }
            MenuAction::Cancel => {
                app.summary_prompt = None;
                return KeyAction::CancelPendingFork;
            }
            MenuAction::Nothing => return KeyAction::Nothing,
        }
    }
    // ── Fork picker (opens summary modal on Chosen) ─────────────────
    if app.fork_picker.is_some() {
        let m = app.fork_picker.as_mut().unwrap();
        match m.handle_key(k) {
            MenuAction::Chosen(payload) => {
                app.fork_picker = None;
                let (path, idx) = payload;
                return KeyAction::ForkChosen(path, idx);
            }
            MenuAction::Cancel => {
                app.fork_picker = None;
                return KeyAction::Nothing;
            }
            MenuAction::Nothing => return KeyAction::Nothing,
        }
    }

    // ── Esc double-tap → open fork picker ───────────────────────────
    // Only when the editor is empty, no menu open, not streaming, and
    // the previous Esc was < 500ms ago. First-Esc-on-empty just records
    // the time (no visible effect). Matches PI's `lastEscapeTime`
    // check (see interactive-mode.ts:2644).
    if k.code == KeyCode::Esc
        && k.modifiers.is_empty()
        && app.input.is_empty()
        && app.palette.is_none()
        && app.model_picker.is_none()
        && app.status != Status::Streaming
    {
        let now = std::time::Instant::now();
        let is_double = app
            .last_esc_at
            .map(|prev| now.duration_since(prev) < std::time::Duration::from_millis(500))
            .unwrap_or(false);
        if is_double {
            app.last_esc_at = None;
            return KeyAction::OpenForkPicker;
        }
        app.last_esc_at = Some(now);
        return KeyAction::Nothing;
    }

    // ── Model picker (highest priority when open) ───────────────────
    if app.model_picker.is_some() {
        let m = app.model_picker.as_mut().unwrap();
        match m.handle_key(k) {
            MenuAction::Chosen(model_id) => {
                app.model_picker = None;
                app.input.clear();
                return KeyAction::SwapModel(model_id);
            }
            MenuAction::Cancel => {
                app.model_picker = None;
                app.input.clear();
                return KeyAction::Nothing;
            }
            MenuAction::Nothing => return KeyAction::Nothing,
        }
    }
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
            if app.capture_custom_prompt {
                // We're in "type your custom summarize prompt" mode.
                // Anything the user submits goes to the summarizer,
                // NOT to the model as a chat turn. Empty text falls
                // back to the default prompt (same as picking
                // "Summarize" instead of "Custom").
                app.capture_custom_prompt = false;
                let t = text.trim().to_string();
                if t.is_empty() {
                    KeyAction::SummaryChosen(SummaryChoice::DefaultSummary)
                } else {
                    KeyAction::RunCustomSummary(t)
                }
            } else if app.status == Status::Streaming {
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
        SlashCmd::Model => KeyAction::OpenModelPicker,
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
    // 120ms ticker keeps the "Elapsed X.Xs" / spinner glyph moving
    // while nothing else is happening. First tick fires immediately;
    // set reset_missed so if the runtime is busy we don't queue up
    // stale ticks.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Initial dock draw.
    term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;

    loop {
        if app.should_exit {
            return Ok(0);
        }

        tokio::select! {
            _ = tick.tick() => {
                // Pick up a completed summarize task. The tick is
                // 120ms, which matches PI's UX for "quick actions"
                // completing without needing a fancy select! arm.
                if let Some(task) = app.summarize_task.take() {
                    if task.is_finished() {
                        match task.await {
                            Ok(outcome) => {
                                handle_action(
                                    KeyAction::SummaryFinished(outcome),
                                    app, term, &agent_slot,
                                    &mut ag_rx, &mut cancel, &mut turn_task,
                                ).await?;
                            }
                            Err(_join_err) => {
                                app.status_note = None;
                                app.pending_fork = None;
                                insert_line(term, Line::from(vec![
                                    Span::styled(
                                        "[summarize task panicked — fork aborted]",
                                        Style::default().fg(Color::Red),
                                    ),
                                ]))?;
                            }
                        }
                    } else {
                        app.summarize_task = Some(task);
                    }
                }
                // Redraw only when there's a live counter to update.
                if app.turn_started_at.is_some()
                    || app.tool_started_at.is_some()
                    || app.status_note.is_some()
                {
                    term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;
                }
            }
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
                            // Muted amber (Indexed 137 #af875f) for
                            // interrupted state — Morandi warm tone.
                            let bg = Color::Indexed(137);
                            let bold = Style::default()
                                .bg(bg)
                                .fg(Color::Indexed(232))
                                .add_modifier(Modifier::BOLD);
                            let dim = Style::default()
                                .bg(bg)
                                .fg(Color::Indexed(235))
                                .add_modifier(Modifier::ITALIC);
                            insert_line_bg(term, Line::from(vec![
                                Span::styled(pending.leading, bold),
                                Span::styled(pending.body, bold),
                                Span::styled("  (interrupted)", dim),
                            ]), Some(bold))?;
                        }
                        ag_rx = None;
                        cancel = None;
                        app.status = Status::Idle;
                        app.turn_started_at = None;
                        app.tool_started_at = None;
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
        KeyAction::ExpandLastTool => {
            // Only expand once per tool result — flip the flag.
            let last = match app.last_tool_output.take() {
                Some(l) if !l.expanded => Some(l),
                other => {
                    app.last_tool_output = other;
                    None
                }
            };
            if let Some(l) = last {
                render_tool_expansion(term, &l.content, l.is_error)?;
                app.last_tool_output = None;
            }
        }
        KeyAction::OpenModelPicker => {
            // Populate the picker with pricing-known models. Highlight
            // the current one so users see where they're switching from.
            let current = {
                let g = agent_slot.lock().await;
                g.as_ref().map(|a| a.model.clone()).unwrap_or_default()
            };
            let items: Vec<MenuItem<String>> = crate::pricing::known_models()
                .into_iter()
                .map(|prefix| {
                    let marker = if current.starts_with(prefix) { "  (current)" } else { "" };
                    MenuItem::new(
                        prefix.to_string(),
                        format!("Switch to {}{}", prefix, marker),
                        prefix.to_string(),
                    )
                })
                .collect();
            app.model_picker = Some(MenuState::new(items));
        }
        KeyAction::SwapModel(new_model) => {
            let mut g = agent_slot.lock().await;
            if let Some(a) = g.as_mut() {
                let new_provider = crate::provider::openai::OpenAiProvider::new(
                    &a.base_url,
                    &a.api_key,
                    &new_model,
                );
                a.provider = Box::new(new_provider);
                a.model = new_model.clone();
                app.model = new_model.clone();
                insert_line(term, Line::from(vec![
                    Span::styled(
                        format!("[model → {}]", new_model),
                        Style::default().fg(Color::Indexed(108)).add_modifier(Modifier::ITALIC),
                    ),
                ]))?;
            }
        }
        KeyAction::OpenForkPicker => {
            // Tree-aware picker: walk the parent_id chain upward from
            // the current session, then render as a depth-first tree
            // (matches PI's Session Tree — see img/pi_fork_tree.jpg).
            //
            // Example: root [A, B, C, D] with a child fork [A, B, B1, B2]
            // renders as
            //     A, B, B1, B2, C, D
            // — at the divergence point (index 2 in root, since A+B
            // are the shared prefix), we descend into the child's
            // "new tail" first (B1, B2), then pop back to the parent
            // and continue with C, D. This is DFS on a nested-branch
            // tree; each session's items are indented by depth so
            // users see which branch a message is on.
            let session_path = {
                let g = agent_slot.lock().await;
                g.as_ref().map(|a| a.session_path.clone())
            };
            let Some(session_path) = session_path else {
                return Ok(());
            };

            let mut chain = session::walk_parent_chain(&session_path);
            if chain.is_empty() {
                insert_line(term, Line::from(vec![
                    Span::styled(
                        "[fork: could not read current session]",
                        Style::default().fg(Color::Red),
                    ),
                ]))?;
                return Ok(());
            }
            // chain[0] = current, last = root. Reverse to [root, …, current]
            // so DFS starts at the root and dips into child branches.
            chain.reverse();

            let mut items: Vec<MenuItem<(PathBuf, usize)>> = Vec::new();
            render_fork_tree(&chain, 0, 0, &session_path, &mut items);

            if items.is_empty() {
                insert_line(term, Line::from(vec![
                    Span::styled(
                        "[fork: no user messages in this session yet]",
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                    ),
                ]))?;
                return Ok(());
            }
            app.fork_picker = Some(MenuState::new(items));
        }
        KeyAction::ForkChosen(source_path, target_idx) => {
            // Compute cut-off (tail of CURRENT session that this fork
            // will abandon) and prefill (target message text if it's a
            // user message). Only sessions where source == current
            // have a meaningful cut-off — cross-session forks to an
            // ancestor abandon nothing summarizable in the ancestor
            // itself, so we skip the summary prompt in that case.
            let current_path = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => a.session_path.clone(),
                    None => return Ok(()),
                }
            };
            let src_entries = match session::read_session(&source_path) {
                Ok((_, e)) => e,
                Err(e) => {
                    insert_line(term, Line::from(vec![
                        Span::styled(
                            format!("[fork read failed: {e}]"),
                            Style::default().fg(Color::Red),
                        ),
                    ]))?;
                    return Ok(());
                }
            };
            let prefill = src_entries.get(target_idx).and_then(|e| match e {
                session::SessionEntry::Message { role, content, .. } if role == "user" => {
                    Some(content.clone())
                }
                _ => None,
            });
            let cut_off = if source_path == current_path && target_idx < src_entries.len() {
                src_entries[target_idx..].to_vec()
            } else {
                Vec::new()
            };
            let pending = PendingFork {
                source_path,
                target_entry_idx: target_idx,
                prefill,
                cut_off,
            };
            if !pending.cut_off.is_empty() {
                app.pending_fork = Some(pending);
                app.summary_prompt = Some(MenuState::new(vec![
                    MenuItem::new(
                        "No summary".to_string(),
                        "Fork without summarizing the abandoned tail".to_string(),
                        SummaryChoice::NoSummary,
                    ),
                    MenuItem::new(
                        "Summarize".to_string(),
                        "Have the model summarize the cut-off tail (default prompt)".to_string(),
                        SummaryChoice::DefaultSummary,
                    ),
                    MenuItem::new(
                        "Summarize with custom prompt".to_string(),
                        "Type your own instructions, then Enter".to_string(),
                        SummaryChoice::CustomSummary,
                    ),
                ]));
            } else {
                execute_fork(pending, None, app, term, agent_slot).await?;
            }
        }
        KeyAction::SummaryChosen(choice) => {
            let Some(pending) = app.pending_fork.take() else {
                return Ok(());
            };
            match choice {
                SummaryChoice::NoSummary => {
                    execute_fork(pending, None, app, term, agent_slot).await?;
                }
                SummaryChoice::DefaultSummary => {
                    spawn_summarize_task(app, agent_slot, pending, None).await;
                }
                SummaryChoice::CustomSummary => {
                    // Stash again; wait for the next TextBuffer Submit
                    // to arrive as RunCustomSummary(text).
                    app.pending_fork = Some(pending);
                    app.capture_custom_prompt = true;
                    app.status_note =
                        Some("custom summarize prompt — Enter to submit".into());
                }
            }
        }
        KeyAction::RunCustomSummary(text) => {
            let Some(pending) = app.pending_fork.take() else {
                app.status_note = None;
                return Ok(());
            };
            app.status_note = None;
            spawn_summarize_task(app, agent_slot, pending, Some(text)).await;
        }
        KeyAction::SummaryFinished(outcome) => {
            app.status_note = None;
            match outcome {
                SummarizeOutcome::Ok { summary, fork } => {
                    execute_fork(fork, summary, app, term, agent_slot).await?;
                }
                SummarizeOutcome::Err { error, fork } => {
                    insert_line(term, Line::from(vec![
                        Span::styled(
                            format!("[summarize error: {error} — forking without summary]"),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]))?;
                    execute_fork(fork, None, app, term, agent_slot).await?;
                }
            }
        }
        KeyAction::CancelPendingFork => {
            app.pending_fork = None;
            app.capture_custom_prompt = false;
            app.status_note = None;
        }
        KeyAction::Compact => {
            app.status_note = Some("compacting…".into());
            term.draw(|f| { let area = f.area(); draw_dock(f.buffer_mut(), area, app); })?;
            let mut g = agent_slot.lock().await;
            if let Some(a) = g.as_mut() {
                let before = a.context.estimate_chars();
                a.compact_now(None, "manual").await;
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
            app.turn_started_at = Some(std::time::Instant::now());
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

/// Finalize a fork: create the new session file, optionally append a
/// BranchSummary entry with the LLM-generated text, load it as the
/// active Agent (transplanting provider / permission / hooks from the
/// old Agent), then update the App's session id + prefill.
async fn execute_fork(
    fork: PendingFork,
    summary: Option<String>,
    app: &mut App,
    term: &mut Term,
    agent_slot: &Arc<Mutex<Option<Agent>>>,
) -> Result<()> {
    let (cwd, model, base_url, api_key, permission, hooks) = {
        let g = agent_slot.lock().await;
        let a = match g.as_ref() {
            Some(a) => a,
            None => return Ok(()),
        };
        (
            a.cwd.clone(),
            a.model.clone(),
            a.base_url.clone(),
            a.api_key.clone(),
            a.permission.clone(),
            a.hooks.clone(),
        )
    };

    let (new_path, new_header, _fresh_prefill) =
        match session::fork_session_at(&cwd, &fork.source_path, fork.target_entry_idx) {
            Ok(t) => t,
            Err(e) => {
                insert_line(term, Line::from(vec![
                    Span::styled(
                        format!("[fork failed: {e}]"),
                        Style::default().fg(Color::Red),
                    ),
                ]))?;
                return Ok(());
            }
        };

    // Persist the summary as a BranchSummary session entry BEFORE
    // loading — that way load_session replays it as part of context.
    if let Some(text) = summary.as_ref().filter(|s| !s.is_empty()) {
        let _ = session::append_entry(
            &new_path,
            &session::SessionEntry::BranchSummary {
                timestamp: crate::util::time::now_iso8601(),
                summary: text.clone(),
            },
        );
    }

    let _ = session::set_active_session(&cwd, &new_path);

    let mut new_agent = match Agent::load_session(&new_path, &cwd) {
        Ok(a) => a,
        Err(e) => {
            insert_line(term, Line::from(vec![
                Span::styled(
                    format!("[fork load failed: {e}]"),
                    Style::default().fg(Color::Red),
                ),
            ]))?;
            return Ok(());
        }
    };
    new_agent.provider = Box::new(crate::provider::openai::OpenAiProvider::new(
        &base_url, &api_key, &model,
    ));
    new_agent.model = model.clone();
    new_agent.base_url = base_url;
    new_agent.api_key = api_key;
    new_agent.permission = permission;
    new_agent.hooks = hooks;
    new_agent.registry = crate::tool::ToolRegistry::standard();
    if new_agent.context.system.is_none() {
        new_agent.context.system = Some(
            crate::agent::system_prompt::build(&cwd, &new_agent.registry.names()),
        );
    }
    new_agent.context.tools = new_agent.registry.all_specs();
    let new_session_id = new_header.id;

    {
        let mut g = agent_slot.lock().await;
        *g = Some(new_agent);
    }

    app.session_id = new_session_id.to_string();
    app.usage = crate::event::Usage::default();
    app.context_chars = 0;
    app.turn_count = 0;
    app.input.clear();
    if let Some(text) = fork.prefill.as_ref().filter(|s| !s.is_empty()) {
        app.input.insert_str(text);
    }

    let short = &new_session_id.to_string()[..8];
    let summary_note = if summary.is_some() { " · with summary" } else { "" };
    insert_line(term, Line::from(vec![
        Span::styled(
            format!(
                "[forked at entry {} → new session {}{}]",
                fork.target_entry_idx, short, summary_note
            ),
            Style::default()
                .fg(Color::Indexed(108))
                .add_modifier(Modifier::ITALIC),
        ),
    ]))?;
    Ok(())
}

/// Spawn the async summarization task and stash its handle on `app`.
/// The main loop's tick polls `summarize_task.is_finished()` to pick
/// up the result and dispatch `SummaryFinished`. A fresh
/// OpenAiProvider is built for the task (the Agent's provider is
/// `Box<dyn Provider>` which isn't Send-safe to share out).
async fn spawn_summarize_task(
    app: &mut App,
    agent_slot: &Arc<Mutex<Option<Agent>>>,
    pending: PendingFork,
    custom: Option<String>,
) {
    let (model, base_url, api_key) = {
        let g = agent_slot.lock().await;
        match g.as_ref() {
            Some(a) => (a.model.clone(), a.base_url.clone(), a.api_key.clone()),
            None => return,
        }
    };
    let provider =
        crate::provider::openai::OpenAiProvider::new(&base_url, &api_key, &model);
    let cut_off = pending.cut_off.clone();
    let task = tokio::spawn(async move {
        let summary = crate::agent::branch_summary::summarize_branch(
            &cut_off,
            custom.as_deref(),
            &provider,
        )
        .await;
        SummarizeOutcome::Ok { summary, fork: pending }
    });
    app.status_note = Some("summarizing branch…".into());
    app.summarize_task = Some(task);
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
            // Real assistant text → flush any pending thinking, then
            // accumulate + emit full lines through the markdown parser.
            flush_thinking_buf(term, app)?;
            app.stream_buf.push_str(&text);
            while let Some(nl) = app.stream_buf.find('\n') {
                let line: String = app.stream_buf.drain(..=nl).collect();
                let trimmed = line.trim_end_matches('\n');
                let spans = crate::render::markdown::render_line(trimmed, &mut app.md_state);
                insert_line(term, Line::from(spans))?;
            }
        }
        AgentEvent::ThinkingDelta { text, .. } => {
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
            let (leading, body) = tool_call_bar_text(&call.name, &call.arguments);
            app.pending_tool_call = Some(PendingBar { leading, body });
            // Start the live "Elapsed X.Xs" clock so the dock can
            // render a blue running-state strip until ToolResult.
            app.tool_started_at = Some(std::time::Instant::now());
        }
        AgentEvent::ToolResult { content, is_error, elapsed_ms, .. } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            render_tool_card(term, app, &content, is_error, elapsed_ms)?;
            app.tool_started_at = None;
            // Stash full output so Ctrl+O can expand it later — with
            // the outcome flag so expansion uses matching bg.
            app.last_tool_output = Some(LastTool { content, is_error, expanded: false });
        }
        AgentEvent::Error { error } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            insert_line(term, Line::from(vec![
                Span::styled(format!("[error: {error}]"), Style::default().fg(Color::Red)),
            ]))?;
        }
        AgentEvent::CompactionStart { reason } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            insert_line(term, Line::from(vec![
                Span::styled(
                    format!("[compacting context ({reason})…]"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]))?;
        }
        AgentEvent::CompactionEnd { replaced_count, used_llm } => {
            let via = if used_llm { "summary" } else { "truncation" };
            insert_line(term, Line::from(vec![
                Span::styled(
                    format!("[compacted {replaced_count} messages via {via}]"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]))?;
            // Refresh cached context estimate for the status footer.
            app.context_chars = 0; // will be re-populated on next event
        }
        AgentEvent::Done { .. } | AgentEvent::Start { .. } => {}
    }
    Ok(())
}

/// If any partial (no trailing newline) assistant text sits in the
/// stream buffer, insert it as a line and clear.
fn flush_stream_buf(term: &mut Term, app: &mut App) -> Result<()> {
    if !app.stream_buf.is_empty() {
        let text = std::mem::take(&mut app.stream_buf);
        let spans = crate::render::markdown::render_line(&text, &mut app.md_state);
        // Own the strings so lifetime isn't tied to `text` which drops
        // at end of scope.
        let owned: Vec<Span<'static>> = spans
            .into_iter()
            .map(|s| Span::styled(s.content.into_owned(), s.style))
            .collect();
        insert_line(term, Line::from(owned))?;
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

/// Number of output lines to show inside a tool card. PI's screenshots
/// show ~4-6 lines of preview.
const TOOL_PREVIEW_LINES: usize = 6;

/// Render the full tool card:
///
///   (blank)
///   [green]  $ ls /tmp                              ← command
///   [green]  file1.txt                              ← output preview
///   [green]  file2.txt
///   [green]  ... (K earlier lines, ctrl+o expand)   ← truncation marker
///   [green]  (blank green row)
///   [green]  Took 32ms                              ← duration
///   (blank)
///
/// All rows share the same bg (green on success, red on error). The
/// pending bar text was stashed by the earlier ToolCall event; if
/// missing (shouldn't happen), we still render output + timing.
fn render_tool_card(
    term: &mut Term,
    app: &mut App,
    content: &str,
    is_error: bool,
    elapsed_ms: u64,
) -> Result<()> {
    // Morandi palette (dusty, low-saturation) — reads clearly on both
    // light and dark terminals without shouting. Use 256-color
    // Indexed(...) so it renders identically on truecolor and
    // 256-only terminals.
    //   65  = #5f875f  muted sage    (success)
    //   131 = #af5f5f  dusty rose    (error)
    let (bar_bg, bar_fg) = if is_error {
        (Color::Indexed(131), Color::Indexed(230))
    } else {
        (Color::Indexed(65), Color::Indexed(230))
    };
    let bar_style = Style::default().bg(bar_bg).fg(bar_fg).add_modifier(Modifier::BOLD);
    // Dimmed foreground on the same bg — for output preview + Took.
    let dim_style = Style::default()
        .bg(bar_bg)
        .fg(Color::Indexed(253));
    let hint_style = Style::default()
        .bg(bar_bg)
        .fg(Color::Indexed(250))
        .add_modifier(Modifier::ITALIC);

    // Breathing room above.
    insert_line(term, Line::from(""))?;

    // Row 1: command bar (from stashed pending_tool_call).
    if let Some(pending) = app.pending_tool_call.take() {
        insert_line_bg(term, Line::from(vec![
            Span::styled(pending.leading, bar_style),
            Span::styled(pending.body, bar_style),
        ]), Some(bar_style))?;
    }

    // Output preview: last N lines. If more, show a truncation marker.
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total > TOOL_PREVIEW_LINES {
        let hidden = total - TOOL_PREVIEW_LINES;
        insert_line_bg(term, Line::from(vec![
            Span::styled(
                format!("  … ({} earlier lines, ctrl+o to expand)", hidden),
                hint_style,
            ),
        ]), Some(bar_style))?;
    }
    let start = total.saturating_sub(TOOL_PREVIEW_LINES);
    for line in &lines[start..] {
        // Prefix with 2 spaces for visual indent inside the card.
        insert_line_bg(term, Line::from(vec![
            Span::styled("  ", dim_style),
            Span::styled(line.to_string(), dim_style),
        ]), Some(dim_style))?;
    }

    // Empty divider row inside the card.
    insert_line_bg(term, Line::from(vec![Span::styled("", bar_style)]), Some(bar_style))?;

    // Took Xs (right-side info, italic dim on the card bg).
    let took_str = if elapsed_ms < 50 {
        format!("Took {}ms", elapsed_ms)
    } else {
        format!("Took {:.1}s", elapsed_ms as f64 / 1000.0)
    };
    insert_line_bg(term, Line::from(vec![
        Span::styled(format!("  {}", took_str), hint_style),
    ]), Some(bar_style))?;

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
    // Layout: 4 palette + 1 status/working + 3 input + 2 footer = 10.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // palette area (blank when closed)
            Constraint::Length(1), // status strip (blue tool bar / thinking spinner / blank)
            Constraint::Length(3), // input box (top border + content + bottom border)
            Constraint::Length(1), // cwd + branch
            Constraint::Length(1), // stats
        ])
        .split(area);

    // ── Overlay menus (dropdown). Priority matches interpret_key:
    // summary modal > fork picker > model picker > slash palette.
    if let Some(m) = &app.summary_prompt {
        draw_menu(buf, chunks[0], m, "summarize branch?");
    } else if let Some(m) = &app.fork_picker {
        draw_menu(buf, chunks[0], m, "user message");
    } else if let Some(m) = &app.model_picker {
        draw_menu(buf, chunks[0], m, "model");
    } else if let Some(m) = &app.palette {
        draw_palette(buf, chunks[0], m);
    }

    // ── Status strip ─────────────────────────────────────────────
    // Priority: tool running (blue bar) > turn thinking > blank.
    draw_status_strip(buf, chunks[1], app);

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
    input_para.render(chunks[2], buf);

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
    Paragraph::new(Line::from(l1)).render(chunks[3], buf);

    // Line 2: tokens + cost + model + context ratio (PI-style)
    let mut l2: Vec<Span> = Vec::new();
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
    // Context ratio (`1.4%/205k (auto)`), color-coded by usage.
    // Auto-compact is always on today; if we add a config toggle we
    // can wire it here.
    if let Some(ratio) = crate::render::status_line::context_ratio(
        &app.model,
        app.context_chars,
        true,
    ) {
        let pct = crate::render::status_line::context_percent(&app.model, app.context_chars)
            .unwrap_or(0.0);
        let color = match crate::render::status_line::context_color(pct) {
            "red" => Color::Red,
            "yellow" => Color::Yellow,
            _ => Color::Indexed(108), // muted sage — matches Morandi theme
        };
        l2.push(Span::raw("  "));
        l2.push(Span::styled(ratio, Style::default().fg(color)));
    }
    if app.status == Status::Streaming {
        l2.push(Span::raw("  · "));
        l2.push(Span::styled(
            "streaming",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
        ));
    }
    Paragraph::new(Line::from(l2)).render(chunks[4], buf);
}

/// Render the full tool output as an extension of the last tool card —
/// same green (or red) bg, with the whole content this time (not the
/// 6-line preview). Matches PI's behavior: the expanded output lives
/// INSIDE the colored zone (see tool-execution.ts:265).
fn render_tool_expansion(term: &mut Term, content: &str, is_error: bool) -> Result<()> {
    let (bar_bg, hint_fg) = if is_error {
        (Color::Indexed(131), Color::Indexed(250))
    } else {
        (Color::Indexed(65), Color::Indexed(250))
    };
    let bar_style = Style::default().bg(bar_bg).fg(Color::Indexed(230));
    let hint_style = Style::default()
        .bg(bar_bg)
        .fg(hint_fg)
        .add_modifier(Modifier::ITALIC);

    // Header: `── expanded (N lines) ──` inside the colored zone.
    let n = content.lines().count();
    insert_line_bg(
        term,
        Line::from(vec![Span::styled(
            format!("  ── expanded output ({n} lines) ──"),
            hint_style,
        )]),
        Some(bar_style),
    )?;
    // Every content line rendered as `  <line>` on the same bg.
    for line in content.lines() {
        insert_line_bg(
            term,
            Line::from(vec![
                Span::styled("  ", bar_style),
                Span::styled(line.to_string(), bar_style),
            ]),
            Some(bar_style),
        )?;
    }
    // Trailing padding row + blank outside for breathing room.
    insert_line_bg(
        term,
        Line::from(vec![Span::styled("", bar_style)]),
        Some(bar_style),
    )?;
    insert_line(term, Line::from(""))?;
    Ok(())
}

/// Draw the 1-row activity strip between palette and input box.
/// - tool running → blue `$ cmd  Elapsed X.Xs` (matches PI's blue bar
///   in img/PI_work_status.jpg)
/// - streaming with no active tool → `⣷ thinking (X.Xs)` dim
/// - idle → blank row
fn draw_status_strip(buf: &mut Buffer, area: Rect, app: &App) {
    const BRAILLE: &[&str] = &["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];
    let frame = ((std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis()).unwrap_or(0) / 120) as usize) % BRAILLE.len();

    // Tool running has priority.
    if let (Some(started), Some(bar)) =
        (app.tool_started_at, app.pending_tool_call.as_ref())
    {
        let elapsed = started.elapsed().as_secs_f64();
        let blue_bg = Color::Indexed(24); // muted navy — matches Morandi
        let bar_style = Style::default()
            .bg(blue_bg)
            .fg(Color::Indexed(230))
            .add_modifier(Modifier::BOLD);
        let dim_style = Style::default()
            .bg(blue_bg)
            .fg(Color::Indexed(250))
            .add_modifier(Modifier::ITALIC);
        // truncate command to fit
        let width_budget = (area.width as usize).saturating_sub(30);
        let body_short = if bar.body.chars().count() > width_budget {
            let taken: String = bar.body.chars().take(width_budget).collect();
            format!("{taken}…")
        } else {
            bar.body.clone()
        };
        let line = Line::from(vec![
            Span::styled(format!("{} ", BRAILLE[frame]), bar_style),
            Span::styled(bar.leading.clone(), bar_style),
            Span::styled(body_short, bar_style),
            Span::styled(format!("  Elapsed {:.1}s", elapsed), dim_style),
        ]);
        // Row-fill with the bar bg so the strip extends to the edge.
        let full = Style::default().bg(blue_bg);
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.set_char(' ').set_style(full);
            }
        }
        buf.set_line(area.x, area.y, &line, area.width);
        return;
    }

    // Assistant thinking / waiting.
    if let Some(started) = app.turn_started_at {
        let elapsed = started.elapsed().as_secs_f64();
        let line = Line::from(vec![
            Span::styled(
                format!("{} thinking ({:.1}s)", BRAILLE[frame], elapsed),
                Style::default().fg(Color::Indexed(108)).add_modifier(Modifier::ITALIC),
            ),
        ]);
        buf.set_line(area.x, area.y, &line, area.width);
    }
    // else: leave blank
}

/// Split a string at the given byte position, returning (pre, post).
fn split_at_col(s: &str, col: usize) -> (&str, &str) {
    let clamped = col.min(s.len());
    s.split_at(clamped)
}

/// Recursively populate `items` with a DFS traversal of the session
/// tree. `chain` is ordered [root, ..., parent, current]. At each
/// level `depth`, we walk the session's user messages starting at
/// `start`; whenever we reach the index where the next-deeper chain
/// entry diverges (its common-prefix-length with us), we descend
/// into that deeper session's "new tail" before emitting our own
/// message at that index. Result: the picker shows fork branches
/// nested inline at their branch points, PI-style
/// (see img/pi_fork_tree.jpg).
fn render_fork_tree(
    chain: &[(PathBuf, session::SessionHeader, Vec<session::TreeRow>)],
    depth: usize,
    start: usize,
    current_path: &Path,
    items: &mut Vec<MenuItem<(PathBuf, usize)>>,
) {
    let (path, hdr, rows) = &chain[depth];
    let deeper_start = if depth + 1 < chain.len() {
        Some(session::common_tree_row_prefix(rows, &chain[depth + 1].2))
    } else {
        None
    };
    let is_current = path == current_path;
    let short = &hdr.id.to_string()[..8];
    let branch_label = if is_current { "current" } else { "past branch" };
    let indent = "  ".repeat(depth);
    let total = rows.len();

    for i in start..rows.len() {
        if Some(i) == deeper_start {
            render_fork_tree(chain, depth + 1, i, current_path, items);
        }
        let row = &rows[i];
        // Payload is the ENTRY index in the source session, not the
        // tree-row index — that's what fork_session_at slices on.
        items.push(MenuItem::new(
            format!("{}{}: {}", indent, row.role, row.preview),
            format!("{} · {} · row {}/{}", branch_label, short, i + 1, total),
            (path.clone(), row.entry_index),
        ));
    }

    // Edge case: deeper session diverges AT OR PAST our end (it fully
    // shares our prefix and adds new rows beyond us). Descend after
    // the loop so those rows still appear.
    if let Some(ds) = deeper_start {
        if ds >= rows.len() {
            render_fork_tree(chain, depth + 1, ds, current_path, items);
        }
    }
}

/// Same as draw_palette but for MenuState<String> (model picker etc).
/// A minimal re-implementation — TODO: unify via a trait once we have
/// a third menu type.
fn draw_menu<T: Clone>(buf: &mut Buffer, area: Rect, m: &MenuState<T>, label: &str) {
    let vis = m.visible();
    let sel = m.cursor();
    if vis.is_empty() {
        let msg = Line::from(vec![Span::styled(
            format!("  (no {label} matches)"),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )]);
        Paragraph::new(msg).render(area, buf);
        return;
    }
    let max_rows = area.height as usize;
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
            Style::default().fg(Color::Indexed(108)).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let desc_style = Style::default().fg(Color::DarkGray);
        lines.push(Line::from(vec![
            Span::styled(arrow, label_style),
            Span::styled(format!("{:<32}", item.label), label_style),
            Span::styled(item.description.clone(), desc_style),
        ]));
    }
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

}

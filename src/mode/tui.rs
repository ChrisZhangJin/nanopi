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

use std::io::{self, Stdout};
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
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::{Agent, HooksConfig};
use crate::agent::permission::PermissionGate;
use crate::event::{AgentEvent, SteerMessage};
use crate::render::menu::{MenuAction, MenuItem, MenuState};
use crate::render::text_buffer::{Action as TbAction, TextBuffer};
use crate::session::{self, SessionChoice};
use crate::settings;
use crate::tool::ToolRegistry;

/// Slash commands available in the palette.
///
/// `Copy` is deliberately dropped so `Skill(String)` can carry the
/// invocation name — a small refactor from v0.8 to match PI's
/// autocomplete provider (`interactive-mode.ts:649-661`) which
/// includes a `skill:<name>` entry per loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashCmd {
    Compact,
    Quit,
    Model,
    /// Start a fresh session in the same cwd.
    NewSession,
    /// Open a picker over recent sessions in this cwd and switch to
    /// the one selected.
    Resume,
    /// Open the fork picker (same as Esc-Esc) — DFS tree render
    /// across the parent chain, selecting any entry forks from
    /// there. PI has a separate `/tree` for tree navigation; we
    /// collapsed both into `/fork` since our picker already shows
    /// the full tree with all entry types.
    Fork,
    /// Dump a keybinding cheat-sheet into scrollback.
    Hotkeys,
    /// Print session stats (tokens, cost, model, turns) to scrollback.
    SessionInfo,
    /// Enter capture mode; next Enter sets the session's `name`.
    Name,
    /// Copy the last assistant message to the OS clipboard via OSC 52.
    Copy,
    /// Enter capture mode; next Enter writes a JSONL copy of the
    /// current session to the typed path.
    Export,
    /// Enter capture mode; next Enter loads a JSONL from the typed
    /// path as a new session and switches to it.
    Import,
    /// Invoke a skill via `/skill:<name>`. Payload is the skill name;
    /// dispatch_slash turns it back into a StartTurn with the
    /// `/skill:<name> <arg>` string so run_turn's expansion path
    /// still handles it.
    Skill(String),
    /// List all loaded skills into scrollback (name · description ·
    /// source · path). Doesn't take an arg. PI does this implicitly
    /// on startup; nanopi adds the on-demand relist.
    ListSkills,
    /// List every tool the model can call, with its source (built-in
    /// or the plugin `.wasm` that supplied it). The registry is the
    /// same one handed to the provider, so this is ground truth —
    /// asking the model to list its own tools is not: it will happily
    /// present plugin tools as built-ins, or invent skills.
    ListTools,
    /// Re-read config.toml + settings.toml + skills without exiting
    /// the session. Mirrors PI's `/reload` — new skills installed
    /// mid-session become visible to the model on the next turn.
    Reload,
    /// v0.9.3: print current interaction settings + path to settings.toml.
    Settings,
    /// v0.9.3: print the keybindings submenu (list of ActionId + spec + toml key).
    Keybindings,
    /// v0.11.0: a command registered by a WASM plugin. Payload is the
    /// command name; the handler is looked up in `App::commands_cache`
    /// at dispatch time rather than carried here, so `SlashCmd` stays
    /// `PartialEq` and cheap for the palette's filter.
    Plugin(String),
    // Not here: /thinking. PI exposes thinking-budget control as a
    // keybinding (Shift+Tab cycle), not a slash command — see
    // packages/coding-agent/src/core/keybindings.ts:73-76.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsRow { Thinking, HideThinking, AutoCompact, DefaultTrust, Keybindings }

fn settings_row_label(sf: &crate::settings_toml::SettingsFile, row: SettingsRow) -> String {
    match row {
        SettingsRow::Thinking => format!("Thinking level          {}", sf.thinking_level.map(|l| l.to_string()).unwrap_or_else(|| "off".into())),
        SettingsRow::HideThinking => format!("Hide thinking output    {}", if sf.hide_thinking.unwrap_or(false) { "on" } else { "off" }),
        SettingsRow::AutoCompact => format!("Auto-compact            {}", if sf.auto_compact.unwrap_or(true) { "on" } else { "off" }),
        SettingsRow::DefaultTrust => format!("Default project trust   {}", match sf.default_project_trust.unwrap_or(crate::settings_toml::TrustLevelSer::Ask) {
            crate::settings_toml::TrustLevelSer::Ask => "ask",
            crate::settings_toml::TrustLevelSer::Trusted => "trusted",
            crate::settings_toml::TrustLevelSer::Distrusted => "distrusted",
        }),
        SettingsRow::Keybindings => "Keybindings             >".into(),
    }
}

fn build_settings_menu(sf: &crate::settings_toml::SettingsFile) -> MenuState<SettingsRow> {
    let rows = [SettingsRow::Thinking, SettingsRow::HideThinking, SettingsRow::AutoCompact, SettingsRow::DefaultTrust, SettingsRow::Keybindings];
    MenuState::new(rows.iter().map(|r| MenuItem::new(settings_row_label(sf, *r), "Enter/Space to change", *r)).collect())
}

fn build_keybindings_menu(bindings: &crate::keys::KeyBindings) -> MenuState<crate::keys::ActionId> {
    let items: Vec<_> = crate::keys::ActionId::all().iter().map(|a| {
        let spec = bindings.get(*a).map(|s| s.to_string()).unwrap_or_else(|| "(unbound)".into());
        MenuItem::new(format!("{:<32} {}", a.label(), spec), "Enter to rebind, Esc to cancel", *a)
    }).collect();
    MenuState::new(items)
}

fn cycle_settings_row(app: &mut App, row: SettingsRow) {
    use crate::agent::thinking::ThinkingLevel as L;
    use crate::settings_toml::TrustLevelSer as T;
    match row {
        SettingsRow::Thinking => {
            let next = match app.settings_file.thinking_level {
                None => Some(L::Minimal), Some(L::Minimal) => Some(L::Low),
                Some(L::Low) => Some(L::Medium), Some(L::Medium) => Some(L::High),
                Some(L::High) => Some(L::Xhigh), Some(L::Xhigh) => Some(L::Max),
                Some(L::Max) => None,
            };
            app.settings_file.thinking_level = next;
            app.thinking = next;
        }
        SettingsRow::HideThinking => { let c = app.settings_file.hide_thinking.unwrap_or(false); app.settings_file.hide_thinking = Some(!c); }
        SettingsRow::AutoCompact => { let c = app.settings_file.auto_compact.unwrap_or(true); app.settings_file.auto_compact = Some(!c); }
        SettingsRow::DefaultTrust => {
            let c = app.settings_file.default_project_trust.unwrap_or(T::Ask);
            app.settings_file.default_project_trust = Some(match c { T::Ask => T::Trusted, T::Trusted => T::Distrusted, T::Distrusted => T::Ask });
        }
        SettingsRow::Keybindings => {}
    }
    let _ = crate::settings_toml::save(&app.settings_file);
}

fn slash_items() -> Vec<MenuItem<SlashCmd>> {
    vec![
        MenuItem::new("/model", "Switch to a different model", SlashCmd::Model),
        MenuItem::new(
            "/new",
            "Start a fresh session in this cwd",
            SlashCmd::NewSession,
        ),
        MenuItem::new(
            "/resume",
            "Open a picker over past sessions",
            SlashCmd::Resume,
        ),
        MenuItem::new("/fork", "Fork the session at a past turn", SlashCmd::Fork),
        MenuItem::new(
            "/session",
            "Show session stats (tokens, cost)",
            SlashCmd::SessionInfo,
        ),
        MenuItem::new("/name", "Set the current session's name", SlashCmd::Name),
        MenuItem::new("/copy", "Copy last assistant reply", SlashCmd::Copy),
        MenuItem::new(
            "/export",
            "Export current session to JSONL",
            SlashCmd::Export,
        ),
        MenuItem::new("/import", "Import a session from JSONL", SlashCmd::Import),
        MenuItem::new("/compact", "Force context compaction", SlashCmd::Compact),
        MenuItem::new("/hotkeys", "Show all keyboard shortcuts", SlashCmd::Hotkeys),
        MenuItem::new("/skills", "List all loaded skills", SlashCmd::ListSkills),
        MenuItem::new(
            "/tools",
            "List all callable tools + their source",
            SlashCmd::ListTools,
        ),
        MenuItem::new(
            "/reload",
            "Reload skills, config, settings",
            SlashCmd::Reload,
        ),
        MenuItem::new(
            "/settings",
            "Show interaction settings + settings.toml path",
            SlashCmd::Settings,
        ),
        MenuItem::new(
            "/keybindings",
            "List keybindings + how to override",
            SlashCmd::Keybindings,
        ),
        MenuItem::new("/quit", "Exit the session", SlashCmd::Quit),
        MenuItem::new("/exit", "Exit the session", SlashCmd::Quit),
    ]
}

/// Palette items for the currently-loaded skills. Called every time
/// the palette opens so a mid-session `/new` or `/resume` picks up
/// its target session's skill list.
fn skill_menu_items(skills: &[crate::resources::Skill]) -> Vec<MenuItem<SlashCmd>> {
    skills
        .iter()
        .map(|s| {
            MenuItem::new(
                format!("/skill:{}", s.name),
                palette_desc(&s.description),
                SlashCmd::Skill(s.name.clone()),
            )
        })
        .collect()
}

/// Truncate a description so a long one doesn't blow out the palette
/// row. Matches PI's autocomplete label rules
/// (interactive-mode.ts prefixAutocompleteDescription).
fn palette_desc(desc: &str) -> String {
    const MAX: usize = 80;
    if desc.chars().count() <= MAX {
        return desc.to_string();
    }
    let mut out: String = desc.chars().take(MAX).collect();
    out.push('…');
    out
}

/// One palette row per plugin command. The `[plugin]` suffix costs a
/// little width but earns it: the collision rules refuse rather than
/// rename, so when something is missing or misbehaving the user needs
/// to know which plugin to look at.
fn command_menu_items(cmds: &[crate::command::PluginCommand]) -> Vec<MenuItem<SlashCmd>> {
    cmds.iter()
        .map(|c| {
            MenuItem::new(
                format!("/{}", c.spec.name),
                palette_desc(&format!("{} [{}]", c.spec.description, c.plugin_name)),
                SlashCmd::Plugin(c.spec.name.clone()),
            )
        })
        .collect()
}

/// `/tools`'s "who's watching me" section (§5.1 / §9 of
/// `docs/v0.12-events.md`): the callable-tools list only answers "what
/// can the model call", not "what is observing every turn" — a plugin
/// with no exported tools but a `list-events` subscription would
/// otherwise be invisible from `/tools`. Returns an empty `Vec` when
/// nothing is subscribed, so the section is omitted entirely rather than
/// growing a permanently-empty heading.
fn subscriptions_section(subs: &[(String, Vec<String>)]) -> Vec<Line<'static>> {
    if subs.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::from(vec![Span::styled(
        format!("Watching events ({} plugins)", subs.len()),
        Style::default()
            .fg(Color::Indexed(108))
            .add_modifier(Modifier::BOLD),
    )])];
    for (plugin, events) in subs {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {plugin:<20}"),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(events.join(", "), Style::default().fg(Color::DarkGray)),
        ]));
    }
    lines
}

const DOCK_HEIGHT: u16 = 10; // palette(4) + status(1) + input(3) + footer(2)

/// Max input content lines shown at once when no overlay menu is open.
/// The input box grows with the buffer up to this, then scrolls to keep
/// the cursor visible (Claude Code's bounded-viewport model, sized to
/// our fixed inline dock — see `draw_dock`). 5 = DOCK_HEIGHT − status(1)
/// − borders(2) − footer(2), i.e. it reclaims the otherwise-blank
/// palette rows when no dropdown is up.
const MAX_INPUT_LINES: usize = 5;

// ─────────────────────────────────────────────────────────────────────
// Entry
// ─────────────────────────────────────────────────────────────────────

pub async fn run_tui_mode(
    // `None` = no explicit `api_kind`; the vendor picks the transport.
    api_kind: Option<crate::provider::ApiKind>,
    // `config.provider` — explicit vendor id overriding the
    // base_url/model sniff.
    cfg_provider: Option<String>,
    base_url: &str,
    model: &str,
    api_key: &str,
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
    // `config.inline_think_tags` — escape hatch for the inline
    // `<think>` splitter (on by default). `None` leaves it on.
    inline_think_tags: Option<bool>,
) -> Result<i32> {
    let permission = PermissionGate::from_cli(no_hooks, approve);

    let choice = session::resolve_session(
        &cwd,
        continue_session,
        session_id.as_deref(),
        fork_id.as_deref(),
        exact_session_id.as_deref(),
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
        SessionChoice::NewWithId(id) => {
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
    let _ = session::set_active_session(&cwd, &session_path);

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
        inline_think_tags,
    );
    let registry = ToolRegistry::standard();

    // v0.11.0: `tool_exec_mode` + `[[extensions]]` come from
    // config.toml, which isn't in this function's parameter list.
    // Re-read it here; a parse failure already surfaced through
    // load_settings, so fall back to defaults rather than re-report.
    let cfg_for_build = crate::config::load_config(&cwd).unwrap_or_default();
    let hooks = match settings::load_settings(&cwd) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("warning: failed to load settings: {e}");
            HooksConfig::default()
        }
    };

    use crate::agent::build::{print_skill_diagnostics, AgentBuildInputs};
    let skill_load_for_rebuilds = skill_load.clone();
    let prompt_overrides_for_rebuilds = prompt_overrides.clone();
    let agent: Agent = if let SessionChoice::Resume(_) = &choice {
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
            &cfg_for_build.extensions,
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
            tool_exec_mode: cfg_for_build.tool_exec_mode,
            extensions: cfg_for_build.extensions.clone(),
        });
        print_skill_diagnostics(&diags);
        a
    };

    let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(Some(agent)));

    // Session-start hook fires once.
    {
        let g = agent_slot.lock().await;
        if let Some(a) = g.as_ref() {
            a.fire_session_start("startup").await;
        }
    }

    // ── Startup banner (one-shot to stdout, scrolls up like normal
    // output). Printed BEFORE we enter raw mode so println! behaves.
    let loaded_skills = {
        let g = agent_slot.lock().await;
        g.as_ref().map(|a| a.skills.clone()).unwrap_or_default()
    };
    print_startup_banner(model, &header.id, &loaded_skills);

    let mut terminal = setup_terminal()?;
    let mut app = App::new(
        header.id.to_string(),
        model.to_string(),
        cwd.clone(),
        api_kind,
        skill_load_for_rebuilds,
        no_context_files,
        prompt_overrides_for_rebuilds,
        cfg_for_build.extensions.clone(),
        session_path.with_extension("history.txt"),
    );
    // v0.9.3: apply settings.toml (keybindings, hide_thinking, etc.).
    let settings_file = crate::settings_toml::load();
    app.bindings = crate::settings_toml::bindings_from(&settings_file);
    // v0.9.3: remember vendor pick for footer + future rebuilds. The
    // `config.provider` override has to be stashed on App too — every
    // later pick_vendor() (model swap, /new, /fork) reads it from there,
    // and it used to sit at None forever, silently ignoring the field.
    app.cfg_provider = cfg_provider.clone();
    app.inline_think_tags = inline_think_tags;
    let initial_vendor = crate::vendor::pick_vendor(cfg_provider.as_deref(), Some(base_url), model);
    app.vendor_id = Some(initial_vendor.id().to_string());
    // Prime the skills cache from the just-built agent so the very
    // first `/` palette open includes /skill:<name> entries.
    refresh_status(&mut app, &agent_slot).await;

    // On resume (--continue / --session / --fork), repaint the prior
    // transcript into scrollback so the user sees the conversation they
    // are picking up on — mirrors the in-session `/resume` behavior (and
    // PI). Without this, --continue loaded the history into context but
    // left the screen blank below the banner.
    if let SessionChoice::Resume(_) = &choice {
        let entries = session::read_session(&session_path)
            .map(|(_, e)| e)
            .unwrap_or_default();
        if !entries.is_empty() {
            let short = &header.id.to_string()[..8.min(header.id.to_string().len())];
            let title = match header.name.as_deref() {
                Some(n) => format!("─── Resumed session {short} · {n} ───"),
                None => format!("─── Resumed session {short} ───"),
            };
            insert_line(
                &mut terminal,
                Line::from(vec![Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::BOLD),
                )]),
            )?;
            insert_line(&mut terminal, Line::from(""))?;
            replay_history(&mut terminal, &mut app, &entries)?;
        }
    }

    let result = run_app(&mut terminal, &mut app, agent_slot.clone()).await;
    let _ = teardown_terminal(&mut terminal);

    // Session-end hook.
    {
        let g = agent_slot.lock().await;
        if let Some(a) = g.as_ref() {
            a.fire_session_shutdown("quit").await;
        }
    }

    // Closing lines: `✓ session saved` + resume hint so the user knows
    // how to pick up where they left off.
    // `app.session_id`, NOT the `header` bound at startup: `/new`,
    // `/resume`, `/fork` and `/import` all switch the live session and
    // update `app.session_id`, but that binding is never reassigned. So
    // ending a run in a session you switched into printed the id of the
    // one you STARTED in — and the `--session <id>` line under it sent
    // you back to a conversation you had abandoned.
    let sid = app.session_id.clone();
    let sid_short = &sid[..8.min(sid.len())];
    println!("\n\x1b[2m✓ session {sid_short} saved\x1b[0m");
    println!("\x1b[2mTo resume:  nanopi --continue    or    nanopi --session {sid}\x1b[0m");

    result
}

fn print_startup_banner(model: &str, session_id: &str, skills: &[crate::resources::Skill]) {
    let sid_short = &session_id[..8.min(session_id.len())];
    // Title line: bold nanopi + dim details.
    println!(
        "\x1b[1mnanopi\x1b[0m \x1b[2mv{}  {}  session {}\x1b[0m",
        env!("CARGO_PKG_VERSION"),
        model,
        sid_short
    );
    // Loaded resources — mirrors PI's `showLoadedResources` startup
    // section (interactive-mode.ts:1480). Skills grouped by source.
    if !skills.is_empty() {
        print_startup_skills(skills);
    }
    // Hint line, dim.
    println!(
        "\x1b[2mCtrl-C interrupt · Ctrl-D exit · / commands · /skills to relist · Enter submit\x1b[0m"
    );
    println!();
}

/// Print a compact "Skills" section listing all loaded skills grouped
/// by source. Same shape as PI's addLoadedSection for skills.
fn print_startup_skills(skills: &[crate::resources::Skill]) {
    use crate::resources::SkillSource;
    let mut by_source: std::collections::BTreeMap<&str, Vec<&crate::resources::Skill>> =
        std::collections::BTreeMap::new();
    for s in skills {
        let key = match s.source {
            SkillSource::User => "user",
            SkillSource::Project => "project",
            SkillSource::Cli => "cli",
        };
        by_source.entry(key).or_default().push(s);
    }
    let total = skills.len();
    println!(
        "\x1b[2mSkills ({total}): \x1b[0m{}",
        skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    // Per-source breakdown, one line each — dim.
    for (src, list) in by_source {
        let names: Vec<String> = list.iter().map(|s| s.name.clone()).collect();
        println!("  \x1b[2m{src}:\x1b[0m {}", names.join(", "));
    }
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
    /// When Some, the `/resume` picker over past sessions in this
    /// cwd is open. Payload is the file path of the chosen session.
    resume_picker: Option<MenuState<PathBuf>>,
    settings_menu: Option<MenuState<SettingsRow>>,
    keybindings_menu: Option<MenuState<crate::keys::ActionId>>,
    capture_key_for: Option<crate::keys::ActionId>,
    settings_file: crate::settings_toml::SettingsFile,
    /// After picking a fork target: modal asking whether to summarize
    /// the cut-off branch. Three options match PI's UX:
    /// No summary / Summarize (default prompt) / Summarize with custom
    /// prompt. On Chosen, dispatch RunSummary.
    summary_prompt: Option<MenuState<SummaryChoice>>,
    /// A fork the user picked that is waiting on a summary decision.
    /// Carries the fork target and the cut-off entries so the summary
    /// LLM call doesn't need to re-read the session file.
    pending_fork: Option<PendingFork>,
    /// What the next Enter submit should do — chat turn, or
    /// consumed by a slash-command capture flow (`/name` /
    /// summarize-custom / `/export` / `/import`).
    capture: CaptureMode,
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
    /// Which wire protocol to build follow-up Providers with — used
    /// by `/model` swap and fork-agent rebuild so a session that
    /// started on Anthropic stays on Anthropic. `None` = unspecified,
    /// let the vendor decide per base_url (a `/model` swap can change
    /// the answer, so this stays an Option rather than being resolved
    /// once at startup).
    api_kind: Option<crate::provider::ApiKind>,
    /// v0.9.3: keybindings loaded from settings.toml + defaults.
    bindings: crate::keys::KeyBindings,
    /// v0.9.3: cached `config.provider` string used by every
    /// `pick_vendor()` call at Agent build. Populated at startup.
    cfg_provider: Option<String>,
    /// `config.inline_think_tags` — escape hatch for the inline
    /// `<think>` splitter (on by default), threaded to every follow-up
    /// `provider::build()` call the same way `cfg_provider` is.
    inline_think_tags: Option<bool>,
    /// v0.9.3: id from `pick_vendor` at last Agent build. `None`
    /// before first build; `Some("fallback")` when no signal matched.
    /// Footer suppresses the fallback string.
    vendor_id: Option<String>,
    usage: crate::event::Usage,
    context_chars: usize,
    turn_count: u32,
    /// Cached thinking level from the current Agent, for the footer.
    /// Synced by `refresh_status` (or immediately after SetThinking).
    thinking: Option<crate::agent::thinking::ThinkingLevel>,
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
    /// Tool calls whose bars are not yet drawn, keyed by call id and
    /// kept in arrival order. On the matching tool-result marker we
    /// colour a bar green (success) or red (failure) using the
    /// marker's separator (`→` vs `✗`).
    ///
    /// A `Vec`, not an `Option`: with `tool_exec_mode = "parallel"`
    /// (the default) every `ToolCall` in a batch arrives before the
    /// first `ToolResult`, so a single slot meant the last call
    /// overwrote its predecessors — the first card was then labelled
    /// with the wrong tool and the rest rendered with no header line
    /// at all. Order is preserved for the orphan flush, which has no
    /// ids to match against.
    pending_tool_calls: Vec<(String, PendingBar)>,
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
    /// Set by CancelTurn (Esc / Ctrl+C during streaming); consumed by
    /// the recv-None wrap-up so the transcript gets an
    /// "[Operation aborted]" marker matching PI's UX.
    turn_was_cancelled: bool,
    /// Skill-load policy remembered from startup so `/new`, `/fork`,
    /// `/resume`, and `/model` can rebuild the Agent with the same
    /// discovery rules the user asked for on the command line.
    skill_load: crate::agent::build::SkillLoadPolicy,
    /// `--no-context-files` policy, remembered from startup so `/new`,
    /// `/fork`, `/resume`, and `/model` rebuild the Agent with the same
    /// AGENTS.md/CLAUDE.md discovery rule.
    no_context_files: bool,
    /// `--system-prompt` / `--append-system-prompt` policy, remembered
    /// from startup so `/new`, `/fork`, `/resume`, and `/model` rebuild
    /// the Agent with the same prompt policy the user asked for on the
    /// command line.
    prompt_overrides: crate::agent::prompt_override::PromptOverrides,
    /// `[[extensions]]` remembered from startup so `/new`, `/fork`,
    /// `/resume`, and `/import` rebuild the Agent with the same plugin
    /// set. Kept here rather than re-read per rebuild so a config edit
    /// mid-session can't swap plugins under a live registry — plugin
    /// reload needs an unregister path that doesn't exist yet.
    extensions: Vec<crate::config::ExtensionConfig>,
    /// Most recent skill invocation, captured collapsed on scrollback.
    /// Ctrl-O expands it once. `None` outside a skill invocation.
    last_skill_block: Option<CollapsedSkill>,
    /// Snapshot of agent.skills so `sync_palette` can list `/skill:X`
    /// entries without reaching into the agent (which is behind an
    /// async lock). Refreshed by `refresh_status` after every rebuild.
    skills_cache: Vec<crate::resources::Skill>,
    /// Snapshot of agent.plugin_commands, refreshed by `refresh_status`
    /// alongside `skills_cache` and for the same reason. Always empty
    /// in a build without `--features wasm`.
    commands_cache: Vec<crate::command::PluginCommand>,
    /// Snapshot of agent.event_subscribers.subscriptions(), refreshed by
    /// `refresh_status` alongside `commands_cache`. `(plugin_name, sorted
    /// event names)` per plugin, non-gated (`src/subscriber.rs`), so this
    /// field carries no WASM feature gate into `tui.rs`. Always empty in
    /// a build without `--features wasm`.
    subscriptions_cache: Vec<(String, Vec<String>)>,
    /// In-flight plugin command, run on the blocking pool.
    ///
    /// Not awaited inline: `handle_action` runs inside the `select!`
    /// key arm, and a guest call there would freeze the ticker, the SSE
    /// stream and key handling for up to the full epoch budget — longer
    /// if a model-driven tool call already holds the plugin's mutex,
    /// since epoch interruption cannot preempt a thread parked in
    /// `lock()`. The main loop's tick polls this the same way it polls
    /// `summarize_task`.
    command_task:
        Option<tokio::task::JoinHandle<(String, Result<crate::command::CommandAction, String>)>>,
}

impl App {
    fn new(
        session_id: String,
        model: String,
        cwd: PathBuf,
        api_kind: Option<crate::provider::ApiKind>,
        skill_load: crate::agent::build::SkillLoadPolicy,
        no_context_files: bool,
        prompt_overrides: crate::agent::prompt_override::PromptOverrides,
        extensions: Vec<crate::config::ExtensionConfig>,
        history_path: PathBuf,
    ) -> Self {
        Self {
            input: TextBuffer::with_history(history_path),
            palette: None,
            model_picker: None,
            status: Status::Idle,
            session_id,
            model,
            should_exit: false,
            cwd,
            api_kind,
            bindings: crate::keys::KeyBindings::default(),
            cfg_provider: None,
            inline_think_tags: None,
            vendor_id: None,
            usage: crate::event::Usage::default(),
            context_chars: 0,
            turn_count: 0,
            stream_buf: String::new(),
            thinking_buf: String::new(),
            pending_tool_calls: Vec::new(),
            tool_started_at: None,
            turn_started_at: None,
            status_note: None,
            md_state: crate::render::markdown::MdState::default(),
            last_tool_output: None,
            fork_picker: None,
            last_esc_at: None,
            summary_prompt: None,
            pending_fork: None,
            summarize_task: None,
            thinking: None,
            turn_was_cancelled: false,
            resume_picker: None,
            settings_menu: None,
            keybindings_menu: None,
            capture_key_for: None,
            settings_file: crate::settings_toml::SettingsFile::default(),
            capture: CaptureMode::None,
            skill_load,
            no_context_files,
            prompt_overrides,
            extensions,
            last_skill_block: None,
            skills_cache: Vec::new(),
            commands_cache: Vec::new(),
            subscriptions_cache: Vec::new(),
            command_task: None,
        }
    }
}

/// What the input box should do with the next Enter submit besides
/// starting a chat turn. Each slash command that needs free-form text
/// input flips this into its dedicated mode; the Submit path checks
/// this flag before falling through to StartTurn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CaptureMode {
    None,
    /// After picking "Summarize with custom prompt" on the fork
    /// summary modal — text becomes the summarizer's system prompt.
    CustomSummary,
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
    Ok {
        summary: Option<String>,
        fork: PendingFork,
    },
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

/// Remove and return the pending bar for `call_id`.
///
/// Falls back to the oldest pending entry when the id is unknown,
/// which covers providers that renumber tool_call ids between the
/// stream and the result. Dropping the bar instead would leave a
/// headerless card, and an out-of-order label is still strictly more
/// informative than none.
fn take_pending_bar(app: &mut App, call_id: &str) -> Option<PendingBar> {
    let idx = app
        .pending_tool_calls
        .iter()
        .position(|(id, _)| id == call_id)
        .or(if app.pending_tool_calls.is_empty() {
            None
        } else {
            Some(0)
        })?;
    Some(app.pending_tool_calls.remove(idx).1)
}

#[derive(Debug, Clone)]
struct LastTool {
    content: String,
    is_error: bool,
    expanded: bool,
}

/// A skill invocation captured on scrollback in collapsed form. Ctrl-O
/// expands it once (append-only), matching how tool output expansion
/// works today. Mirrors PI's `SkillInvocationMessageComponent`
/// (skill-invocation-message.ts).
#[derive(Debug, Clone)]
struct CollapsedSkill {
    name: String,
    #[allow(dead_code)]
    location: String,
    base_dir: String,
    body: String,
    user_message: Option<String>,
    expanded: bool,
}

// ─────────────────────────────────────────────────────────────────────
// Key → action
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum KeyAction {
    Nothing,
    StartTurn(String),
    /// v0.11.0: user typed + hit Enter WHILE the agent was streaming.
    /// The text is routed into the running turn's steer channel as a
    /// `SteerMessage::Steering` instead of starting a new turn.
    /// Matches Pi's behavior (type mid-stream → steer the agent).
    SteerTurn(String),
    /// v0.11.0: the user picked a plugin command out of the palette.
    RunPluginCommand {
        name: String,
        args: String,
    },
    /// v0.11.0: the blocking plugin-command task finished. Dispatched
    /// by the main loop's tick, the same way `SummaryFinished` is.
    PluginCommandFinished {
        name: String,
        outcome: Result<crate::command::CommandAction, String>,
    },
    CancelTurn,
    Exit,
    Compact,
    OpenModelPicker,
    SwapModel(String),
    /// Shift+Tab (PI's `app.thinking.cycle`): step through Off →
    /// Minimal → Low → Medium → High → Xhigh → Max → Off and apply
    /// the next level to the current Agent.
    CycleThinking,
    ExpandLastTool,
    /// `/new`: swap the Agent for a fresh session in the same cwd.
    NewSession,
    /// `/resume`: open a picker over sessions on disk.
    OpenResumePicker,
    /// User picked a session in the resume picker.
    ResumeSession(PathBuf),
    /// `/hotkeys`: dump keybinding help into scrollback.
    ShowHotkeys,
    /// v0.9.3: `/settings` — dump interaction settings + toml path.
    ShowSettings,
    /// v0.9.3: `/keybindings` — dump keybindings + toml override key.
    ShowKeybindings,
    /// `/skills`: dump the loaded skill list into scrollback.
    ShowSkills,
    /// `/tools`: dump the live tool registry into scrollback.
    ShowTools,
    /// `/session`: dump usage / cost / model summary into scrollback.
    ShowSessionInfo,
    /// Bare `/name` — print the session's current name to scrollback.
    ShowCurrentName,
    /// `/name X` — set the session name to `X` on the header.
    ApplyName(String),
    /// `/copy`: copy the last assistant message via OSC 52.
    CopyLastReply,
    /// `/export [path]` — write the current session to a file.
    /// Empty path auto-generates `./nanopi-session-<ts>_<8>.html`.
    /// `.jsonl` extension picks JSONL, everything else defaults to HTML.
    ApplyExport(String),
    /// `/import <path>` — load a JSONL and switch to it. Bare
    /// `/import` (empty) prints a usage warning.
    ApplyImport(String),
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
    /// `/reload`: re-read config.toml / settings.toml / skills without
    /// exiting the session. New skills become visible on the next turn.
    Reload,
}

/// Dispatch one key event. Palette (if open) claims navigation keys
/// first; everything else flows through TextBuffer. After the key is
/// handled, the palette's open/closed state is re-synced against the
/// input buffer (open ⇔ first line starts with `/`).
fn interpret_key(app: &mut App, k: KeyEvent) -> KeyAction {
    if let Some(action) = app.capture_key_for {
        if k.code == KeyCode::Esc {
            app.capture_key_for = None;
            app.keybindings_menu = Some(build_keybindings_menu(&app.bindings));
            return KeyAction::Nothing;
        }
        let spec = crate::keys::KeySpec { code: k.code, mods: k.modifiers };
        app.bindings.set(action, spec);
        app.settings_file.keybindings = app.bindings.overrides();
        let _ = crate::settings_toml::save(&app.settings_file);
        app.capture_key_for = None;
        app.keybindings_menu = Some(build_keybindings_menu(&app.bindings));
        return KeyAction::Nothing;
    }
    if app.keybindings_menu.is_some() {
        let m = app.keybindings_menu.as_mut().unwrap();
        match m.handle_key(k) {
            MenuAction::Chosen(action) => { app.keybindings_menu = None; app.capture_key_for = Some(action); return KeyAction::Nothing; }
            MenuAction::Cancel => { app.keybindings_menu = None; app.settings_menu = Some(build_settings_menu(&app.settings_file)); return KeyAction::Nothing; }
            MenuAction::Nothing | MenuAction::Filled(_) | MenuAction::ChosenRaw(_) => {
                return KeyAction::Nothing
            }
        }
    }
    if app.settings_menu.is_some() {
        let m = app.settings_menu.as_mut().unwrap();
        match m.handle_key(k) {
            MenuAction::Chosen(row) => {
                if row == SettingsRow::Keybindings {
                    app.settings_menu = None;
                    app.keybindings_menu = Some(build_keybindings_menu(&app.bindings));
                } else {
                    cycle_settings_row(app, row);
                    app.settings_menu = Some(build_settings_menu(&app.settings_file));
                }
                return KeyAction::Nothing;
            }
            MenuAction::Cancel => { app.settings_menu = None; return KeyAction::Nothing; }
            MenuAction::Nothing | MenuAction::Filled(_) | MenuAction::ChosenRaw(_) => {
                return KeyAction::Nothing
            }
        }
    }
    // Ctrl+O — expand the last tool output (PI's `app.tools.expand`).
    if app.bindings.matches(crate::keys::ActionId::ExpandLastTool, k) {
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
            // Tab in a modal picker is a no-op — nothing to autocomplete
            // when the user is choosing one of a fixed short list.
            MenuAction::Nothing | MenuAction::Filled(_) | MenuAction::ChosenRaw(_) => {
                return KeyAction::Nothing
            }
        }
    }
    // ── Resume picker ───────────────────────────────────────────────
    if app.resume_picker.is_some() {
        let m = app.resume_picker.as_mut().unwrap();
        match m.handle_key(k) {
            MenuAction::Chosen(path) => {
                app.resume_picker = None;
                app.input.clear();
                return KeyAction::ResumeSession(path);
            }
            MenuAction::Cancel => {
                app.resume_picker = None;
                app.input.clear();
                return KeyAction::Nothing;
            }
            MenuAction::Nothing | MenuAction::Filled(_) | MenuAction::ChosenRaw(_) => {
                return KeyAction::Nothing
            }
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
            MenuAction::Nothing | MenuAction::Filled(_) | MenuAction::ChosenRaw(_) => {
                return KeyAction::Nothing
            }
        }
    }

    // ── Esc during a streaming turn = interrupt (PI's app.interrupt,
    // keybindings.ts:66). Cancels the current run; the turn wrap-up
    // will insert an "[Operation aborted]" marker.
    if k.code == KeyCode::Esc && k.modifiers.is_empty() && app.status == Status::Streaming {
        return KeyAction::CancelTurn;
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

    // ── Shift+Tab cycles thinking level (PI's app.thinking.cycle). ──
    // Fires regardless of streaming state — advancing the setting is
    // safe mid-turn (next turn picks it up), and PI allows the same.
    if app.bindings.matches(crate::keys::ActionId::ThinkingCycle, k) {
        return KeyAction::CycleThinking;
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
            // Typed an id the catalogue doesn't have — take it at face
            // value. The catalogue always trails new model releases, and
            // being unable to name a model your key can serve is the
            // exact problem this picker had.
            MenuAction::ChosenRaw(typed) => {
                app.model_picker = None;
                app.input.clear();
                return KeyAction::SwapModel(typed);
            }
            MenuAction::Cancel => {
                app.model_picker = None;
                app.input.clear();
                return KeyAction::Nothing;
            }
            MenuAction::Nothing | MenuAction::Filled(_) => return KeyAction::Nothing,
        }
    }
    // ── Palette owns navigation keys when open. ─────────────────────
    if app.palette.is_some() {
        let claims_nav = matches!(
            k.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Esc | KeyCode::Tab
        ) || (k.code == KeyCode::Enter
            && !k.modifiers.contains(KeyModifiers::SHIFT));
        if claims_nav {
            let m = app.palette.as_mut().unwrap();
            match m.handle_key(k) {
                MenuAction::Chosen(cmd) => {
                    // Split off any inline argument the user typed
                    // after the command, e.g. "/name my-experiment".
                    // dispatch_slash gets to see it so commands like
                    // /name can act immediately without a capture
                    // step (PI parity, see interactive-mode.ts:5701).
                    let full = app.input.as_string();
                    let arg = slash_line(&full)
                        .split_once(' ')
                        .map(|(_, rest)| rest.trim().to_string())
                        .unwrap_or_default();
                    app.palette = None;
                    app.input.clear();
                    return dispatch_slash(cmd, arg);
                }
                MenuAction::Filled(label) => {
                    // Tab-complete: replace input with the label + a
                    // trailing space so the user can type args or hit
                    // Enter to commit. Palette stays open — sync_palette
                    // below rebuilds the filter against the new input,
                    // so `/skill:name ` produces an empty match set and
                    // the palette collapses naturally.
                    app.input.clear();
                    app.input.insert_str(&format!("{label} "));
                    return KeyAction::Nothing;
                }
                MenuAction::Cancel => {
                    app.palette = None;
                    app.input.clear();
                    return KeyAction::Nothing;
                }
                // The palette's filter is driven externally by
                // sync_palette, so it is never a free-text menu and
                // ChosenRaw never fires.
                MenuAction::Nothing | MenuAction::ChosenRaw(_) => {
                    // Enter on a filter that matches nothing means the
                    // user is writing prose, not invoking a command —
                    // "/etc/nginx/nginx.conf is misconfigured" is a
                    // question, not a typo. Send it, matching PI, where
                    // an unmatched `/word` falls through to the LLM as
                    // an ordinary message and there is no
                    // "unknown command" error anywhere
                    // (`agent-session.ts:1122-1129` runs the extension
                    // lookup, then lets unhandled text continue).
                    //
                    // Before slash_line() trimmed for the palette, a
                    // leading space was the only way to send such a
                    // line; that accident is gone, so this is the
                    // replacement — and it also frees the user from
                    // being stuck on "(no matches)" with text they
                    // cannot submit.
                    if k.code == KeyCode::Enter
                        && app.palette.as_ref().is_some_and(|m| m.is_empty())
                    {
                        let text = app.input.as_string();
                        app.palette = None;
                        app.input.clear();
                        return submit_or_chat(app, text);
                    }
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
            let mode = std::mem::replace(&mut app.capture, CaptureMode::None);
            match mode {
                CaptureMode::CustomSummary => {
                    let t = text.trim().to_string();
                    if t.is_empty() {
                        KeyAction::SummaryChosen(SummaryChoice::DefaultSummary)
                    } else {
                        KeyAction::RunCustomSummary(t)
                    }
                }
                CaptureMode::None => submit_or_chat(app, text),
            }
        }
    };
    sync_palette(app);
    out
}

/// Hand a mid-stream message to the running turn.
///
/// `Ok(msg)` means it reached the turn; `Err(msg)` hands the text back
/// because there is no live turn to steer — the caller queues it as the
/// next turn rather than dropping it. Both arms return the message so
/// the caller can echo it either way.
///
/// Extracted from `handle_action` purely so this decision is reachable
/// from a test: `handle_action` needs a live `Term` and agent slot,
/// which is why the silent-drop bug survived in there in the first
/// place.
async fn try_steer(
    steer_tx: Option<&mpsc::Sender<SteerMessage>>,
    msg: String,
) -> Result<String, String> {
    let stx = match steer_tx {
        Some(s) => s,
        // No turn has ever run. Not reachable through the TUI today,
        // since steering is only offered while streaming, but handled
        // rather than assumed away.
        None => return Err(msg),
    };
    // Cloned so the message survives a failed send. `SendError` does
    // return the payload, but unwrapping it back out of the enum is
    // more code than copying a line of user input.
    match stx.send(SteerMessage::Steering { text: msg.clone() }).await {
        Ok(()) => Ok(msg),
        // The receiver is gone: `run_turn` returned between the
        // keypress and now.
        Err(_) => Err(msg),
    }
}

/// Interpret a bare Enter submit (no capture mode active) as either
/// exit, compact, or a chat turn. Extracted so the Submit path in
/// interpret_key stays readable now that capture modes have their
/// own branches.
/// Only ever sees text the palette declined, so it no longer needs to
/// recognise command names. It used to carry its own list of three
/// (`/quit`, `/exit`, `/compact`) — a third source of truth beside
/// `slash_items()` and `dispatch_slash`, and the reason a leading space
/// routed the other fourteen commands to the model. The palette is now
/// the only thing that resolves a command; anything reaching here is
/// prose, including prose that happens to start with `/`.
fn submit_or_chat(app: &App, text: String) -> KeyAction {
    let t = text.trim();
    if t.is_empty() {
        return KeyAction::Nothing;
    }
    if app.status == Status::Streaming {
        // v0.11.0: mid-stream Enter steers the running turn instead of
        // silently doing nothing.
        return KeyAction::SteerTurn(t.to_string());
    }
    KeyAction::StartTurn(t.to_string())
}

/// The single line every slash path parses: the first line of the
/// buffer, with leading whitespace removed.
///
/// Everything that inspects slash input MUST go through this.
/// `sync_palette` used to test the raw line while `submit_or_chat`
/// trimmed, so one leading space closed the palette and handed
/// `" /session"` to `submit_or_chat` — which knew only three command
/// names and forwarded the other fourteen to the model as chat text.
fn slash_line(input: &str) -> &str {
    input.lines().next().unwrap_or("").trim_start()
}

/// Sync palette state with the input buffer: open if the first line
/// starts with `/`, else close. Also refreshes the filter query. When
/// skills are loaded, their `/skill:<name>` entries are appended to
/// the built-in list — mirrors PI's autocomplete provider
/// (interactive-mode.ts:649-661).
fn sync_palette(app: &mut App) {
    let full = app.input.as_string();
    let first_line = slash_line(&full);
    if first_line.starts_with('/') {
        if app.palette.is_none() {
            // Built-ins first, then skills, then plugin commands: a
            // built-in always sorts above a plugin row, so even if the
            // reserved-name guard ever failed the built-in stays
            // reachable.
            let mut items = slash_items();
            items.extend(skill_menu_items(&app.skills_cache));
            items.extend(command_menu_items(&app.commands_cache));
            app.palette = Some(MenuState::new(items));
        }
        if let Some(m) = app.palette.as_mut() {
            // Filter only on the command WORD (chars up to the first
            // whitespace). Anything after the space is the command's
            // argument — e.g. "/name 123" filters as "name" so the
            // /name item stays selectable, and the "123" is picked
            // up by dispatch_slash's arg-splitter on Enter.
            let after_slash = first_line.strip_prefix('/').unwrap_or("");
            let filter = after_slash
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            m.set_filter(filter);
        }
    } else if app.palette.is_some() {
        app.palette = None;
    }
}

fn dispatch_slash(cmd: SlashCmd, arg: String) -> KeyAction {
    match cmd {
        SlashCmd::Compact => KeyAction::Compact,
        SlashCmd::Quit => KeyAction::Exit,
        SlashCmd::Model => KeyAction::OpenModelPicker,
        SlashCmd::NewSession => KeyAction::NewSession,
        SlashCmd::Resume => KeyAction::OpenResumePicker,
        SlashCmd::Fork => KeyAction::OpenForkPicker,
        SlashCmd::Hotkeys => KeyAction::ShowHotkeys,
        SlashCmd::ListSkills => KeyAction::ShowSkills,
        SlashCmd::ListTools => KeyAction::ShowTools,
        SlashCmd::SessionInfo => KeyAction::ShowSessionInfo,
        // PI's /name: bare shows current, `/name X` sets it. See
        // packages/coding-agent/src/modes/interactive/interactive-
        // mode.ts:5699-5721.
        SlashCmd::Name => {
            if arg.is_empty() {
                KeyAction::ShowCurrentName
            } else {
                KeyAction::ApplyName(arg)
            }
        }
        SlashCmd::Copy => KeyAction::CopyLastReply,
        // PI's /export (interactive-mode.ts:5501-5544):
        //   * bare `/export` → HTML file in cwd, auto-named
        //   * `/export <path.html>` → HTML at that path
        //   * `/export <path.jsonl>` → JSONL at that path
        //   * `/export <path>` no extension → HTML at path.html
        SlashCmd::Export => KeyAction::ApplyExport(arg),
        // /import always inline; PI errors on bare /import.
        SlashCmd::Import => KeyAction::ApplyImport(arg),
        // /skill:<name>: hand back a StartTurn with the full raw
        // string so run_turn's expander does the heavy lifting.
        // Matches PI's approach — the palette entry is UX; expansion
        // is done in one place (agent-session.ts:1301).
        SlashCmd::Skill(name) => {
            let full = if arg.is_empty() {
                format!("/skill:{name}")
            } else {
                format!("/skill:{name} {arg}")
            };
            KeyAction::StartTurn(full)
        }
        SlashCmd::Reload => KeyAction::Reload,
        SlashCmd::Settings => KeyAction::ShowSettings,
        SlashCmd::Keybindings => KeyAction::ShowKeybindings,
        // `arg` is already the trimmed remainder after the first space,
        // computed by the caller. Reusing it rather than reimplementing
        // PI's split keeps plugin commands consistent with `/name`,
        // `/export` and `/import`; a plugin command that treated
        // whitespace differently would be the odd one out.
        SlashCmd::Plugin(name) => KeyAction::RunPluginCommand { name, args: arg },
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
    let mut steer_tx_slot: Option<mpsc::Sender<SteerMessage>> = None;
    // A queue, not a slot: two messages can land in the dead window
    // between a turn ending and the completion handler running, and a
    // single slot silently kept only the second — after echoing both.
    let mut follow_up_slot: std::collections::VecDeque<String> =
        std::collections::VecDeque::new();
    let mut turn_task: Option<tokio::task::JoinHandle<Result<String, String>>> = None;
    let mut cancel: Option<CancellationToken> = None;
    // 120ms ticker keeps the "Elapsed X.Xs" / spinner glyph moving
    // while nothing else is happening. First tick fires immediately;
    // set reset_missed so if the runtime is busy we don't queue up
    // stale ticks.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Initial dock draw.
    term.draw(|f| {
        let area = f.area();
        draw_dock(f.buffer_mut(), area, app);
    })?;

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
                                    &mut ag_rx, &mut steer_tx_slot, &mut follow_up_slot, &mut cancel, &mut turn_task,
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
                // Same shape for a finished plugin command. It runs on
                // the blocking pool precisely so this arm keeps running
                // — awaiting the guest call inline would freeze the
                // ticker, the stream, and key handling at once.
                if let Some(task) = app.command_task.take() {
                    if task.is_finished() {
                        match task.await {
                            Ok((name, outcome)) => {
                                handle_action(
                                    KeyAction::PluginCommandFinished { name, outcome },
                                    app, term, &agent_slot,
                                    &mut ag_rx, &mut steer_tx_slot, &mut follow_up_slot, &mut cancel, &mut turn_task,
                                ).await?;
                            }
                            Err(_join_err) => {
                                app.status_note = None;
                                insert_line(term, Line::from(vec![
                                    Span::styled(
                                        "[plugin command panicked]",
                                        Style::default().fg(Color::Red),
                                    ),
                                ]))?;
                            }
                        }
                    } else {
                        app.command_task = Some(task);
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
                                      &mut ag_rx, &mut steer_tx_slot, &mut follow_up_slot,
                                      &mut cancel, &mut turn_task).await?;
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
                        // Every call still in flight is orphaned, not
                        // just the newest one — a cancelled parallel
                        // batch leaves several.
                        for (_, pending) in std::mem::take(&mut app.pending_tool_calls) {
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
                        // Esc / Ctrl+C during streaming → mark the turn
                        // as aborted in the transcript, PI-style. See
                        // interactive-mode.ts:3040-3045 where PI emits
                        // "Operation aborted" for stopReason=="aborted".
                        if app.turn_was_cancelled {
                            app.turn_was_cancelled = false;
                            insert_line(term, Line::from(vec![
                                Span::styled(
                                    "Operation aborted",
                                    Style::default()
                                        .fg(Color::Indexed(131))
                                        .add_modifier(Modifier::ITALIC),
                                ),
                            ]))?;
                        }
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

                        // v0.11.0: auto-start the next turn instead of
                        // idling back to the prompt (Pi's
                        // getFollowUpMessages semantic). Two sources
                        // feed this, and they cannot both be one field:
                        // a `SteerMessage::FollowUp` handled inside the
                        // turn lands on `agent.pending_follow_ups`, but a
                        // steer that missed its turn is noticed out here,
                        // in a window where the agent has been taken out
                        // of the slot and cannot be written to.
                        let follow_up = {
                            let mut g = agent_slot.lock().await;
                            g.as_mut().and_then(|a| a.pending_follow_ups.pop_front())
                        }
                        .or_else(|| follow_up_slot.pop_front());
                        if let Some(text) = follow_up {
                            handle_action(
                                KeyAction::StartTurn(text),
                                app, term, &agent_slot,
                                &mut ag_rx, &mut steer_tx_slot, &mut follow_up_slot, &mut cancel, &mut turn_task,
                            ).await?;
                        }
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
    steer_tx: &mut Option<mpsc::Sender<SteerMessage>>,
    // Text the user typed mid-stream that arrived too late to steer.
    // Drained by the turn-completion handler, which starts it as the
    // next turn instead of letting it vanish.
    follow_up: &mut std::collections::VecDeque<String>,
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
                app.turn_was_cancelled = true;
            }
        }
        KeyAction::ExpandLastTool => {
            // Only expand once per source. Priority: tool result first
            // (matches historical behavior), otherwise the last skill
            // block. Expand-once model — collapse-back is BACKLOG.md.
            let tool = match app.last_tool_output.take() {
                Some(l) if !l.expanded => Some(l),
                other => {
                    app.last_tool_output = other;
                    None
                }
            };
            if let Some(l) = tool {
                render_tool_expansion(term, &l.content, l.is_error)?;
                app.last_tool_output = None;
            } else {
                let skill = match app.last_skill_block.take() {
                    Some(s) if !s.expanded => Some(s),
                    other => {
                        app.last_skill_block = other;
                        None
                    }
                };
                if let Some(s) = skill {
                    render_skill_expansion(term, &s)?;
                    app.last_skill_block = None;
                }
            }
        }
        KeyAction::OpenModelPicker => {
            // Offer the models the ACTIVE vendor serves. Listing every
            // model we know about would mostly offer things the current
            // API key can't reach, which just turns into a 401 after the
            // switch. PI scopes its picker the same way ("Only showing
            // models from configured providers").
            let (current, base_url) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => (a.model.clone(), a.base_url.clone()),
                    None => (String::new(), String::new()),
                }
            };
            let vendor =
                crate::vendor::pick_vendor(app.cfg_provider.as_deref(), Some(&base_url), &current);
            let items: Vec<MenuItem<String>> = crate::models::models_for_vendor(vendor.id())
                .into_iter()
                .map(|m| {
                    let marker = if current == m.id { "  (current)" } else { "" };
                    MenuItem::new(
                        m.id.to_string(),
                        format!(
                            "{} ctx{}",
                            crate::models::fmt_tokens(m.context_window),
                            marker
                        ),
                        m.id.to_string(),
                    )
                })
                .collect();
            app.model_picker = Some(MenuState::new(items).with_free_text());
        }
        KeyAction::CycleThinking => {
            use crate::agent::thinking::ThinkingLevel;
            let (current, model) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => (a.context.thinking, a.model.clone()),
                    None => return Ok(()),
                }
            };
            // Cycle: None → Minimal → Low → Medium → High → Xhigh → Max → None
            let next: Option<ThinkingLevel> = match current {
                None => Some(ThinkingLevel::Minimal),
                Some(ThinkingLevel::Minimal) => Some(ThinkingLevel::Low),
                Some(ThinkingLevel::Low) => Some(ThinkingLevel::Medium),
                Some(ThinkingLevel::Medium) => Some(ThinkingLevel::High),
                Some(ThinkingLevel::High) => Some(ThinkingLevel::Xhigh),
                Some(ThinkingLevel::Xhigh) => Some(ThinkingLevel::Max),
                Some(ThinkingLevel::Max) => None,
            };
            {
                let mut g = agent_slot.lock().await;
                if let Some(a) = g.as_mut() {
                    a.context.thinking = next;
                }
            }
            app.thinking = next;
            let label = match next {
                None => "off".to_string(),
                Some(l) => l.to_string(),
            };
            let note = if !crate::agent::thinking::supports_thinking(&model) && next.is_some() {
                "  (not supported by this model)"
            } else {
                ""
            };
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!("[thinking → {}{}]", label, note),
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::ITALIC),
                )]),
            )?;
        }
        KeyAction::SwapModel(new_model) => {
            let mut g = agent_slot.lock().await;
            if let Some(a) = g.as_mut() {
                let new_provider =
                    crate::provider::build(app.api_kind, &a.base_url, &a.api_key, &new_model, Some(crate::vendor::pick_vendor(app.cfg_provider.as_deref(), Some(&a.base_url), &new_model)), app.inline_think_tags);
                a.provider = new_provider;
                a.model = new_model.clone();
                app.model = new_model.clone();
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("[model → {}]", new_model),
                        Style::default()
                            .fg(Color::Indexed(108))
                            .add_modifier(Modifier::ITALIC),
                    )]),
                )?;
            }
        }
        KeyAction::NewSession => {
            // Fresh session in the same cwd. Preserves model, api,
            // hooks, permission from the current agent.
            let (cwd, model, base_url, api_key, permission, hooks) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => (
                        a.cwd.clone(),
                        a.model.clone(),
                        a.base_url.clone(),
                        a.api_key.clone(),
                        a.permission.clone(),
                        a.hooks.clone(),
                    ),
                    None => return Ok(()),
                }
            };
            let (new_path, new_header) = match session::new_session(&cwd, &model, &base_url) {
                Ok(t) => t,
                Err(e) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[/new failed: {e}]"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                    return Ok(());
                }
            };
            let _ = session::set_active_session(&cwd, &new_path);
            let registry = crate::tool::ToolRegistry::standard();
            // `/new` rebuilds the agent from scratch, so re-read
            // config for exec mode + extensions the same way startup
            // does. Picks up an edited config.toml without a restart.
            let cfg_now = crate::config::load_config(&cwd).unwrap_or_default();
            let (new_agent, diags) = Agent::build_fresh(crate::agent::build::AgentBuildInputs {
                cwd: cwd.clone(),
                registry,
                provider: crate::provider::build(app.api_kind, &base_url, &api_key, &model, Some(crate::vendor::pick_vendor(app.cfg_provider.as_deref(), Some(&base_url), &model)), app.inline_think_tags),
                session_path: new_path,
                session_id: new_header.id.clone(),
                permission,
                hooks,
                model: model.clone(),
                base_url,
                api_key,
                skill_load: app.skill_load.clone(),
                no_context_files: app.no_context_files,
                prompt_overrides: app.prompt_overrides.clone(),
                initial_follow_up: None,
                tool_exec_mode: cfg_now.tool_exec_mode,
                extensions: cfg_now.extensions,
            });
            crate::agent::build::print_skill_diagnostics(&diags);
            swap_agent_with_reason(agent_slot, new_agent, "new").await;
            app.session_id = new_header.id.clone();
            app.usage = crate::event::Usage::default();
            app.context_chars = 0;
            app.turn_count = 0;
            app.input.clear();
            app.thinking = None;
            // PI's /new clears the visible chat area but does NOT
            // purge terminal scrollback — users can still scroll up
            // to review earlier turns (see interactive-mode.ts:5938
            // handleClearCommand). ESC[2J clears the visible screen,
            // ESC[H homes the cursor; scrollback stays intact.
            use std::io::Write;
            let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
            let _ = std::io::stdout().flush();
            let _ = term.clear();
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!(
                        "✓ New session started ({})",
                        &new_header.id.to_string()[..8]
                    ),
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::BOLD),
                )]),
            )?;
            refresh_status(app, &agent_slot).await;
        }
        KeyAction::OpenResumePicker => {
            let (cwd, current_path) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => (a.cwd.clone(), a.session_path.clone()),
                    None => return Ok(()),
                }
            };
            let items_meta = session::list_sessions_for_cwd(&cwd, Some(&current_path));
            if items_meta.is_empty() {
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        "[no other sessions in this cwd]",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )]),
                )?;
                return Ok(());
            }
            let items: Vec<MenuItem<PathBuf>> = items_meta
                .into_iter()
                .map(|s| {
                    let short = &s.header.id.to_string()[..8];
                    let preview = if s.preview.is_empty() {
                        "(no user messages)".to_string()
                    } else {
                        s.preview
                    };
                    MenuItem::new(preview, format!("{} · {}", short, s.header.model), s.path)
                })
                .collect();
            app.resume_picker = Some(MenuState::new(items));
        }
        KeyAction::ResumeSession(path) => {
            // Snapshot the caller's Agent trappings (api/hooks/etc.)
            // then swap the underlying session by loading `path`.
            let (cwd, model, base_url, api_key, permission, hooks) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => (
                        a.cwd.clone(),
                        a.model.clone(),
                        a.base_url.clone(),
                        a.api_key.clone(),
                        a.permission.clone(),
                        a.hooks.clone(),
                    ),
                    None => return Ok(()),
                }
            };
            let mut new_agent = match Agent::load_session(&path, &cwd) {
                Ok(a) => a,
                Err(e) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[/resume load failed: {e}]"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                    return Ok(());
                }
            };
            let provider = crate::provider::build(app.api_kind, &base_url, &api_key, &model, Some(crate::vendor::pick_vendor(app.cfg_provider.as_deref(), Some(&base_url), &model)), app.inline_think_tags);
            let registry = crate::tool::ToolRegistry::standard();
            new_agent.context.tools = registry.all_specs();
            let diags = new_agent.hydrate_resumed(
                provider,
                registry,
                permission,
                hooks,
                model.clone(),
                base_url,
                api_key,
                app.skill_load.clone(),
                app.no_context_files,
                app.prompt_overrides.clone(),
                &app.extensions,
            );
            crate::agent::build::print_skill_diagnostics(&diags);
            let new_session_id = new_agent.session_id.clone();
            let _ = session::set_active_session(&cwd, &path);
            swap_agent_with_reason(agent_slot, new_agent, "resume").await;
            app.session_id = new_session_id.clone();
            app.usage = crate::event::Usage::default();
            app.context_chars = 0;
            app.turn_count = 0;
            app.input.clear();
            app.thinking = None;
            refresh_status(app, agent_slot).await;

            // Redraw the resumed session's transcript into scrollback
            // so users see the full history they're picking up on
            // (matches PI's /resume — the whole conversation gets
            // repainted, not just a "resumed" marker).
            let entries = match session::read_session(&path) {
                Ok((_, e)) => e,
                Err(_) => Vec::new(),
            };
            // Clear visible screen first so the replay starts at the
            // top of the view. Scrollback stays; ESC[2J only clears
            // the viewport, not the terminal's history buffer.
            use std::io::Write as _;
            let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
            let _ = std::io::stdout().flush();
            let _ = term.clear();
            let short = crate::render::status_line::short_session_id(&new_session_id);
            let short = short.as_str();
            let hdr_name = session::read_session(&path).ok().and_then(|(h, _)| h.name);
            let title = match hdr_name {
                Some(n) => format!("─── Resumed session {short} · {n} ───"),
                None => format!("─── Resumed session {short} ───"),
            };
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::BOLD),
                )]),
            )?;
            insert_line(term, Line::from(""))?;
            replay_history(term, app, &entries)?;
        }
        KeyAction::ShowSessionInfo => {
            let (session_id, model, turn_count, usage, ctx_chars, name) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => {
                        // Header on disk carries the /name value.
                        let name = session::read_session(&a.session_path)
                            .ok()
                            .and_then(|(h, _)| h.name);
                        (
                            a.session_id.to_string(),
                            a.model.clone(),
                            a.turn_count,
                            a.usage_total.clone(),
                            a.context.estimate_chars(),
                            name,
                        )
                    }
                    None => return Ok(()),
                }
            };
            let ctx_pct = crate::render::status_line::context_percent(&model, ctx_chars)
                .map(|p| format!("{p:.1}%"))
                .unwrap_or_else(|| "?%".into());
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    "Session info",
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::BOLD),
                )]),
            )?;
            let dim = Style::default().fg(Color::DarkGray);
            let cyan = Style::default().fg(Color::Cyan);
            let row = |k: &str, v: &str| -> Line {
                Line::from(vec![
                    Span::styled(format!("  {:<12}", k), dim),
                    Span::styled(v.to_string(), cyan),
                ])
            };
            if let Some(n) = name.as_ref() {
                insert_line(term, row("name", n))?;
            }
            insert_line(
                term,
                row("id", &session_id[..std::cmp::min(session_id.len(), 8)]),
            )?;
            insert_line(term, row("model", &model))?;
            insert_line(term, row("turns", &turn_count.to_string()))?;
            insert_line(
                term,
                row(
                    "tokens",
                    &format!(
                        "↑{}k ↓{}k (cache R{}k W{}k)",
                        usage.input_tokens / 1000,
                        usage.output_tokens / 1000,
                        usage.cache_read_tokens / 1000,
                        usage.cache_write_tokens / 1000,
                    ),
                ),
            )?;
            insert_line(
                term,
                row("context", &format!("{ctx_pct} ({ctx_chars} chars)")),
            )?;
            insert_line(term, Line::from(""))?;
        }
        KeyAction::ShowCurrentName => {
            let current_name = {
                let g = agent_slot.lock().await;
                g.as_ref().and_then(|a| {
                    session::read_session(&a.session_path)
                        .ok()
                        .and_then(|(h, _)| h.name)
                })
            };
            match current_name {
                None => {
                    // No name set — PI prints a usage warning
                    // (interactive-mode.ts:5701).
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            "Warning: Usage: /name <name>",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )]),
                    )?;
                }
                Some(n) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("Session name: {n}"),
                            Style::default().fg(Color::Indexed(108)),
                        )]),
                    )?;
                }
            }
        }
        KeyAction::ApplyName(new_name) => {
            app.status_note = None;
            let (path, name_to_write) = {
                let g = agent_slot.lock().await;
                let a = match g.as_ref() {
                    Some(a) => a,
                    None => return Ok(()),
                };
                let n = if new_name.is_empty() {
                    None
                } else {
                    Some(new_name.clone())
                };
                (a.session_path.clone(), n)
            };
            match session::set_session_name(&path, name_to_write.clone()) {
                Ok(()) => {
                    // PI wording (interactive-mode.ts:5715):
                    // "Session name set: <name>".
                    let text = match name_to_write.as_deref() {
                        Some(n) => format!("Session name set: {n}"),
                        None => "Session name cleared".to_string(),
                    };
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            text,
                            Style::default().fg(Color::Indexed(108)),
                        )]),
                    )?;
                }
                Err(e) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[/name failed: {e}]"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                }
            }
        }
        KeyAction::CopyLastReply => {
            // Find the last Assistant text block in context.
            let last_reply = {
                let g = agent_slot.lock().await;
                g.as_ref().and_then(|a| {
                    a.context.messages.iter().rev().find_map(|m| match m {
                        crate::agent::context::ContextMessage::Assistant { content } => {
                            let text: String = content
                                .iter()
                                .filter_map(|b| match b {
                                    crate::agent::context::AssistantBlock::Text { text } => {
                                        Some(text.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            if text.trim().is_empty() {
                                None
                            } else {
                                Some(text)
                            }
                        }
                        _ => None,
                    })
                })
            };
            match last_reply {
                Some(text) => {
                    // OSC 52 clipboard write. Modern terminals (iTerm2,
                    // Alacritty, WezTerm, tmux with `set-clipboard on`,
                    // most native emulators) honor this. Fallback: user
                    // sees the escape output visibly if the terminal
                    // strips it, no harm done.
                    use base64::Engine;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
                    use std::io::Write;
                    // Write to stdout so crossterm's raw-mode-managed
                    // TTY handles it; the sequence is invisible.
                    let _ = write!(std::io::stdout(), "\x1b]52;c;{}\x1b\\", b64);
                    let _ = std::io::stdout().flush();
                    let preview: String = text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(60)
                        .collect();
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[copied last reply ({} bytes) — {}]", text.len(), preview),
                            Style::default()
                                .fg(Color::Indexed(108))
                                .add_modifier(Modifier::ITALIC),
                        )]),
                    )?;
                }
                None => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            "[/copy: no assistant reply to copy yet]",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )]),
                    )?;
                }
            }
        }
        KeyAction::ApplyExport(typed_path) => {
            let (session_path, session_id, cwd) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => (a.session_path.clone(), a.session_id.clone(), a.cwd.clone()),
                    None => return Ok(()),
                }
            };
            // Resolve final target path + format. PI's naming shape:
            // `pi-session-<ISO>_<uuid>.html`; nanopi uses the same
            // ISO-plus-shortid layout but `nanopi-` prefixed.
            let short = &session_id.to_string()[..8];
            let target: PathBuf = if typed_path.is_empty() {
                // ISO timestamp, filename-safe (colons → dashes).
                let ts = crate::util::time::now_iso8601().replace(':', "-");
                cwd.join(format!("nanopi-session-{ts}_{short}.html"))
            } else {
                let mut p = std::path::PathBuf::from(&typed_path);
                if p.is_absolute() {
                    p.clone()
                } else {
                    p = cwd.join(&p);
                    p
                }
            };
            // Extension → format. .jsonl = raw session dump; anything
            // else (including no extension) = HTML (matches PI's default).
            let is_jsonl = target
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("jsonl"))
                .unwrap_or(false);
            let effective_target: PathBuf = if is_jsonl {
                target
            } else if target.extension().is_none() {
                // No extension → default to .html so the browser opens it.
                let mut t = target.clone();
                t.set_extension("html");
                t
            } else {
                target
            };
            let result: std::io::Result<u64> = if is_jsonl {
                // JSONL export = plain file copy of the on-disk session.
                std::fs::copy(&session_path, &effective_target)
            } else {
                // HTML export = render header + entries with the export
                // template, then write.
                match session::read_session(&session_path) {
                    Ok((hdr, entries)) => {
                        let html = crate::render::export_html::build(&hdr, &entries);
                        std::fs::write(&effective_target, &html).map(|_| html.len() as u64)
                    }
                    Err(e) => {
                        insert_line(
                            term,
                            Line::from(vec![Span::styled(
                                format!("[/export read failed: {e}]"),
                                Style::default().fg(Color::Red),
                            )]),
                        )?;
                        return Ok(());
                    }
                }
            };
            match result {
                Ok(bytes) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!(
                                "Session exported to: {} ({} bytes)",
                                effective_target.display(),
                                bytes
                            ),
                            Style::default().fg(Color::Indexed(108)),
                        )]),
                    )?;
                }
                Err(e) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[/export failed: {e}]"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                }
            }
        }
        KeyAction::ApplyImport(typed_path) => {
            if typed_path.is_empty() {
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        "Warning: Usage: /import <path.jsonl>",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )]),
                )?;
                return Ok(());
            }
            let (cwd, model, base_url, api_key, permission, hooks) = {
                let g = agent_slot.lock().await;
                match g.as_ref() {
                    Some(a) => (
                        a.cwd.clone(),
                        a.model.clone(),
                        a.base_url.clone(),
                        a.api_key.clone(),
                        a.permission.clone(),
                        a.hooks.clone(),
                    ),
                    None => return Ok(()),
                }
            };
            let source: PathBuf = {
                let p = PathBuf::from(&typed_path);
                if p.is_absolute() {
                    p
                } else {
                    cwd.join(p)
                }
            };
            // Copy the source into sessions_dir so it participates in
            // /resume + --continue like every other session.
            let dest = match session::sessions_dir() {
                Some(dir) => {
                    let _ = std::fs::create_dir_all(&dir);
                    let new_id = crate::util::uuid::v7();
                    dir.join(format!("{new_id}.jsonl"))
                }
                None => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            "[/import: cannot locate sessions dir]",
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                    return Ok(());
                }
            };
            if let Err(e) = std::fs::copy(&source, &dest) {
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("[/import copy failed: {e}]"),
                        Style::default().fg(Color::Red),
                    )]),
                )?;
                return Ok(());
            }
            // Validate we can load it as a session.
            let mut new_agent = match Agent::load_session(&dest, &cwd) {
                Ok(a) => a,
                Err(e) => {
                    let _ = std::fs::remove_file(&dest);
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[/import load failed: {e}]"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                    return Ok(());
                }
            };
            let provider = crate::provider::build(app.api_kind, &base_url, &api_key, &model, Some(crate::vendor::pick_vendor(app.cfg_provider.as_deref(), Some(&base_url), &model)), app.inline_think_tags);
            let registry = crate::tool::ToolRegistry::standard();
            new_agent.context.tools = registry.all_specs();
            let diags = new_agent.hydrate_resumed(
                provider,
                registry,
                permission,
                hooks,
                model.clone(),
                base_url,
                api_key,
                app.skill_load.clone(),
                app.no_context_files,
                app.prompt_overrides.clone(),
                &app.extensions,
            );
            crate::agent::build::print_skill_diagnostics(&diags);
            let new_session_id = new_agent.session_id.clone();
            let _ = session::set_active_session(&cwd, &dest);
            swap_agent_with_reason(agent_slot, new_agent, "import").await;
            app.session_id = new_session_id.clone();
            app.usage = crate::event::Usage::default();
            app.context_chars = 0;
            app.turn_count = 0;
            app.input.clear();
            app.thinking = None;
            refresh_status(app, agent_slot).await;
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!(
                        "[imported → new session {} (from {})]",
                        crate::render::status_line::short_session_id(&new_session_id),
                        source.display(),
                    ),
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::ITALIC),
                )]),
            )?;
        }
        KeyAction::ShowHotkeys => {
            let hotkey_lines: &[(&str, &str)] = &[
                ("Enter", "submit prompt"),
                ("Shift+Enter", "insert newline (multi-line input)"),
                (
                    "Esc",
                    "interrupt streaming turn / open fork picker on empty (double-tap)",
                ),
                ("Ctrl+C", "same as Esc (interrupt / exit on empty)"),
                ("Ctrl+D", "delete char forward, or exit when empty"),
                ("Ctrl+O", "expand last tool output into scrollback"),
                ("Ctrl+A / E", "cursor to line start / end"),
                ("Ctrl+K / U", "kill to line end / start"),
                ("Ctrl+W", "kill previous word"),
                ("Ctrl+Y", "yank last killed text"),
                ("Ctrl+Z / -", "undo (fish-style word-coalesced)"),
                ("Alt+B / F", "move cursor by word"),
                ("↑ / ↓", "prompt history when editor empty"),
                (
                    "Shift+Tab",
                    "cycle thinking level (Off → Minimal → … → Max)",
                ),
                ("/", "open slash-command palette"),
            ];
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    "Keyboard shortcuts",
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::BOLD),
                )]),
            )?;
            for (k, desc) in hotkey_lines {
                insert_line(
                    term,
                    Line::from(vec![
                        Span::styled(format!("  {:<15}", k), Style::default().fg(Color::Cyan)),
                        Span::styled(desc.to_string(), Style::default().fg(Color::Gray)),
                    ]),
                )?;
            }
            insert_line(term, Line::from(""))?;
        }
        KeyAction::ShowSettings => {
            app.settings_file = crate::settings_toml::load();
            app.settings_menu = Some(build_settings_menu(&app.settings_file));
        }
        KeyAction::ShowKeybindings => {
            app.keybindings_menu = Some(build_keybindings_menu(&app.bindings));
        }
        KeyAction::ShowSkills => {
            // Snapshot skills from the current agent so /new, /resume,
            // /fork changes are reflected immediately.
            let skills = {
                let g = agent_slot.lock().await;
                g.as_ref().map(|a| a.skills.clone()).unwrap_or_default()
            };
            if skills.is_empty() {
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        "No skills loaded.".to_string(),
                        Style::default().fg(Color::DarkGray),
                    )]),
                )?;
                insert_line(term, Line::from(vec![
                    Span::styled(
                        "  Add SKILL.md files under ~/.nanopi/skills/ or <cwd>/.nanopi/skills/ (project needs `-a` to trust).".to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))?;
                insert_line(term, Line::from(""))?;
            } else {
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("Loaded skills ({})", skills.len()),
                        Style::default()
                            .fg(Color::Indexed(108))
                            .add_modifier(Modifier::BOLD),
                    )]),
                )?;
                for s in &skills {
                    let src = s.source.label();
                    let hidden = if s.disable_model_invocation {
                        " (hidden)"
                    } else {
                        ""
                    };
                    insert_line(
                        term,
                        Line::from(vec![
                            Span::styled(
                                format!("  /skill:{:<24}", s.name),
                                Style::default().fg(Color::Cyan),
                            ),
                            Span::styled(
                                format!("[{src}]{hidden} "),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(s.description.clone(), Style::default().fg(Color::Gray)),
                        ]),
                    )?;
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("      {}", s.file_path.display()),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::DIM),
                        )]),
                    )?;
                }
                insert_line(term, Line::from(""))?;
            }
        }
        KeyAction::ShowTools => {
            // Read the live registry rather than a cache: `/new`,
            // `/resume` and `/fork` rebuild the Agent (and reload
            // plugins), so a snapshot taken at startup would go stale
            // in exactly the sessions where a user is most likely to
            // ask what changed.
            let entries = {
                let g = agent_slot.lock().await;
                g.as_ref().map(|a| a.registry.entries()).unwrap_or_default()
            };
            let plugin_count = entries
                .iter()
                .filter(|(_, src)| !matches!(src, crate::tool::ToolSource::Builtin))
                .count();
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!(
                        "Callable tools ({}, {} from plugins)",
                        entries.len(),
                        plugin_count
                    ),
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::BOLD),
                )]),
            )?;
            for (spec, src) in &entries {
                let tag = match src {
                    crate::tool::ToolSource::Builtin => "[builtin]".to_string(),
                    crate::tool::ToolSource::Plugin { name, .. } => format!("[plugin:{name}]"),
                };
                insert_line(
                    term,
                    Line::from(vec![
                        Span::styled(
                            format!("  {:<20}", spec.name),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            format!("{tag} "),
                            Style::default().fg(Color::DarkGray),
                        ),
                        // First line only: a plugin author's
                        // description can be a paragraph, and the
                        // point here is the inventory, not the docs.
                        Span::styled(
                            spec.description
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .to_string(),
                            Style::default().fg(Color::Gray),
                        ),
                    ]),
                )?;
                if let crate::tool::ToolSource::Plugin { path, .. } = src {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("      {path}"),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::DIM),
                        )]),
                    )?;
                }
            }
            if plugin_count == 0 {
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        "  No plugin tools. Declare [[extensions]] in config.toml \
                         (needs a build with --features wasm)."
                            .to_string(),
                        Style::default().fg(Color::DarkGray),
                    )]),
                )?;
            }
            insert_line(term, Line::from(""))?;

            // §5.1: the inventory above answers "what can the model
            // call" — this section answers "what is watching me", which
            // is the other half of a plugin's blast radius and just as
            // worth an operator's attention. Nothing printed when no
            // plugin subscribed to any event.
            for line in subscriptions_section(&app.subscriptions_cache) {
                insert_line(term, line)?;
            }
            if !app.subscriptions_cache.is_empty() {
                insert_line(term, Line::from(""))?;
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

            // PI-parity: the picker lists user messages from THIS session
            // only. To fork off an ancestor, `/resume` it first, then
            // `/fork`. See `packages/coding-agent/src/modes/interactive/
            // components/user-message-selector.ts` — PI has no cross-
            // session tree either.
            let entries = match session::read_session(&session_path) {
                Ok((_, e)) => e,
                Err(_) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            "[fork: could not read current session]",
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                    return Ok(());
                }
            };
            let rows = session::tree_items(&entries);
            let user_rows: Vec<&session::TreeRow> =
                rows.iter().filter(|r| r.role == "user").collect();
            if user_rows.is_empty() {
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        "[fork: no user messages in this session yet]",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )]),
                )?;
                return Ok(());
            }
            let total = user_rows.len();
            let items: Vec<MenuItem<(PathBuf, usize)>> = user_rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    MenuItem::new(
                        row.preview.clone(),
                        format!("Message {} of {}", i + 1, total),
                        (session_path.clone(), row.entry_index),
                    )
                })
                .collect();
            app.fork_picker = Some(MenuState::new(items));
        }
        KeyAction::ForkChosen(source_path, target_idx) => {
            // Since the picker only surfaces the current session,
            // `source_path` is always the active session and the cut-off
            // is the tail from target_idx onward. prefill = the target
            // user message's text (all picker items are user rows now).
            let src_entries = match session::read_session(&source_path) {
                Ok((_, e)) => e,
                Err(e) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[fork read failed: {e}]"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                    return Ok(());
                }
            };
            let prefill = src_entries.get(target_idx).and_then(|e| match e {
                session::SessionEntry::Message { role, content, .. } if role == "user" => {
                    Some(content.clone())
                }
                _ => None,
            });
            let cut_off = if target_idx < src_entries.len() {
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
                    app.capture = CaptureMode::CustomSummary;
                    app.status_note = Some("custom summarize prompt — Enter to submit".into());
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
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[summarize error: {error} — forking without summary]"),
                            Style::default().fg(Color::Yellow),
                        )]),
                    )?;
                    execute_fork(fork, None, app, term, agent_slot).await?;
                }
            }
        }
        KeyAction::CancelPendingFork => {
            app.pending_fork = None;
            app.capture = CaptureMode::None;
            app.status_note = None;
        }
        KeyAction::Reload => {
            handle_reload(term, app, agent_slot).await?;
        }
        KeyAction::Compact => {
            app.status_note = Some("compacting…".into());
            term.draw(|f| {
                let area = f.area();
                draw_dock(f.buffer_mut(), area, app);
            })?;
            let mut g = agent_slot.lock().await;
            if let Some(a) = g.as_mut() {
                let before = a.context.estimate_chars();
                a.compact_now(None, "manual").await;
                let after = a.context.estimate_chars();
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("[compacted: {before} → {after} chars]"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )]),
                )?;
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
            // Echo user message into scrollback, PI-style gray card.
            render_user_echo(term, &msg)?;
            let (tx, rx) = mpsc::channel::<AgentEvent>(64);
            let (steer_tx_inner, steer_rx) = mpsc::channel::<SteerMessage>(32);
            let ct = CancellationToken::new();
            let ct_task = ct.clone();
            let agent_task_slot = agent_slot.clone();
            let task = tokio::spawn(async move {
                let mut guard = agent_task_slot.lock().await;
                let mut a = guard.take().ok_or_else(|| "agent slot empty".to_string())?;
                drop(guard);
                let result = a.run_turn(&msg, &tx, Some(ct_task), Some(steer_rx)).await;
                let mut guard = agent_task_slot.lock().await;
                *guard = Some(a);
                result.map_err(|e| e.to_string())
            });
            *steer_tx = Some(steer_tx_inner);
            *ag_rx = Some(rx);
            *cancel = Some(ct);
            *turn_task = Some(task);
            app.status = Status::Streaming;
            app.turn_started_at = Some(std::time::Instant::now());
        }
        KeyAction::SteerTurn(msg) => {
            steer_or_queue(term, steer_tx.as_ref(), follow_up, msg).await?;
        }
        KeyAction::RunPluginCommand { name, args } => {
            let Some(cmd) = app
                .commands_cache
                .iter()
                .find(|c| c.spec.name == name)
                .cloned()
            else {
                // Only reachable if the cache went stale between the
                // palette opening and Enter — a rebuild in between.
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("[/{name} is no longer registered]"),
                        Style::default().fg(Color::Red),
                    )]),
                )?;
                return Ok(());
            };
            if app.command_task.is_some() {
                // Serializing is not cosmetic: the plugin's own mutex
                // would serialize them anyway, and queueing two
                // 30-second commands behind each other with no visible
                // reason is worse than refusing the second.
                app.status_note = Some("a plugin command is already running…".into());
                return Ok(());
            }
            app.status_note = Some(format!("running /{name}…"));
            let handler = cmd.handler.clone();
            let task_name = name.clone();
            app.command_task = Some(tokio::task::spawn_blocking(move || {
                let outcome = handler.run(&task_name, &args);
                (task_name, outcome)
            }));
        }
        KeyAction::PluginCommandFinished { name, outcome } => {
            app.status_note = None;
            match outcome {
                Ok(crate::command::CommandAction::Print(text)) => {
                    // Straight to scrollback: never enters the context
                    // and never reaches the session JSONL. Works
                    // identically mid-stream, since nothing here
                    // touches the agent.
                    for line in text.lines() {
                        insert_line(term, Line::from(line.to_string()))?;
                    }
                }
                Ok(crate::command::CommandAction::SendUserMessage(text)) => {
                    if app.status == Status::Streaming {
                        steer_or_queue(term, steer_tx.as_ref(), follow_up, text).await?;
                    } else {
                        // Echo first, always. The user must see verbatim
                        // what a plugin said on their behalf — dropping
                        // this is what would make the feature a
                        // security problem rather than a convenience.
                        Box::pin(handle_action(
                            KeyAction::StartTurn(text),
                            app,
                            term,
                            agent_slot,
                            ag_rx,
                            steer_tx,
                            follow_up,
                            cancel,
                            turn_task,
                        ))
                        .await?;
                    }
                }
                Ok(crate::command::CommandAction::Error(msg)) => {
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[/{name}] {msg}"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                }
                Err(e) => {
                    // A plugin-side failure — trap, malformed payload.
                    // Shown to the user, never forwarded to the model,
                    // matching PI, where a throwing command handler is
                    // swallowed rather than becoming a prompt.
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            format!("[/{name} failed] {e}"),
                            Style::default().fg(Color::Red),
                        )]),
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Route a mid-stream message into the running turn, or queue it as the
/// next turn if the turn ended first.
///
/// Extracted so the plugin-command path can reach it: `handle_action`
/// is `async`, so recursing into it would need boxing, and routing a
/// plugin's `send_user_message` through `KeyAction::StartTurn` instead
/// would hit its already-streaming early return and silently drop the
/// text — the bug `SlashCmd::Skill` still has.
///
/// The send is attempted BEFORE the echo. It used to be the other way
/// around, which meant the one case that can fail — the turn ending
/// between the keypress and this dispatch, dropping the receiver —
/// printed "[steer] ..." and then discarded the text. The user saw
/// their message land and it was gone.
async fn steer_or_queue(
    term: &mut Term,
    steer_tx: Option<&mpsc::Sender<SteerMessage>>,
    follow_up: &mut std::collections::VecDeque<String>,
    msg: String,
) -> Result<()> {
    match try_steer(steer_tx, msg).await {
        // Marked so it reads differently from a normal turn.
        Ok(landed) => render_user_echo(term, &format!("[steer] {landed}"))?,
        Err(missed) => {
            // Nothing left to steer. Queue it as the next turn rather
            // than throwing away something the user typed — the
            // turn-completion handler starts it. Echoed as queued, not
            // as a steer, so the distinction is visible.
            render_user_echo(term, &format!("[queued] {missed}"))?;
            follow_up.push_back(missed);
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

/// Swap the live Agent for `new_agent`, firing the session-lifecycle
/// hooks in between (spec §2.2c). Used at all four agent-swap sites
/// (`/new`, `/resume`, `/import`, fork).
///
/// Order is load-bearing: `session_shutdown` MUST fire on the OUTGOING
/// agent, still installed in the slot, before the slot is swapped —
/// one line late (swap first, fire second) and the shutdown payload
/// would carry the incoming session's id instead of the outgoing one,
/// silently misattributing the teardown to the wrong session for any
/// audit or cleanup hook. `session_start` then fires on the
/// newly-installed incoming agent. Holding the mutex guard across both
/// `await`s is correct here: this all runs on one task, and the hook
/// subprocess has no path back into `agent_slot`.
async fn swap_agent_with_reason(
    agent_slot: &Arc<Mutex<Option<Agent>>>,
    incoming_agent: Agent,
    reason: &str,
) {
    let mut g = agent_slot.lock().await;
    if let Some(outgoing) = g.as_ref() {
        outgoing.fire_session_shutdown(reason).await;
    }
    g.replace(incoming_agent);
    if let Some(incoming) = g.as_ref() {
        incoming.fire_session_start(reason).await;
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
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("[fork failed: {e}]"),
                        Style::default().fg(Color::Red),
                    )]),
                )?;
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
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!("[fork load failed: {e}]"),
                    Style::default().fg(Color::Red),
                )]),
            )?;
            return Ok(());
        }
    };
    let provider = crate::provider::build(app.api_kind, &base_url, &api_key, &model, Some(crate::vendor::pick_vendor(app.cfg_provider.as_deref(), Some(&base_url), &model)), app.inline_think_tags);
    let registry = crate::tool::ToolRegistry::standard();
    new_agent.context.tools = registry.all_specs();
    let diags = new_agent.hydrate_resumed(
        provider,
        registry,
        permission,
        hooks,
        model.clone(),
        base_url,
        api_key,
        app.skill_load.clone(),
        app.no_context_files,
        app.prompt_overrides.clone(),
        &app.extensions,
    );
    crate::agent::build::print_skill_diagnostics(&diags);
    let new_session_id = new_header.id;

    swap_agent_with_reason(agent_slot, new_agent, "fork").await;

    app.session_id = new_session_id.to_string();
    app.usage = crate::event::Usage::default();
    app.context_chars = 0;
    app.turn_count = 0;
    app.input.clear();
    if let Some(text) = fork.prefill.as_ref().filter(|s| !s.is_empty()) {
        app.input.insert_str(text);
    }

    let short = &new_session_id.to_string()[..8];
    let summary_note = if summary.is_some() {
        " · with summary"
    } else {
        ""
    };
    insert_line(
        term,
        Line::from(vec![Span::styled(
            format!(
                "[forked at entry {} → new session {}{}]",
                fork.target_entry_idx, short, summary_note
            ),
            Style::default()
                .fg(Color::Indexed(108))
                .add_modifier(Modifier::ITALIC),
        )]),
    )?;
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
    let provider = crate::provider::build(app.api_kind, &base_url, &api_key, &model, Some(crate::vendor::pick_vendor(app.cfg_provider.as_deref(), Some(&base_url), &model)), app.inline_think_tags);
    let cut_off = pending.cut_off.clone();
    let task = tokio::spawn(async move {
        match crate::agent::branch_summary::summarize_branch(
            &cut_off,
            custom.as_deref(),
            provider.as_ref(),
        )
        .await
        {
            Ok(summary) => SummarizeOutcome::Ok {
                summary,
                fork: pending,
            },
            Err(error) => SummarizeOutcome::Err {
                error,
                fork: pending,
            },
        }
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
        app.thinking = a.context.thinking;
        app.skills_cache = a.skills.clone();
        app.commands_cache = a.plugin_commands.clone();
        app.subscriptions_cache = a.event_subscribers.subscriptions();
    }
}

/// `/reload` handler: re-reads `config.toml`, `settings.toml`, and
/// re-discovers skills, then updates the live Agent in place. Mirrors
/// PI's `session.reload()` (`agent-session.ts:2602`) minus the
/// extension system and provider swap — mid-session provider swaps
/// stay behind `/model` to avoid accidentally dropping an in-flight
/// streaming connection.
///
/// What it touches: `agent.skills`, `agent.hooks`, `agent.context.
/// system` (rebuilt via `compose_system_prompt` so newly installed
/// skills appear in `<available_skills>`). What it does NOT touch:
/// `agent.provider`, `agent.model`, `agent.base_url`, `agent.api_key`,
/// session state, or messages.
///
/// `[[extensions]]` is deliberately NOT reloaded, and the report line
/// says so rather than leaving the user to guess. Reloading is blocked,
/// not merely skipped: `ToolRegistry` has no unregister, so a second
/// `load_extensions` would hit `register_external`'s collision refusal
/// for every tool and print a wall of spurious warnings; the
/// `Arc<dyn Tool>` clones already handed out would keep the previous
/// `ComponentBridge` alive, leaking a wasmtime `Store` and its epoch
/// ticker thread per reload; and re-instantiating while a turn holds
/// the bridge mutex hangs. Real hot-reload needs an unregister path
/// plus a generation counter on the bridge — its own feature. Use
/// `/new` or restart.
async fn handle_reload(
    term: &mut Term,
    app: &mut App,
    agent: &Arc<Mutex<Option<Agent>>>,
) -> Result<()> {
    use ratatui::text::Line as L;

    // ── 1. config.toml (only skills.disabled is applied live; model /
    //       base_url / api_key changes need /model or restart) ──
    let config_note: Option<String> = match crate::config::load_config(&app.cwd) {
        Ok(cfg) => {
            app.skill_load.disabled = cfg.skills.disabled.clone();
            None
        }
        Err(e) => Some(format!("config.toml: {e}")),
    };

    // ── 2. settings.toml → hooks ──
    let (hooks_new, settings_note): (Option<HooksConfig>, Option<String>) =
        match settings::load_settings(&app.cwd) {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(format!("settings.toml: {e}"))),
        };

    // ── 3. skills — uses the (possibly refreshed) disabled list ──
    let skill_result = crate::resources::load_skills(app.skill_load.clone().into_options());
    let n_skills = skill_result.skills.len();
    let n_diagnostics = skill_result.diagnostics.len();

    // ── 4. apply to the live agent under the async lock ──
    let n_hooks: usize = {
        let mut g = agent.lock().await;
        if let Some(a) = g.as_mut() {
            a.skills = skill_result.skills.clone();
            if let Some(ref h) = hooks_new {
                a.hooks = h.clone();
            }
            let tool_names = a.registry.names();
            a.context.system = Some(crate::agent::build::compose_system_prompt(
                &a.cwd,
                &tool_names,
                &a.skills,
                a.no_context_files,
                &a.prompt_overrides,
            ));
            let h = &a.hooks;
            h.tool_execution_start.len()
                + h.tool_execution_end.len()
                + h.input.len()
                + h.session_start.len()
                + h.session_shutdown.len()
        } else {
            0
        }
    };
    app.skills_cache = skill_result.skills;

    // ── 5. report to scrollback ──
    insert_line(
        term,
        L::from(vec![Span::styled(
            format!(
                "[reloaded] {n_skills} skill(s), {n_hooks} hook(s) · \
                 extensions unchanged (use /new or restart){}",
                if let Some(e) = &config_note {
                    format!(" · {e}")
                } else {
                    String::new()
                },
            ),
            Style::default()
                .fg(Color::Indexed(108))
                .add_modifier(Modifier::BOLD),
        )]),
    )?;
    if let Some(e) = &settings_note {
        insert_line(
            term,
            L::from(vec![Span::styled(
                format!("  {e}"),
                Style::default().fg(Color::Yellow),
            )]),
        )?;
    }
    for d in &skill_result.diagnostics {
        insert_line(
            term,
            L::from(vec![Span::styled(
                format!("  skill: {} ({})", d.message, d.path.display()),
                Style::default().fg(Color::Yellow),
            )]),
        )?;
    }
    if n_diagnostics == 0 && settings_note.is_none() && config_note.is_none() {
        insert_line(
            term,
            L::from(vec![Span::styled(
                "  no warnings".to_string(),
                Style::default().fg(Color::DarkGray),
            )]),
        )?;
    }
    insert_line(term, L::from(""))?;
    Ok(())
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
                    insert_line(
                        term,
                        Line::from(vec![Span::styled(
                            trimmed.to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )]),
                    )?;
                }
            }
        }
        AgentEvent::ToolCall { call, .. } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            let (leading, body) = tool_call_bar_text(&call.name, &call.arguments);
            app.pending_tool_calls
                .push((call.id.clone(), PendingBar { leading, body }));
            // Start the live "Elapsed X.Xs" clock so the dock can
            // render a blue running-state strip until ToolResult.
            app.tool_started_at = Some(std::time::Instant::now());
        }
        AgentEvent::ToolCallRewritten {
            call_id,
            tool_name,
            arguments,
        } => {
            // The card is only drawn when the result arrives, so the
            // stashed bar can still be corrected in place — the user
            // ends up seeing what actually ran rather than what the
            // model asked for. `↻` replaces the usual chip so the
            // difference is visible rather than silently swapped.
            let (_, body) = tool_call_bar_text(&tool_name, &arguments);
            if let Some(slot) = app
                .pending_tool_calls
                .iter_mut()
                .find(|(id, _)| *id == call_id)
            {
                slot.1.leading = format!(" ↻ {} ", tool_name.to_ascii_lowercase());
                slot.1.body = body;
            }
        }
        AgentEvent::ToolResult {
            call_id,
            tool_name,
            content,
            is_error,
            elapsed_ms,
        } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            render_tool_card(
                term,
                app,
                &call_id,
                &tool_name,
                &content,
                is_error,
                elapsed_ms,
            )?;
            // Only once every in-flight call has reported: with
            // parallel execution the batch is still running.
            if app.pending_tool_calls.is_empty() {
                app.tool_started_at = None;
            }
            // Stash full output so Ctrl+O can expand it later — with
            // the outcome flag so expansion uses matching bg.
            app.last_tool_output = Some(LastTool {
                content,
                is_error,
                expanded: false,
            });
        }
        AgentEvent::Error { error } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!("[error: {error}]"),
                    Style::default().fg(Color::Red),
                )]),
            )?;
        }
        AgentEvent::CompactionStart { reason } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!("[compacting context ({reason})…]"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )]),
            )?;
        }
        AgentEvent::CompactionEnd {
            replaced_count,
            used_llm,
        } => {
            let via = if used_llm { "summary" } else { "truncation" };
            insert_line(
                term,
                Line::from(vec![Span::styled(
                    format!("[compacted {replaced_count} messages via {via}]"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )]),
            )?;
            // Refresh cached context estimate for the status footer.
            app.context_chars = 0; // will be re-populated on next event
        }
        AgentEvent::SkillInvocation {
            name,
            location,
            base_dir,
            body,
            user_message,
        } => {
            flush_stream_buf(term, app)?;
            flush_thinking_buf(term, app)?;
            render_skill_card(term, &name)?;
            // Stash for Ctrl-O expansion, using a second slot alongside
            // last_tool_output so tool + skill expand paths don't
            // collide. Expand-once semantics match tool expansion (see
            // BACKLOG.md — collapse-back isn't feasible on our
            // append-only scrollback).
            app.last_skill_block = Some(CollapsedSkill {
                name,
                location,
                base_dir,
                body,
                user_message,
                expanded: false,
            });
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
        insert_line(
            term,
            Line::from(vec![Span::styled(
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )]),
        )?;
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
            (
                format!(" {} ", name_lc),
                truncate_bar_body(&format!("'{pat}'")),
            )
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
/// Replace `\t` with four spaces so styled backgrounds fill the whole
/// visible line inside tool / skill / user cards. Bare `\t` bytes
/// reach the terminal and are expanded by the terminal itself using
/// its DEFAULT (unstyled) background — the styled span covers only
/// the one tab character, leaving 3-7 columns of black gap per tab.
/// Most visible on TypeScript / Go / Makefile output where tabs
/// are the indent standard. See SCR-20260810-bfry.png for the
/// bug we're fixing.
fn expand_tabs(s: &str) -> String {
    s.replace('\t', "    ")
}

fn render_tool_card(
    term: &mut Term,
    app: &mut App,
    call_id: &str,
    tool_name: &str,
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
    let bar_style = Style::default()
        .bg(bar_bg)
        .fg(bar_fg)
        .add_modifier(Modifier::BOLD);
    // Dimmed foreground on the same bg — for output preview + Took.
    let dim_style = Style::default().bg(bar_bg).fg(Color::Indexed(253));
    let hint_style = Style::default()
        .bg(bar_bg)
        .fg(Color::Indexed(250))
        .add_modifier(Modifier::ITALIC);

    // Breathing room above.
    insert_line(term, Line::from(""))?;

    // Row 1: command bar — the stash left by this call's ToolCall
    // event, matched on call id so a parallel batch can't mislabel a
    // card. Missing stash falls back to the tool's name rather than
    // dropping the row: a card with no header is unreadable, and the
    // name is the one thing the result event always carries.
    let pending = take_pending_bar(app, call_id).unwrap_or_else(|| PendingBar {
        // Replayed history carries no tool name on the result entry,
        // so the generic word beats an empty chip.
        leading: match tool_name {
            "" => " tool ".to_string(),
            n => format!(" {} ", n.to_ascii_lowercase()),
        },
        body: String::new(),
    });
    insert_line_bg(
        term,
        Line::from(vec![
            Span::styled(pending.leading, bar_style),
            Span::styled(pending.body, bar_style),
        ]),
        Some(bar_style),
    )?;

    // Output preview: last N lines. If more, show a truncation marker.
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total > TOOL_PREVIEW_LINES {
        let hidden = total - TOOL_PREVIEW_LINES;
        insert_line_bg(
            term,
            Line::from(vec![Span::styled(
                format!("  … ({} earlier lines, ctrl+o to expand)", hidden),
                hint_style,
            )]),
            Some(bar_style),
        )?;
    }
    let start = total.saturating_sub(TOOL_PREVIEW_LINES);
    for line in &lines[start..] {
        // Prefix with 2 spaces for visual indent inside the card.
        // Expand tabs so the bg doesn't tear open on tab-indented output.
        insert_line_bg(
            term,
            Line::from(vec![
                Span::styled("  ", dim_style),
                Span::styled(expand_tabs(line), dim_style),
            ]),
            Some(dim_style),
        )?;
    }

    // Empty divider row inside the card.
    insert_line_bg(
        term,
        Line::from(vec![Span::styled("", bar_style)]),
        Some(bar_style),
    )?;

    // Took Xs (right-side info, italic dim on the card bg).
    let took_str = if elapsed_ms < 50 {
        format!("Took {}ms", elapsed_ms)
    } else {
        format!("Took {:.1}s", elapsed_ms as f64 / 1000.0)
    };
    insert_line_bg(
        term,
        Line::from(vec![Span::styled(format!("  {}", took_str), hint_style)]),
        Some(bar_style),
    )?;

    // Breathing room below.
    insert_line(term, Line::from(""))?;
    Ok(())
}

/// Insert a collapsed skill invocation card into scrollback. Mirrors
/// PI's `SkillInvocationMessageComponent` (`skill-invocation-message.ts`)
/// in its collapsed state: a single row with `[skill] name (Ctrl+O to
/// expand)`, styled with the same subtle bg used for custom messages.
/// The expanded body lands in scrollback via `expand_last_skill_block`.
/// Rows of the collapsed skill card as `(line, row_background)` pairs.
///
/// Split out from `render_skill_card` so the card's shape is testable
/// without a live terminal — `insert_before` needs a real `Term`.
///
/// The two padded rows carrying `bg` are what make this read as a
/// block rather than a lone tinted line. PI gets them from
/// `SkillInvocationMessageComponent extends Box(1, 1, customMessageBg)`
/// — a padding of 1 whose rows are painted with the background. Our
/// `render_user_echo` already builds the same pad/content/pad shape
/// for user messages; the skill card was emitting unstyled blanks
/// instead, so an invoked skill barely announced itself.
fn skill_card_rows(name: &str) -> Vec<(Line<'static>, Option<Style>)> {
    let bg = Style::default()
        .bg(Color::Indexed(236))
        .fg(Color::Indexed(255));
    let dim = Style::default().bg(Color::Indexed(236)).fg(Color::DarkGray);
    let pad = || (Line::from(vec![Span::styled("", bg)]), Some(bg));
    vec![
        (Line::from(""), None),
        pad(),
        (
            Line::from(vec![
                Span::styled("  [skill] ".to_string(), bg.add_modifier(Modifier::BOLD)),
                Span::styled(name.to_string(), bg),
                Span::styled(" (Ctrl+O to expand)".to_string(), dim),
            ]),
            Some(bg),
        ),
        pad(),
        (Line::from(""), None),
    ]
}

fn render_skill_card(term: &mut Term, name: &str) -> Result<()> {
    for (line, bg) in skill_card_rows(name) {
        match bg {
            Some(b) => insert_line_bg(term, line, Some(b))?,
            None => insert_line(term, line)?,
        }
    }
    Ok(())
}

/// Insert the PI-style gray "user echo" card into scrollback. Used
/// both by StartTurn (live) and by replay_history when resuming a
/// past session (`/resume`). 3-row block: pad, content, pad, with a
/// blank line above + below.
fn render_user_echo(term: &mut Term, msg: &str) -> Result<()> {
    // 256-color palette (Indexed 238) — visible on tmux-256color too;
    // truecolor fails silently there.
    let user_bg = Style::default()
        .bg(Color::Indexed(238))
        .fg(Color::Indexed(255));
    insert_line(term, Line::from(""))?;
    insert_line_bg(
        term,
        Line::from(vec![Span::styled("", user_bg)]),
        Some(user_bg),
    )?;
    for line in msg.lines() {
        insert_line_bg(
            term,
            Line::from(vec![
                Span::styled("  ", user_bg),
                Span::styled(expand_tabs(line), user_bg),
            ]),
            Some(user_bg),
        )?;
    }
    insert_line_bg(
        term,
        Line::from(vec![Span::styled("", user_bg)]),
        Some(user_bg),
    )?;
    insert_line(term, Line::from(""))?;
    Ok(())
}

/// Redraw a session's transcript into scrollback by walking its
/// entries and dispatching each to `on_agent_event` (assistant text
/// / tool calls / tool results) or rendering directly (user echo,
/// compaction markers, branch summaries). Called from
/// KeyAction::ResumeSession so `/resume` shows the full past
/// conversation, matching PI's behavior.
fn replay_history(term: &mut Term, app: &mut App, entries: &[session::SessionEntry]) -> Result<()> {
    // Ensure any live-streaming state is fresh (fenced-block toggle etc).
    app.md_state = crate::render::markdown::MdState::default();
    app.stream_buf.clear();
    app.thinking_buf.clear();
    app.pending_tool_calls.clear();

    for entry in entries {
        match entry {
            session::SessionEntry::Message { role, content, .. } => match role.as_str() {
                "user" => render_user_echo(term, content)?,
                "assistant" => {
                    // Feed a single big TextDelta so on_agent_event
                    // splits on newlines + renders through markdown
                    // exactly like the live streaming path.
                    let mut text = content.clone();
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    on_agent_event(
                        term,
                        app,
                        AgentEvent::TextDelta {
                            content_index: 0,
                            text,
                        },
                    )?;
                    // Blank spacer between assistant reply and next turn.
                    insert_line(term, Line::from(""))?;
                }
                _ => {}
            },
            session::SessionEntry::ToolCall {
                id,
                tool_name,
                arguments,
                ..
            } => {
                on_agent_event(
                    term,
                    app,
                    AgentEvent::ToolCall {
                        content_index: 0,
                        call: crate::event::ToolCall {
                            id: id.clone(),
                            name: tool_name.clone(),
                            arguments: arguments.clone(),
                        },
                    },
                )?;
            }
            session::SessionEntry::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                on_agent_event(
                    term,
                    app,
                    AgentEvent::ToolResult {
                        call_id: tool_call_id.clone(),
                        tool_name: String::new(),
                        content: content.clone(),
                        is_error: *is_error,
                        elapsed_ms: 0,
                    },
                )?;
            }
            session::SessionEntry::Compaction {
                summary,
                replaced_count,
                ..
            } => {
                let preview: String = summary
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(80)
                    .collect();
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("[compaction: {replaced_count} msgs → {preview}]"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )]),
                )?;
            }
            session::SessionEntry::BranchSummary { summary, .. } => {
                let preview: String = summary
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(80)
                    .collect();
                insert_line(
                    term,
                    Line::from(vec![Span::styled(
                        format!("[branch summary: {preview}]"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )]),
                )?;
            }
            session::SessionEntry::SkillInvocation {
                name,
                location,
                base_dir,
                body,
                user_message,
                ..
            } => {
                on_agent_event(
                    term,
                    app,
                    AgentEvent::SkillInvocation {
                        name: name.clone(),
                        location: location.clone(),
                        base_dir: base_dir.clone(),
                        body: body.clone(),
                        user_message: user_message.clone(),
                    },
                )?;
            }
            _ => {}
        }
    }
    // Flush any tail assistant text that didn't end on a newline.
    flush_stream_buf(term, app)?;
    Ok(())
}

/// Insert a line into scrollback above the viewport. Wraps to
/// multiple rows when the content exceeds the terminal width so long
/// assistant replies stay visible instead of getting silently
/// truncated at the right edge (matches PI's wrapping — see
/// SCR-20260808-sihi.png).
fn insert_line(term: &mut Term, line: Line<'_>) -> Result<()> {
    // Fast path for empty lines.
    if line.spans.iter().all(|s| s.content.is_empty()) {
        return insert_line_bg(term, line, None);
    }
    let width = term.size().map(|r| r.width).unwrap_or(80).max(1) as usize;

    // We lay the line out ourselves — ONE terminal column per `char` —
    // instead of handing off to ratatui's `Paragraph`. Paragraph measures
    // glyphs via the `unicode-width` crate, which classifies CJK as
    // East-Asian *Wide* (2 columns) and reserves a blank cell after each
    // one. On terminals/fonts that draw CJK single-width that reserved
    // cell shows up as a gap — assistant text ends up "你 好" while the
    // user's own echoed input (rendered by `insert_line_bg`, also 1
    // col/char) stays tight. Matching that path keeps output visually
    // identical to input. Leading whitespace is preserved (no trimming),
    // so indented code / markdown stays aligned.
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            let style = s.style;
            s.content.chars().map(move |c| (c, style))
        })
        .collect();

    let mut rows = wrap_chars(&chars, width);
    rows.truncate(200); // guard against runaway
    let rows_needed = rows.len().max(1) as u16;

    term.insert_before(rows_needed, move |buf: &mut Buffer| {
        for (row_idx, row) in rows.iter().enumerate() {
            let y = row_idx as u16;
            if y >= buf.area.height {
                break;
            }
            for (col, (ch, style)) in row.iter().enumerate() {
                if col as u16 >= buf.area.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((col as u16, y)) {
                    cell.set_char(*ch).set_style(*style);
                }
            }
        }
    })?;
    Ok(())
}

/// Soft-wrap a flat list of styled chars into rows of at most `width`
/// columns, counting exactly one column per char (matching the user-echo
/// renderer, `insert_line_bg`). Breaks at the last space when a word
/// would overflow; hard-breaks words longer than the line.
fn wrap_chars(chars: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut last_space: Option<usize> = None; // index in `cur` of last ' '

    for &(ch, style) in chars {
        cur.push((ch, style));
        if ch == ' ' {
            last_space = Some(cur.len() - 1);
        }
        if cur.len() > width {
            match last_space {
                // Break at the last space: keep the head, drop the space,
                // carry the tail to the next row.
                Some(sp) if sp > 0 => {
                    let tail: Vec<(char, Style)> = cur.split_off(sp + 1);
                    cur.pop(); // drop the trailing space
                    rows.push(std::mem::take(&mut cur));
                    cur = tail;
                    last_space = cur.iter().rposition(|(c, _)| *c == ' ');
                }
                // No usable space — hard-break the over-long word.
                _ => {
                    let carried = cur.pop().unwrap();
                    rows.push(std::mem::take(&mut cur));
                    cur.push(carried);
                    last_space = None;
                }
            }
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
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

/// Compute the `[start, end)` slice of buffer lines to show in the input
/// box so the cursor row stays visible. Once the buffer overflows the
/// `visible` window, the window anchors to the bottom (cursor pinned to
/// the last visible row) — Claude Code's bounded-viewport scroll.
fn input_scroll_window(cursor_row: usize, total: usize, visible: usize) -> (usize, usize) {
    let visible = visible.max(1);
    let start = if cursor_row < visible {
        0
    } else {
        cursor_row + 1 - visible
    };
    let end = (start + visible).min(total);
    (start, end)
}

fn draw_dock(buf: &mut Buffer, area: Rect, app: &App) {
    // Is a dropdown overlay open? When one is, it claims the top 4 rows
    // and the input box collapses to a single content line. When none is
    // open, the input box grows into those rows (up to MAX_INPUT_LINES)
    // and scrolls internally to keep the cursor visible.
    let overlay_open = app.summary_prompt.is_some()
        || app.resume_picker.is_some()
        || app.fork_picker.is_some()
        || app.model_picker.is_some()
        || app.palette.is_some()
        || app.settings_menu.is_some()
        || app.keybindings_menu.is_some();
    let max_input_lines = if overlay_open { 1 } else { MAX_INPUT_LINES };
    let input_content_h = app.input.row_count().clamp(1, max_input_lines) as u16;

    // Layout: [overlay/top-slack] + status(1) + input(2 border+content)
    // + footer(2) = DOCK_HEIGHT. `Min(0)` absorbs the slack at the top so
    // the input box stays anchored just above the footer and grows
    // upward; when an overlay is open it lands in that same top region.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),                      // palette / overlay / slack
            Constraint::Length(1),                   // status strip
            Constraint::Length(2 + input_content_h), // input box (borders + content)
            Constraint::Length(1),                   // cwd + branch
            Constraint::Length(1),                   // stats
        ])
        .split(area);

    // ── Overlay menus (dropdown). Priority matches interpret_key:
    // summary > resume > fork > model > slash palette.
    if let Some(m) = &app.summary_prompt {
        draw_menu(buf, chunks[0], m, "summarize branch?");
    } else if let Some(m) = &app.settings_menu {
        draw_menu(buf, chunks[0], m, "settings");
    } else if let Some(m) = &app.keybindings_menu {
        draw_menu(buf, chunks[0], m, "keybindings");
    } else if let Some(m) = &app.resume_picker {
        draw_menu(buf, chunks[0], m, "resume session");
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
    // The box shows up to `input_content_h` buffer lines, scrolling so
    // the cursor's line stays visible. The bordered block is
    // (2 + input_content_h) rows total (top ─, content…, bottom ─).
    let (cursor_row, cursor_col) = app.input.cursor();
    let lines = app.input.lines();
    let total = lines.len();
    let visible = input_content_h as usize;
    let (start, end) = input_scroll_window(cursor_row, total, visible);

    let marker_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut input_lines: Vec<Line> = Vec::with_capacity(end - start);
    for row in start..end {
        let text = lines.get(row).map(String::as_str).unwrap_or("");
        // Prompt marker on the first logical line; continuation lines get
        // matching 2-space indent so text stays column-aligned.
        let marker = if row == 0 { "> " } else { "  " };
        let mut spans: Vec<Span> = vec![Span::styled(marker, marker_style)];
        if row == cursor_row {
            let (pre, post) = split_at_col(text, cursor_col);
            spans.push(Span::raw(pre.to_string()));
            // Reverse-video block as cursor; char under it or a space.
            let cursor_char: String = post
                .chars()
                .next()
                .map(|c| c.to_string())
                .unwrap_or_else(|| " ".into());
            spans.push(Span::styled(
                cursor_char,
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::raw(post.chars().skip(1).collect::<String>()));
            if let Some(note) = &app.status_note {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("({note})"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
        } else {
            spans.push(Span::raw(text.to_string()));
        }
        // Only when the buffer overflows the box do we hint at scroll
        // position — shown once, on the top visible row.
        if total > visible && row == start {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("(line {}/{})", cursor_row + 1, total),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        input_lines.push(Line::from(spans));
    }
    let input_para = Paragraph::new(input_lines)
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
    l2.push(Span::raw(" · "));
    l2.push(Span::styled(
        app.model.clone(),
        Style::default().fg(Color::LightBlue),
    ));
    if let Some(lvl) = app.thinking {
        l2.push(Span::raw(" · "));
        l2.push(Span::styled(
            format!("think:{}", lvl),
            Style::default()
                .fg(Color::Indexed(108))
                .add_modifier(Modifier::ITALIC),
        ));
    }
    // v0.9.3: vendor:<id> segment when a non-fallback vendor was chosen.
    if let Some(vid) = app.vendor_id.as_deref() {
        if vid != "fallback" {
            l2.push(Span::raw(" · "));
            l2.push(Span::styled(
                format!("vendor:{}", vid),
                Style::default()
                    .fg(Color::Indexed(108))
                    .add_modifier(Modifier::ITALIC),
            ));
        }
    }
    // Context ratio (`1.4%/205k (auto)`), color-coded by usage.
    // Auto-compact is always on today; if we add a config toggle we
    // can wire it here.
    if let Some(ratio) =
        crate::render::status_line::context_ratio(&app.model, app.context_chars, true)
    {
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
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
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
    // Expand tabs so the bg doesn't tear open on tab-indented output.
    for line in content.lines() {
        insert_line_bg(
            term,
            Line::from(vec![
                Span::styled("  ", bar_style),
                Span::styled(expand_tabs(line), bar_style),
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

/// Expanded skill card: header + full SKILL.md body rendered through
/// the markdown pipeline, styled with the same subtle bg as the
/// collapsed row. If the user passed args after `/skill:name`, they
/// appear as an italic footer. PI equivalent: expanded
/// `SkillInvocationMessageComponent`.
fn render_skill_expansion(term: &mut Term, s: &CollapsedSkill) -> Result<()> {
    let bg = Style::default()
        .bg(Color::Indexed(236))
        .fg(Color::Indexed(255));
    let dim = Style::default()
        .bg(Color::Indexed(236))
        .fg(Color::Indexed(250));
    insert_line_bg(
        term,
        Line::from(vec![Span::styled(
            format!("  ── skill: {} ({}) ──", s.name, s.base_dir),
            dim.add_modifier(Modifier::ITALIC),
        )]),
        Some(bg),
    )?;
    // Render body through the shared markdown parser so headings /
    // code / emphasis look consistent with assistant replies. Expand
    // tabs BEFORE parsing so both the markdown parser and the bg
    // paint agree on column widths.
    let mut md = crate::render::markdown::MdState::default();
    for raw_line in s.body.lines() {
        let expanded = expand_tabs(raw_line);
        let spans = crate::render::markdown::render_line(&expanded, &mut md);
        let mut owned: Vec<Span<'static>> = vec![Span::styled("  ", bg)];
        for sp in spans {
            owned.push(Span::styled(sp.content.into_owned(), bg.patch(sp.style)));
        }
        insert_line_bg(term, Line::from(owned), Some(bg))?;
    }
    if let Some(u) = &s.user_message {
        insert_line_bg(term, Line::from(vec![Span::styled("", bg)]), Some(bg))?;
        insert_line_bg(
            term,
            Line::from(vec![Span::styled(
                format!("  > {u}"),
                dim.add_modifier(Modifier::ITALIC),
            )]),
            Some(bg),
        )?;
    }
    insert_line_bg(term, Line::from(vec![Span::styled("", bg)]), Some(bg))?;
    insert_line(term, Line::from(""))?;
    Ok(())
}

/// Draw the 1-row activity strip between palette and input box.
/// - tool running → blue `$ cmd  Elapsed X.Xs` (matches PI's blue bar
///   in img/PI_work_status.jpg)
/// - streaming with no active tool → `⣷ thinking (X.Xs)` dim
/// - idle → blank row
fn draw_status_strip(buf: &mut Buffer, area: Rect, app: &App) {
    const BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let frame = ((std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        / 120) as usize)
        % BRAILLE.len();

    // Tool running has priority.
    // Oldest in-flight call: with a parallel batch the strip can only
    // show one, and the one that has been running longest is the one
    // the elapsed clock actually belongs to.
    if let (Some(started), Some((_, bar))) =
        (app.tool_started_at, app.pending_tool_calls.first())
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
        let line = Line::from(vec![Span::styled(
            format!("{} thinking ({:.1}s)", BRAILLE[frame], elapsed),
            Style::default()
                .fg(Color::Indexed(108))
                .add_modifier(Modifier::ITALIC),
        )]);
        buf.set_line(area.x, area.y, &line, area.width);
    }
    // else: leave blank
}

/// Split a string at the given byte position, returning (pre, post).
fn split_at_col(s: &str, col: usize) -> (&str, &str) {
    let clamped = col.min(s.len());
    s.split_at(clamped)
}

/// Same as draw_palette but for MenuState<String> (model picker etc).
/// A minimal re-implementation — TODO: unify via a trait once we have
/// a third menu type.
fn draw_menu<T: Clone>(buf: &mut Buffer, area: Rect, m: &MenuState<T>, label: &str) {
    let vis = m.visible();
    let sel = m.cursor();
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    // Free-text menus own their filter, so echo it — otherwise the user
    // is typing blind (the input box isn't receiving these keystrokes).
    let mut lines: Vec<Line> = Vec::new();
    let typed = m.is_free_text().then(|| m.filter()).unwrap_or("");
    if !typed.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(format!("  {label}: "), dim),
            Span::styled(
                typed.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    if vis.is_empty() {
        // On a free-text menu "no match" is not a dead end: Enter takes
        // the typed id verbatim. Say so, or the user assumes they're stuck.
        lines.push(if m.is_free_text() && !typed.is_empty() {
            Line::from(vec![
                Span::styled("  (no match)  ", dim),
                Span::styled(
                    format!("⏎ use \"{typed}\""),
                    Style::default().fg(Color::Indexed(108)),
                ),
            ])
        } else {
            Line::from(vec![Span::styled(format!("  (no {label} matches)"), dim)])
        });
        Paragraph::new(lines).render(area, buf);
        return;
    }
    let max_rows = (area.height as usize).saturating_sub(lines.len()).max(1);
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
    let total_w = area.width as usize;
    let arrow_w = 2; // "→ " or "  " — both 2 display cols
    let gap_w = 2; // gap between label and right-aligned description
    for (i, item) in vis[start..end].iter().enumerate() {
        let absolute = start + i;
        let is_sel = absolute == sel;
        let arrow = if is_sel { "→ " } else { "  " };
        let label_style = if is_sel {
            Style::default()
                .fg(Color::Indexed(108))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let desc_style = Style::default().fg(Color::DarkGray);

        // Right-align description to the row's right edge; truncate
        // the label with an ellipsis when the two would collide.
        // Without this, long labels (e.g. a full user-message preview)
        // push the description off past the right edge and the column
        // stops aligning between rows.
        let desc_w = item.description.chars().count();
        let label_max = total_w.saturating_sub(arrow_w + gap_w + desc_w);
        let label_w = item.label.chars().count();
        let label_str = if label_w > label_max {
            let take = label_max.saturating_sub(1);
            let mut s: String = item.label.chars().take(take).collect();
            s.push('…');
            s
        } else {
            item.label.clone()
        };
        let pad = label_max.saturating_sub(label_str.chars().count());
        let label_padded = format!("{}{}", label_str, " ".repeat(pad));

        lines.push(Line::from(vec![
            Span::styled(arrow, label_style),
            Span::styled(label_padded, label_style),
            Span::styled(" ".repeat(gap_w), desc_style),
            Span::styled(item.description.clone(), desc_style),
        ]));
    }
    if end < vis.len() {
        let count = vis.len() - end;
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled(
                format!("   (+{count} more)"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
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
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
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
    // Dynamic label pad: line up descriptions past the widest visible
    // label (with a 2-space gutter). Prevents descriptions from
    // crashing into long `/skill:<name>` labels when the palette
    // mixes built-in commands with skills.
    let label_w = vis
        .iter()
        .map(|it| it.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(10); // baseline for the short built-ins
    let pad = label_w + 2;
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in vis[start..end].iter().enumerate() {
        let absolute = start + i;
        let is_sel = absolute == sel;
        let arrow = if is_sel { "→ " } else { "  " };
        let label_style = if is_sel {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let desc_style = Style::default().fg(Color::DarkGray);
        lines.push(Line::from(vec![
            Span::styled(arrow, label_style),
            Span::styled(format!("{:<pad$}", item.label, pad = pad), label_style),
            Span::styled(item.description.clone(), desc_style),
        ]));
    }
    // Overflow indicator on last row if more items exist below.
    if end < vis.len() {
        let count = vis.len() - end;
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled(
                format!("   (+{count} more)"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
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
    use crate::agent::hook::HookConfig;

    /// Tabs must be expanded to spaces before we hand a line to
    /// ratatui. Otherwise the terminal's own tab expansion paints
    /// with default bg, tearing black holes in tool / skill cards
    /// (see SCR-20260810-bfry.png).
    fn as_strings(rows: &[Vec<(char, Style)>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|(c, _)| *c).collect::<String>())
            .collect()
    }

    /// One flattened string per rendered `Line`, for assertions that
    /// don't care about per-span styling.
    fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn subscriptions_section_is_empty_when_nothing_subscribed() {
        assert!(subscriptions_section(&[]).is_empty());
    }

    #[test]
    fn subscriptions_section_lists_header_and_one_line_per_plugin() {
        let subs = vec![
            ("watcher".to_string(), vec!["turn_end".to_string(), "turn_start".to_string()]),
            ("logger".to_string(), vec!["input".to_string()]),
        ];
        let lines = subscriptions_section(&subs);
        let texts = line_texts(&lines);
        assert_eq!(texts.len(), 3, "header + one line per plugin");
        assert!(texts[0].contains("Watching events"));
        assert!(texts[0].contains("2"), "header should count the plugins");
        assert!(texts[1].contains("watcher"));
        assert!(texts[1].contains("turn_end, turn_start"));
        assert!(texts[2].contains("logger"));
        assert!(texts[2].contains("input"));
    }

    #[test]
    fn wrap_chars_one_column_per_char() {
        // CJK counts as ONE column here (unlike unicode-width), so a run
        // of 5 chars fits in width 5 on a single row — no double-width gap.
        let chars: Vec<(char, Style)> =
            "你好世界吗".chars().map(|c| (c, Style::default())).collect();
        let rows = wrap_chars(&chars, 5);
        assert_eq!(as_strings(&rows), vec!["你好世界吗"]);
    }

    #[test]
    fn wrap_chars_breaks_at_space() {
        let chars: Vec<(char, Style)> = "hello world foo"
            .chars()
            .map(|c| (c, Style::default()))
            .collect();
        // width 11 → "hello world" fits (11), then "foo" wraps.
        let rows = wrap_chars(&chars, 11);
        assert_eq!(as_strings(&rows), vec!["hello world", "foo"]);
    }

    #[test]
    fn wrap_chars_hard_breaks_long_word() {
        let chars: Vec<(char, Style)> = "abcdefghij"
            .chars()
            .map(|c| (c, Style::default()))
            .collect();
        let rows = wrap_chars(&chars, 4);
        assert_eq!(as_strings(&rows), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_chars_preserves_leading_indent() {
        let chars: Vec<(char, Style)> =
            "    indented".chars().map(|c| (c, Style::default())).collect();
        let rows = wrap_chars(&chars, 80);
        assert_eq!(as_strings(&rows), vec!["    indented"]);
    }

    #[test]
    fn input_scroll_window_fits_without_scroll() {
        // 3 lines, cursor on line 1, window of 5 → show all, no scroll.
        assert_eq!(input_scroll_window(1, 3, 5), (0, 3));
    }

    #[test]
    fn input_scroll_window_anchors_to_cursor_on_overflow() {
        // 33 lines, cursor on the last (32), window 5 → show 28..33.
        assert_eq!(input_scroll_window(32, 33, 5), (28, 33));
        // Cursor mid-buffer past the first window → window ends at cursor+1.
        assert_eq!(input_scroll_window(10, 33, 5), (6, 11));
        // Cursor still within the first window → pinned to top.
        assert_eq!(input_scroll_window(4, 33, 5), (0, 5));
    }

    #[test]
    fn input_scroll_window_handles_degenerate_visible() {
        // visible clamps to at least 1 so we never produce an empty box.
        assert_eq!(input_scroll_window(0, 1, 0), (0, 1));
    }

    // ─────── swap_agent_with_reason (Task 3, §2.2c) ───────

    /// Test-only Provider — never called by these tests (they only
    /// exercise `fire_session_shutdown` / `fire_session_start`, not
    /// `run_turn`), so it can be a stub that panics if invoked.
    struct DeadProvider;

    #[async_trait::async_trait]
    impl crate::agent::loop_::Provider for DeadProvider {
        fn id(&self) -> &'static str {
            "dead"
        }
        async fn stream_turn(
            &self,
            _ctx: &crate::agent::context::Context,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<crate::event::Usage, String> {
            panic!("swap_agent_with_reason tests must not drive a turn");
        }
    }

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-tui-swap-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn agent_with_id(dir: &std::path::Path, session_id: &str, hooks: HooksConfig) -> Agent {
        let session_path = dir.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &session_path,
            format!(
                "{{\"type\":\"session\",\"version\":2,\"id\":\"{session_id}\",\"timestamp\":\"2026-08-10T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"m\",\"base_url\":\"\"}}\n"
            ),
        )
        .unwrap();
        Agent {
            context: crate::agent::context::Context::default(),
            provider: Box::new(DeadProvider),
            registry: ToolRegistry::standard(),
            session_path,
            session_id: session_id.to_string(),
            cwd: dir.to_path_buf(),
            permission: PermissionGate::from_cli(false, None),
            hooks,
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            usage_total: crate::event::Usage::default(),
            turn_count: 0,
            skills: Vec::new(),
            no_context_files: false,
            pending_follow_ups: Default::default(),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            plugin_commands: Vec::new(),
            event_subscribers: Default::default(),
            prompt_overrides: crate::agent::prompt_override::PromptOverrides::default(),
        }
    }

    /// The actual ordering assertion: `session_shutdown` fires against
    /// the OUTGOING agent (session id A) BEFORE `session_start` fires
    /// against the INCOMING one (session id B). A one-line-late
    /// implementation (swap first, fire second) would still produce two
    /// hook firings with the right event names and reason — only the
    /// `session_id` on the first object catches the transposition.
    #[tokio::test]
    async fn swap_agent_with_reason_fires_shutdown_before_start_in_order() {
        let dir = tmp_dir();
        let transcript = dir.join("transcript.jsonl");
        let hook_script = dir.join("hook.sh");
        std::fs::write(
            &hook_script,
            format!("#!/usr/bin/env bash\ncat >> {}\n", transcript.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hook_cfg = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: hook_script.display().to_string(),
            timeout: 3000,
        };
        let hooks = HooksConfig {
            session_start: vec![hook_cfg.clone()],
            session_shutdown: vec![hook_cfg],
            ..Default::default()
        };

        let outgoing = agent_with_id(&dir, "session-A", hooks.clone());
        let incoming = agent_with_id(&dir, "session-B", hooks);
        let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(Some(outgoing)));

        swap_agent_with_reason(&agent_slot, incoming, "new").await;

        let text = std::fs::read_to_string(&transcript).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "expected exactly two hook firings, got:\n{text}");

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();

        // The load-bearing assertion: session_id on the FIRST firing is
        // the OUTGOING agent's id, not the incoming one.
        assert_eq!(first["event"], "session_shutdown");
        assert_eq!(first["arguments"]["reason"], "new");
        assert_eq!(first["session_id"], "session-A");

        assert_eq!(second["event"], "session_start");
        assert_eq!(second["arguments"]["reason"], "new");
        assert_eq!(second["session_id"], "session-B");

        let g = agent_slot.lock().await;
        assert_eq!(g.as_ref().unwrap().session_id, "session-B");
        drop(g);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--no-hooks` must still swap the agent but fire nothing.
    #[tokio::test]
    async fn swap_agent_with_reason_no_hooks_swaps_but_fires_nothing() {
        let dir = tmp_dir();
        let transcript = dir.join("transcript.jsonl");
        let hook_script = dir.join("hook.sh");
        std::fs::write(
            &hook_script,
            format!("#!/usr/bin/env bash\ncat >> {}\n", transcript.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let hook_cfg = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: hook_script.display().to_string(),
            timeout: 3000,
        };
        let hooks = HooksConfig {
            session_start: vec![hook_cfg.clone()],
            session_shutdown: vec![hook_cfg],
            ..Default::default()
        };

        let mut outgoing = agent_with_id(&dir, "session-A", hooks.clone());
        outgoing.permission = PermissionGate::from_cli(true /*no_hooks*/, None);
        let mut incoming = agent_with_id(&dir, "session-B", hooks);
        incoming.permission = PermissionGate::from_cli(true /*no_hooks*/, None);

        let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(Some(outgoing)));

        swap_agent_with_reason(&agent_slot, incoming, "new").await;

        assert!(
            !transcript.exists(),
            "--no-hooks must suppress both session_shutdown and session_start"
        );

        let g = agent_slot.lock().await;
        assert_eq!(g.as_ref().unwrap().session_id, "session-B");
        drop(g);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_tabs_replaces_all_tabs_with_four_spaces() {
        assert_eq!(expand_tabs("no tabs"), "no tabs");
        assert_eq!(expand_tabs("\thello"), "    hello");
        assert_eq!(expand_tabs("a\tb\tc"), "a    b    c");
        assert_eq!(expand_tabs("\t\t"), "        ");
        // Leaves other whitespace alone.
        assert_eq!(expand_tabs("  a\tb"), "  a    b");
    }

    fn mkapp() -> App {
        App::new(
            "s".into(),
            "m".into(),
            std::path::PathBuf::from("/tmp"),
            Some(crate::provider::ApiKind::Openai),
            crate::agent::build::SkillLoadPolicy::default(),
            false,
            crate::agent::prompt_override::PromptOverrides::from_cli(None, Vec::new(), false),
            Vec::new(),
            std::path::PathBuf::from("/tmp/nanopi-test-history.txt"),
        )
    }

    fn seed_input(app: &mut App, text: &str) {
        app.input.insert_str(text);
        sync_palette(app);
    }

    #[test]
    fn typing_appends() {
        let mut app = mkapp();
        assert!(matches!(
            interpret_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
            ),
            KeyAction::Nothing
        ));
        assert_eq!(app.input.as_string(), "a");
    }

    #[test]
    fn backspace_pops() {
        let mut app = mkapp();
        seed_input(&mut app, "abc");
        interpret_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
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

    /// v0.11.0: typing + Enter WHILE the agent is streaming steers the
    /// running turn instead of doing nothing (the pre-v0.11 behavior).
    /// Mirrors Pi — mid-stream typing injects a steering message.
    #[test]
    fn enter_while_streaming_steers() {
        let mut app = mkapp();
        app.status = Status::Streaming;
        seed_input(&mut app, "also check the logs");
        match interpret_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            KeyAction::SteerTurn(s) => assert_eq!(s, "also check the logs"),
            other => panic!("expected SteerTurn, got {other:?}"),
        }
    }

    /// A steer that reaches a live turn arrives as a `Steering`
    /// message, and the text comes back for the caller to echo.
    #[tokio::test]
    async fn steer_reaches_a_live_turn() {
        let (tx, mut rx) = mpsc::channel::<SteerMessage>(4);
        let got = try_steer(Some(&tx), "also check the logs".into()).await;
        assert_eq!(got.as_deref(), Ok("also check the logs"));
        match rx.recv().await {
            Some(SteerMessage::Steering { text }) => assert_eq!(text, "also check the logs"),
            other => panic!("expected a Steering message, got {other:?}"),
        }
    }

    /// Regression: when the turn ended between the keypress and the
    /// dispatch, the send failed and the text was dropped — after the
    /// TUI had already echoed "[steer] ...", so the user believed it
    /// had landed. It must come back to be queued instead.
    #[tokio::test]
    async fn steer_that_missed_its_turn_is_handed_back() {
        let (tx, rx) = mpsc::channel::<SteerMessage>(4);
        drop(rx); // the turn finished; run_turn dropped the receiver
        let got = try_steer(Some(&tx), "also update the tests".into()).await;
        assert_eq!(
            got.as_deref().unwrap_err(),
            "also update the tests",
            "a steer with nowhere to go must be returned, not swallowed"
        );
    }

    #[tokio::test]
    async fn steer_with_no_channel_is_handed_back() {
        let got = try_steer(None, "hello".into()).await;
        assert_eq!(got.as_deref().unwrap_err(), "hello");
    }

    /// A slash command typed mid-stream resolves as a command rather
    /// than being steered into the model as prose — "/compact" as a
    /// user message would be nonsense.
    ///
    /// Pinned POSITIVELY to `Compact`. This used to assert only
    /// `!matches!(got, SteerTurn(_))`, which is true of every other
    /// variant too, so the test would have passed no matter which
    /// command it dispatched — or if it silently did nothing. What
    /// actually keeps `/compact` harmless here is downstream, not in
    /// `interpret_key`: `StartTurn` takes the Agent out of its slot
    /// while streaming, so the handler's `if let Some(a)` finds `None`
    /// and no-ops.
    #[test]
    fn slash_command_while_streaming_resolves_as_a_command() {
        let mut app = mkapp();
        app.status = Status::Streaming;
        seed_input(&mut app, "/compact");
        let got = interpret_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(got, KeyAction::Compact),
            "expected the palette to resolve /compact, got {got:?}"
        );
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

    /// A plugin command with no wasm behind it — proof that the TUI
    /// side needs nothing feature-gated. These tests run in the DEFAULT
    /// build.
    fn fake_command(name: &str, plugin: &str) -> crate::command::PluginCommand {
        struct H;
        impl crate::command::CommandHandler for H {
            fn run(&self, _n: &str, _a: &str) -> Result<crate::command::CommandAction, String> {
                Ok(crate::command::CommandAction::Print("ok".into()))
            }
        }
        crate::command::PluginCommand {
            spec: crate::command::CommandSpec {
                name: name.into(),
                description: "does a thing".into(),
            },
            plugin_name: std::sync::Arc::from(plugin),
            handler: std::sync::Arc::new(H),
        }
    }

    #[test]
    fn a_plugin_command_appears_in_the_palette_and_dispatches() {
        let mut app = mkapp();
        app.commands_cache = vec![fake_command("todo", "demo")];
        seed_input(&mut app, "/todo buy milk");

        let m = app.palette.as_ref().expect("palette opens");
        let row = m
            .visible()
            .into_iter()
            .find(|it| it.label == "/todo")
            .expect("the plugin row is offered");
        assert!(
            row.description.contains("[demo]"),
            "the row must name its plugin: {}",
            row.description
        );

        let got = interpret_key(&mut app, KeyEvent::from(KeyCode::Enter));
        match got {
            KeyAction::RunPluginCommand { name, args } => {
                assert_eq!(name, "todo");
                assert_eq!(args, "buy milk");
            }
            other => panic!("expected RunPluginCommand, got {other:?}"),
        }
    }

    #[test]
    fn a_plugin_command_with_no_argument_gets_an_empty_string() {
        let mut app = mkapp();
        app.commands_cache = vec![fake_command("todo", "demo")];
        seed_input(&mut app, "/todo");
        match interpret_key(&mut app, KeyEvent::from(KeyCode::Enter)) {
            KeyAction::RunPluginCommand { args, .. } => assert_eq!(args, ""),
            other => panic!("expected RunPluginCommand, got {other:?}"),
        }
    }

    /// PI runs extension commands immediately even mid-stream
    /// (`agent-session.ts:1119-1129`, checked before the isStreaming
    /// queue branch), so this must dispatch rather than no-op.
    ///
    /// Asserted POSITIVELY. The older sibling below asserts only
    /// `!matches!(got, SteerTurn(_))`, which passes while receiving
    /// `KeyAction::Compact` — that is why it never caught anything.
    #[test]
    fn a_plugin_command_dispatches_mid_stream() {
        let mut app = mkapp();
        app.status = Status::Streaming;
        app.commands_cache = vec![fake_command("todo", "demo")];
        seed_input(&mut app, "/todo now");
        let got = interpret_key(&mut app, KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(got, KeyAction::RunPluginCommand { .. }),
            "a plugin command must run mid-stream, got {got:?}"
        );
    }

    /// A built-in must win: `resolve_commands` refuses the name, so a
    /// plugin row with that name should never be in the cache — but if
    /// one ever were, the built-in still sorts first and stays
    /// reachable.
    #[test]
    fn a_builtin_outranks_a_same_named_plugin_row() {
        let mut app = mkapp();
        app.commands_cache = vec![fake_command("compact", "rogue")];
        seed_input(&mut app, "/compact");
        assert!(
            matches!(
                interpret_key(&mut app, KeyEvent::from(KeyCode::Enter)),
                KeyAction::Compact
            ),
            "the built-in must stay reachable"
        );
    }

    /// `command::RESERVED_COMMAND_NAMES` is hand-maintained, and it has
    /// to be: plugin commands are registered inside `Agent::build_fresh`,
    /// which knows nothing about the TUI and also runs in print mode.
    /// This guard is what makes that safe. Without it the two lists drift
    /// and a plugin silently shadows a built-in — exactly the bug PI has,
    /// where `/debug` and friends are dispatched but missing from
    /// `BUILTIN_SLASH_COMMANDS`, so they never autocomplete and never
    /// participate in conflict warnings.
    #[test]
    fn reserved_command_names_match_the_builtin_palette() {
        use std::collections::BTreeSet;

        let from_palette: BTreeSet<String> = slash_items()
            .iter()
            .map(|it| it.label.trim_start_matches('/').to_string())
            .collect();
        let reserved: BTreeSet<String> = crate::command::RESERVED_COMMAND_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(
            from_palette, reserved,
            "add/remove a built-in slash command and RESERVED_COMMAND_NAMES \
             must move with it, or a plugin can claim the name"
        );
    }

    /// Regression: `sync_palette` tested the untrimmed first line while
    /// `submit_or_chat` trimmed, so one leading space closed the palette
    /// and `" /session"` was sent to the model as chat text. `/compact`
    /// happened to survive only because it was one of three names
    /// `submit_or_chat` hardcoded; the other fourteen leaked.
    #[test]
    fn a_leading_space_still_resolves_the_command() {
        for text in [" /session", "  /session", "\t/session"] {
            let mut app = mkapp();
            seed_input(&mut app, text);
            assert!(
                app.palette.is_some(),
                "palette must open for {text:?} — leading whitespace is not meaningful"
            );
            let got = interpret_key(&mut app, KeyEvent::from(KeyCode::Enter));
            assert!(
                matches!(got, KeyAction::ShowSessionInfo),
                "{text:?} must run /session, got {got:?}"
            );
        }
    }

    /// The argument splitter has to trim the same way the palette does,
    /// or `" /name x"` splits at the leading space and yields the whole
    /// line as the argument.
    #[test]
    fn a_leading_space_does_not_corrupt_the_argument() {
        let mut app = mkapp();
        seed_input(&mut app, "  /name my-experiment");
        let got = interpret_key(&mut app, KeyEvent::from(KeyCode::Enter));
        match got {
            KeyAction::ApplyName(n) => assert_eq!(n, "my-experiment"),
            other => panic!("expected ApplyName(\"my-experiment\"), got {other:?}"),
        }
    }

    /// Trimming for the palette closed the accidental escape hatch that
    /// a leading space used to provide, so an unmatched `/…` line must
    /// fall through to the model instead of leaving the user stuck on
    /// "(no matches)" with text they cannot submit. Matches PI, which
    /// has no "unknown command" error — `/typo` silently becomes a
    /// prompt.
    #[test]
    fn an_unmatched_slash_line_is_sent_as_chat() {
        for text in [
            "/etc/nginx/nginx.conf is misconfigured",
            " /usr/bin/env is missing",
            "/nosuchcommand",
        ] {
            let mut app = mkapp();
            seed_input(&mut app, text);
            assert!(app.palette.is_some(), "palette opens for {text:?}");
            assert!(
                app.palette.as_ref().unwrap().is_empty(),
                "{text:?} must match no command"
            );
            let got = interpret_key(&mut app, KeyEvent::from(KeyCode::Enter));
            match got {
                KeyAction::StartTurn(sent) => assert_eq!(sent, text.trim()),
                other => panic!("expected {text:?} to be sent as chat, got {other:?}"),
            }
        }
    }

    /// The fall-through must not hijack a line that *does* match — a
    /// prefix like `/comp` still resolves rather than being chatted.
    #[test]
    fn a_partial_command_still_resolves_rather_than_chatting() {
        let mut app = mkapp();
        seed_input(&mut app, "/comp");
        let got = interpret_key(&mut app, KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(got, KeyAction::Compact),
            "expected /comp to resolve to /compact, got {got:?}"
        );
    }

    #[test]
    fn ctrl_d_exits() {
        let mut app = mkapp();
        assert!(matches!(
            interpret_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            KeyAction::Exit
        ));
    }

    #[test]
    fn ctrl_c_streaming_cancels() {
        let mut app = mkapp();
        app.status = Status::Streaming;
        assert!(matches!(
            interpret_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::CancelTurn
        ));
    }

    #[test]
    fn ctrl_c_idle_exits() {
        let mut app = mkapp();
        assert!(matches!(
            interpret_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::Exit
        ));
    }

    /// Render the dock into an 80x24 buffer and return the non-blank
    /// rows of the input box, marker included, as plain strings.
    fn render_input_rows(app: &App) -> Vec<String> {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        draw_dock(&mut buf, area, app);
        let mut out = Vec::new();
        for y in 0..area.height {
            let row: String = (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string();
            let body = row.trim_start_matches('│').trim();
            if body.starts_with('>') || (!body.is_empty() && out.last().is_some()) {
                out.push(row);
            }
        }
        out
    }

    /// A multi-line paste must occupy multiple rows in the input box.
    /// Terminals in raw mode send bare CR for line breaks inside a
    /// bracketed paste, so all three encodings must land the same way.
    #[test]
    fn multiline_paste_renders_on_multiple_rows() {
        for (name, pasted) in [
            ("LF", "alpha\nbravo\ncharlie"),
            ("CRLF", "alpha\r\nbravo\r\ncharlie"),
            ("CR", "alpha\rbravo\rcharlie"),
            ("mixed", "alpha\r\nbravo\rcharlie"),
        ] {
            let mut app = mkapp();
            app.input.insert_str(pasted);
            assert_eq!(
                app.input.lines(),
                &["alpha".to_string(), "bravo".into(), "charlie".into()],
                "{name}: wrong buffer rows"
            );
            let rows = render_input_rows(&app);
            assert!(
                rows.len() >= 3,
                "{name}: input box rendered {} row(s), want >=3: {rows:#?}",
                rows.len()
            );
        }
    }

    /// The collapsed skill card must be a solid block — background on
    /// the pad rows above and below the label, matching PI's
    /// `Box(1, 1, customMessageBg)` and our own `render_user_echo`.
    /// Without the padding it renders as one thin tinted line and an
    /// invoked skill is easy to miss in scrollback.
    #[test]
    fn skill_card_is_a_padded_block() {
        let rows = skill_card_rows("wiz-system-prompt-njoffice");

        // Three consecutive background rows: pad, label, pad.
        let bg_run: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, (_, bg))| bg.is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(bg_run.len(), 3, "want pad+label+pad on background");
        assert_eq!(
            bg_run,
            vec![bg_run[0], bg_run[0] + 1, bg_run[0] + 2],
            "background rows must be contiguous"
        );

        // The label sits in the middle row and names the skill.
        let mid: String = rows[bg_run[1]]
            .0
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(mid.contains("[skill]"), "middle row: {mid:?}");
        assert!(mid.contains("wiz-system-prompt-njoffice"), "middle row: {mid:?}");

        // The rows flanking the block are unstyled separators.
        assert!(rows.first().unwrap().1.is_none());
        assert!(rows.last().unwrap().1.is_none());
    }

/// A rewrite corrects the stashed bar in place.
    ///
    /// The card is only drawn when the result arrives, which is what
    /// makes this possible: the user ends up seeing the command that
    /// actually ran. Without it the card showed `echo hello` while
    /// `echo REWRITTEN` executed — in manual testing the model saw its
    /// own request answered differently and invented a sandbox to
    /// explain it.
    #[test]
    fn a_rewrite_corrects_the_pending_bar() {
        let mut app = mkapp();
        app.pending_tool_calls.push((
            "call_1".to_string(),
            PendingBar {
                leading: " $ ".into(),
                body: "echo hello".into(),
            },
        ));

        let (leading, body) =
            tool_call_bar_text("bash", &serde_json::json!({"command": "echo REWRITTEN"}));
        assert_eq!(body, "echo REWRITTEN", "sanity: {leading:?}");

        // What the event handler does, asserted on the stash.
        if let Some(slot) = app
            .pending_tool_calls
            .iter_mut()
            .find(|(id, _)| id == "call_1")
        {
            slot.1.leading = " ↻ bash ".to_string();
            slot.1.body = body;
        }

        let bar = take_pending_bar(&mut app, "call_1").expect("bar");
        assert_eq!(bar.body, "echo REWRITTEN", "the card must show what ran");
        assert!(
            bar.leading.contains('↻'),
            "the rewrite must be visible, not a silent swap: {:?}",
            bar.leading
        );
    }

    /// Regression: with `tool_exec_mode = "parallel"` (the default)
    /// every ToolCall in a batch arrives before the first ToolResult.
    /// A single-slot stash meant the last call overwrote the rest, so
    /// the first card was labelled with the wrong tool and later cards
    /// rendered with no header line at all — which is what made
    /// plugin tools look like they printed nothing.
    #[test]
    fn parallel_tool_calls_each_keep_their_own_bar() {
        let mut app = mkapp();
        for (id, name) in [("call_a", "greet"), ("call_b", "fetch_head")] {
            app.pending_tool_calls.push((
                id.to_string(),
                PendingBar {
                    leading: format!(" {name} "),
                    body: "{}".into(),
                },
            ));
        }
        // Results may come back in either order; each must find its own.
        let second = take_pending_bar(&mut app, "call_b").expect("call_b bar");
        assert_eq!(second.leading, " fetch_head ");
        let first = take_pending_bar(&mut app, "call_a").expect("call_a bar");
        assert_eq!(first.leading, " greet ");
        assert!(app.pending_tool_calls.is_empty());
    }

    /// An unknown id must not silently drop the row. Providers do
    /// renumber tool_call ids, and a card with no header at all is
    /// worse than one labelled from the oldest pending call.
    #[test]
    fn an_unmatched_result_falls_back_to_the_oldest_bar() {
        let mut app = mkapp();
        app.pending_tool_calls.push((
            "call_a".into(),
            PendingBar {
                leading: " greet ".into(),
                body: String::new(),
            },
        ));
        let bar = take_pending_bar(&mut app, "some-renumbered-id").expect("fallback bar");
        assert_eq!(bar.leading, " greet ");
        assert!(take_pending_bar(&mut app, "call_a").is_none());
    }

    /// Typing a command name in full must preselect that command.
    #[test]
    fn typing_a_command_name_preselects_it() {
        for cmd in [
            "/settings",
            "/skills",
            "/model",
            "/compact",
            "/name",
            "/export",
        ] {
            let mut app = mkapp();
            seed_input(&mut app, cmd);
            let sel = app.palette.as_ref().unwrap().selected().unwrap();
            assert_eq!(
                sel.label, cmd,
                "typing {cmd:?} preselected {:?} instead",
                sel.label
            );
        }
    }
}


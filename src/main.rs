//! nanopi v0.5 — CLI entry point.
//!
//! Parses args with clap, dispatches to `mode::print` or `mode::interactive`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use nanopi::config;
use nanopi::mode::{interactive, print, tui};
use nanopi::provider;
use nanopi::wizard;

/// Minimal CLI — covers the v0.5 acceptance criteria.
#[derive(Parser, Debug)]
#[command(name = "nanopi", version, about = "minimal Pi port in Rust")]
struct Args {
    /// OpenAI-compatible API base URL. Falls back to OPENAI_BASE_URL
    /// env var, then to https://api.openai.com/v1.
    #[arg(long)]
    base_url: Option<String>,

    /// Model identifier (provider-specific). Falls back to OPENAI_MODEL
    /// env var.
    #[arg(long)]
    model: Option<String>,

    /// API key. Falls back to OPENAI_API_KEY env var.
    #[arg(long)]
    api_key: Option<String>,

    /// User message. If absent, read from stdin (interactive mode only).
    /// Can also be passed as the first positional argument.
    #[arg(short = 'm', long)]
    message: Option<String>,

    /// Positional message (alternative to --message / -m).
    #[arg(value_name = "MESSAGE")]
    positional_message: Option<String>,

    /// Non-interactive print mode (Claude Code's -p semantics).
    #[arg(short = 'p', long = "print")]
    print: bool,

    /// Output format for -p mode.
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    output: String,

    /// Disable all hooks (emergency switch).
    #[arg(long)]
    no_hooks: bool,

    /// Trust project-local resources for this run.
    #[arg(short = 'a', long = "approve")]
    approve: bool,

    /// Distrust project-local resources for this run.
    #[arg(short = 'N', long = "distrust")]
    no_approve: bool,

    /// Resume the most recently used session for this cwd.
    /// Falls back to a fresh session if no history exists.
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Resume a specific session by id (full UUID or prefix).
    #[arg(long = "session", value_name = "SESSION_ID", conflicts_with_all = ["continue_session", "fork_id"])]
    session_id: Option<String>,

    /// Fork a session: copy its history into a new session (parent_id set),
    /// then use the new session. Original is untouched.
    #[arg(long = "fork", value_name = "SESSION_ID", conflicts_with_all = ["continue_session"])]
    fork_id: Option<String>,

    /// Force the full ratatui TUI (alt-screen) even if stdin isn't a
    /// TTY. Without this or `--no-tui`, the mode is auto-selected:
    /// TTY → TUI, pipe/non-TTY → rustyline classic mode.
    #[arg(long, conflicts_with = "no_tui")]
    tui: bool,

    /// Force the rustyline classic mode (line-oriented, pipe-friendly)
    /// even in a TTY. Useful for scripts, CI, and users who prefer to
    /// keep terminal scrollback.
    #[arg(long = "no-tui")]
    no_tui: bool,

    /// Tool whitelist (comma-separated names). Default: all standard tools.
    #[arg(long, value_delimiter = ',')]
    tools: Vec<String>,

    /// Which wire protocol to use against `base_url`. Overrides
    /// `api_kind` in config.toml. `openai` (default) talks to
    /// `/chat/completions`; `anthropic` talks to `/v1/messages`.
    /// Accepts `openai` or `anthropic` (aliases: `claude`).
    #[arg(long = "api-kind", value_name = "KIND")]
    api_kind: Option<String>,

    /// Load a skill file or directory (repeatable). Additive:
    /// still loads even when `--no-skills` is set.
    /// Mirrors PI's `--skill` (`pi/packages/coding-agent/src/cli/args.ts:156`).
    #[arg(long = "skill", value_name = "PATH")]
    skill: Vec<PathBuf>,

    /// Disable user + project skill discovery. Explicit `--skill` paths
    /// still load. Mirrors PI's `--no-skills` / `-ns`.
    #[arg(long = "no-skills", short = 'S')]
    no_skills: bool,

    /// Disable AGENTS.md / CLAUDE.md discovery and loading. Mirrors PI's
    /// `--no-context-files` / `-nc`.
    #[arg(long = "no-context-files", short = 'C')]
    no_context_files: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // `nanopi init` — explicit invocation of the first-run wizard.
    // Recognized as a positional message value with no other options
    // that would imply a real chat turn. Runs BEFORE load_config so
    // a broken config.toml doesn't block reconfiguration.
    if is_init_subcommand(&args) {
        return match wizard::run_wizard(true).await {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(2)
            }
        };
    }

    // Load ~/.nanopi/config.toml + ./.nanopi/config.toml (both optional).
    // Failures here (malformed TOML) are fatal — better to surface early
    // than to silently ignore user intent.
    let mut cfg = match config::load_config(&cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    // First-run fallthrough: if the user provided no CLI creds, no env
    // creds, and no on-disk config, launch the wizard and then re-load
    // the config so the rest of main.rs can pick up the freshly-written
    // values. Only fires when ALL three axes (model, api_key, base_url)
    // are simultaneously unresolved — partial-config users get their
    // existing targeted error message so they know what to fix.
    let model_missing = args.model.is_none()
        && std::env::var("OPENAI_MODEL").is_err()
        && cfg.model.is_none();
    let key_missing = args.api_key.is_none()
        && std::env::var("OPENAI_API_KEY").is_err()
        && cfg.api_key.is_none()
        && cfg.api_key_file.is_none();
    let base_missing = args.base_url.is_none()
        && std::env::var("OPENAI_BASE_URL").is_err()
        && cfg.base_url.is_none();
    if model_missing && key_missing && base_missing {
        eprintln!("nanopi: no config found — launching first-run wizard (Ctrl-C to abort)");
        if let Err(e) = wizard::run_wizard(false).await {
            eprintln!("error: {e:#}");
            return ExitCode::from(2);
        }
        // Reload config now that the wizard has (hopefully) written one.
        cfg = match config::load_config(&cwd) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        };
    }

    // Resolve model: flag > OPENAI_MODEL env > config.toml `model` > error.
    let model = args
        .model
        .clone()
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .or_else(|| cfg.model.clone());
    let Some(model) = model else {
        eprintln!("error: no --model / OPENAI_MODEL / model in ~/.nanopi/config.toml");
        return ExitCode::from(2);
    };

    // Resolve base URL: flag > OPENAI_BASE_URL env > config.toml `base_url`
    // > OpenAI default.
    let base_url = args
        .base_url
        .clone()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .or_else(|| cfg.base_url.clone())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    // Resolve API key: flag > OPENAI_API_KEY env > config.api_key (with
    // warning) > config.api_key_file (read file) > error.
    let api_key = match args.api_key.clone() {
        Some(k) => k,
        None => match std::env::var("OPENAI_API_KEY") {
            Ok(v) => v,
            Err(_) => match &cfg.api_key {
                Some(k) => {
                    eprintln!(
                        "⚠ api_key is inline in config.toml — consider api_key_file \
                         or OPENAI_API_KEY env var to avoid accidental commits"
                    );
                    k.clone()
                }
                None => match &cfg.api_key_file {
                    Some(p) => {
                        let path = expand_tilde(p);
                        match std::fs::read_to_string(&path) {
                            Ok(s) => s.trim().to_string(),
                            Err(e) => {
                                eprintln!(
                                    "error: cannot read api_key_file {}: {e}",
                                    path.display()
                                );
                                return ExitCode::from(2);
                            }
                        }
                    }
                    None => {
                        eprintln!(
                            "error: no --api-key / OPENAI_API_KEY / \
                             api_key / api_key_file in config.toml"
                        );
                        return ExitCode::from(2);
                    }
                },
            },
        },
    };

    let approve = if args.approve {
        Some(true)
    } else if args.no_approve {
        Some(false)
    } else {
        None
    };

    // Project trust decides whether project-local `.nanopi/skills/` is
    // discovered. Precedence: explicit CLI (`-a`/`-N`) > persisted
    // decision in `~/.nanopi/trust/`. Default is untrusted — matches
    // PI's "trust prompt on first encounter" model with the prompt
    // stubbed out for now.
    let project_trusted = match approve {
        Some(v) => v,
        None => matches!(
            nanopi::trust::check_trust_status(&cwd),
            nanopi::trust::TrustStatus::AlreadyTrusted
        ),
    };
    let skill_load = nanopi::agent::build::SkillLoadPolicy::from_cli(
        &cwd,
        args.skill.clone(),
        args.no_skills,
        project_trusted,
        cfg.skills.disabled.clone(),
    );

    let output_format = if args.output == "json" {
        print::OutputFormat::Json
    } else {
        print::OutputFormat::Text
    };

    // Resolve wire-protocol kind: CLI --api-kind overrides config's
    // api_kind, which itself defaults to "openai". Announce the choice
    // once at startup so users don't have to guess.
    let api_kind =
        provider::ApiKind::from_config(args.api_kind.as_deref().or(cfg.api_kind.as_deref()));
    if matches!(api_kind, provider::ApiKind::Anthropic) {
        eprintln!("• api_kind = anthropic — talking to {base_url}/v1/messages");
    }

    let result = if args.print {
        let message = args
            .message
            .as_deref()
            .or(args.positional_message.as_deref());
        let message = match message {
            Some(m) => m,
            None => {
                eprintln!("error: -p mode requires --message");
                return ExitCode::from(2);
            }
        };
        print::run_print_mode(
            api_kind,
            &base_url,
            &model,
            &api_key,
            message,
            output_format,
            cwd,
            args.no_hooks,
            approve,
            args.continue_session,
            args.session_id.clone(),
            args.fork_id.clone(),
            skill_load.clone(),
            args.no_context_files,
        )
        .await
    } else if should_use_tui(&args) {
        tui::run_tui_mode(
            api_kind,
            &base_url,
            &model,
            &api_key,
            cwd,
            args.no_hooks,
            approve,
            args.continue_session,
            args.session_id.clone(),
            args.fork_id.clone(),
            skill_load.clone(),
            args.no_context_files,
        )
        .await
    } else {
        let message = args.message.clone().or(args.positional_message.clone());
        interactive::run_interactive_mode(
            api_kind,
            &base_url,
            &model,
            &api_key,
            message,
            cwd,
            args.no_hooks,
            approve,
            args.continue_session,
            args.session_id.clone(),
            args.fork_id.clone(),
            skill_load.clone(),
            args.no_context_files,
        )
        .await
    };

    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Decide whether to enter the full ratatui TUI. Priority:
///   --tui explicit    →  TUI
///   --no-tui explicit →  rustyline (classic)
///   stdin is a TTY    →  TUI  (nanopi's PI-style default)
///   otherwise         →  rustyline (pipe/CI-friendly, single-shot)
///
/// The `-p` (print) branch is decided elsewhere in main and never
/// reaches this function, so we don't need to handle it here.
fn should_use_tui(args: &Args) -> bool {
    if args.tui {
        return true;
    }
    if args.no_tui {
        return false;
    }
    // Auto: TTY on stdin means an interactive user session.
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Detect `nanopi init`: the positional message is exactly "init" AND
/// no other flag that would imply a real chat turn is set. Keeping the
/// check narrow means a user who literally wants to send the word "init"
/// as a prompt can still do so with `-m init`.
fn is_init_subcommand(args: &Args) -> bool {
    if args.positional_message.as_deref() != Some("init") {
        return false;
    }
    args.message.is_none()
        && !args.print
        && !args.continue_session
        && args.session_id.is_none()
        && args.fork_id.is_none()
        && args.model.is_none()
        && args.base_url.is_none()
        && args.api_key.is_none()
}

/// Expand a leading `~/` to `$HOME/`. Best-effort — if HOME is unset,
/// the path is returned unchanged.
fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

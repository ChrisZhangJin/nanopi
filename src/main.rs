//! nanopi v0.5 — CLI entry point.
//!
//! Parses args with clap, dispatches to `mode::print` or `mode::interactive`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use nanopi::config;
use nanopi::mode::{print, tui};
use nanopi::provider;
use nanopi::vendor;
use nanopi::wizard;

/// Display name for an `ApiKind`, matching the strings accepted in
/// `api_kind` so a warning can be pasted straight back into config.
fn kind_str(k: provider::ApiKind) -> &'static str {
    match k {
        provider::ApiKind::Openai => "openai",
        provider::ApiKind::Anthropic => "anthropic",
    }
}

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

    /// User message. If absent in `-p` mode, read from piped stdin.
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

    /// Use this exact session id, creating the session if it doesn't
    /// exist. For scripts that want a stable, self-chosen session to
    /// resume across runs without looking up a generated UUID.
    ///
    /// With `--fork` this names the NEW session instead, and must not
    /// already exist. Mutually exclusive with `--session`/`--continue`,
    /// which select an already-existing session.
    #[arg(long = "session-id", value_name = "ID", conflicts_with_all = ["continue_session", "session_id"])]
    exact_session_id: Option<String>,

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

    /// Replace the built-in system prompt. Accepts literal text or a path
    /// to a file. Suppresses `.nanopi/SYSTEM.md` discovery.
    #[arg(long = "system-prompt", value_name = "TEXT_OR_PATH")]
    system_prompt: Option<String>,

    /// Append to the system prompt (repeatable; values joined by a blank
    /// line). Accepts literal text or a path. Suppresses
    /// `.nanopi/APPEND_SYSTEM.md` discovery.
    #[arg(long = "append-system-prompt", value_name = "TEXT_OR_PATH")]
    append_system_prompt: Vec<String>,
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
    let prompt_overrides = nanopi::agent::prompt_override::PromptOverrides::from_cli(
        args.system_prompt.clone(),
        args.append_system_prompt.clone(),
        project_trusted,
    );

    let output_format = if args.output == "json" {
        print::OutputFormat::Json
    } else {
        print::OutputFormat::Text
    };

    // Resolve wire-protocol kind: CLI --api-kind overrides config's
    // api_kind. `None` = the user didn't say, so the vendor sniff gets
    // to decide (see provider::build).
    let api_kind_raw = args.api_kind.as_deref().or(cfg.api_kind.as_deref());
    let api_kind = provider::ApiKind::from_config_opt(api_kind_raw);
    if let Some(raw) = api_kind_raw {
        if api_kind.is_none() && !raw.trim().is_empty() {
            eprintln!(
                "nanopi: unknown api_kind `{raw}` (expected `openai` or `anthropic`) \
                 — falling through to vendor sniff"
            );
        }
    }

    // Announce the protocol we will ACTUALLY speak, resolved the same
    // way provider::build resolves it. Announcing the configured kind
    // instead used to print `/v1/messages` while the vendor sniff
    // quietly rerouted the request to `/chat/completions`.
    let startup_vendor = vendor::pick_vendor(cfg.provider.as_deref(), Some(&base_url), &model);
    let effective_kind =
        provider::effective_kind(api_kind, Some(startup_vendor.as_ref()), &base_url);
    if matches!(effective_kind, provider::ApiKind::Anthropic) {
        eprintln!(
            "• api_kind = anthropic — talking to {}/v1/messages",
            base_url.trim_end_matches('/')
        );
    }
    // An explicit api_kind that contradicts the vendor's own surface is
    // usually a mistake (e.g. `api_kind = "openai"` against a
    // `/anthropic` base_url). We honor the config — it's explicit — but
    // say so, because the failure mode downstream is a bare 404.
    if let Some(k) = api_kind {
        let vendor_says = startup_vendor.transport_for(&base_url);
        if vendor_says != k {
            eprintln!(
                "nanopi: warning: api_kind = `{}` but vendor `{}` expects `{}` for {} \
                 — honoring your api_kind",
                kind_str(k),
                startup_vendor.id(),
                kind_str(vendor_says),
                base_url.trim_end_matches('/'),
            );
        }
    }

    // The TUI needs a real terminal. `-p` is the explicit non-interactive
    // mode, but a piped invocation (`echo "..." | nanopi`) implies the
    // same thing — before, that case fell through to the rustyline mode;
    // now it routes here rather than letting ratatui fail against a pipe
    // with a bare "No such device or address".
    let non_interactive = args.print || !std::io::stdin().is_terminal();

    let result = if non_interactive {
        let from_stdin: Option<String>;
        let message = match args
            .message
            .as_deref()
            .or(args.positional_message.as_deref())
        {
            Some(m) => m,
            // No message argument: read the prompt from stdin, so
            // `echo "..." | nanopi -p` works. Only when stdin is
            // actually piped — on a TTY this would silently block
            // waiting for input the user doesn't know we want.
            None if !std::io::stdin().is_terminal() => {
                let mut buf = String::new();
                if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf) {
                    eprintln!("error: reading message from stdin: {e}");
                    return ExitCode::from(2);
                }
                if buf.trim().is_empty() {
                    eprintln!("error: no message (pass one as an argument or pipe it on stdin)");
                    return ExitCode::from(2);
                }
                from_stdin = Some(buf.trim_end().to_string());
                from_stdin.as_deref().unwrap()
            }
            None => {
                eprintln!("error: no message (pass one as an argument or pipe it on stdin)");
                return ExitCode::from(2);
            }
        };
        print::run_print_mode(
            api_kind,
            cfg.provider.clone(),
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
            args.exact_session_id.clone(),
            skill_load.clone(),
            args.no_context_files,
            prompt_overrides.clone(),
        )
        .await
    } else {
        tui::run_tui_mode(
            api_kind,
            cfg.provider.clone(),
            &base_url,
            &model,
            &api_key,
            cwd,
            args.no_hooks,
            approve,
            args.continue_session,
            args.session_id.clone(),
            args.fork_id.clone(),
            args.exact_session_id.clone(),
            skill_load.clone(),
            args.no_context_files,
            prompt_overrides.clone(),
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
        && args.system_prompt.is_none()
        && args.append_system_prompt.is_empty()
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

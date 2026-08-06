//! nanopi v0.5 — CLI entry point.
//!
//! Parses args with clap, dispatches to `mode::print` or `mode::interactive`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use nanopi::mode::{interactive, print};

/// Minimal CLI — covers the v0.5 acceptance criteria.
#[derive(Parser, Debug)]
#[command(name = "nanopi", version, about = "minimal Pi port in Rust (v0.5)")]
struct Args {
    /// OpenAI-compatible API base URL.
    #[arg(long, default_value = "https://api.openai.com/v1")]
    base_url: String,

    /// Model identifier (provider-specific).
    #[arg(long)]
    model: String,

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

    /// Skip all gates: trust prompts, hook blocks, permission dialogs.
    #[arg(long)]
    yolo: bool,

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

    /// Tool whitelist (comma-separated names). Default: all standard tools.
    #[arg(long, value_delimiter = ',')]
    tools: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    // YOLO mode warning (per docs/v0.5-research.md §4.5).
    if args.yolo {
        eprintln!(
            "⚠ --yolo enabled. Skipping: project trust prompts, beforeToolCall blocks,\n  \
             permission dialogs. Bash output truncation (30KB) still applies."
        );
    }

    // Resolve API key.
    let api_key = match args.api_key {
        Some(k) => k,
        None => match std::env::var("OPENAI_API_KEY") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("error: no --api-key and OPENAI_API_KEY is unset");
                return ExitCode::from(2);
            }
        },
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let approve = if args.approve {
        Some(true)
    } else if args.no_approve {
        Some(false)
    } else {
        None
    };

    let output_format = if args.output == "json" {
        print::OutputFormat::Json
    } else {
        print::OutputFormat::Text
    };

    let result = if args.print {
        let message = args.message.as_deref().or(args.positional_message.as_deref());
        let message = match message {
            Some(m) => m,
            None => {
                eprintln!("error: -p mode requires --message");
                return ExitCode::from(2);
            }
        };
        print::run_print_mode(
            &args.base_url,
            &args.model,
            &api_key,
            message,
            output_format,
            cwd,
            args.yolo,
            args.no_hooks,
            approve,
            args.continue_session,
            args.session_id.clone(),
            args.fork_id.clone(),
        )
        .await
    } else {
        let message = args.message.clone().or(args.positional_message.clone());
        interactive::run_interactive_mode(
            &args.base_url,
            &args.model,
            &api_key,
            message,
            cwd,
            args.yolo,
            args.no_hooks,
            approve,
            args.continue_session,
            args.session_id.clone(),
            args.fork_id.clone(),
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
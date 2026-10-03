//! Print mode (`-p`) — non-interactive, output to stdout, exit on completion.
//!
//! See `docs/v0.5-research.md` §5 for the design.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::agent::loop_::Agent;
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
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct JsonEnvelope {
    pub session_id: String,
    pub model: String,
    pub finish_reason: String,
    pub duration_ms: u64,
    pub usage: Value,
    pub messages: Vec<Value>,
    /// `completed` | `limit_reached` | `failed`. Absent on old readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `max_turns` | `token_budget` when `status == limit_reached`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// The `--brief` path (the child's report lives there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    /// `NANOPI_AGENT_ID` when running as a subagent child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Error text when `status == failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Child-side (`subagent` process) options for print mode.
#[derive(Debug, Clone, Default)]
pub struct ChildOptions {
    /// `--session-file`: exact transcript path; never the active session.
    pub session_file: Option<PathBuf>,
    /// `--max-turns`.
    pub max_turns: Option<u32>,
    /// `--token-budget`.
    pub token_budget: Option<u64>,
    /// `--brief`.
    pub brief: Option<PathBuf>,
    /// `NANOPI_AGENT_ID`.
    pub agent_id: Option<String>,
    /// Running as a child: `subagent` is never registered (D-05).
    pub agent_mode: bool,
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
    // `config.inline_think_tags` — escape hatch for the inline
    // `<think>` splitter (on by default). `None` leaves it on.
    inline_think_tags: Option<bool>,
    // `--tools` allowlist. Empty = all built-in tools load.
    tools_allow: Vec<String>,
    // `--no-session`: ephemeral run. Nothing is persisted to
    // `~/.nanopi/sessions/` and this run never becomes the cwd's active
    // session. Mirrors PI's `SessionManager.inMemory` (`main.ts:367`).
    no_session: bool,
    child: ChildOptions,
) -> Result<i32> {
    let started = std::time::Instant::now();

    // `-p` is non-interactive/scriptable: suppress startup diagnostic
    // blocks (e.g. `[Extensions]`) that are TUI terminal chrome. Set
    // before extensions load so their notices never emit.
    crate::render::notice::set_quiet(true);

    // Resolve which session to use. For an ephemeral run there is nothing
    // to resolve — `--no-session` is incompatible with the resume flags
    // (rejected in main.rs) — so we go straight to a temp-file session.
    // `--session-file` (clap rejects it alongside every other selector):
    // open or create the transcript at that exact path.
    let session_file = match &child.session_file {
        Some(p) => Some((
            p.clone(),
            session::open_or_create_at(p, &cwd, model, base_url)
                .map_err(|e| anyhow::anyhow!("open session file {}: {e}", p.display()))?,
        )),
        None => None,
    };

    let choice = if no_session {
        None
    } else if let Some((p, (_, resumed))) = &session_file {
        Some(if *resumed {
            session::SessionChoice::Resume(p.clone())
        } else {
            // Fresh file already created above; handled specially below.
            session::SessionChoice::New
        })
    } else {
        // --fork > --session > --continue > new.
        Some(
            session::resolve_session(
                &cwd,
                continue_session,
                session_id.as_deref(),
                fork_id.as_deref(),
                exact_session_id.as_deref(),
            )
            .map_err(|e| anyhow::anyhow!("resolve session: {e}"))?,
        )
    };

    let (session_path, header) = match &choice {
        // Ephemeral: temp file, deleted at end of run. An explicit
        // --session-id is still honored as the id.
        None => session::new_ephemeral_session(&cwd, model, base_url, exact_session_id.as_deref())
            .map_err(|e| anyhow::anyhow!("create ephemeral session: {e}"))?,
        Some(session::SessionChoice::Resume(p)) => {
            // Reuse the existing session. We trust its recorded model /
            // base_url; if those are wrong the user can pass them again
            // via flags and the next turn will pick them up.
            let (h, _entries) = session::read_session(p)
                .map_err(|e| anyhow::anyhow!("read resumed session: {e}"))?;
            (p.clone(), h)
        }
        Some(session::SessionChoice::New) => match &session_file {
            Some((p, (h, _))) => (p.clone(), h.clone()),
            None => session::new_session(&cwd, model, base_url)
                .map_err(|e| anyhow::anyhow!("create session: {e}"))?,
        },
        Some(session::SessionChoice::NewWithId(id)) => {
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
    // Skipped for ephemeral runs — they must leave no trace.
    // Also skipped for `--session-file`: a child's transcript must never
    // become the cwd's active session (research anti-pattern).
    if !no_session && session_file.is_none() {
        let _ = session::set_active_session(&cwd, &session_path);
    }

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
        inline_think_tags,
    );
    let permission = PermissionGate::from_cli(no_hooks, approve);

    // Every `SettingsError` is a mistake in the user's own config —
    // unparseable TOML, or a matcher that provably can never fire.
    // Refusing to start is the only honest response: the fallback used
    // to be `HooksConfig::default()`, which drops *every* hook in the
    // file, not just the offending one. A `check-rm-rf.sh` veto hook
    // then silently stops running while the warning scrolls away —
    // zero protection with a one-line notice, the same failure shape
    // T2.4 was written to eliminate. Same severity as an unknown hook
    // event key, which has always been fatal.
    let hooks = settings::load_settings(&cwd).map_err(|e| anyhow::anyhow!("{e}"))?;

    // `--tools`: built-in names restrict the standard set; any other
    // name may be a WASM plugin tool, resolved once extensions load
    // (D-14). Unlisted plugin tools are never registered.
    let (builtin_allow, _, plugin_candidates) =
        crate::tool::split_tool_allowlist(&tools_allow, &[]);
    let mut registry = if tools_allow.is_empty() {
        ToolRegistry::standard()
    } else if builtin_allow.is_empty() {
        ToolRegistry::new()
    } else {
        ToolRegistry::standard_with_allowlist(&builtin_allow).map_err(|e| anyhow::anyhow!("{e}"))?
    };
    if !tools_allow.is_empty() {
        registry.set_plugin_allowlist(&plugin_candidates);
    }
    // Depth 1 (D-05): a child never gets the subagent tool, even if the
    // parent listed it.
    if child.agent_mode {
        registry.remove("subagent");
    }

    // v0.11.0: `tool_exec_mode` + `[[extensions]]` live in config.toml,
    // which isn't threaded through this function's parameter list.
    // Re-read it here rather than widening the signature; a parse
    // failure already surfaced above via load_settings, so fall back
    // to defaults quietly instead of double-reporting.
    let cfg_for_build = crate::config::load_config(&cwd).unwrap_or_default();

    // If we resumed an existing session, hydrate the Agent with its
    // history (so the model sees prior turns). Otherwise start fresh.
    use crate::agent::build::{print_skill_diagnostics, AgentBuildInputs};
    let agent = if let Some(session::SessionChoice::Resume(_)) = &choice {
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
            tool_exec_overrides: cfg_for_build.tool_exec_overrides.clone(),
            extensions: cfg_for_build.extensions,
        });
        print_skill_diagnostics(&diags);
        a
    };
    let mut agent = agent;

    // Any `--tools` name that is neither built-in nor a loaded plugin
    // tool is a hard error, same as before plugins were allowed.
    let loaded_plugins = agent.registry.plugin_tool_names();
    let (_, _, unknown) = crate::tool::split_tool_allowlist(&plugin_candidates, &loaded_plugins);
    if !unknown.is_empty() {
        let mut valid = ToolRegistry::standard().names();
        valid.extend(loaded_plugins);
        anyhow::bail!(
            "unknown tool {:?} in --tools; valid tools: {}",
            unknown[0],
            valid.join(", ")
        );
    }

    // Child limits bound the whole run, including the brief self-check
    // turns (CR-01), not each `run_turn` call.
    if child.brief.is_some() || child.max_turns.is_some() || child.token_budget.is_some() {
        agent.set_run_scoped_limits(true);
    }
    if let Some(n) = child.max_turns {
        agent.set_max_turns(n);
    }
    if child.token_budget.is_some() {
        agent.set_token_budget(child.token_budget);
    }

    // Fire session_start hooks before the first turn.
    agent.fire_session_start("startup").await;

    // `--brief` (RT-09): the brief file is the task; a positional
    // message, if any, follows it.
    let brief_path = child.brief.clone();
    let task = match &brief_path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(b) if message.trim().is_empty() => b,
            Ok(b) => format!("{b}\n\n{message}"),
            Err(e) => {
                eprintln!("nanopi: cannot read brief {}: {e}", p.display());
                message.to_string()
            }
        },
        None => message.to_string(),
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let agent_task = {
        let mut agent = agent; // move
        let brief_path = brief_path.clone();
        tokio::spawn(async move {
            let steer = brief_path.as_ref().map(|p| start_brief_watch(p));
            let mut r = agent.run_turn(task.as_str(), &tx, None, steer).await;
            let mut limit = agent.last_limit_hit();
            let mut checklist_reply: Option<String> = None;
            // Bounded self-check (D-11, T-01-16): at most
            // SELF_CHECK_TURNS extra turns, none after a limit or error.
            if let Some(p) = brief_path.as_ref() {
                for _ in 0..SELF_CHECK_TURNS {
                    if r.is_err() || limit.is_some() {
                        break;
                    }
                    let current = std::fs::read_to_string(p).unwrap_or_default();
                    let prompt = self_check_prompt(&current);
                    r = agent
                        .run_turn(prompt.as_str(), &tx, None, Some(start_brief_watch(p)))
                        .await;
                    limit = agent.last_limit_hit();
                    match &r {
                        Ok(text) => {
                            checklist_reply = Some(text.clone());
                            if !has_open_items(text) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            // Fire session_end regardless of turn outcome so cleanup
            // hooks (e.g. flush metrics) always run.
            agent.fire_session_shutdown("quit").await;
            (r, limit, checklist_reply)
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
                if stops_spinner(&ev) {
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
    let (turn_result, limit_hit, checklist_reply) = agent_task.await?;
    let status = if turn_result.is_err() {
        "failed"
    } else if limit_hit.is_some() {
        "limit_reached"
    } else {
        "completed"
    };
    // report.md on every exit path (D-11). Best effort.
    let report_path = brief_path.as_deref().map(|p| {
        let rp = report_path_for(p);
        let summary = match &turn_result {
            Ok(t) => t.clone(),
            Err(e) => format!("error: {e}"),
        };
        let items = checklist_items(checklist_reply.as_deref(), status);
        let body = crate::agent::brief::render_report(status, &summary, &items);
        if let Err(e) = write_private(&rp, &body) {
            eprintln!("nanopi: cannot write report {}: {e}", rp.display());
        }
        rp
    });
    // Text mode keeps the old behavior (error -> exit 1 via main). JSON
    // mode reports the failure in-band so the parent can parse it.
    let turn_error = match turn_result {
        Ok(_) => None,
        Err(e) if output == OutputFormat::Json => Some(e.to_string()),
        Err(e) => return Err(e.into()),
    };

    let duration_ms = started.elapsed().as_millis() as u64;

    let result = match output {
        OutputFormat::Text => {
            if no_session {
                eprintln!("\n✓ ephemeral session {} (not saved)", header.id);
            } else {
                eprintln!(
                    "\n✓ session {} saved to {}",
                    header.id,
                    session_path.display()
                );
            }
            Ok(0)
        }
        OutputFormat::Json => {
            let envelope = JsonEnvelope {
                session_id: header.id.clone(),
                model: model.to_string(),
                finish_reason: "stop".into(),
                duration_ms,
                usage: json!({}),
                // Read entries back BEFORE the ephemeral file is deleted.
                messages: collect_messages(&session_path)?,
                status: Some(status.to_string()),
                limit: limit_hit.map(str::to_string),
                report_path: report_path.as_ref().map(|p| p.display().to_string()),
                agent_id: child.agent_id.clone(),
                error: turn_error.clone(),
            };
            let s = serde_json::to_string(&envelope)?;
            println!("{s}");
            Ok(if turn_error.is_some() { 1 } else { 0 })
        }
    };

    // Ephemeral cleanup: the JSON envelope (if any) has already read the
    // messages back, so the temp file has served its purpose.
    if no_session {
        let _ = std::fs::remove_file(&session_path);
    }

    result
}

/// Poll interval for brief amendments (assumption A3).
const BRIEF_POLL: std::time::Duration = std::time::Duration::from_millis(500);
/// Hard cap on extra self-check turns (T-01-16).
const SELF_CHECK_TURNS: usize = 2;

/// Start an amendment watcher for one `run_turn`; the watcher exits when
/// the returned receiver is dropped at the end of that turn.
fn start_brief_watch(p: &std::path::Path) -> mpsc::Receiver<crate::event::SteerMessage> {
    let content = std::fs::read_to_string(p).unwrap_or_default();
    let last = crate::agent::brief::next_amendment_number(&content).saturating_sub(1);
    let (stx, srx) = mpsc::channel(8);
    crate::mode::brief_watch::spawn_brief_watcher(p.to_path_buf(), last, stx, BRIEF_POLL);
    srx
}

fn self_check_prompt(brief: &str) -> String {
    format!(
        "Before finishing, re-read your brief below and check your work against it.\n\n\
         ---\n{brief}\n---\n\n\
         Reply with a checklist block, one line per requirement and amendment, \
         formatted exactly as `- [x] item — note` (done) or `- [ ] item — note` (not done). \
         If any item is not done, keep working on it first, then reply with the updated checklist."
    )
}

fn checklist_lines(text: &str) -> impl Iterator<Item = (bool, &str)> {
    text.lines().filter_map(|l| {
        let l = l.trim_start();
        if let Some(r) = l.strip_prefix("- [ ]") {
            Some((false, r.trim()))
        } else if let Some(r) = l.strip_prefix("- [x]").or_else(|| l.strip_prefix("- [X]")) {
            Some((true, r.trim()))
        } else {
            None
        }
    })
}

fn has_open_items(text: &str) -> bool {
    checklist_lines(text).any(|(done, _)| !done)
}

/// Parse the last self-check reply into checklist items.
fn checklist_items(reply: Option<&str>, status: &str) -> Vec<crate::agent::brief::ChecklistItem> {
    use crate::agent::brief::ChecklistItem;
    let items: Vec<ChecklistItem> = reply
        .map(|t| {
            checklist_lines(t)
                .map(|(done, rest)| {
                    let (label, note) = match rest.split_once(" — ") {
                        Some((a, b)) => (a.trim(), b.trim()),
                        None => (rest, ""),
                    };
                    ChecklistItem {
                        label: label.to_string(),
                        done,
                        note: note.to_string(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        vec![ChecklistItem {
            label: "checklist missing".into(),
            done: false,
            note: format!("no self-check checklist produced (status: {status})"),
        }]
    } else {
        items
    }
}

/// `report.md` next to the brief.
fn report_path_for(brief: &std::path::Path) -> PathBuf {
    brief
        .parent()
        .map(|d| d.join("report.md"))
        .unwrap_or_else(|| PathBuf::from("report.md"))
}

fn write_private(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(body.as_bytes())
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

/// Does this event mean the spinner has to go?
///
/// A predicate rather than an inline `matches!` so the decision is
/// testable. It cannot be tested through the process's output: the
/// spinner writes to **stderr** and everything else here goes to
/// stdout, so a piped run separates them and sees nothing wrong. The
/// damage only appears on a TTY, where both land on one screen.
///
/// The rule: anything that streams output of its own stops the
/// spinner, because the spinner redraws with `\r\x1b[K` and shreds
/// whatever shares the terminal with it.
///
/// `ThinkingDelta` belongs here and was missing. Reported from a real
/// minimax session as reasoning interleaved with `⠼ thinking (1.7s)`
/// and broken across lines at arbitrary points. Latent since thinking
/// existed — the Anthropic wire always hit it — and invisible on the
/// OpenAI wire until inline `<think>` began arriving as ThinkingDelta
/// rather than as ordinary text.
fn stops_spinner(ev: &AgentEvent) -> bool {
    matches!(
        ev,
        AgentEvent::TextDelta { .. }
            | AgentEvent::ThinkingDelta { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::Error { .. }
    )
}

#[cfg(test)]
mod spinner_tests {
    use super::*;
    use crate::event::{FinishReason, Usage};

    #[test]
    fn anything_that_streams_output_stops_the_spinner() {
        for ev in [
            AgentEvent::TextDelta {
                content_index: 0,
                text: "hi".into(),
            },
            AgentEvent::ThinkingDelta {
                content_index: 0,
                text: "musing".into(),
            },
            AgentEvent::ToolCall {
                content_index: 0,
                call: crate::event::ToolCall {
                    id: "c".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                },
            },
            AgentEvent::Error {
                error: "boom".into(),
            },
        ] {
            assert!(stops_spinner(&ev), "should stop the spinner: {ev:?}");
        }
    }

    /// Events that print nothing leave it running — otherwise the
    /// spinner would vanish at `Start` and the user would stare at a
    /// blank screen for the whole first token latency, which is the
    /// reason it exists.
    #[test]
    fn silent_events_leave_the_spinner_running() {
        for ev in [
            AgentEvent::Start {
                message_id: "m".into(),
            },
            AgentEvent::Done {
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            },
        ] {
            assert!(!stops_spinner(&ev), "should NOT stop the spinner: {ev:?}");
        }
    }
}

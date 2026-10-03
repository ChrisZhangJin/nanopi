//! `subagent` — delegate a task to a specialized agent in an isolated
//! context window.
//!
//! Mirrors PI's subagent extension
//! (`pi/examples/extensions/subagent/index.ts`), but in-tree as a
//! built-in rather than a plugin: the WASM plugin path is bounded by
//! `plugin_tools::PLUGIN_TOOL_DEADLINE` (30s), which a real subagent
//! LLM run blows straight past.
//!
//! The mechanism is exactly PI's: spawn `nanopi` as a child process
//! with a delegated system prompt and (optional) restricted toolset,
//! then read back its `-p --output json` envelope. A separate process
//! is a separate context window — that is the whole point.
//!
//! v1 ships **single mode** only (`{ agent, task }`). Parallel and
//! chain modes and streaming output are deferred; see `docs/` for the
//! staged plan. Subagent runs are ephemeral: the child is spawned with
//! `--no-session` so it leaves no session file behind.
//!
//! ## Known limitations (v1)
//! - **Provider inheritance.** The child resolves its own provider
//!   config from the environment and `config.toml`, the same as any
//!   `nanopi` invocation. A parent configured purely via `--api-key` /
//!   `--base-url` flags (with nothing in env or config) will not pass
//!   those down. The agent's `model:` frontmatter is honored via
//!   `--model`; otherwise the child uses its own default model.
//!   (These are the only inheritance gaps; session pollution is solved
//!   — the child runs with `--no-session`, so nothing is persisted.)

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::agent::agents::{discover_agents, AgentConfig, AgentScope, AgentSource};
use crate::agent::context::ToolSpec;
use crate::mode::print::JsonEnvelope;
use crate::tool::{ExecutionMode, Tool, ToolContext, ToolError, ToolOutput};

pub struct SubagentTool;

/// Hard cap on how many subagents a single `parallel` call may fan out
/// to, mirroring PI's `MAX_PARALLEL_TASKS = 8`. Keeps a runaway model
/// from spawning a fork bomb of `nanopi` processes.
const MAX_TASKS: usize = 8;

/// How many subagent processes may run at once, mirroring PI's
/// `MAX_CONCURRENCY = 4`. Bounds memory/CPU while still overlapping the
/// LLM latency that dominates each run.
const MAX_CONCURRENCY: usize = 4;

/// The three shapes the tool accepts. Exactly one must be present.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Single,
    Parallel,
    Chain,
}

/// One unit of work in a `parallel`/`chain` batch (also models the
/// single-mode call after parsing).
#[derive(Debug, Clone)]
struct SubagentItem {
    agent: String,
    task: String,
    cwd: Option<String>,
}

/// Decide which mode the arguments select, enforcing exactly-one-of
/// `task` / `tasks` / `chain`. Pure so it can be unit-tested directly.
fn select_mode(args: &Value) -> Result<Mode, String> {
    let has_task = args.get("task").is_some();
    let has_tasks = args.get("tasks").is_some();
    let has_chain = args.get("chain").is_some();
    match (has_task, has_tasks, has_chain) {
        (true, false, false) => Ok(Mode::Single),
        (false, true, false) => Ok(Mode::Parallel),
        (false, false, true) => Ok(Mode::Chain),
        (false, false, false) => {
            Err("provide exactly one of `task`, `tasks`, or `chain`".into())
        }
        _ => Err("provide exactly one of `task`, `tasks`, or `chain` (got more than one)".into()),
    }
}

/// Substitute the literal `{previous}` placeholder with the prior
/// step's output. The first chain step sees an empty string. Pure so
/// it can be unit-tested directly. Mirrors PI's `String.replace(/\{previous\}/g, ...)`.
fn substitute_previous(task: &str, previous: &str) -> String {
    task.replace("{previous}", previous)
}

/// Parse a `tasks`/`chain` array into concrete items, validating each
/// entry's shape. `field` names the array for error messages.
fn parse_items(value: &Value, field: &str) -> Result<Vec<SubagentItem>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| format!("`{field}` must be an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, it) in arr.iter().enumerate() {
        let agent = it
            .get("agent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("`{field}[{i}].agent` must be a string"))?
            .to_string();
        let task = it
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("`{field}[{i}].task` must be a string"))?
            .to_string();
        let cwd = it.get("cwd").and_then(|v| v.as_str()).map(String::from);
        out.push(SubagentItem { agent, task, cwd });
    }
    Ok(out)
}

/// A soft (in-band) error: reported to the model as a failed tool
/// result rather than aborting the batch.
fn soft_error(content: String) -> ToolOutput {
    ToolOutput {
        content,
        is_error: true,
        metadata: None,
        images: Vec::new(),
    }
}

/// Resolve a `cwd` override against `base`, or default to `base`.
fn resolve_cwd(base: &Path, rel: Option<&str>) -> PathBuf {
    match rel {
        Some(rel) => {
            let p = PathBuf::from(rel);
            if p.is_absolute() {
                p
            } else {
                base.join(rel)
            }
        }
        None => base.to_path_buf(),
    }
}

/// Compact per-item metadata for aggregation output.
fn item_meta(agent: &str, out: &ToolOutput) -> Value {
    json!({
        "agent": agent,
        "finish_reason": out.metadata.as_ref().and_then(|m| m.get("finish_reason").cloned()),
        "usage": out.metadata.as_ref().and_then(|m| m.get("usage").cloned()),
    })
}

/// Aggregate parallel results (in submission order) into one output:
/// a section per task, `is_error` set if any task failed. Pure.
fn format_parallel(results: &[(String, ToolOutput)]) -> ToolOutput {
    let success = results.iter().filter(|(_, o)| !o.is_error).count();
    let any_error = results.iter().any(|(_, o)| o.is_error);
    let sections: Vec<String> = results
        .iter()
        .map(|(agent, out)| {
            let status = if out.is_error { "error" } else { "ok" };
            format!("### [{agent}] {status}\n\n{}", out.content)
        })
        .collect();
    let meta: Vec<Value> = results
        .iter()
        .map(|(agent, out)| item_meta(agent, out))
        .collect();
    let header = format!("Parallel: {success}/{} succeeded", results.len());
    ToolOutput {
        content: format!("{header}\n\n{}", sections.join("\n\n---\n\n")),
        is_error: any_error,
        metadata: Some(json!({ "mode": "parallel", "tasks": meta })),
        images: Vec::new(),
    }
}

/// Aggregate chain steps into one output. `failed_at`, if set, is the
/// zero-based index of the step that stopped the chain. The final
/// content is the last step's output, followed by a per-step
/// transcript. Pure.
fn format_chain(steps: &[(String, ToolOutput)], failed_at: Option<usize>) -> ToolOutput {
    let transcript: Vec<String> = steps
        .iter()
        .enumerate()
        .map(|(i, (agent, out))| {
            let status = if out.is_error { "error" } else { "ok" };
            format!("### Step {} [{agent}] {status}\n\n{}", i + 1, out.content)
        })
        .collect();
    let meta: Vec<Value> = steps
        .iter()
        .enumerate()
        .map(|(i, (agent, out))| {
            let mut m = item_meta(agent, out);
            m["step"] = json!(i + 1);
            m
        })
        .collect();
    let joined = transcript.join("\n\n---\n\n");
    let (content, is_error) = match failed_at {
        Some(idx) => {
            let (agent, out) = &steps[idx];
            (
                format!(
                    "Chain stopped at step {} ({agent}): {}\n\n{joined}",
                    idx + 1,
                    out.content
                ),
                true,
            )
        }
        None => {
            let last = steps
                .last()
                .map(|(_, o)| o.content.clone())
                .unwrap_or_else(|| "(no output)".to_string());
            (format!("{last}\n\n---\n\n{joined}"), false)
        }
    };
    ToolOutput {
        content,
        is_error,
        metadata: Some(json!({ "mode": "chain", "steps": meta })),
        images: Vec::new(),
    }
}

/// Parse the `agent_scope` argument. Defaults to `user` (only
/// `~/.nanopi/agents`) — the safe scope, since project agents are
/// repo-controlled prompts that can instruct the model to run bash.
fn parse_scope(args: &Value) -> Result<AgentScope, String> {
    match args.get("agent_scope").and_then(|v| v.as_str()) {
        None | Some("user") => Ok(AgentScope::User),
        Some("project") => Ok(AgentScope::Project),
        Some("both") => Ok(AgentScope::Both),
        Some(other) => Err(format!(
            "agent_scope must be one of user|project|both, got {other:?}"
        )),
    }
}

/// Locate the `nanopi` executable to re-invoke. Prefer the running
/// binary so a subagent uses the exact same build as its parent.
fn nanopi_invocation() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nanopi"))
}

/// Extract the final assistant text from a parsed envelope's messages.
/// Messages are `{"role": "...", "content": "..."}` per
/// `mode::print::collect_messages`.
fn final_assistant_text(env: &JsonEnvelope) -> String {
    for m in env.messages.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                if !c.trim().is_empty() {
                    return c.to_string();
                }
            }
        }
    }
    String::new()
}

#[async_trait]
impl Tool for SubagentTool {
    // A subagent spawns a whole `nanopi` process; like `bash`, what it
    // touches is opaque, so serialize the batch it appears in.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "subagent".into(),
            description: concat!(
                "Delegate tasks to specialized agents that each run in an isolated ",
                "context window (a separate nanopi process). Use this to keep ",
                "large, self-contained subtasks (recon, planning, review) out of ",
                "your own context. Agents are defined as markdown files in ",
                "~/.nanopi/agents (user) or .nanopi/agents (project). The default ",
                "agent_scope is \"user\"; \"project\"/\"both\" require a trusted ",
                "project.\n\n",
                "Provide EXACTLY ONE of three modes:\n",
                "- single: {agent, task} — one agent, returns its final answer.\n",
                "- parallel: {tasks: [{agent, task, cwd?}, ...]} — runs concurrently ",
                "(max 8 tasks, 4 at a time); returns a section per task.\n",
                "- chain: {chain: [{agent, task, cwd?}, ...]} — runs sequentially; the ",
                "literal `{previous}` in each task is replaced by the prior step's ",
                "output (empty for the first step); stops at the first failed step."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "single mode: name of the agent to invoke (its frontmatter `name`)."
                    },
                    "task": {
                        "type": "string",
                        "description": "single mode: the task to delegate. Be specific and self-contained: the agent shares none of your context."
                    },
                    "tasks": {
                        "type": "array",
                        "description": "parallel mode: run these agents concurrently (max 8; up to 4 at a time).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": {"type": "string", "description": "Name of the agent to invoke."},
                                "task": {"type": "string", "description": "Task to delegate to the agent."},
                                "cwd": {"type": "string", "description": "Optional working directory for this agent."}
                            },
                            "required": ["agent", "task"]
                        }
                    },
                    "chain": {
                        "type": "array",
                        "description": "chain mode: run these agents in order. Use the literal `{previous}` in a task to inject the prior step's output.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": {"type": "string", "description": "Name of the agent to invoke."},
                                "task": {"type": "string", "description": "Task with optional {previous} placeholder for prior output."},
                                "cwd": {"type": "string", "description": "Optional working directory for this agent."}
                            },
                            "required": ["agent", "task"]
                        }
                    },
                    "agent_scope": {
                        "type": "string",
                        "enum": ["user", "project", "both"],
                        "description": "Which agent directories to search. Default \"user\"."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "single mode: optional working directory for the agent process. Defaults to the current directory."
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let scope = parse_scope(&args).map_err(ToolError::InvalidArgs)?;
        let mode = select_mode(&args).map_err(ToolError::InvalidArgs)?;

        match mode {
            Mode::Single => {
                let agent_name = args
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidArgs("agent must be a string".into()))?;
                let task = args
                    .get("task")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidArgs("task must be a string".into()))?;
                let run_cwd = resolve_cwd(&ctx.cwd, args.get("cwd").and_then(|v| v.as_str()));
                run_item(agent_name, task, scope, &run_cwd).await
            }
            Mode::Parallel => {
                let items = parse_items(&args["tasks"], "tasks").map_err(ToolError::InvalidArgs)?;
                if items.len() > MAX_TASKS {
                    return Err(ToolError::InvalidArgs(format!(
                        "too many parallel tasks ({}); max is {MAX_TASKS}",
                        items.len()
                    )));
                }
                run_parallel(items, scope, &ctx.cwd).await
            }
            Mode::Chain => {
                let items = parse_items(&args["chain"], "chain").map_err(ToolError::InvalidArgs)?;
                run_chain(items, scope, &ctx.cwd).await
            }
        }
    }
}

/// Resolve an agent by name under `scope` at `run_cwd`, applying the
/// project-trust gate, then run it. Agent-resolution and trust
/// failures come back as soft (in-band) errors so a batch item can fail
/// without aborting its siblings.
async fn run_item(
    agent_name: &str,
    task: &str,
    scope: AgentScope,
    run_cwd: &Path,
) -> Result<ToolOutput, ToolError> {
    match resolve_agent(agent_name, scope, run_cwd) {
        Ok(agent) => run_single(&agent, task, run_cwd).await,
        Err(soft) => Ok(soft),
    }
}

/// Discover + trust-gate a single agent. On failure returns a
/// soft-error `ToolOutput` describing what went wrong.
fn resolve_agent(
    agent_name: &str,
    scope: AgentScope,
    run_cwd: &Path,
) -> Result<AgentConfig, ToolOutput> {
    let discovery = discover_agents(run_cwd, scope);
    let Some(agent) = discovery.agents.iter().find(|a| a.name == agent_name).cloned() else {
        let available = if discovery.agents.is_empty() {
            "none".to_string()
        } else {
            discovery
                .agents
                .iter()
                .map(|a| format!("{} ({:?})", a.name, a.source))
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(soft_error(format!(
            "Unknown agent {agent_name:?}. Available agents: {available}."
        )));
    };

    // Trust gate: project-sourced agents are repo-controlled prompts
    // that can instruct the model to run bash. Refuse them unless the
    // project is already trusted. User agents are always fine.
    if agent.source == AgentSource::Project
        && !matches!(
            crate::trust::check_trust_status(run_cwd),
            crate::trust::TrustStatus::AlreadyTrusted
        )
    {
        return Err(soft_error(format!(
            "Refusing to run project-local agent {agent_name:?}: this project is not trusted. \
             Approve it (nanopi -a) or use a user-level agent."
        )));
    }

    Ok(agent)
}

/// Run a `parallel` batch: fan out with a `Semaphore`-bounded
/// `join_all`. Each future owns its own `Command`/`Child`, so a parent
/// cancellation that drops this future propagates `kill_on_drop` to
/// every in-flight child — no detached tasks outlive cancellation.
async fn run_parallel(
    items: Vec<SubagentItem>,
    scope: AgentScope,
    base_cwd: &Path,
) -> Result<ToolOutput, ToolError> {
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENCY));
    let futs = items.into_iter().map(|item| {
        let sem = Arc::clone(&sem);
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        async move {
            // A permit is only released when this guard drops with the
            // future, so a cancelled batch can't leak concurrency slots.
            let _permit = sem
                .acquire()
                .await
                .expect("subagent semaphore is never closed");
            let out = run_item(&item.agent, &item.task, scope, &run_cwd).await;
            (item.agent, out)
        }
    });

    let mut results = Vec::new();
    for (agent, out) in join_all(futs).await {
        results.push((agent, out?));
    }
    Ok(format_parallel(&results))
}

/// Run a `chain`: steps in order, each seeing the prior step's output
/// via `{previous}`. Stops at the first failed step and reports where.
async fn run_chain(
    items: Vec<SubagentItem>,
    scope: AgentScope,
    base_cwd: &Path,
) -> Result<ToolOutput, ToolError> {
    let mut previous = String::new();
    let mut steps: Vec<(String, ToolOutput)> = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let task = substitute_previous(&item.task, &previous);
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        let out = run_item(&item.agent, &task, scope, &run_cwd).await?;
        let failed = out.is_error;
        previous = out.content.clone();
        steps.push((item.agent, out));
        if failed {
            return Ok(format_chain(&steps, Some(i)));
        }
    }
    Ok(format_chain(&steps, None))
}

/// Spawn one subagent process and collect its final answer.
async fn run_single(
    agent: &AgentConfig,
    task: &str,
    cwd: &std::path::Path,
) -> Result<ToolOutput, ToolError> {
    // Write the agent's system prompt to a temp file (0o600) and pass
    // it via --append-system-prompt, exactly as PI does. A file avoids
    // both argv length limits and leaking the prompt via `ps`.
    let prompt_file = if agent.system_prompt.trim().is_empty() {
        None
    } else {
        Some(write_prompt_tempfile(&agent.name, &agent.system_prompt).map_err(|e| {
            ToolError::Execution(format!("failed to stage system prompt: {e}"))
        })?)
    };

    let mut command = Command::new(nanopi_invocation());
    command
        .arg("-p")
        .arg("--output")
        .arg("json")
        // Ephemeral: a subagent is a throwaway context window, so it must
        // not pollute `~/.nanopi/sessions/` or steal the cwd's active
        // session pointer.
        .arg("--no-session")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(model) = &agent.model {
        command.arg("--model").arg(model);
    }
    if let Some(tools) = &agent.tools {
        if !tools.is_empty() {
            command.arg("--tools").arg(tools.join(","));
        }
    }
    if let Some(path) = &prompt_file {
        command.arg("--append-system-prompt").arg(path);
    }
    command.arg(format!("Task: {task}"));

    // Keep the child on this future's stack: a drop (parent cancelled)
    // propagates through kill_on_drop → SIGKILL, the same discipline
    // `bash` uses so Esc kills a long subagent immediately.
    let result = spawn_and_collect(command).await;

    if let Some(path) = prompt_file {
        let _ = std::fs::remove_file(path);
    }

    result
}

async fn spawn_and_collect(mut command: Command) -> Result<ToolOutput, ToolError> {
    let mut child = command
        .spawn()
        .map_err(|e| ToolError::Execution(format!("failed to spawn subagent: {e}")))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("subagent stdout not captured".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution("subagent stderr not captured".into()))?;

    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let read_and_wait = async {
        let drain_out = async {
            let mut r = BufReader::new(&mut stdout);
            let _ = r.read_to_end(&mut out_buf).await;
        };
        let drain_err = async {
            let mut r = BufReader::new(&mut stderr);
            let _ = r.read_to_end(&mut err_buf).await;
        };
        let (_, _, status) = tokio::join!(drain_out, drain_err, child.wait());
        status
    };
    let status = read_and_wait
        .await
        .map_err(|e| ToolError::Execution(format!("waiting for subagent: {e}")))?;

    let stdout_str = String::from_utf8_lossy(&out_buf);
    let stderr_str = String::from_utf8_lossy(&err_buf);

    if !status.success() {
        let detail = if !stderr_str.trim().is_empty() {
            stderr_str.trim().to_string()
        } else if !stdout_str.trim().is_empty() {
            stdout_str.trim().to_string()
        } else {
            "(no output)".to_string()
        };
        return Ok(ToolOutput {
            content: format!("Subagent exited with an error: {detail}"),
            is_error: true,
            metadata: None,
            images: Vec::new(),
        });
    }

    let env: JsonEnvelope = serde_json::from_str(stdout_str.trim()).map_err(|e| {
        ToolError::Execution(format!(
            "subagent produced unparseable output: {e}\n---stdout---\n{}\n---stderr---\n{}",
            stdout_str.trim(),
            stderr_str.trim()
        ))
    })?;

    let text = final_assistant_text(&env);
    let content = if text.trim().is_empty() {
        "(subagent produced no output)".to_string()
    } else {
        text
    };
    let is_error = env.finish_reason == "error";

    Ok(ToolOutput {
        content,
        is_error,
        metadata: Some(json!({
            "session_id": env.session_id,
            "model": env.model,
            "finish_reason": env.finish_reason,
            "duration_ms": env.duration_ms,
            "usage": env.usage,
        })),
        images: Vec::new(),
    })
}

/// Write `prompt` to a private temp file and return its path.
fn write_prompt_tempfile(agent_name: &str, prompt: &str) -> std::io::Result<PathBuf> {
    let safe: String = agent_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let mut path = std::env::temp_dir();
    path.push(format!("nanopi-subagent-{safe}-{}.md", crate::util::uuid::v7()));
    std::fs::write(&path, prompt)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scope_defaults_to_user() {
        assert_eq!(parse_scope(&json!({})).unwrap(), AgentScope::User);
        assert_eq!(
            parse_scope(&json!({"agent_scope": "project"})).unwrap(),
            AgentScope::Project
        );
        assert_eq!(
            parse_scope(&json!({"agent_scope": "both"})).unwrap(),
            AgentScope::Both
        );
        assert!(parse_scope(&json!({"agent_scope": "nope"})).is_err());
    }

    #[test]
    fn final_assistant_text_picks_last_nonempty_assistant() {
        let env = JsonEnvelope {
            session_id: "s".into(),
            model: "m".into(),
            finish_reason: "stop".into(),
            duration_ms: 0,
            usage: json!({}),
            messages: vec![
                json!({"role": "user", "content": "hi"}),
                json!({"role": "assistant", "content": "first"}),
                json!({"role": "user", "content": "again"}),
                json!({"role": "assistant", "content": "final answer"}),
            ],
        };
        assert_eq!(final_assistant_text(&env), "final answer");
    }

    #[test]
    fn final_assistant_text_empty_when_no_assistant() {
        let env = JsonEnvelope {
            session_id: "s".into(),
            model: "m".into(),
            finish_reason: "stop".into(),
            duration_ms: 0,
            usage: json!({}),
            messages: vec![json!({"role": "user", "content": "hi"})],
        };
        assert_eq!(final_assistant_text(&env), "");
    }

    fn ok_output(content: &str) -> ToolOutput {
        ToolOutput {
            content: content.into(),
            is_error: false,
            metadata: Some(json!({"finish_reason": "stop", "usage": {"input": 1}})),
            images: Vec::new(),
        }
    }

    #[test]
    fn select_mode_requires_exactly_one() {
        assert_eq!(select_mode(&json!({"task": "x"})).unwrap(), Mode::Single);
        assert_eq!(select_mode(&json!({"tasks": []})).unwrap(), Mode::Parallel);
        assert_eq!(select_mode(&json!({"chain": []})).unwrap(), Mode::Chain);
        // none
        assert!(select_mode(&json!({"agent": "a"})).is_err());
        // more than one
        assert!(select_mode(&json!({"task": "x", "tasks": []})).is_err());
        assert!(select_mode(&json!({"tasks": [], "chain": []})).is_err());
        assert!(select_mode(&json!({"task": "x", "tasks": [], "chain": []})).is_err());
    }

    #[test]
    fn substitute_previous_replaces_all_occurrences() {
        assert_eq!(substitute_previous("a {previous} b", "X"), "a X b");
        assert_eq!(substitute_previous("{previous}{previous}", "X"), "XX");
        // first step: empty previous
        assert_eq!(substitute_previous("do {previous}", ""), "do ");
        // no placeholder: untouched
        assert_eq!(substitute_previous("plain", "X"), "plain");
    }

    #[test]
    fn parse_items_validates_shape() {
        let items = parse_items(
            &json!([{"agent": "a", "task": "t"}, {"agent": "b", "task": "u", "cwd": "sub"}]),
            "tasks",
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].cwd.as_deref(), Some("sub"));
        assert!(parse_items(&json!({}), "tasks").is_err());
        assert!(parse_items(&json!([{"task": "t"}]), "tasks").is_err());
        assert!(parse_items(&json!([{"agent": "a"}]), "tasks").is_err());
    }

    #[tokio::test]
    async fn parallel_rejects_over_cap() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-cap-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = SubagentTool;
        let ctx = ToolContext::new(dir.clone());
        let tasks: Vec<Value> = (0..MAX_TASKS + 1)
            .map(|_| json!({"agent": "x", "task": "y"}))
            .collect();
        let err = tool
            .execute(json!({"tasks": tasks}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_parallel_aggregates_and_flags_errors() {
        let results = vec![
            ("scout".to_string(), ok_output("found it")),
            ("lint".to_string(), soft_error("Unknown agent \"lint\".".to_string())),
        ];
        let out = format_parallel(&results);
        assert!(out.is_error, "any failing task marks the batch errored");
        assert!(out.content.contains("Parallel: 1/2 succeeded"));
        assert!(out.content.contains("### [scout] ok"));
        assert!(out.content.contains("### [lint] error"));
        assert!(out.content.contains("found it"));
        let meta = out.metadata.unwrap();
        assert_eq!(meta["mode"], "parallel");
        assert_eq!(meta["tasks"].as_array().unwrap().len(), 2);

        // all ok -> not an error
        let out = format_parallel(&[("a".into(), ok_output("x"))]);
        assert!(!out.is_error);
    }

    #[test]
    fn format_chain_reports_failed_step_and_success() {
        // Failure at step 2 (index 1).
        let steps = vec![
            ("a".to_string(), ok_output("one")),
            ("b".to_string(), soft_error("boom".to_string())),
        ];
        let out = format_chain(&steps, Some(1));
        assert!(out.is_error);
        assert!(out.content.contains("Chain stopped at step 2 (b): boom"));
        assert!(out.content.contains("### Step 1 [a] ok"));

        // Full success: final content is the last step's output.
        let steps = vec![
            ("a".to_string(), ok_output("one")),
            ("b".to_string(), ok_output("two")),
        ];
        let out = format_chain(&steps, None);
        assert!(!out.is_error);
        assert!(out.content.starts_with("two"));
        let meta = out.metadata.unwrap();
        assert_eq!(meta["steps"][1]["step"], 2);
    }

    #[tokio::test]
    async fn parallel_item_unknown_agent_is_soft_error() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-par-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run_parallel(
            vec![SubagentItem {
                agent: "__definitely_not_an_agent__".into(),
                task: "x".into(),
                cwd: None,
            }],
            AgentScope::Project,
            &dir,
        )
        .await
        .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("Unknown agent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn chain_stops_at_unknown_agent() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-chn-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run_chain(
            vec![SubagentItem {
                agent: "__definitely_not_an_agent__".into(),
                task: "x".into(),
                cwd: None,
            }],
            AgentScope::Project,
            &dir,
        )
        .await
        .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("Chain stopped at step 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_agent_is_a_soft_error_listing_availability() {
        // A cwd with no project agents; scope=user may still pick up the
        // real user dir, so just assert the shape when the agent is
        // absent by using a name that cannot exist.
        let dir = std::env::temp_dir().join(format!("nanopi-sa-test-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = SubagentTool;
        let ctx = ToolContext::new(dir.clone());
        let out = tool
            .execute(
                json!({"agent": "__definitely_not_an_agent__", "task": "x", "agent_scope": "project"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("Unknown agent"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

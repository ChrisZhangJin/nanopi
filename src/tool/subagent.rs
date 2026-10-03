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
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::agent::agents::{discover_agents, AgentConfig, AgentScope, AgentSource};
use crate::agent::context::ToolSpec;
use crate::mode::print::JsonEnvelope;
use crate::subagent_registry::ChildGuard;
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
        (false, false, false) => Err("provide exactly one of `task`, `tasks`, or `chain`".into()),
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
    let Some(agent) = discovery
        .agents
        .iter()
        .find(|a| a.name == agent_name)
        .cloned()
    else {
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
        Some(
            write_prompt_tempfile(&agent.name, &agent.system_prompt)
                .map_err(|e| ToolError::Execution(format!("failed to stage system prompt: {e}")))?,
        )
    };

    let mut command = Command::new(nanopi_invocation());
    command
        .arg("-p")
        .arg("--output")
        .arg("json")
        .arg("--no-session")
        .current_dir(cwd);

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
    let result = Ok(spawn_and_collect(command, Duration::from_secs(1800)).await);

    if let Some(path) = prompt_file {
        let _ = std::fs::remove_file(path);
    }

    result
}

/// Max bytes of child stdout retained (T-01-11). A larger envelope is
/// treated as unparseable rather than buffered without bound.
const STDOUT_CAP: usize = 8 * 1024 * 1024;
/// Max bytes of child stderr retained — the *tail* is kept.
const STDERR_TAIL: usize = 64 * 1024;

/// The executable (plus leading args) used to launch a child. Defaults
/// to the running binary; tests substitute `sh -c ...`.
#[derive(Debug, Clone)]
pub struct ChildProgram {
    pub program: PathBuf,
    pub leading_args: Vec<String>,
}

impl Default for ChildProgram {
    fn default() -> Self {
        Self {
            program: nanopi_invocation(),
            leading_args: Vec::new(),
        }
    }
}

impl ChildProgram {
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.leading_args);
        c
    }
}

/// Human name for a common signal number.
fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => "signal",
    }
}

/// In-band failure result: never an `Err`, so one child's fault cannot
/// abort the parent turn or its siblings.
fn failed_output(reason: &str, stderr_tail: &str) -> ToolOutput {
    let tail = stderr_tail.trim();
    let content = if tail.is_empty() {
        format!("Subagent failed: {reason}")
    } else {
        format!("Subagent failed: {reason}\n--- stderr (tail) ---\n{tail}")
    };
    ToolOutput {
        content,
        is_error: true,
        metadata: Some(json!({
            "status": "failed",
            "error": reason,
            "stderr_tail": tail,
        })),
        images: Vec::new(),
    }
}

/// Read `r` to EOF keeping at most `cap` bytes (head). Returns whether
/// anything was dropped.
async fn drain_head<R: tokio::io::AsyncRead + Unpin>(r: R, buf: &mut Vec<u8>, cap: usize) -> bool {
    let mut r = BufReader::new(r);
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return truncated,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                if n > room {
                    truncated = true;
                }
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
}

/// Read `r` to EOF keeping only the last `cap` bytes.
async fn drain_tail<R: tokio::io::AsyncRead + Unpin>(r: R, buf: &mut Vec<u8>, cap: usize) {
    let mut r = BufReader::new(r);
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > cap {
                    let excess = buf.len() - cap;
                    buf.drain(..excess);
                }
            }
        }
    }
}

/// Spawn a child and collect its `-p --output json` envelope. Every
/// fault (spawn error, non-zero exit, signal, timeout, garbage stdout)
/// maps to an in-band `failed_output`; this never returns `Err`.
pub async fn spawn_and_collect(command: Command, timeout: Duration) -> ToolOutput {
    spawn_and_collect_with(command, timeout, |_| {}).await
}

/// [`spawn_and_collect`] with a hook invoked with the child pid right
/// after spawn (used to record it in the registry).
async fn spawn_and_collect_with(
    mut command: Command,
    timeout: Duration,
    on_pid: impl FnOnce(u32),
) -> ToolOutput {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // RT-03: own process group, so a terminal SIGINT aimed at the parent's
    // foreground group does not reach the child, and the guard can kill
    // the child together with anything it backgrounded.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return failed_output(&format!("failed to spawn subagent: {e}"), ""),
    };
    let pid = child.id();
    // Kills the whole group if this future is dropped (cancel) or times out.
    let guard = ChildGuard::new(pid);
    if let Some(pid) = pid {
        on_pid(pid);
    }

    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return failed_output("subagent stdio not captured", "");
    };

    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let mut out_truncated = false;
    let waited = tokio::time::timeout(timeout, async {
        let (t, _, status) = tokio::join!(
            drain_head(stdout, &mut out_buf, STDOUT_CAP),
            drain_tail(stderr, &mut err_buf, STDERR_TAIL),
            child.wait()
        );
        out_truncated = t;
        status
    })
    .await;

    let stderr_tail = String::from_utf8_lossy(&err_buf).to_string();
    let status = match waited {
        Err(_) => {
            drop(guard); // SIGKILL the group
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return failed_output(
                &format!("timed out after {}s", timeout.as_secs_f64()),
                &stderr_tail,
            );
        }
        Ok(Err(e)) => return failed_output(&format!("waiting for subagent: {e}"), &stderr_tail),
        Ok(Ok(s)) => s,
    };
    // Reaped normally; still sweep the group for stray grandchildren.
    drop(guard);

    if !status.success() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                return failed_output(
                    &format!("killed by signal {sig} ({})", signal_name(sig)),
                    &stderr_tail,
                );
            }
        }
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".into());
        return failed_output(&format!("exit code {code}"), &stderr_tail);
    }
    if out_truncated {
        return failed_output("unparseable output (stdout exceeded 8 MiB)", &stderr_tail);
    }

    let stdout_str = String::from_utf8_lossy(&out_buf);
    let env: JsonEnvelope = match serde_json::from_str(stdout_str.trim()) {
        Ok(e) => e,
        Err(e) => return failed_output(&format!("unparseable output: {e}"), &stderr_tail),
    };
    envelope_output(env, &stderr_tail)
}

/// Map a parsed child envelope to the tool result.
fn envelope_output(env: JsonEnvelope, stderr_tail: &str) -> ToolOutput {
    let status = match env.status.as_deref() {
        Some(s) => s.to_string(),
        None if env.finish_reason == "error" => "failed".into(),
        None => "completed".into(),
    };
    if status == "failed" {
        let reason = env
            .error
            .clone()
            .unwrap_or_else(|| "child reported failure".into());
        let mut out = failed_output(&reason, stderr_tail);
        if let Some(m) = out.metadata.as_mut() {
            m["agent_id"] = json!(env.agent_id);
            m["report_path"] = json!(env.report_path);
        }
        return out;
    }
    let text = final_assistant_text(&env);
    let mut content = if text.trim().is_empty() {
        "(subagent produced no output)".to_string()
    } else {
        text
    };
    if status == "limit_reached" {
        let limit = env.limit.clone().unwrap_or_else(|| "limit".into());
        content = format!("[subagent stopped: {limit} reached]\n\n{content}");
    }
    ToolOutput {
        content,
        is_error: false,
        metadata: Some(json!({
            "status": status,
            "limit": env.limit,
            "agent_id": env.agent_id,
            "report_path": env.report_path,
            "session_id": env.session_id,
            "model": env.model,
            "finish_reason": env.finish_reason,
            "duration_ms": env.duration_ms,
            "usage": env.usage,
        })),
        images: Vec::new(),
    }
}

/// Write `prompt` to a private temp file and return its path.
fn write_prompt_tempfile(agent_name: &str, prompt: &str) -> std::io::Result<PathBuf> {
    let safe: String = agent_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut path = std::env::temp_dir();
    path.push(format!(
        "nanopi-subagent-{safe}-{}.md",
        crate::util::uuid::v7()
    ));
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
            ..Default::default()
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
            ..Default::default()
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
        let ctx = ToolContext { cwd: dir.clone() };
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
            (
                "lint".to_string(),
                soft_error("Unknown agent \"lint\".".to_string()),
            ),
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
        let ctx = ToolContext { cwd: dir.clone() };
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

    // ── Task 1: failure isolation (unix) ──

    #[cfg(unix)]
    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        c
    }

    #[cfg(unix)]
    fn pid_dead(pid: i32) -> bool {
        // Zombies count as dead (container PID 1 may not reap).
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(s) => s
                .rsplit(')')
                .next()
                .map(|r| r.trim_start().starts_with('Z'))
                .unwrap_or(false),
        }
    }

    #[cfg(unix)]
    fn status_of(out: &ToolOutput) -> String {
        out.metadata.as_ref().unwrap()["status"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_nonzero_exit_is_in_band() {
        let out = spawn_and_collect(sh("echo boom >&2; exit 3"), Duration::from_secs(10)).await;
        assert!(out.is_error);
        assert_eq!(status_of(&out), "failed");
        assert!(out.content.contains("exit code 3"), "{}", out.content);
        assert!(out.content.contains("boom"), "{}", out.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_signal_names_sigkill() {
        let out = spawn_and_collect(sh("kill -9 $$"), Duration::from_secs(10)).await;
        assert!(out.is_error);
        assert_eq!(status_of(&out), "failed");
        assert!(
            out.content.contains("SIGKILL") && out.content.contains("signal 9"),
            "{}",
            out.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_garbage_stdout_is_unparseable() {
        let out = spawn_and_collect(sh("echo not json"), Duration::from_secs(10)).await;
        assert!(out.is_error);
        assert_eq!(status_of(&out), "failed");
        assert!(
            out.content.contains("unparseable output"),
            "{}",
            out.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_timeout_kills_group() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-to-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pid");
        let script = format!("sleep 30 & echo $! > {}; wait", f.display());
        let out = spawn_and_collect(sh(&script), Duration::from_secs(1)).await;
        assert!(out.is_error);
        assert_eq!(status_of(&out), "failed");
        assert!(out.content.contains("timed out"), "{}", out.content);
        let pid: i32 = std::fs::read_to_string(&f).unwrap().trim().parse().unwrap();
        let mut dead = false;
        for _ in 0..50 {
            if pid_dead(pid) {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(dead, "grandchild {pid} survived timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_spawn_error_is_in_band() {
        let out = spawn_and_collect(
            Command::new("/definitely/not/a/program-xyz"),
            Duration::from_secs(5),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(status_of(&out), "failed");
        assert!(out.content.contains("failed to spawn"), "{}", out.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_stderr_tail_is_capped() {
        // ~200 KiB of stderr, then a distinctive last line.
        let script = "i=0; while [ $i -lt 2000 ]; do printf '%0100d\\n' 0 >&2; i=$((i+1)); done; echo LAST-LINE >&2; exit 1";
        let out = spawn_and_collect(sh(script), Duration::from_secs(20)).await;
        let tail = out.metadata.as_ref().unwrap()["stderr_tail"]
            .as_str()
            .unwrap();
        assert!(tail.len() <= STDERR_TAIL, "tail {} bytes", tail.len());
        assert!(tail.ends_with("LAST-LINE"), "keeps the tail");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_envelope_statuses_map() {
        let ok = r#"{"session_id":"s","model":"m","finish_reason":"stop","duration_ms":1,"usage":{},"messages":[{"role":"assistant","content":"hi"}],"status":"limit_reached","limit":"max_turns"}"#;
        let out = spawn_and_collect(sh(&format!("echo '{ok}'")), Duration::from_secs(5)).await;
        assert!(!out.is_error);
        assert_eq!(status_of(&out), "limit_reached");
        assert!(out.content.contains("max_turns"));
        let bad = r#"{"session_id":"s","model":"m","finish_reason":"error","duration_ms":1,"usage":{},"messages":[],"status":"failed","error":"provider down"}"#;
        let out = spawn_and_collect(sh(&format!("echo '{bad}'")), Duration::from_secs(5)).await;
        assert!(out.is_error);
        assert_eq!(status_of(&out), "failed");
        assert!(out.content.contains("provider down"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_on_cancel_kills_backgrounded_grandchild() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-kc-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pid");
        let script = format!("sleep 300 & echo $! > {}; wait", f.display());
        let handle = tokio::spawn(spawn_and_collect(sh(&script), Duration::from_secs(600)));
        let mut pid = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&f) {
                if let Ok(p) = s.trim().parse::<i32>() {
                    pid = Some(p);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pid = pid.expect("pidfile appeared");
        assert!(!pid_dead(pid));
        handle.abort();
        let _ = handle.await;
        let mut dead = false;
        for _ in 0..50 {
            if pid_dead(pid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(dead, "grandchild {pid} survived cancel");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

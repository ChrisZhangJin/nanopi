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
//! Modes: single, parallel, chain. Every dispatch is supervised:
//! registered in [`SubagentRegistry`] (max_live cap, max_concurrency
//! queue), given an agent dir `.nanopi/agents/<run>/<id>/` holding
//! `brief.md`, `transcript.jsonl` and the child's `report.md`, launched in
//! its own process group with the parent's provider settings
//! ([`ChildLaunchSpec`]; key via `OPENAI_API_KEY` env only), and killed as
//! a group on cancel or timeout. Child faults are always in-band
//! (`status: failed`), never `Err`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::agent::agents::{discover_agents, AgentConfig, AgentScope, AgentSource};
use crate::agent::brief::{render_brief, BriefSpec};
use crate::agent::context::ToolSpec;
use crate::mode::print::JsonEnvelope;
use crate::subagent_registry::{self, AgentState, ChildGuard, SubagentRegistry};
use crate::tool::{ExecutionMode, Tool, ToolContext, ToolError, ToolOutput};

/// Provider/limit settings a child inherits from its parent (D-01).
/// The API key travels via `OPENAI_API_KEY` only — never argv (T-01-12).
#[derive(Debug, Clone)]
pub struct ChildLaunchSpec {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_kind: Option<String>,
    pub api_key: Option<String>,
    /// Parent's resolved project trust: `--approve` / `--distrust`, so the
    /// child never prompts.
    pub trust: Option<bool>,
    pub max_turns: u32,
    pub token_budget: u64,
    pub timeout: Duration,
}

impl Default for ChildLaunchSpec {
    fn default() -> Self {
        let c = crate::config::SubagentConfig::default();
        Self {
            model: None,
            base_url: None,
            api_kind: None,
            api_key: None,
            trust: None,
            max_turns: c.max_turns,
            token_budget: c.token_budget,
            timeout: Duration::from_secs(c.timeout_secs),
        }
    }
}

static LAUNCH_SPEC: OnceLock<ChildLaunchSpec> = OnceLock::new();

/// Install the parent's resolved launch settings (first call wins).
pub fn set_launch_spec(spec: ChildLaunchSpec) {
    let _ = LAUNCH_SPEC.set(spec);
}

/// The `subagent` tool. Fields are overrides; unset ones resolve to the
/// process-wide registry / launch spec at execute time.
#[derive(Default)]
pub struct SubagentTool {
    registry: Option<Arc<SubagentRegistry>>,
    spec: Option<ChildLaunchSpec>,
    program: Option<ChildProgram>,
    /// Fallback registry when no global one was installed.
    fallback: OnceLock<Arc<SubagentRegistry>>,
}

impl SubagentTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fully specified tool (tests, embedding).
    pub fn with_parts(
        registry: Arc<SubagentRegistry>,
        spec: ChildLaunchSpec,
        program: ChildProgram,
    ) -> Self {
        Self {
            registry: Some(registry),
            spec: Some(spec),
            program: Some(program),
            fallback: OnceLock::new(),
        }
    }

    pub fn launcher(&self) -> Launcher {
        let registry = self
            .registry
            .clone()
            .or_else(subagent_registry::global)
            .unwrap_or_else(|| {
                self.fallback
                    .get_or_init(
                        || SubagentRegistry::new(&crate::config::SubagentConfig::default()),
                    )
                    .clone()
            });
        Launcher {
            registry,
            spec: self
                .spec
                .clone()
                .or_else(|| LAUNCH_SPEC.get().cloned())
                .unwrap_or_default(),
            program: self.program.clone().unwrap_or_default(),
        }
    }
}

/// Everything one dispatch needs, resolved once per tool call.
#[derive(Clone)]
pub struct Launcher {
    pub registry: Arc<SubagentRegistry>,
    pub spec: ChildLaunchSpec,
    pub program: ChildProgram,
}

/// Hard cap on how many subagents a single `parallel` call may fan out
/// to, mirroring PI's `MAX_PARALLEL_TASKS = 8`. Keeps a runaway model
/// from spawning a fork bomb of `nanopi` processes.
const MAX_TASKS: usize = 8;

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
        let l = self.launcher();

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
                Ok(run_item(&l, agent_name, task, scope, &run_cwd).await)
            }
            Mode::Parallel => {
                let items = parse_items(&args["tasks"], "tasks").map_err(ToolError::InvalidArgs)?;
                if items.len() > MAX_TASKS {
                    return Err(ToolError::InvalidArgs(format!(
                        "too many parallel tasks ({}); max is {MAX_TASKS}",
                        items.len()
                    )));
                }
                Ok(run_parallel(&l, items, scope, &ctx.cwd).await)
            }
            Mode::Chain => {
                let items = parse_items(&args["chain"], "chain").map_err(ToolError::InvalidArgs)?;
                Ok(run_chain(&l, items, scope, &ctx.cwd).await)
            }
        }
    }
}

/// Resolve an agent by name under `scope` at `run_cwd`, applying the
/// project-trust gate, then run it. Every failure is in-band.
async fn run_item(
    l: &Launcher,
    agent_name: &str,
    task: &str,
    scope: AgentScope,
    run_cwd: &Path,
) -> ToolOutput {
    match resolve_agent(agent_name, scope, run_cwd) {
        Ok(agent) => run_single(l, &agent, task, run_cwd).await,
        Err(soft) => soft,
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

/// Run a `parallel` batch. Concurrency is bounded by the registry's
/// `max_concurrency` semaphore (extra items queue); each future owns its
/// child, so dropping the batch kills every in-flight process group.
async fn run_parallel(
    l: &Launcher,
    items: Vec<SubagentItem>,
    scope: AgentScope,
    base_cwd: &Path,
) -> ToolOutput {
    let futs = items.into_iter().map(|item| {
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        async move {
            let out = run_item(l, &item.agent, &item.task, scope, &run_cwd).await;
            (item.agent, out)
        }
    });
    let results: Vec<(String, ToolOutput)> = join_all(futs).await;
    format_parallel(&results)
}

/// Run a `chain`: steps in order, each seeing the prior step's output
/// via `{previous}`. Stops at the first failed step and reports where.
async fn run_chain(
    l: &Launcher,
    items: Vec<SubagentItem>,
    scope: AgentScope,
    base_cwd: &Path,
) -> ToolOutput {
    let mut previous = String::new();
    let mut steps: Vec<(String, ToolOutput)> = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let task = substitute_previous(&item.task, &previous);
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        let out = run_item(l, &item.agent, &task, scope, &run_cwd).await;
        let failed = out.is_error;
        previous = out.content.clone();
        steps.push((item.agent, out));
        if failed {
            return format_chain(&steps, Some(i));
        }
    }
    format_chain(&steps, None)
}

/// Child argv (pure). Never contains the API key (T-01-12) and never
/// `--no-session`: the transcript lives in the agent dir (D-03).
pub fn build_child_args(
    spec: &ChildLaunchSpec,
    dir: &Path,
    tools: &[String],
    agent_model: Option<&str>,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-p".into(),
        "--output".into(),
        "json".into(),
        "--brief".into(),
        dir.join("brief.md").to_string_lossy().into_owned(),
        "--session-file".into(),
        dir.join("transcript.jsonl").to_string_lossy().into_owned(),
        "--max-turns".into(),
        spec.max_turns.to_string(),
        "--token-budget".into(),
        spec.token_budget.to_string(),
    ];
    if let Some(m) = agent_model.or(spec.model.as_deref()) {
        a.extend(["--model".into(), m.to_string()]);
    }
    if let Some(u) = &spec.base_url {
        a.extend(["--base-url".into(), u.clone()]);
    }
    if let Some(k) = &spec.api_kind {
        a.extend(["--api-kind".into(), k.clone()]);
    }
    if !tools.is_empty() {
        a.extend(["--tools".into(), tools.join(",")]);
    }
    match spec.trust {
        Some(true) => a.push("--approve".into()),
        Some(false) | None => a.push("--distrust".into()),
    }
    a
}

/// Child environment additions (pure).
pub fn build_child_env(spec: &ChildLaunchSpec, agent_id: &str) -> Vec<(String, String)> {
    let mut e = vec![
        ("NANOPI_AGENT_ID".to_string(), agent_id.to_string()),
        (
            "NANOPI_PARENT_PID".to_string(),
            std::process::id().to_string(),
        ),
    ];
    if let Some(k) = &spec.api_key {
        e.push(("OPENAI_API_KEY".to_string(), k.clone()));
    }
    e
}

/// Max bytes of `report.md` returned to the parent model.
const REPORT_CAP: usize = 64 * 1024;

/// Marks a registry entry `Stopped` if the dispatch future is dropped
/// before it records a terminal state.
struct StateGuard<'a> {
    reg: &'a SubagentRegistry,
    id: String,
    done: bool,
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.reg.set_state(&self.id, AgentState::Stopped);
        }
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// One tracked, capped, briefed child dispatch. Always in-band.
pub async fn run_single(l: &Launcher, agent: &AgentConfig, task: &str, cwd: &Path) -> ToolOutput {
    let reg = &*l.registry;
    // D-03: agent dir exists before spawn; max_live enforced here.
    let (id, dir) = match reg.reserve(&cwd.join(".nanopi").join("agents")) {
        Ok(v) => v,
        Err(e) => return soft_error(format!("Cannot start subagent {:?}: {e}", agent.name)),
    };
    let mut sg = StateGuard {
        reg,
        id: id.clone(),
        done: false,
    };
    let tools = agent.tools.clone().unwrap_or_default();
    let role = if agent.system_prompt.trim().is_empty() {
        (!agent.description.trim().is_empty()).then(|| agent.description.clone())
    } else {
        Some(agent.system_prompt.clone())
    };
    let brief = render_brief(&BriefSpec {
        task: task.to_string(),
        role,
        tools: tools.clone(),
        model: agent.model.clone().or_else(|| l.spec.model.clone()),
    });
    if let Err(e) = write_private(&dir.join("brief.md"), &brief) {
        reg.set_state(&id, AgentState::Failed);
        sg.done = true;
        return failed_output(&format!("cannot write brief: {e}"), "");
    }

    // Beyond max_concurrency: queue here (state stays Queued).
    let _permit = reg.acquire_run().await;
    reg.set_state(&id, AgentState::Running);

    let mut command = l.program.command();
    command
        .args(build_child_args(
            &l.spec,
            &dir,
            &tools,
            agent.model.as_deref(),
        ))
        .envs(build_child_env(&l.spec, &id))
        .current_dir(cwd);
    let mut out =
        spawn_and_collect_with(command, l.spec.timeout, |pid| reg.set_pid(&id, pid)).await;

    let status = out
        .metadata
        .as_ref()
        .and_then(|m| m.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("failed")
        .to_string();

    let report = dir.join("report.md");
    if let Ok(text) = std::fs::read_to_string(&report) {
        let mut text = text;
        if text.len() > REPORT_CAP {
            let mut cut = REPORT_CAP;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n…(report truncated)");
        }
        out.content = if out.is_error {
            format!("{}\n\n--- report.md ---\n{text}", out.content)
        } else if status == "limit_reached" {
            let limit = out
                .metadata
                .as_ref()
                .and_then(|m| m["limit"].as_str())
                .unwrap_or("limit")
                .to_string();
            format!("[subagent stopped: {limit} reached]\n\n{text}")
        } else {
            text
        };
        if let Some(m) = out.metadata.as_mut() {
            m["report_path"] = json!(report.to_string_lossy());
        }
    }
    if let Some(m) = out.metadata.as_mut() {
        m["agent_id"] = json!(id);
        m["agent_dir"] = json!(dir.to_string_lossy());
    }

    let state = match status.as_str() {
        "completed" => AgentState::Completed,
        "limit_reached" => AgentState::LimitReached,
        _ => AgentState::Failed,
    };
    reg.set_state(&id, state);
    sg.done = true;
    out
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
        // CR-02: a JSON-mode child that fails prints a `status: failed`
        // envelope and exits 1. Prefer that envelope (it carries the real
        // error, agent_id and report_path) over the bare exit code.
        if !out_truncated {
            if let Ok(env) =
                serde_json::from_str::<JsonEnvelope>(String::from_utf8_lossy(&out_buf).trim())
            {
                if env.status.as_deref() == Some("failed") {
                    return envelope_output(env, &stderr_tail);
                }
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
        let tool = SubagentTool::new();
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
            &SubagentTool::new().launcher(),
            vec![SubagentItem {
                agent: "__definitely_not_an_agent__".into(),
                task: "x".into(),
                cwd: None,
            }],
            AgentScope::Project,
            &dir,
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("Unknown agent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn chain_stops_at_unknown_agent() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-chn-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run_chain(
            &SubagentTool::new().launcher(),
            vec![SubagentItem {
                agent: "__definitely_not_an_agent__".into(),
                task: "x".into(),
                cwd: None,
            }],
            AgentScope::Project,
            &dir,
        )
        .await;
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
        let tool = SubagentTool::new();
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

    /// CR-02 regression: a real JSON-mode child prints its failed
    /// envelope and exits 1; the parent must surface the envelope's
    /// error text rather than a bare "exit code 1".
    #[tokio::test]
    async fn failed_envelope_with_exit_1_surfaces_error() {
        let bad = r#"{"session_id":"s","model":"m","finish_reason":"error","duration_ms":1,"usage":{},"messages":[],"status":"failed","error":"provider down: 503","agent_id":"a1","report_path":"/tmp/x/report.md"}"#;
        let out = spawn_and_collect(sh(&format!("echo '{bad}'; exit 1")), Duration::from_secs(5)).await;
        assert!(out.is_error);
        assert_eq!(status_of(&out), "failed");
        assert!(out.content.contains("provider down: 503"), "{}", out.content);
        let m = out.metadata.as_ref().unwrap();
        assert_eq!(m["agent_id"], "a1");
        assert_eq!(m["report_path"], "/tmp/x/report.md");
        // Non-envelope stdout with exit 1 still maps to the exit code.
        let out = spawn_and_collect(sh("echo garbage; exit 1"), Duration::from_secs(5)).await;
        assert!(out.is_error);
        assert!(out.content.contains("exit code 1"), "{}", out.content);
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

    // ── Task 2: registry, agent dir, brief, argv/env ──

    fn spec_fixture() -> ChildLaunchSpec {
        ChildLaunchSpec {
            model: Some("M".into()),
            base_url: Some("U".into()),
            api_kind: Some("K".into()),
            api_key: Some("sk-SECRET-123".into()),
            trust: Some(false),
            ..ChildLaunchSpec::default()
        }
    }

    #[test]
    fn build_child_args_contract() {
        let dir = Path::new("/tmp/agents/run/a1");
        let a = build_child_args(&spec_fixture(), dir, &["a".into(), "b".into()], None);
        let j = a.join(" ");
        assert!(j.starts_with("-p --output json --brief /tmp/agents/run/a1/brief.md --session-file /tmp/agents/run/a1/transcript.jsonl --max-turns 50 --token-budget 300000"), "{j}");
        for want in [
            "--model M",
            "--base-url U",
            "--api-kind K",
            "--tools a,b",
            "--distrust",
        ] {
            assert!(j.contains(want), "missing {want}: {j}");
        }
        assert!(!a.iter().any(|x| x == "--no-session"));
        // agent model overrides the inherited one
        let a = build_child_args(&spec_fixture(), dir, &[], Some("X"));
        assert!(a.join(" ").contains("--model X"));
        assert!(!a.iter().any(|x| x == "--tools"));
    }

    #[test]
    fn no_key_in_argv() {
        let a = build_child_args(&spec_fixture(), Path::new("/d"), &[], None);
        assert!(
            !a.iter().any(|x| x.contains("SECRET")),
            "api key must not contain argv: {a:?}"
        );
    }

    #[test]
    fn build_child_env_sets_ids_and_key() {
        let e = build_child_env(&spec_fixture(), "a7");
        let get = |k: &str| e.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("NANOPI_AGENT_ID").as_deref(), Some("a7"));
        assert_eq!(
            get("NANOPI_PARENT_PID"),
            Some(std::process::id().to_string())
        );
        assert_eq!(get("OPENAI_API_KEY").as_deref(), Some("sk-SECRET-123"));
    }

    #[cfg(unix)]
    const OK_ENV: &str = r#"{"session_id":"s","model":"m","finish_reason":"stop","duration_ms":1,"usage":{},"messages":[{"role":"assistant","content":"DONE"}],"status":"completed"}"#;

    #[cfg(unix)]
    fn agent_fixture() -> AgentConfig {
        AgentConfig {
            name: "scout".into(),
            description: "d".into(),
            tools: Some(vec!["read".into(), "grep".into()]),
            model: None,
            system_prompt: "You are a scout.".into(),
            source: AgentSource::User,
            file_path: PathBuf::from("/nonexistent/scout.md"),
        }
    }

    /// `sh -c script` as the child; argv after the script becomes $0.., so
    /// `$4` is the brief path.
    #[cfg(unix)]
    fn launcher(script: &str, max_live: usize, max_conc: usize) -> Launcher {
        let cfg = crate::config::SubagentConfig {
            max_live,
            max_concurrency: max_conc,
            ..crate::config::SubagentConfig::default()
        };
        SubagentTool::with_parts(
            SubagentRegistry::new(&cfg),
            spec_fixture(),
            ChildProgram {
                program: "sh".into(),
                leading_args: vec!["-c".into(), script.into()],
            },
        )
        .launcher()
    }

    #[cfg(unix)]
    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nanopi-sa-{tag}-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn brief_and_dir_exist_before_spawn() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = tmp("brief");
        // Child fails (exit 7) unless brief.md is already there.
        let script = format!(
            "[ -f \"$4\" ] || exit 7; [ \"$NANOPI_AGENT_ID\" = a1 ] || exit 8; echo '{OK_ENV}'"
        );
        let l = launcher(&script, 8, 4);
        let out = run_single(&l, &agent_fixture(), "find the thing", &cwd).await;
        assert!(!out.is_error, "{}", out.content);
        let dir = cwd
            .join(".nanopi/agents")
            .join(l.registry.run_id())
            .join("a1");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let brief = dir.join("brief.md");
        assert_eq!(
            std::fs::metadata(&brief).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let b = std::fs::read_to_string(&brief).unwrap();
        for want in [
            "find the thing",
            "You are a scout.",
            "- read",
            "- grep",
            "M",
        ] {
            assert!(b.contains(want), "brief missing {want}: {b}");
        }
        assert_eq!(l.registry.snapshot()[0].state, AgentState::Completed);
        assert!(l.registry.snapshot()[0].pid.is_some());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn max_live_returns_in_band_limit_error() {
        let cwd = tmp("live");
        let l = launcher(&format!("echo '{OK_ENV}'"), 1, 4);
        l.registry.reserve(&cwd.join(".nanopi/agents")).unwrap(); // one live entry
        let out = run_single(&l, &agent_fixture(), "t", &cwd).await;
        assert!(out.is_error);
        assert!(
            out.content.contains("subagent limit reached"),
            "{}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn max_concurrency_one_runs_sequentially() {
        let cwd = tmp("conc");
        let l = launcher(&format!("sleep 0.4; echo '{OK_ENV}'"), 8, 1);
        let a = agent_fixture();
        let t0 = std::time::Instant::now();
        let (x, y) = tokio::join!(run_single(&l, &a, "1", &cwd), run_single(&l, &a, "2", &cwd));
        assert!(!x.is_error && !y.is_error);
        assert!(
            t0.elapsed() >= Duration::from_millis(800),
            "ran concurrently: {:?}",
            t0.elapsed()
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_state_tracks_outcome_and_report_is_used() {
        let cwd = tmp("state");
        let lim = r#"{"session_id":"s","model":"m","finish_reason":"stop","duration_ms":1,"usage":{},"messages":[],"status":"limit_reached","limit":"max_turns"}"#;
        let script = format!(
            "case \"$NANOPI_AGENT_ID\" in a1) exit 3;; a2) echo REPORT-BODY > \"$(dirname \"$4\")/report.md\"; echo '{OK_ENV}';; *) echo '{lim}';; esac"
        );
        let l = launcher(&script, 8, 4);
        let a = agent_fixture();
        let f = run_single(&l, &a, "t", &cwd).await;
        assert!(f.is_error);
        let ok = run_single(&l, &a, "t", &cwd).await;
        assert!(!ok.is_error);
        assert!(ok.content.contains("REPORT-BODY"), "{}", ok.content);
        let rp = ok.metadata.as_ref().unwrap()["report_path"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(rp.ends_with("/a2/report.md"), "{rp}");
        let lr = run_single(&l, &a, "t", &cwd).await;
        assert!(!lr.is_error);
        let st: Vec<AgentState> = l.registry.snapshot().iter().map(|e| e.state).collect();
        assert_eq!(
            st,
            vec![
                AgentState::Failed,
                AgentState::Completed,
                AgentState::LimitReached
            ]
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

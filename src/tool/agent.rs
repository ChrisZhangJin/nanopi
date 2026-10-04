//! `agent` — delegate a task to a specialized agent in an isolated
//! context window.
//!
//! Mirrors PI's agent extension
//! (`pi/examples/extensions/agent/index.ts`), but in-tree as a
//! built-in rather than a plugin: the WASM plugin path is bounded by
//! `plugin_tools::PLUGIN_TOOL_DEADLINE` (30s), which a real agent
//! LLM run blows straight past.
//!
//! The mechanism is exactly PI's: spawn `nanopi` as a child process
//! with a delegated system prompt and (optional) restricted toolset,
//! then read back its `-p --output json` envelope. A separate process
//! is a separate context window — that is the whole point.
//!
//! Modes: single, parallel, chain. Every dispatch is supervised:
//! registered in [`AgentRegistry`] (max_live cap, max_concurrency
//! queue), given an agent dir `.nanopi/agents/<run>/<id>/` holding
//! `brief.md`, `transcript.jsonl` and the child's `report.md`, launched in
//! its own process group with the parent's provider settings
//! ([`ChildLaunchSpec`]; key via `OPENAI_API_KEY` env only), and killed as
//! a group on cancel or timeout. Child faults are always in-band
//! (`status: failed`), never `Err`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::agent::agents::{discover_agents, AgentConfig, AgentScope, AgentSource, GENERAL_PURPOSE_NAME};
use crate::agent::brief::{render_brief_with_meta, render_report, BriefMeta, BriefSpec, ReportMeta};
use crate::agent::context::ToolSpec;
use crate::archive;
use crate::mode::print::JsonEnvelope;
use crate::agent_registry::{self, AgentState, ChildGuard, AgentRegistry};
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
    /// The active provider's vendor id (`vendor::Vendor::id()`), used by
    /// [`validate_model`] to reject inline model overrides from a
    /// different vendor (D-05). `None`/`"fallback"` means "accept any
    /// registry-known model" (custom endpoint, catalogue doesn't apply).
    pub vendor: Option<String>,
}

impl Default for ChildLaunchSpec {
    fn default() -> Self {
        let c = crate::config::AgentConfig::default();
        Self {
            model: None,
            base_url: None,
            api_kind: None,
            api_key: None,
            trust: None,
            max_turns: c.max_turns,
            token_budget: c.token_budget,
            timeout: Duration::from_secs(c.timeout_secs),
            vendor: None,
        }
    }
}

static LAUNCH_SPEC: OnceLock<ChildLaunchSpec> = OnceLock::new();

/// Install the parent's resolved launch settings (first call wins).
pub fn set_launch_spec(spec: ChildLaunchSpec) {
    let _ = LAUNCH_SPEC.set(spec);
}

/// The `agent` tool. Fields are overrides; unset ones resolve to the
/// process-wide registry / launch spec at execute time.
#[derive(Default)]
pub struct AgentTool {
    registry: Option<Arc<AgentRegistry>>,
    spec: Option<ChildLaunchSpec>,
    program: Option<ChildProgram>,
    /// Fallback registry when no global one was installed.
    fallback: OnceLock<Arc<AgentRegistry>>,
}

impl AgentTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fully specified tool (tests, embedding).
    pub fn with_parts(
        registry: Arc<AgentRegistry>,
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
            .or_else(agent_registry::global)
            .unwrap_or_else(|| {
                self.fallback
                    .get_or_init(
                        || AgentRegistry::new(&crate::config::AgentConfig::default()),
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
    pub registry: Arc<AgentRegistry>,
    pub spec: ChildLaunchSpec,
    pub program: ChildProgram,
}

/// Hard cap on how many agents a single `parallel` call may fan out
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
///
/// `agent: None` dispatches the built-in general-purpose agent
/// (DYN-01). `role`/`tools`/`model`/`description` are inline
/// overrides applied on top of the resolved agent (DYN-02).
#[derive(Debug, Clone)]
struct AgentItem {
    agent: Option<String>,
    task: String,
    cwd: Option<String>,
    role: Option<String>,
    tools: Option<Vec<String>>,
    model: Option<String>,
    description: Option<String>,
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

/// Prefix an error-message key with `prefix` (`tasks[0]` etc.), or
/// leave it bare when `prefix` is empty (single-mode top-level args).
fn errkey(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Read an optional string field, erroring if present but not a string.
fn opt_str_field(it: &Value, prefix: &str, key: &str) -> Result<Option<String>, String> {
    match it.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{}` must be a string", errkey(prefix, key))),
    }
}

/// Parse one dispatch item's shape, shared by `parse_items` (`prefix`
/// = `tasks[i]`/`chain[i]`) and the Single branch of `execute()`
/// (`prefix` empty, reading the top-level args object) — D-01.
fn parse_item(it: &Value, prefix: &str) -> Result<AgentItem, String> {
    let agent = opt_str_field(it, prefix, "agent")?;
    let task = it
        .get("task")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("`{}` must be a string", errkey(prefix, "task")))?
        .to_string();
    let cwd = opt_str_field(it, prefix, "cwd")?;
    let role = opt_str_field(it, prefix, "role")?;
    let model = opt_str_field(it, prefix, "model")?;
    let description = opt_str_field(it, prefix, "description")?;
    let tools = match it.get("tools") {
        None | Some(Value::Null) => None,
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                return Err(format!(
                    "`{}` must list at least one tool; omit it to allow all tools",
                    errkey(prefix, "tools")
                ));
            }
            let mut v = Vec::with_capacity(arr.len());
            for x in arr {
                match x.as_str() {
                    Some(s) => v.push(s.to_string()),
                    None => {
                        return Err(format!(
                            "`{}` must be an array of strings",
                            errkey(prefix, "tools")
                        ))
                    }
                }
            }
            Some(v)
        }
        Some(_) => {
            return Err(format!(
                "`{}` must be an array of strings",
                errkey(prefix, "tools")
            ))
        }
    };
    Ok(AgentItem {
        agent,
        task,
        cwd,
        role,
        tools,
        model,
        description,
    })
}

/// Parse a `tasks`/`chain` array into concrete items, validating each
/// entry's shape. `field` names the array for error messages.
fn parse_items(value: &Value, field: &str) -> Result<Vec<AgentItem>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| format!("`{field}` must be an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, it) in arr.iter().enumerate() {
        out.push(parse_item(it, &format!("{field}[{i}]"))?);
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
/// binary so an agent uses the exact same build as its parent.
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
impl Tool for AgentTool {
    // An agent spawns a whole `nanopi` process; like `bash`, what it
    // touches is opaque, so serialize the batch it appears in.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent".into(),
            description: concat!(
                "Delegate independent or exploratory work, or reads that would flood ",
                "your own context (recon, planning, review, large file scans), to an ",
                "agent that runs in an isolated context window (a separate nanopi ",
                "process). Give it a complete, self-contained task: the agent sees ",
                "none of your conversation. It returns a capped report, not its ",
                "transcript. `agent` is optional — omit it to use the built-in ",
                "general-purpose agent; name one of the markdown files in ",
                "~/.nanopi/agents (user) or .nanopi/agents (project) to use a ",
                "predefined agent instead. The default agent_scope is \"user\"; ",
                "\"project\"/\"both\" require a trusted project. Prefer one agent ",
                "working through a sequence of dependent steps (chain mode) over ",
                "several short, separate dispatches.\n\n",
                "Provide EXACTLY ONE of three modes:\n",
                "- single: {task, agent?, role?, tools?, model?, description?} — one ",
                "agent, returns its final answer.\n",
                "- parallel: {tasks: [{task, agent?, role?, tools?, model?, ",
                "description?, cwd?}, ...]} — runs concurrently (max 8 tasks, 4 at a ",
                "time); returns a section per task.\n",
                "- chain: {chain: [{task, agent?, role?, tools?, model?, ",
                "description?, cwd?}, ...]} — runs sequentially; the literal ",
                "`{previous}` in each task is replaced by the prior step's output ",
                "(empty for the first step); stops at the first failed step."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "optional: name of a predefined agent file; omit to use the built-in general-purpose agent."
                    },
                    "task": {
                        "type": "string",
                        "description": "single mode: the task to delegate. Be specific and self-contained: the agent shares none of your context."
                    },
                    "role": {
                        "type": "string",
                        "description": "optional: role prompt for this agent (added to the general-purpose prompt, or replacing a named agent's prompt)."
                    },
                    "tools": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "optional: exactly these tools (by name); `agent` is never allowed."
                    },
                    "model": {
                        "type": "string",
                        "description": "optional: model id; must be served by the active provider; defaults to yours."
                    },
                    "description": {
                        "type": "string",
                        "description": "optional: short label (aim for 3-6 words; may be truncated if longer), shown in listings."
                    },
                    "tasks": {
                        "type": "array",
                        "description": "parallel mode: run these agents concurrently (max 8; up to 4 at a time).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": {"type": "string", "description": "optional: name of a predefined agent file; omit to use the built-in general-purpose agent."},
                                "task": {"type": "string", "description": "Task to delegate to the agent."},
                                "cwd": {"type": "string", "description": "Optional working directory for this agent."},
                                "role": {"type": "string", "description": "optional: role prompt for this agent (added to the general-purpose prompt, or replacing a named agent's prompt)."},
                                "tools": {"type": "array", "items": {"type": "string"}, "description": "optional: exactly these tools (by name); `agent` is never allowed."},
                                "model": {"type": "string", "description": "optional: model id; must be served by the active provider; defaults to yours."},
                                "description": {"type": "string", "description": "optional: short label (aim for 3-6 words; may be truncated if longer), shown in listings."}
                            },
                            "required": ["task"]
                        }
                    },
                    "chain": {
                        "type": "array",
                        "description": "chain mode: run these agents in order. Use the literal `{previous}` in a task to inject the prior step's output.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": {"type": "string", "description": "optional: name of a predefined agent file; omit to use the built-in general-purpose agent."},
                                "task": {"type": "string", "description": "Task with optional {previous} placeholder for prior output."},
                                "cwd": {"type": "string", "description": "Optional working directory for this agent."},
                                "role": {"type": "string", "description": "optional: role prompt for this agent (added to the general-purpose prompt, or replacing a named agent's prompt)."},
                                "tools": {"type": "array", "items": {"type": "string"}, "description": "optional: exactly these tools (by name); `agent` is never allowed."},
                                "model": {"type": "string", "description": "optional: model id; must be served by the active provider; defaults to yours."},
                                "description": {"type": "string", "description": "optional: short label (aim for 3-6 words; may be truncated if longer), shown in listings."}
                            },
                            "required": ["task"]
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
                    },
                    "background": {
                        "type": "boolean",
                        "description": "single mode only: if true, return {id, state, archive_path} immediately and finish the agent in the background; its report is injected into your context once done. Default false."
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let scope = parse_scope(&args).map_err(ToolError::InvalidArgs)?;
        let mode = select_mode(&args).map_err(ToolError::InvalidArgs)?;
        let background = match args.get("background") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(ToolError::InvalidArgs("`background` must be a boolean".into())),
        };
        let l = self.launcher();

        if background && mode != Mode::Single {
            return Err(ToolError::InvalidArgs(
                "`background` is only valid in single mode".into(),
            ));
        }

        match mode {
            Mode::Single => {
                let item = parse_item(&args, "").map_err(ToolError::InvalidArgs)?;
                let run_cwd = resolve_cwd(&ctx.cwd, item.cwd.as_deref());
                let task = item.task.clone();
                if background {
                    Ok(run_item_background(&l, &item, &task, scope, &run_cwd))
                } else {
                    Ok(run_item(&l, &item, &task, scope, &run_cwd).await)
                }
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

/// Resolve `agent_name` (or the built-in general-purpose agent when
/// `None`, DYN-01) under `scope` at `run_cwd`, applying the
/// project-trust gate for named agents. Every failure is in-band.
fn resolve_agent_config(
    agent_name: Option<&str>,
    scope: AgentScope,
    run_cwd: &Path,
) -> Result<AgentConfig, ToolOutput> {
    match agent_name {
        Some(name) => resolve_agent(name, scope, run_cwd),
        None => Ok(AgentConfig::general_purpose()),
    }
}

/// Apply inline `role`/`tools`/`model` overrides on top of a resolved
/// `base` agent config (DYN-02). Per D-01/D-03: on the built-in
/// general-purpose agent a `role` is appended after the base prompt
/// (blank-line separated); on a named agent file it replaces the
/// system prompt outright. `tools`/`model` always replace. Absent
/// fields leave the corresponding value untouched. Pure.
fn apply_inline_overrides(
    base: AgentConfig,
    is_builtin: bool,
    role: Option<&str>,
    tools: Option<Vec<String>>,
    model: Option<&str>,
) -> AgentConfig {
    let mut cfg = base;
    if let Some(role) = role {
        if is_builtin {
            cfg.system_prompt = if cfg.system_prompt.trim().is_empty() {
                role.to_string()
            } else {
                format!("{}\n\n{role}", cfg.system_prompt)
            };
        } else {
            cfg.system_prompt = role.to_string();
        }
    }
    if let Some(tools) = tools {
        cfg.tools = Some(tools);
    }
    if let Some(model) = model {
        cfg.model = Some(model.to_string());
    }
    cfg
}

/// Tool names that are never allowed in an inline `tools` override,
/// regardless of what `ToolRegistry::standard()` reports (T-03-03
/// deny-list always wins; mirrors the deny applied to a spawned child's
/// own registry in `mode::print`). Checked case-insensitively.
const DENIED_TOOLS: &[&str] = &["agent", "subagent"];

/// Main-process-only control tools (CTL-02/03/04): never valid as an
/// inline `tools` override and never passed to a child's `--tools`
/// (T-04-06). Checked case-insensitively, same as [`DENIED_TOOLS`].
pub(crate) const CONTROL_TOOLS: &[&str] = &["send_message", "stop_agent", "list_agents"];

/// Canonicalize and validate an inline `tools` override before spawn
/// (D-04). Rejects deny-listed names (`agent`/`subagent`, elevation of
/// privilege) and unknown names, listing the allowed set. Returns the
/// canonical names, deduplicated in input order. Pure.
fn validate_tools(tools: &[String]) -> Result<Vec<String>, String> {
    let registry = crate::tool::ToolRegistry::standard();
    let allowed: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|n| !DENIED_TOOLS.iter().any(|d| d.eq_ignore_ascii_case(n)))
        .collect();
    let mut bad: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for t in tools {
        if DENIED_TOOLS.iter().any(|d| d.eq_ignore_ascii_case(t)) {
            bad.push(t.clone());
            continue;
        }
        match registry.canonical_name(t) {
            // Re-check the deny-list on the *canonical* name — canonicalization
            // can fold a mangled name (e.g. "agent_tool") back onto a denied
            // literal ("agent"), and that must still be denied.
            Some(canonical) if DENIED_TOOLS.iter().any(|d| d.eq_ignore_ascii_case(&canonical)) => {
                bad.push(t.clone());
            }
            Some(canonical) => {
                if !out.contains(&canonical) {
                    out.push(canonical);
                }
            }
            None => bad.push(t.clone()),
        }
    }
    if !bad.is_empty() {
        return Err(format!(
            "unknown or denied tool(s): {}. Allowed tools: {}",
            bad.join(", "),
            allowed.join(", ")
        ));
    }
    Ok(out)
}

/// Validate an inline `model` override before spawn (D-05 safe
/// default): the parent's own configured model is always accepted
/// (case-insensitive); otherwise the model must be a known id served by
/// the active provider's vendor. With no active vendor (custom
/// endpoint) or the `fallback` vendor, any registry-known model is
/// accepted since there is no catalogue to check cross-vendor against.
///
/// Assumption: nanopi config is single-provider, so "a provider
/// configured for that vendor" can only be the one currently active —
/// there is no multi-provider routing to fall back to.
fn validate_model(
    model: &str,
    parent_model: Option<&str>,
    parent_vendor: Option<&str>,
) -> Result<(), String> {
    if let Some(pm) = parent_model {
        if pm.eq_ignore_ascii_case(model) {
            return Ok(());
        }
    }
    let Some(model_vendor) = crate::models::model_vendor(model) else {
        // When there's no active vendor, or it's the `fallback`/custom
        // vendor, there is no catalogue to suggest from — phrase the
        // error without implying a (vendor-scoped, but actually empty)
        // suggestion list exists.
        return match parent_vendor {
            None | Some("fallback") => Err(format!(
                "unknown model {model:?}; it is not in nanopi's model catalogue"
            )),
            Some(p) => {
                let ids: Vec<&str> = crate::models::models_for_vendor(p)
                    .iter()
                    .map(|m| m.id)
                    .collect();
                Err(format!(
                    "unknown model {model:?}. Models available from the active provider ({p}): {}",
                    ids.join(", ")
                ))
            }
        };
    };
    match parent_vendor {
        Some(p) if p != "fallback" && !p.eq_ignore_ascii_case(model_vendor) => Err(format!(
            "model {model:?} belongs to vendor {model_vendor}, but only the active provider \
             ({p}) is configured; cross-vendor agents need a configured provider for that vendor"
        )),
        _ => Ok(()),
    }
}

/// Resolve + override + validate + run one dispatch item. Every
/// failure is in-band (no spawn occurs on a validation failure).
async fn run_item(
    l: &Launcher,
    item: &AgentItem,
    task: &str,
    scope: AgentScope,
    run_cwd: &Path,
) -> ToolOutput {
    let is_builtin = item.agent.is_none();
    let base = match resolve_agent_config(item.agent.as_deref(), scope, run_cwd) {
        Ok(agent) => agent,
        Err(soft) => return soft,
    };
    let mut cfg = apply_inline_overrides(
        base,
        is_builtin,
        item.role.as_deref(),
        item.tools.clone(),
        item.model.as_deref(),
    );
    // Pre-spawn validation (D-04/D-05): only for inline overrides — an
    // agent file's own tools/model are unchanged behaviour (DYN-03).
    if item.tools.is_some() {
        match validate_tools(cfg.tools.as_deref().unwrap_or_default()) {
            Ok(canonical) => cfg.tools = Some(canonical),
            Err(e) => return soft_error(e),
        }
    }
    if let Some(model) = &item.model {
        if let Err(e) = validate_model(model, l.spec.model.as_deref(), l.spec.vendor.as_deref()) {
            return soft_error(e);
        }
    }
    run_single(l, &cfg, task, run_cwd, item.description.as_deref()).await
}

/// `background: true` counterpart to [`run_item`]: identical
/// resolve/override/validate pipeline, then `prepare_run` +
/// `spawn_background` instead of awaiting `run_single` (CTL-01).
fn run_item_background(
    l: &Launcher,
    item: &AgentItem,
    task: &str,
    scope: AgentScope,
    run_cwd: &Path,
) -> ToolOutput {
    let is_builtin = item.agent.is_none();
    let base = match resolve_agent_config(item.agent.as_deref(), scope, run_cwd) {
        Ok(agent) => agent,
        Err(soft) => return soft,
    };
    let mut cfg = apply_inline_overrides(
        base,
        is_builtin,
        item.role.as_deref(),
        item.tools.clone(),
        item.model.as_deref(),
    );
    if item.tools.is_some() {
        match validate_tools(cfg.tools.as_deref().unwrap_or_default()) {
            Ok(canonical) => cfg.tools = Some(canonical),
            Err(e) => return soft_error(e),
        }
    }
    if let Some(model) = &item.model {
        if let Err(e) = validate_model(model, l.spec.model.as_deref(), l.spec.vendor.as_deref()) {
            return soft_error(e);
        }
    }
    match prepare_run(l, &cfg, task, run_cwd, item.description.as_deref()) {
        Ok(prepared) => spawn_background(&l.registry, prepared),
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
    items: Vec<AgentItem>,
    scope: AgentScope,
    base_cwd: &Path,
) -> ToolOutput {
    let futs = items.into_iter().map(|item| {
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        async move {
            let display = item
                .agent
                .clone()
                .unwrap_or_else(|| GENERAL_PURPOSE_NAME.to_string());
            let task = item.task.clone();
            let out = run_item(l, &item, &task, scope, &run_cwd).await;
            (display, out)
        }
    });
    let results: Vec<(String, ToolOutput)> = join_all(futs).await;
    format_parallel(&results)
}

/// Run a `chain`: steps in order, each seeing the prior step's output
/// via `{previous}`. Stops at the first failed step and reports where.
async fn run_chain(
    l: &Launcher,
    items: Vec<AgentItem>,
    scope: AgentScope,
    base_cwd: &Path,
) -> ToolOutput {
    let mut previous = String::new();
    let mut steps: Vec<(String, ToolOutput)> = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let task = substitute_previous(&item.task, &previous);
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        let display = item
            .agent
            .clone()
            .unwrap_or_else(|| GENERAL_PURPOSE_NAME.to_string());
        let out = run_item(l, &item, &task, scope, &run_cwd).await;
        let failed = out.is_error;
        previous = out.content.clone();
        steps.push((display, out));
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
    // T-04-06: a control tool name can never reach a child, however it
    // got into `tools` (defense in depth on top of `validate_tools`,
    // which already rejects it as "unknown" since `standard()` never
    // registers these).
    let tools: Vec<&str> = tools
        .iter()
        .map(String::as_str)
        .filter(|t| !CONTROL_TOOLS.iter().any(|c| c.eq_ignore_ascii_case(t)))
        .collect();
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

/// Max bytes of `report.md` returned to the parent model (D-06): the
/// parent sees only a capped excerpt plus a pointer to the full
/// `report.md` on disk, never the child's transcript.
const PARENT_REPORT_CAP: usize = 8 * 1024;

/// Cap `text` to `PARENT_REPORT_CAP` bytes at a UTF-8 char boundary,
/// appending a truncation note naming `report_path` when cut (D-06).
/// Pure.
fn cap_report(text: String, report_path: &Path) -> String {
    if text.len() <= PARENT_REPORT_CAP {
        return text;
    }
    let mut cut = PARENT_REPORT_CAP;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut capped = text;
    capped.truncate(cut);
    capped.push_str(&format!(
        "\n…(report truncated at {} KiB; full report: {})",
        PARENT_REPORT_CAP / 1024,
        report_path.display()
    ));
    capped
}

/// Marks a registry entry `Stopped` if the dispatch future is dropped
/// before it records a terminal state.
struct StateGuard<'a> {
    reg: &'a AgentRegistry,
    id: String,
    dir: PathBuf,
    done: bool,
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            ensure_report(&self.dir, &self.id, AgentState::Stopped, "stopped by parent");
            self.reg.set_state(&self.id, AgentState::Stopped);
        }
    }
}

/// cwds that have already had `.nanopi/agents/` registered in
/// `.gitignore` this process (D-07; one attempt per cwd is enough).
fn gitignored_cwds() -> &'static Mutex<HashSet<PathBuf>> {
    static SEEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

fn ensure_gitignore_once(cwd: &Path) {
    let mut seen = gitignored_cwds()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if seen.contains(cwd) {
        return;
    }
    match archive::ensure_gitignore(cwd) {
        Ok(_) => {
            seen.insert(cwd.to_path_buf());
        }
        Err(e) => crate::note!("nanopi: debug: ensure_gitignore({}): {e}", cwd.display()),
    }
}

/// D-04: if the child left no `report.md` (killed, crashed, stopped,
/// timed out), write one atomically so the parent always has something
/// to return / show. Never overwrites an existing report.
fn ensure_report(dir: &Path, id: &str, state: AgentState, error_text: &str) {
    let report = dir.join("report.md");
    if report.exists() {
        return;
    }
    let ended = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let summary = if error_text.trim().is_empty() {
        "agent ended without a report".to_string()
    } else {
        error_text.to_string()
    };
    let body = render_report(
        &ReportMeta {
            id: id.to_string(),
            state: state.as_str().to_string(),
            ended,
            turns: None,
            tokens: None,
            worktree: None,
            branch: None,
        },
        &summary,
        &[],
        &[summary.clone()],
        &[],
    );
    // Pre-create at 0600 so the atomic rename preserves perms.
    if let Err(e) = write_private(&report, "") {
        crate::note!("nanopi: debug: precreate report({}): {e}", report.display());
        return;
    }
    if let Err(e) = crate::tool::file_state::atomic_write(&report, body.as_bytes()) {
        crate::note!("nanopi: debug: ensure_report({}): {e}", report.display());
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

/// Output of [`prepare_run`]: the synchronous half of a dispatch
/// (reserve the slot, write `brief.md`, regenerate the index, build the
/// child command) with nothing async yet started. `run_body` consumes
/// it; `spawn_background` moves it into a `tokio::spawn`'d task.
pub(crate) struct PreparedRun {
    id: String,
    dir: PathBuf,
    command: Command,
    timeout: Duration,
}

impl PreparedRun {
    /// `.nanopi/agents/<run>/<id>/`, for `{id, state, archive_path}`.
    fn archive_path(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }
}

/// The synchronous prepare step shared by `run_single` and
/// `spawn_background` (CTL-01): reserve the slot, write `brief.md`,
/// regenerate the run index, build the child command. `Err` carries an
/// already-terminal `ToolOutput` (reserve failed, or brief-write failed
/// and the entry was marked `Failed`) — no spawn occurs either way.
fn prepare_run(
    l: &Launcher,
    agent: &AgentConfig,
    task: &str,
    cwd: &Path,
    label: Option<&str>,
) -> Result<PreparedRun, ToolOutput> {
    let reg = &*l.registry;
    // D-07: register .nanopi/agents/ in the project .gitignore before the
    // first archive directory is created, once per cwd per process.
    ensure_gitignore_once(cwd);
    // D-03: agent dir exists before spawn; max_live enforced here.
    let (id, dir) = reg
        .reserve(&crate::paths::project_agents_dir(cwd))
        .map_err(|e| soft_error(format!("Cannot start agent {:?}: {e}", agent.name)))?;
    let tools = agent.tools.clone().unwrap_or_default();
    let role = if agent.system_prompt.trim().is_empty() {
        (!agent.description.trim().is_empty()).then(|| agent.description.clone())
    } else {
        Some(agent.system_prompt.clone())
    };
    let started = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let brief = render_brief_with_meta(
        &BriefSpec {
            task: task.to_string(),
            role,
            tools: tools.clone(),
            model: agent.model.clone().or_else(|| l.spec.model.clone()),
        },
        &BriefMeta {
            id: id.clone(),
            state: "queued".to_string(),
            started,
            parent: reg.run_id().to_string(),
            label: label.map(str::to_string),
        },
    );
    if let Err(e) = write_private(&dir.join("brief.md"), &brief) {
        ensure_report(&dir, &id, AgentState::Failed, &format!("cannot write brief: {e}"));
        reg.set_state(&id, AgentState::Failed);
        return Err(failed_output(&format!("cannot write brief: {e}"), ""));
    }
    if let Some(run_dir) = dir.parent() {
        if let Err(e) = archive::regenerate_index(run_dir) {
            crate::note!("nanopi: debug: regenerate_index({}): {e}", run_dir.display());
        }
    }

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

    Ok(PreparedRun {
        id,
        dir,
        command,
        timeout: l.spec.timeout,
    })
}

/// The async half of a dispatch: acquire the concurrency permit, run
/// the child, cap and attach `report.md`, record the terminal state.
/// Returns the same `ToolOutput` shape `run_single` has always
/// returned. Shared by the foreground path and `spawn_background`'s
/// spawned task.
pub(crate) async fn run_body(reg: &AgentRegistry, prepared: PreparedRun) -> ToolOutput {
    let PreparedRun {
        id,
        dir,
        command,
        timeout,
    } = prepared;
    let mut sg = StateGuard {
        reg,
        id: id.clone(),
        dir: dir.clone(),
        done: false,
    };

    // Beyond max_concurrency: queue here (state stays Queued).
    let _permit = reg.acquire_run().await;
    reg.set_state(&id, AgentState::Running);

    let mut out =
        spawn_and_collect_with(command, timeout, |pid| reg.set_pid(&id, pid)).await;

    let status = out
        .metadata
        .as_ref()
        .and_then(|m| m.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("failed")
        .to_string();
    let state = match status.as_str() {
        "completed" => AgentState::Completed,
        "limit_reached" => AgentState::LimitReached,
        _ => AgentState::Failed,
    };
    // D-04: the child may have died without writing report.md (killed,
    // crashed, timed out) — guarantee one exists before reading it back.
    let error_text = if out.is_error {
        out.content.clone()
    } else {
        String::new()
    };
    ensure_report(&dir, &id, state, &error_text);

    let report = dir.join("report.md");
    if let Ok(text) = std::fs::read_to_string(&report) {
        let text = cap_report(text, &report);
        out.content = if out.is_error {
            format!("{}\n\n--- report.md ---\n{text}", out.content)
        } else if status == "limit_reached" {
            let limit = out
                .metadata
                .as_ref()
                .and_then(|m| m["limit"].as_str())
                .unwrap_or("limit")
                .to_string();
            format!("[agent stopped: {limit} reached]\n\n{text}")
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

    reg.set_state(&id, state);
    sg.done = true;
    out
}

/// One tracked, capped, briefed child dispatch. Always in-band.
pub async fn run_single(
    l: &Launcher,
    agent: &AgentConfig,
    task: &str,
    cwd: &Path,
    label: Option<&str>,
) -> ToolOutput {
    match prepare_run(l, agent, task, cwd, label) {
        Ok(prepared) => run_body(&l.registry, prepared).await,
        Err(soft) => soft,
    }
}

/// `background: true` dispatch (CTL-01/D-01): prepare synchronously,
/// then hand `run_body` to `tokio::spawn` raced against a cancellation
/// token, and return `{id, state, archive_path}` WITHOUT awaiting the
/// task. When finished (normally or stopped), the capped report is
/// pushed to the registry's outbox for later injection (CTL-05/D-06).
pub(crate) fn spawn_background(reg: &Arc<AgentRegistry>, prepared: PreparedRun) -> ToolOutput {
    let id = prepared.id.clone();
    let archive_path = prepared.archive_path();
    let dir = prepared.dir.clone();

    let token = CancellationToken::new();
    let reg_task = Arc::clone(reg);
    let token_task = token.clone();
    let id_task = id.clone();
    let dir_task = dir.clone();

    let handle = tokio::spawn(async move {
        let reg_ref = &*reg_task;
        let final_state = tokio::select! {
            biased;
            _ = token_task.cancelled() => {
                // `run_body`'s `StateGuard` is dropped mid-flight by the
                // cancellation of this branch's sibling, which already
                // marks the entry Stopped and writes the partial report
                // (ChildGuard in spawn_and_collect_with kills the group
                // on drop too). Nothing left to do here but read it back.
                AgentState::Stopped
            }
            out = run_body(reg_ref, prepared) => {
                let _ = out;
                reg_ref
                    .snapshot()
                    .into_iter()
                    .find(|e| e.id == id_task)
                    .map(|e| e.state)
                    .unwrap_or(AgentState::Failed)
            }
        };
        let report_path = dir_task.join("report.md");
        let text = std::fs::read_to_string(&report_path).unwrap_or_default();
        let capped = cap_report(text, &report_path);
        reg_task.push_report(&id_task, final_state, &capped);
    });

    reg.track_background(&id, handle, token);

    ToolOutput {
        content: json!({
            "id": id,
            "state": AgentState::Queued.as_str(),
            "archive_path": archive_path,
        })
        .to_string(),
        is_error: false,
        metadata: Some(json!({"agent_id": id, "agent_dir": archive_path})),
        images: Vec::new(),
    }
}

/// Build the child command to continue a finished agent (CTL-06): same
/// id and dir, same `--session-file` transcript (which auto-resumes —
/// `session::open_or_create_at`), brief.md re-read as the task (it now
/// carries the amendment appended by the caller). Pure apart from
/// reading `l`'s resolved spec/program.
pub(crate) fn prepare_continue(l: &Launcher, id: &str, dir: &Path, cwd: &Path) -> PreparedRun {
    let mut command = l.program.command();
    command
        .args(build_child_args(&l.spec, dir, &[], None))
        .envs(build_child_env(&l.spec, id))
        .current_dir(cwd);
    PreparedRun {
        id: id.to_string(),
        dir: dir.to_path_buf(),
        command,
        timeout: l.spec.timeout,
    }
}

/// Continue a finished agent in the background under the same id
/// (CTL-06/D-05). Unlike [`spawn_background`], the previous `report.md`
/// (if any) is preserved: the new run's report is appended after a
/// `## Continued` marker rather than overwriting it.
pub(crate) fn spawn_continue_background(reg: &Arc<AgentRegistry>, prepared: PreparedRun) {
    let id = prepared.id.clone();
    let dir = prepared.dir.clone();
    let prev_report = std::fs::read_to_string(dir.join("report.md")).ok();

    let token = CancellationToken::new();
    let reg_task = Arc::clone(reg);
    let token_task = token.clone();
    let id_task = id.clone();
    let dir_task = dir.clone();

    let handle = tokio::spawn(async move {
        let reg_ref = &*reg_task;
        let final_state = tokio::select! {
            biased;
            _ = token_task.cancelled() => AgentState::Stopped,
            out = run_body(reg_ref, prepared) => {
                let _ = out;
                reg_ref
                    .snapshot()
                    .into_iter()
                    .find(|e| e.id == id_task)
                    .map(|e| e.state)
                    .unwrap_or(AgentState::Failed)
            }
        };
        let report_path = dir_task.join("report.md");
        if let Some(prev) = prev_report {
            if let Ok(new_text) = std::fs::read_to_string(&report_path) {
                let merged = format!("{prev}\n\n## Continued\n\n{new_text}");
                if let Err(e) = crate::tool::file_state::atomic_write(&report_path, merged.as_bytes()) {
                    crate::note!("nanopi: debug: merge continued report({}): {e}", report_path.display());
                }
            }
        }
        let text = std::fs::read_to_string(&report_path).unwrap_or_default();
        let capped = cap_report(text, &report_path);
        reg_task.push_report(&id_task, final_state, &capped);
    });

    reg.track_background(&id, handle, token);
}

/// Max bytes of child stdout retained (T-01-11). A larger envelope is
/// treated as unparseable rather than buffered without bound.
const STDOUT_CAP: usize = 8 * 1024 * 1024;
/// Max bytes of child stderr retained — the *tail* is kept.
const STDERR_TAIL: usize = 64 * 1024;
/// How long to keep draining pipes after the child exits (WR-04).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

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
        format!("Agent failed: {reason}")
    } else {
        format!("Agent failed: {reason}\n--- stderr (tail) ---\n{tail}")
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
        Err(e) => return failed_output(&format!("failed to spawn agent: {e}"), ""),
    };
    let pid = child.id();
    // Kills the whole group if this future is dropped (cancel) or times out.
    let guard = ChildGuard::new(pid);
    if let Some(pid) = pid {
        on_pid(pid);
    }

    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return failed_output("agent stdio not captured", "");
    };

    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    // `Ok(status)` = reaped normally; `Err(reason)` = failure already mapped.
    let (outcome, out_truncated) = {
        let drains = async {
            let (t, _) = tokio::join!(
                drain_head(stdout, &mut out_buf, STDOUT_CAP),
                drain_tail(stderr, &mut err_buf, STDERR_TAIL)
            );
            t
        };
        tokio::pin!(drains);
        let mut drained: Option<bool> = None;
        // Wait for the leader to exit WITHOUT reaping it (WR-03): the
        // zombie keeps the pgid reserved, so the group sweep below cannot
        // hit an unrelated group that reused the id.
        let waited = tokio::time::timeout(timeout, async {
            let exit = wait_exited(&mut child, pid);
            tokio::pin!(exit);
            tokio::select! {
                t = &mut drains => {
                    drained = Some(t);
                    (&mut exit).await
                }
                r = &mut exit => r,
            }
        })
        .await;
        match waited {
            Err(_) => {
                drop(guard); // SIGKILL the group
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                (Err(format!("timed out after {}s", timeout.as_secs_f64())), false)
            }
            Ok(Err(e)) => (Err(format!("waiting for agent: {e}")), false),
            Ok(Ok(())) => {
                // WR-04: the leader is gone. Something it spawned may
                // still hold the pipes open; give the drains a short
                // grace period instead of waiting out the full timeout.
                if drained.is_none() {
                    drained = tokio::time::timeout(DRAIN_GRACE, &mut drains).await.ok();
                }
                // Sweep stray grandchildren while the zombie still owns
                // the pgid, then let the drains see EOF and reap.
                drop(guard);
                if drained.is_none() {
                    drained = tokio::time::timeout(DRAIN_GRACE, &mut drains).await.ok();
                }
                match child.wait().await {
                    Ok(s) => (Ok(s), drained.unwrap_or(false)),
                    Err(e) => (Err(format!("waiting for agent: {e}")), false),
                }
            }
        }
    };

    let stderr_tail = String::from_utf8_lossy(&err_buf).to_string();
    let status = match outcome {
        Ok(s) => s,
        Err(reason) => return failed_output(&reason, &stderr_tail),
    };

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

/// Resolve once the child has exited. On unix this uses
/// `waitid(WNOWAIT)` so the child is left as a zombie (not reaped) and
/// its pid/pgid cannot be reused until `child.wait()` is called.
async fn wait_exited(child: &mut tokio::process::Child, pid: Option<u32>) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(pid) = pid {
        return tokio::task::spawn_blocking(move || loop {
            // SAFETY: zeroed siginfo_t is a valid out-parameter for waitid.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if r == 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        })
        .await
        .map_err(std::io::Error::other)?;
    }
    let _ = pid;
    child.wait().await.map(|_| ())
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
        "(agent produced no output)".to_string()
    } else {
        text
    };
    if status == "limit_reached" {
        let limit = env.limit.clone().unwrap_or_else(|| "limit".into());
        content = format!("[agent stopped: {limit} reached]\n\n{content}");
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
    fn cap_report_leaves_small_report_unchanged() {
        let text = "a".repeat(100);
        let path = Path::new("/tmp/report.md");
        assert_eq!(cap_report(text.clone(), path), text);
    }

    #[test]
    fn cap_report_truncates_at_8kb_with_path_note() {
        let text = "x".repeat(20 * 1024);
        let path = Path::new("/tmp/agents/run/a1/report.md");
        let capped = cap_report(text, path);
        assert!(capped.len() <= PARENT_REPORT_CAP + 200);
        assert!(capped.contains("report truncated"));
        assert!(capped.contains("/tmp/agents/run/a1/report.md"));
    }

    #[test]
    fn cap_report_never_splits_a_utf8_char() {
        // Multi-byte char ('é', 2 bytes) straddling the 8 KiB cut point.
        let mut text = "a".repeat(PARENT_REPORT_CAP - 1);
        text.push('é');
        text.push_str(&"b".repeat(100));
        let path = Path::new("/tmp/report.md");
        let capped = cap_report(text, path);
        // Must be valid UTF-8 (String guarantees this) and must not
        // contain a replacement character from a bad cut.
        assert!(!capped.contains('\u{FFFD}'));
        assert!(capped.contains("report truncated"));
    }

    #[test]
    fn spec_schema_exposes_inline_overrides_and_optional_agent() {
        let spec = AgentTool::default().spec();
        let params = spec.parameters;
        let props = &params["properties"];
        for key in [
            "agent",
            "task",
            "role",
            "tools",
            "model",
            "description",
            "tasks",
            "chain",
            "agent_scope",
            "cwd",
        ] {
            assert!(props[key].is_object(), "missing top-level property {key}");
        }
        assert_eq!(props["tools"]["type"], json!("array"));
        assert_eq!(props["tools"]["items"]["type"], json!("string"));

        for list in ["tasks", "chain"] {
            let item_props = &props[list]["items"]["properties"];
            for key in ["agent", "task", "cwd", "role", "tools", "model", "description"] {
                assert!(
                    item_props[key].is_object(),
                    "missing {list} item property {key}"
                );
            }
            assert_eq!(props[list]["items"]["required"], json!(["task"]));
        }

        let desc = spec.description;
        assert!(desc.contains("optional"));
        assert!(desc.contains("sequence of dependent steps"));
    }

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
        // `agent` is optional (DYN-01): a bare `{task}` item is valid and
        // resolves to the built-in general-purpose agent downstream.
        let items = parse_items(&json!([{"task": "t"}]), "tasks").unwrap();
        assert_eq!(items[0].agent, None);
        // `task` is still required.
        assert!(parse_items(&json!([{"agent": "a"}]), "tasks").is_err());
    }

    #[test]
    fn parse_item_rejects_non_string_optional_fields() {
        let err = parse_item(&json!({"task": "t", "agent": 1}), "tasks[0]").unwrap_err();
        assert_eq!(err, "`tasks[0].agent` must be a string");
        let err = parse_item(&json!({"task": "t", "role": 1}), "tasks[0]").unwrap_err();
        assert_eq!(err, "`tasks[0].role` must be a string");
        let err = parse_item(&json!({"task": "t", "model": 1}), "tasks[0]").unwrap_err();
        assert_eq!(err, "`tasks[0].model` must be a string");
        let err = parse_item(&json!({"task": "t", "description": 1}), "tasks[0]").unwrap_err();
        assert_eq!(err, "`tasks[0].description` must be a string");
        let err = parse_item(&json!({"task": "t", "tools": "read"}), "tasks[0]").unwrap_err();
        assert_eq!(err, "`tasks[0].tools` must be an array of strings");
        let err = parse_item(&json!({"task": "t", "tools": [1]}), "tasks[0]").unwrap_err();
        assert_eq!(err, "`tasks[0].tools` must be an array of strings");
        let err = parse_item(&json!({"task": "t", "tools": []}), "tasks[0]").unwrap_err();
        assert_eq!(
            err,
            "`tasks[0].tools` must list at least one tool; omit it to allow all tools"
        );
        // single-mode: no prefix.
        let err = parse_item(&json!({"task": "t", "agent": 1}), "").unwrap_err();
        assert_eq!(err, "`agent` must be a string");
    }

    #[test]
    fn single_mode_call_with_only_task_has_no_agent() {
        let item = parse_item(&json!({"task": "do it"}), "").unwrap();
        assert_eq!(item.agent, None);
        assert_eq!(item.task, "do it");
    }

    #[test]
    fn resolve_agent_config_none_is_general_purpose() {
        let dir = std::env::temp_dir();
        let cfg = resolve_agent_config(None, AgentScope::Project, &dir).unwrap();
        assert_eq!(cfg.name, crate::agent::agents::GENERAL_PURPOSE_NAME);
    }

    #[test]
    fn resolve_agent_config_unknown_name_is_soft_error() {
        let dir = std::env::temp_dir();
        let out =
            resolve_agent_config(Some("__definitely_not_an_agent__"), AgentScope::Project, &dir)
                .unwrap_err();
        assert!(out.is_error);
        assert!(out.content.contains("Unknown agent"));
    }

    #[cfg(unix)]
    #[test]
    fn apply_inline_overrides_role_appends_on_builtin_replaces_on_named() {
        let builtin = AgentConfig::general_purpose();
        let out = apply_inline_overrides(builtin.clone(), true, Some("extra role"), None, None);
        assert!(out.system_prompt.starts_with(&builtin.system_prompt));
        assert!(out.system_prompt.ends_with("extra role"));
        assert!(out.system_prompt.contains("\n\nextra role"));

        let named = agent_fixture();
        let out = apply_inline_overrides(named, false, Some("new role"), None, None);
        assert_eq!(out.system_prompt, "new role");
    }

    #[cfg(unix)]
    #[test]
    fn apply_inline_overrides_tools_and_model_always_replace() {
        let named = agent_fixture();
        let out = apply_inline_overrides(
            named,
            false,
            None,
            Some(vec!["bash".into()]),
            Some("some-model"),
        );
        assert_eq!(out.tools, Some(vec!["bash".to_string()]));
        assert_eq!(out.model.as_deref(), Some("some-model"));
    }

    #[cfg(unix)]
    #[test]
    fn apply_inline_overrides_absent_fields_untouched() {
        let named = agent_fixture();
        let out = apply_inline_overrides(named.clone(), false, None, None, None);
        assert_eq!(out.system_prompt, named.system_prompt);
        assert_eq!(out.tools, named.tools);
        assert_eq!(out.model, named.model);
    }

    #[test]
    fn validate_tools_canonicalizes_and_dedupes() {
        let out = validate_tools(&["read".into(), "GREP".into(), "Bash_tool".into()]).unwrap();
        assert_eq!(out, vec!["read".to_string(), "grep".to_string(), "bash".to_string()]);
    }

    #[test]
    fn validate_tools_rejects_unknown_and_lists_allowed() {
        let err = validate_tools(&["read".into(), "nope".into()]).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(!err.contains("\"agent\""), "{err}");
        // "agent" tool must not appear in the allowed list.
        let allowed_part = err.split("Allowed tools: ").nth(1).unwrap();
        assert!(!allowed_part.split(", ").any(|n| n == "agent"), "{err}");
    }

    #[test]
    fn validate_tools_denies_agent_and_subagent() {
        assert!(validate_tools(&["agent".into()]).is_err());
        assert!(validate_tools(&["subagent".into()]).is_err());
        assert!(validate_tools(&["AGENT".into()]).is_err());
    }

    #[test]
    fn validate_tools_denies_mangled_agent_names() {
        assert!(validate_tools(&["agent_tool".into()]).is_err());
        assert!(validate_tools(&["AGENT_TOOL".into()]).is_err());
        assert!(validate_tools(&["Agent_Tool".into()]).is_err());
    }

    #[test]
    fn validate_model_parent_model_always_ok() {
        assert!(validate_model("some-weird-model", Some("some-weird-model"), Some("anthropic")).is_ok());
    }

    #[test]
    fn validate_model_known_model_same_vendor_ok_other_vendor_errors() {
        assert!(validate_model("claude-opus-4-7", Some("other"), Some("anthropic")).is_ok());
        let err = validate_model("claude-opus-4-7", Some("other"), Some("deepseek")).unwrap_err();
        assert!(err.contains("anthropic"), "{err}");
        assert!(err.contains("deepseek"), "{err}");
    }

    #[test]
    fn validate_model_unknown_id_lists_vendor_models() {
        let err = validate_model("no-such-model-xyz", Some("other"), Some("anthropic")).unwrap_err();
        assert!(err.contains("anthropic"), "{err}");
    }

    #[test]
    fn validate_model_no_or_fallback_vendor_accepts_any_known_model() {
        assert!(validate_model("claude-opus-4-7", Some("other"), None).is_ok());
        assert!(validate_model("claude-opus-4-7", Some("other"), Some("fallback")).is_ok());
    }

    #[tokio::test]
    async fn parallel_rejects_over_cap() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-cap-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = AgentTool::new();
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
            &AgentTool::new().launcher(),
            vec![AgentItem {
                agent: Some("__definitely_not_an_agent__".into()),
                task: "x".into(),
                cwd: None,
                role: None,
                tools: None,
                model: None,
                description: None,
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
            &AgentTool::new().launcher(),
            vec![AgentItem {
                agent: Some("__definitely_not_an_agent__".into()),
                task: "x".into(),
                cwd: None,
                role: None,
                tools: None,
                model: None,
                description: None,
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
        let tool = AgentTool::new();
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

    /// WR-03: the group sweep runs before the leader is reaped, and still
    /// kills a detached grandchild after a normal exit.
    #[cfg(unix)]
    #[tokio::test]
    async fn normal_exit_sweeps_grandchild_before_reap() {
        let dir = std::env::temp_dir().join(format!("nanopi-sa-sw-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pid");
        let ok = r#"{"session_id":"s","model":"m","finish_reason":"stop","duration_ms":1,"usage":{},"messages":[{"role":"assistant","content":"hi"}],"status":"completed"}"#;
        let script = format!(
            "sleep 300 </dev/null >/dev/null 2>&1 & echo $! > {}; echo '{ok}'",
            f.display()
        );
        let out = spawn_and_collect(sh(&script), Duration::from_secs(10)).await;
        assert!(!out.is_error, "{}", out.content);
        let pid: i32 = std::fs::read_to_string(&f).unwrap().trim().parse().unwrap();
        let mut dead = false;
        for _ in 0..50 {
            if pid_dead(pid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(dead, "grandchild {pid} survived the post-exit sweep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WR-04: a grandchild that inherited stdout must not turn a
    /// finished child into a timeout; the buffered envelope is used.
    #[cfg(unix)]
    #[tokio::test]
    async fn grandchild_holding_stdout_does_not_cause_timeout() {
        let ok = r#"{"session_id":"s","model":"m","finish_reason":"stop","duration_ms":1,"usage":{},"messages":[{"role":"assistant","content":"hi"}],"status":"completed"}"#;
        let script = format!("echo '{ok}'; sleep 300 &");
        let started = std::time::Instant::now();
        let out = spawn_and_collect(sh(&script), Duration::from_secs(60)).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(status_of(&out), "completed");
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
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

    /// T-04-06: control tool names are stripped from `--tools` even if
    /// they somehow end up in the `tools` slice (e.g. a bypassed
    /// `validate_tools`), since a child must never obtain them.
    #[test]
    fn build_child_args_strips_control_tools() {
        let dir = Path::new("/tmp/agents/run/a1");
        let a = build_child_args(
            &spec_fixture(),
            dir,
            &["read".into(), "send_message".into(), "stop_agent".into(), "list_agents".into()],
            None,
        );
        let j = a.join(" ");
        assert!(j.contains("--tools read"), "{j}");
        for denied in ["send_message", "stop_agent", "list_agents"] {
            assert!(!j.contains(denied), "control tool leaked: {denied} in {j}");
        }
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
        let cfg = crate::config::AgentConfig {
            max_live,
            max_concurrency: max_conc,
            ..crate::config::AgentConfig::default()
        };
        AgentTool::with_parts(
            AgentRegistry::new(&cfg),
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
        let out = run_single(&l, &agent_fixture(), "find the thing", &cwd, None).await;
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
    async fn background_dispatch_returns_immediately() {
        let cwd = tmp("bg");
        // Slow fake child: a synchronous await would take 5s.
        let l = launcher(&format!("sleep 5; echo '{OK_ENV}'"), 8, 4);
        let start = std::time::Instant::now();
        let out = AgentTool::with_parts(
            l.registry.clone(),
            l.spec.clone(),
            l.program.clone(),
        )
        .execute(
            json!({"task": "slow task", "background": true}),
            &ToolContext { cwd: cwd.clone() },
        )
        .await
        .expect("in-band dispatch");
        assert!(start.elapsed() < Duration::from_secs(2), "must not block on the child");
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["id"].as_str(), Some("a1"));
        assert!(
            matches!(v["state"].as_str(), Some("queued") | Some("running")),
            "{v}"
        );
        assert!(v["archive_path"].as_str().unwrap().contains("a1"));
        l.registry.wait_background().await;
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_rejected_outside_single_mode() {
        let cwd = tmp("bg-reject");
        let l = launcher(&format!("echo '{OK_ENV}'"), 8, 4);
        let tool = AgentTool::with_parts(
            l.registry.clone(),
            l.spec.clone(),
            l.program.clone(),
        );
        let err = tool
            .execute(
                json!({"tasks": [{"task": "t1"}], "background": true}),
                &ToolContext { cwd: cwd.clone() },
            )
            .await
            .expect_err("background must be rejected outside single mode");
        let msg = match err {
            ToolError::InvalidArgs(m) => m,
            other => panic!("expected InvalidArgs, got {other:?}"),
        };
        assert!(msg.contains("single mode"), "{msg}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn max_live_returns_in_band_limit_error() {
        let cwd = tmp("live");
        let l = launcher(&format!("echo '{OK_ENV}'"), 1, 4);
        l.registry.reserve(&cwd.join(".nanopi/agents")).unwrap(); // one live entry
        let out = run_single(&l, &agent_fixture(), "t", &cwd, None).await;
        assert!(out.is_error);
        assert!(
            out.content.contains("agent limit reached"),
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
        let (x, y) = tokio::join!(run_single(&l, &a, "1", &cwd, None), run_single(&l, &a, "2", &cwd, None));
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
        let f = run_single(&l, &a, "t", &cwd, None).await;
        assert!(f.is_error);
        let ok = run_single(&l, &a, "t", &cwd, None).await;
        assert!(!ok.is_error);
        assert!(ok.content.contains("REPORT-BODY"), "{}", ok.content);
        let rp = ok.metadata.as_ref().unwrap()["report_path"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(rp.ends_with("/a2/report.md"), "{rp}");
        let lr = run_single(&l, &a, "t", &cwd, None).await;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn fallback_report_written_when_child_leaves_none() {
        let cwd = tmp("fallback");
        let script = "exit 3";
        let l = launcher(script, 8, 4);
        let out = run_single(&l, &agent_fixture(), "t", &cwd, None).await;
        assert!(out.is_error);
        let dir = cwd
            .join(".nanopi/agents")
            .join(l.registry.run_id())
            .join("a1");
        let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert_eq!(
            crate::agent::brief::front_matter_get(&report, "state").as_deref(),
            Some("failed")
        );
        assert!(out.content.contains(&report) || out.content.contains("report.md"));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fallback_report_not_overwritten_when_child_writes_one() {
        let cwd = tmp("fallback-keep");
        let script = format!(
            "echo MINE > \"$(dirname \"$4\")/report.md\"; echo '{OK_ENV}'"
        );
        let l = launcher(&script, 8, 4);
        let out = run_single(&l, &agent_fixture(), "t", &cwd, None).await;
        assert!(!out.is_error);
        let dir = cwd
            .join(".nanopi/agents")
            .join(l.registry.run_id())
            .join("a1");
        let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(report.contains("MINE"), "{report}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn brief_front_matter_and_index_after_dispatch() {
        let cwd = tmp("frontmatter");
        let l = launcher(&format!("echo '{OK_ENV}'"), 8, 4);
        let out = run_single(&l, &agent_fixture(), "t", &cwd, None).await;
        assert!(!out.is_error, "{}", out.content);
        let run_dir = cwd.join(".nanopi/agents").join(l.registry.run_id());
        let brief = std::fs::read_to_string(run_dir.join("a1").join("brief.md")).unwrap();
        assert_eq!(
            crate::agent::brief::front_matter_get(&brief, "state").as_deref(),
            Some("done")
        );
        assert_eq!(
            crate::agent::brief::front_matter_get(&brief, "parent").as_deref(),
            Some(l.registry.run_id())
        );
        let index = std::fs::read_to_string(run_dir.join("index.md")).unwrap();
        assert!(index.contains("a1"));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_registers_gitignore_entry() {
        let cwd = tmp("gitignore");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let l = launcher(&format!("echo '{OK_ENV}'"), 8, 4);
        let out = run_single(&l, &agent_fixture(), "t", &cwd, None).await;
        assert!(!out.is_error, "{}", out.content);
        let gi = std::fs::read_to_string(cwd.join(".gitignore")).unwrap_or_default();
        assert!(gi.contains(".nanopi/agents"), "{gi}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// D-04/D-05: a dispatch whose inline `tools`/`model` fail validation
    /// is rejected in-band before any spawn — no agent dir is reserved.
    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_inline_override_fails_before_spawn() {
        let cwd = tmp("novspawn");
        let l = launcher(&format!("echo '{OK_ENV}'"), 8, 4);
        let item = AgentItem {
            agent: None,
            task: "t".into(),
            cwd: None,
            role: None,
            tools: Some(vec!["nope".into()]),
            model: None,
            description: None,
        };
        let out = run_item(&l, &item, &item.task, AgentScope::Project, &cwd).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown or denied tool"), "{}", out.content);
        let run_dir = cwd.join(".nanopi/agents").join(l.registry.run_id());
        assert!(!run_dir.exists(), "no agent dir should have been reserved");

        let item = AgentItem {
            agent: None,
            task: "t".into(),
            cwd: None,
            role: None,
            tools: None,
            model: Some("no-such-model-xyz".into()),
            description: None,
        };
        let out = run_item(&l, &item, &item.task, AgentScope::Project, &cwd).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown model"), "{}", out.content);
        assert!(!run_dir.exists(), "no agent dir should have been reserved");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}

//! `subagent` — delegate a task to a specialized agent in an isolated
//! context window.
//!
//! v0.13.0, Phase 1 (D-02): subagents now run **in-process**, as
//! `tokio::spawn`ed `Agent` turns built with `Agent::build_fresh`, each
//! with its own provider instance — never a child `nanopi` process.
//! There is no fallback to the old child-process runtime; it has been
//! removed entirely.
//!
//! The dispatcher (`run_subagent`) does, per item:
//! - fetch the parent's `SpawnTemplate` from the shared
//!   `SubagentRegistry` (missing → soft in-band error, the runtime was
//!   never initialised);
//! - allocate a short id (`"a1"`, `"a2"`, ...) and reserve a live slot
//!   (`max_live`, D-08);
//! - derive a cancel token: a child of the calling turn's own token when
//!   present (foreground — Esc stops it, D-05), else a child of the
//!   registry's root token;
//! - wait for a global concurrency permit (`max_concurrency`, D-08),
//!   racing the cancel token so a queued dispatch can still be
//!   cancelled before it ever runs;
//! - build a fresh `Agent` via `Agent::build_fresh` with
//!   `ToolRegistry::for_subagent()` (D-10/D-17) and the agent file's own
//!   system prompt, write its transcript to
//!   `.nanopi/agents/<run>/<id>/transcript.jsonl` (D-12), and
//!   `tokio::spawn` its `run_turn`;
//! - map the outcome to an in-band tool result: `status: "ok"` on a
//!   normal finish, `status: "limit_reached"` naming the limit
//!   (D-09), `status: "cancelled"`, or `status: "failed"` carrying the
//!   error text (D-11) — a subagent failure never panics nanopi.
//!
//! v1 ships **single mode**, **parallel mode** (fan-out bounded by the
//! registry's global semaphore, not a per-call one) and **chain mode**
//! (sequential, `{previous}` substitution, stops at the first failed
//! step). Depth is 1: `ToolRegistry::for_subagent()` strips the
//! `subagent` tool itself, so nothing spawned here can recurse.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::agent::agents::{discover_agents, AgentConfig, AgentScope, AgentSource};
use crate::agent::build::{AgentBuildInputs, SkillLoadPolicy};
use crate::agent::context::ToolSpec;
use crate::agent::loop_::{Agent, StopReason};
use crate::agent::prompt_override::PromptOverrides;
use crate::agent::subagent_registry::AgentState;
use crate::event::AgentEvent;
use crate::tool::{ExecutionMode, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};

pub struct SubagentTool;

/// Hard cap on how many subagents a single `parallel` call may fan out
/// to, mirroring PI's `MAX_PARALLEL_TASKS = 8`. Keeps a runaway model
/// from spawning a fork bomb of in-process agents in one call; the
/// registry's `max_live`/`max_concurrency` bound the rest (D-08).
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
        metadata: Some(json!({ "status": "failed" })),
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
        "status": out.metadata.as_ref().and_then(|m| m.get("status").cloned()),
        "agent_id": out.metadata.as_ref().and_then(|m| m.get("agent_id").cloned()),
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

#[async_trait]
impl Tool for SubagentTool {
    // The agents a dispatch starts run concurrently in-process; the
    // registry's own semaphore and live-cap are the real bound, so the
    // batcher is free to run several `subagent` calls alongside other
    // tools in the same round.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "subagent".into(),
            description: concat!(
                "Delegate tasks to specialized agents that each run in their own ",
                "isolated context window (an in-process agent run, not a nested ",
                "chat). Use this to keep large, self-contained subtasks (recon, ",
                "planning, review) out of your own context. Agents are defined as ",
                "markdown files in ~/.nanopi/agents (user) or .nanopi/agents ",
                "(project). The default agent_scope is \"user\"; \"project\"/\"both\" ",
                "require a trusted project.\n\n",
                "Provide EXACTLY ONE of three modes:\n",
                "- single: {agent, task} — one agent, returns its final answer.\n",
                "- parallel: {tasks: [{agent, task, cwd?}, ...]} — runs concurrently ",
                "(max 8 tasks; bounded by [subagent] max_concurrency); returns a ",
                "section per task.\n",
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
                        "description": "parallel mode: run these agents concurrently (max 8).",
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
                        "description": "single mode: optional working directory for the agent. Defaults to the current directory."
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
                run_item(agent_name, task, scope, &run_cwd, ctx).await
            }
            Mode::Parallel => {
                let items = parse_items(&args["tasks"], "tasks").map_err(ToolError::InvalidArgs)?;
                if items.len() > MAX_TASKS {
                    return Err(ToolError::InvalidArgs(format!(
                        "too many parallel tasks ({}); max is {MAX_TASKS}",
                        items.len()
                    )));
                }
                run_parallel(items, scope, &ctx.cwd, ctx).await
            }
            Mode::Chain => {
                let items = parse_items(&args["chain"], "chain").map_err(ToolError::InvalidArgs)?;
                run_chain(items, scope, &ctx.cwd, ctx).await
            }
        }
    }
}

/// Resolve an agent by name under `scope` at `run_cwd`, applying the
/// project-trust gate, then dispatch it in-process. Agent-resolution
/// and trust failures come back as soft (in-band) errors so a batch
/// item can fail without aborting its siblings.
async fn run_item(
    agent_name: &str,
    task: &str,
    scope: AgentScope,
    run_cwd: &Path,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    // Checked before agent resolution: an uninitialised runtime (no
    // `SpawnTemplate` ever installed on the registry) is a caller
    // configuration error, not a "this particular agent name doesn't
    // exist" error — report it as such regardless of whether the named
    // agent would otherwise have been found.
    if ctx.registry.template().is_none() {
        return Ok(soft_error("subagent runtime not initialised".into()));
    }
    match resolve_agent(agent_name, scope, run_cwd) {
        Ok(agent) => Ok(run_subagent(&agent, task, run_cwd, ctx).await),
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

/// Run a `parallel` batch: fan out every item at once. Concurrency is
/// bounded by the registry's own global semaphore inside
/// `run_subagent`, not a per-call one — a cancelled batch cannot leak
/// slots because each item's `LiveSlot`/permit live inside its own
/// future and drop with it.
async fn run_parallel(
    items: Vec<SubagentItem>,
    scope: AgentScope,
    base_cwd: &Path,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let futs = items.into_iter().map(|item| {
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        async move {
            let out = run_item(&item.agent, &item.task, scope, &run_cwd, ctx).await;
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
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let mut previous = String::new();
    let mut steps: Vec<(String, ToolOutput)> = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let task = substitute_previous(&item.task, &previous);
        let run_cwd = resolve_cwd(base_cwd, item.cwd.as_deref());
        let out = run_item(&item.agent, &task, scope, &run_cwd, ctx).await?;
        let failed = out.is_error;
        previous = out.content.clone();
        steps.push((item.agent, out));
        if failed {
            return Ok(format_chain(&steps, Some(i)));
        }
    }
    Ok(format_chain(&steps, None))
}

/// Build the tool registry a subagent gets: the deny-listed,
/// plugin-free `for_subagent()` set (D-10/D-17), further narrowed by
/// the agent file's own `tools:` allowlist, if it has one. Built
/// entirely through `ToolRegistry`'s public surface (`get` +
/// `register_external`), so no new crate-internal access is needed.
fn subagent_tool_registry(agent: &AgentConfig) -> ToolRegistry {
    let full = ToolRegistry::for_subagent();
    match &agent.tools {
        Some(names) if !names.is_empty() => {
            let mut filtered = ToolRegistry::new();
            for name in names {
                if let Some(canonical) = full.canonical_name(name) {
                    if let Some(tool) = full.get(&canonical) {
                        let _ = filtered.register_external(tool);
                    }
                }
            }
            filtered
        }
        _ => full,
    }
}

/// Dispatch one subagent in-process and wait for its result (D-02).
///
/// Never returns `Err` for a subagent-side failure: a provider error,
/// a hit limit, a cancellation, a missing template or a capacity
/// refusal are all reported as an in-band `ToolOutput` with
/// `metadata.status` naming what happened, so one failing agent never
/// aborts a `parallel`/`chain` batch and never propagates a panic into
/// nanopi's own process (D-11, T-01-12).
async fn run_subagent(
    agent: &AgentConfig,
    task: &str,
    run_cwd: &Path,
    ctx: &ToolContext,
) -> ToolOutput {
    let Some(template) = ctx.registry.template() else {
        return soft_error("subagent runtime not initialised".into());
    };

    let id = ctx.registry.next_id();
    let slot = match ctx.registry.reserve(&id, true) {
        Ok(s) => s,
        Err(e) => return soft_error(e),
    };

    // D-05: foreground when dispatched from a running turn (Esc stops
    // it); otherwise a child of the registry root. Phase 1 dispatches
    // are always foreground in practice, since they come from a live
    // tool call.
    let token = match &ctx.turn_cancel {
        Some(parent) => parent.child_token(),
        None => ctx.registry.background_token(),
    };

    let permit = tokio::select! {
        r = ctx.registry.acquire_permit() => r,
        _ = token.cancelled() => {
            ctx.registry.set_state(&id, AgentState::Cancelled);
            drop(slot);
            return ToolOutput {
                content: "subagent cancelled before it started".into(),
                is_error: true,
                metadata: Some(json!({ "status": "cancelled", "agent_id": id })),
                images: Vec::new(),
            };
        }
    };
    let permit = match permit {
        Ok(p) => p,
        Err(e) => {
            ctx.registry.set_state(&id, AgentState::Failed);
            drop(slot);
            return soft_error(e);
        }
    };

    ctx.registry.set_state(&id, AgentState::Running);

    let sub_registry = subagent_tool_registry(agent);
    let model = agent
        .model
        .clone()
        .unwrap_or_else(|| template.model.clone());
    let provider = (template.provider_factory)(&model);

    let agent_dir = ctx.registry.agents_dir(&template.cwd).join(&id);
    if let Err(e) = std::fs::create_dir_all(&agent_dir) {
        ctx.registry.set_state(&id, AgentState::Failed);
        drop(permit);
        drop(slot);
        return soft_error(format!("failed to create transcript directory: {e}"));
    }
    let session_path = agent_dir.join("transcript.jsonl");

    let system_prompt = if agent.system_prompt.trim().is_empty() {
        None
    } else {
        Some(agent.system_prompt.clone())
    };

    let inputs = AgentBuildInputs {
        cwd: run_cwd.to_path_buf(),
        registry: sub_registry,
        provider,
        session_path,
        session_id: format!("{}-{id}", ctx.registry.run_id()),
        permission: template.permission.clone(),
        hooks: template.hooks.clone(),
        model: model.clone(),
        base_url: template.base_url.clone(),
        api_key: template.api_key.clone(),
        skill_load: SkillLoadPolicy::default(),
        no_context_files: true,
        prompt_overrides: PromptOverrides::from_cli(system_prompt, Vec::new(), true),
        initial_follow_up: None,
        tool_exec_mode: template.tool_exec_mode,
        tool_exec_overrides: template.tool_exec_overrides.clone(),
        extensions: Vec::new(),
    };

    let (mut sub_agent, _diagnostics) = Agent::build_fresh(inputs);
    sub_agent.agent_id = Some(id.clone());
    sub_agent.limits = Some(ctx.registry.limits());
    sub_agent.subagents = Arc::clone(&ctx.registry);
    sub_agent.file_state = Arc::clone(&ctx.file_state);

    let task_owned = task.to_string();
    let registry_for_task = Arc::clone(&ctx.registry);
    let id_for_task = id.clone();
    let token_for_task = token.clone();

    let handle = tokio::spawn(async move {
        // Held for the whole run so a running agent still counts
        // against `max_concurrency` until it finishes (T-01-13).
        let _permit = permit;
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = sub_agent
            .run_turn(&task_owned, &tx, Some(token_for_task), None)
            .await;
        drop(tx);
        let _ = drain.await;
        registry_for_task.add_usage(&id_for_task, &sub_agent.usage_total);
        (result, sub_agent.stop_reason)
    });

    // JoinHandle is always awaited: a panic inside the spawned task
    // (debug/test builds under `panic = "abort"` would already have
    // aborted the whole process, but any other join failure — e.g. the
    // task being dropped — must still resolve to an in-band `failed`
    // result rather than silently vanishing, D-11/RT-08).
    let joined = handle.await;
    drop(slot);

    match joined {
        Ok((Ok(text), Some(StopReason::LimitReached { limit }))) => {
            ctx.registry.set_state(&id, AgentState::LimitReached);
            ToolOutput {
                content: format!("partial result (limit reached: {limit})\n\n{text}"),
                is_error: false,
                metadata: Some(json!({
                    "status": "limit_reached",
                    "agent_id": id,
                    "limit": limit,
                })),
                images: Vec::new(),
            }
        }
        Ok((Ok(_text), Some(StopReason::Cancelled))) => {
            ctx.registry.set_state(&id, AgentState::Cancelled);
            ToolOutput {
                content: "subagent cancelled".into(),
                is_error: true,
                metadata: Some(json!({ "status": "cancelled", "agent_id": id })),
                images: Vec::new(),
            }
        }
        Ok((Ok(text), _)) => {
            ctx.registry.set_state(&id, AgentState::Done);
            ToolOutput {
                content: text,
                is_error: false,
                metadata: Some(json!({ "status": "ok", "agent_id": id })),
                images: Vec::new(),
            }
        }
        Ok((Err(e), _)) => {
            ctx.registry.set_state(&id, AgentState::Failed);
            ToolOutput {
                content: format!("Subagent failed: {e}"),
                is_error: true,
                metadata: Some(json!({
                    "status": "failed",
                    "agent_id": id,
                    "error": e.to_string(),
                })),
                images: Vec::new(),
            }
        }
        Err(join_err) => {
            ctx.registry.set_state(&id, AgentState::Failed);
            ToolOutput {
                content: format!("Subagent task did not complete: {join_err}"),
                is_error: true,
                metadata: Some(json!({ "status": "failed", "agent_id": id })),
                images: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::loop_::{HooksConfig, Provider};
    use crate::agent::permission::PermissionGate;
    use crate::agent::subagent_registry::SubagentRegistry;
    use crate::config::SubagentConfig;
    use crate::event::{AgentEvent, FinishReason, ToolCall, Usage};
    use crate::agent::context::Context;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-sa-{tag}-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Write a user-scope agent definition under `$NANOPI_HOME/agents`.
    fn write_user_agent(home: &Path, name: &str, model: Option<&str>) {
        let dir = home.join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        let model_line = model.map(|m| format!("model: {m}\n")).unwrap_or_default();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: test agent\n{model_line}---\nYou are a test agent.\n"
            ),
        )
        .unwrap();
    }

    fn template(cwd: &Path, factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync>) -> crate::agent::subagent_registry::SpawnTemplate {
        crate::agent::subagent_registry::SpawnTemplate {
            cwd: cwd.to_path_buf(),
            model: "test-model".into(),
            base_url: String::new(),
            api_key: String::new(),
            hooks: HooksConfig::default(),
            permission: PermissionGate::from_cli(true, Some(true)),
            tool_exec_mode: crate::config::ToolExecMode::default(),
            tool_exec_overrides: Default::default(),
            provider_factory: factory,
        }
    }

    fn registry_with(
        cwd: &Path,
        cfg: SubagentConfig,
        factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync>,
    ) -> Arc<SubagentRegistry> {
        let reg = Arc::new(SubagentRegistry::new(cfg));
        reg.set_template(template(cwd, factory));
        reg
    }

    fn ctx_for(cwd: PathBuf, registry: Arc<SubagentRegistry>) -> ToolContext {
        ToolContext {
            cwd,
            registry,
            agent_id: None,
            turn_cancel: None,
            file_state: Arc::new(crate::tool::file_state::FileStateTracker::default()),
        }
    }

    fn default_cfg() -> SubagentConfig {
        SubagentConfig {
            max_concurrency: 4,
            max_live: 8,
            max_turns: 50,
            token_budget: 300_000,
        }
    }

    /// Replies with a fixed text and stops normally.
    struct TextProvider(String);
    #[async_trait::async_trait]
    impl Provider for TextProvider {
        fn id(&self) -> &'static str {
            "text"
        }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
            let _ = tx
                .send(AgentEvent::TextDelta { content_index: 0, text: self.0.clone() })
                .await;
            let _ = tx
                .send(AgentEvent::Done { finish_reason: FinishReason::Stop, usage: Usage::default() })
                .await;
            Ok(Usage::default())
        }
    }

    struct ErrProvider;
    #[async_trait::async_trait]
    impl Provider for ErrProvider {
        fn id(&self) -> &'static str {
            "err"
        }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            Err("boom from provider".into())
        }
    }

    /// Always emits a tool call, never stops naturally — only a turn
    /// limit can end it.
    struct AlwaysToolCallProvider;
    #[async_trait::async_trait]
    impl Provider for AlwaysToolCallProvider {
        fn id(&self) -> &'static str {
            "always-tool-call"
        }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
            let _ = tx
                .send(AgentEvent::ToolCall {
                    content_index: 0,
                    call: ToolCall {
                        id: format!("call_{}", crate::util::uuid::v7()),
                        name: "ls".into(),
                        arguments: json!({"path": "."}),
                    },
                })
                .await;
            let _ = tx
                .send(AgentEvent::Done { finish_reason: FinishReason::ToolCalls, usage: Usage::default() })
                .await;
            Ok(Usage::default())
        }
    }

    /// Never completes until its cancel token fires.
    struct HangingProvider;
    #[async_trait::async_trait]
    impl Provider for HangingProvider {
        fn id(&self) -> &'static str {
            "hanging"
        }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    /// Records concurrently-live calls, bumping/dropping a shared
    /// counter and tracking the observed max.
    struct ConcurrencyProvider {
        live: Arc<std::sync::atomic::AtomicUsize>,
        max_seen: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Provider for ConcurrencyProvider {
        fn id(&self) -> &'static str {
            "concurrency"
        }
        async fn stream_turn(
            &self,
            _ctx: &Context,
            tx: mpsc::Sender<AgentEvent>,
        ) -> Result<Usage, String> {
            use std::sync::atomic::Ordering;
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            let _ = tx.send(AgentEvent::Start { message_id: "m".into() }).await;
            let _ = tx
                .send(AgentEvent::TextDelta { content_index: 0, text: "done".into() })
                .await;
            let _ = tx
                .send(AgentEvent::Done { finish_reason: FinishReason::Stop, usage: Usage::default() })
                .await;
            Ok(Usage::default())
        }
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
    fn select_mode_requires_exactly_one() {
        assert_eq!(select_mode(&json!({"task": "x"})).unwrap(), Mode::Single);
        assert_eq!(select_mode(&json!({"tasks": []})).unwrap(), Mode::Parallel);
        assert_eq!(select_mode(&json!({"chain": []})).unwrap(), Mode::Chain);
        assert!(select_mode(&json!({"agent": "a"})).is_err());
        assert!(select_mode(&json!({"task": "x", "tasks": []})).is_err());
        assert!(select_mode(&json!({"tasks": [], "chain": []})).is_err());
        assert!(select_mode(&json!({"task": "x", "tasks": [], "chain": []})).is_err());
    }

    #[test]
    fn substitute_previous_replaces_all_occurrences() {
        assert_eq!(substitute_previous("a {previous} b", "X"), "a X b");
        assert_eq!(substitute_previous("{previous}{previous}", "X"), "XX");
        assert_eq!(substitute_previous("do {previous}", ""), "do ");
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

    #[test]
    fn format_parallel_aggregates_and_flags_errors() {
        fn ok_output(content: &str) -> ToolOutput {
            ToolOutput {
                content: content.into(),
                is_error: false,
                metadata: Some(json!({"status": "ok"})),
                images: Vec::new(),
            }
        }
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

        let out = format_parallel(&[("a".into(), ok_output("x"))]);
        assert!(!out.is_error);
    }

    #[test]
    fn format_chain_reports_failed_step_and_success() {
        fn ok_output(content: &str) -> ToolOutput {
            ToolOutput {
                content: content.into(),
                is_error: false,
                metadata: Some(json!({"status": "ok"})),
                images: Vec::new(),
            }
        }
        let steps = vec![
            ("a".to_string(), ok_output("one")),
            ("b".to_string(), soft_error("boom".to_string())),
        ];
        let out = format_chain(&steps, Some(1));
        assert!(out.is_error);
        assert!(out.content.contains("Chain stopped at step 2 (b): boom"));
        assert!(out.content.contains("### Step 1 [a] ok"));

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
    async fn single_mode_runs_in_process_and_writes_a_transcript() {
        let _h = crate::TempNanopiHome::new();
        let home = _h.path().to_path_buf();
        write_user_agent(&home, "scout", None);

        let cwd = tmpdir("single");
        let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
            Arc::new(|_model| Box::new(TextProvider("hello from scout".into())));
        let registry = registry_with(&cwd, default_cfg(), factory);
        let ctx = ctx_for(cwd.clone(), registry);

        let tool = SubagentTool;
        let out = tool
            .execute(json!({"agent": "scout", "task": "say hi"}), &ctx)
            .await
            .unwrap();

        assert!(!out.is_error, "{:?}", out);
        assert!(out.content.contains("hello from scout"), "{:?}", out);
        let meta = out.metadata.unwrap();
        assert_eq!(meta["status"], "ok");
        assert_eq!(meta["agent_id"], "a1");

        let transcript = ctx_dir(&ctx).join("a1").join("transcript.jsonl");
        assert!(transcript.exists(), "missing transcript at {transcript:?}");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    fn ctx_dir(ctx: &ToolContext) -> PathBuf {
        ctx.registry.agents_dir(&ctx.cwd)
    }

    #[tokio::test]
    async fn parallel_never_exceeds_max_concurrency() {
        let _h = crate::TempNanopiHome::new();
        let home = _h.path().to_path_buf();
        write_user_agent(&home, "worker", None);

        let cwd = tmpdir("parallel");
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live2 = live.clone();
        let max2 = max_seen.clone();
        let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
            Arc::new(move |_model| {
                Box::new(ConcurrencyProvider {
                    live: live2.clone(),
                    max_seen: max2.clone(),
                })
            });
        let mut cfg = default_cfg();
        cfg.max_concurrency = 1;
        let registry = registry_with(&cwd, cfg, factory);
        let ctx = ctx_for(cwd.clone(), registry);

        let tool = SubagentTool;
        let tasks = vec![
            json!({"agent": "worker", "task": "a"}),
            json!({"agent": "worker", "task": "b"}),
            json!({"agent": "worker", "task": "c"}),
        ];
        let out = tool.execute(json!({"tasks": tasks}), &ctx).await.unwrap();
        assert!(out.content.contains("Parallel: 3/3 succeeded"), "{:?}", out);
        assert_eq!(
            max_seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "max_concurrency=1 must never allow more than one running at once"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn chain_substitutes_previous_and_stops_on_failure() {
        let _h = crate::TempNanopiHome::new();
        let home = _h.path().to_path_buf();
        write_user_agent(&home, "step", None);

        let cwd = tmpdir("chain");
        let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
            Arc::new(|_model| Box::new(TextProvider("step output".into())));
        let registry = registry_with(&cwd, default_cfg(), factory);
        let ctx = ctx_for(cwd.clone(), registry);

        let tool = SubagentTool;
        let chain = vec![
            json!({"agent": "step", "task": "first"}),
            json!({"agent": "step", "task": "then {previous}"}),
        ];
        let out = tool.execute(json!({"chain": chain}), &ctx).await.unwrap();
        assert!(!out.is_error, "{:?}", out);
        assert!(out.content.contains("step output"));

        // Failure stops the chain: second step names an unknown agent.
        let registry2 = registry_with(
            &cwd,
            default_cfg(),
            Arc::new(|_model| Box::new(TextProvider("ok".into()))),
        );
        let ctx2 = ctx_for(cwd.clone(), registry2);
        let chain2 = vec![
            json!({"agent": "__nope__", "task": "first"}),
            json!({"agent": "step", "task": "second"}),
        ];
        let out2 = tool.execute(json!({"chain": chain2}), &ctx2).await.unwrap();
        assert!(out2.is_error);
        assert!(out2.content.contains("Chain stopped at step 1"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn provider_error_is_reported_as_failed() {
        let _h = crate::TempNanopiHome::new();
        let home = _h.path().to_path_buf();
        write_user_agent(&home, "flaky", None);

        let cwd = tmpdir("failed");
        let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
            Arc::new(|_model| Box::new(ErrProvider));
        let registry = registry_with(&cwd, default_cfg(), factory);
        let ctx = ctx_for(cwd.clone(), registry.clone());

        let tool = SubagentTool;
        let out = tool
            .execute(json!({"agent": "flaky", "task": "x"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        let meta = out.metadata.unwrap();
        assert_eq!(meta["status"], "failed");
        assert!(out.content.contains("boom from provider"), "{:?}", out.content);
        assert_eq!(registry.snapshot().len(), 0, "slot must be freed after the run");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn turn_limit_reports_limit_reached() {
        let _h = crate::TempNanopiHome::new();
        let home = _h.path().to_path_buf();
        write_user_agent(&home, "looper", None);

        let cwd = tmpdir("limit");
        let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
            Arc::new(|_model| Box::new(AlwaysToolCallProvider));
        let mut cfg = default_cfg();
        cfg.max_turns = 1;
        let registry = registry_with(&cwd, cfg, factory);
        let ctx = ctx_for(cwd.clone(), registry);

        let tool = SubagentTool;
        let out = tool
            .execute(json!({"agent": "looper", "task": "go"}), &ctx)
            .await
            .unwrap();
        let meta = out.metadata.unwrap();
        assert_eq!(meta["status"], "limit_reached");
        assert_eq!(meta["limit"], "max_turns");
        assert!(out.content.contains("limit reached: max_turns"), "{:?}", out.content);

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn cancelling_the_turn_token_stops_a_hanging_subagent() {
        let _h = crate::TempNanopiHome::new();
        let home = _h.path().to_path_buf();
        write_user_agent(&home, "hangs", None);

        let cwd = tmpdir("cancel");
        let factory: Arc<dyn Fn(&str) -> Box<dyn Provider> + Send + Sync> =
            Arc::new(|_model| Box::new(HangingProvider));
        let registry = registry_with(&cwd, default_cfg(), factory);
        let turn_cancel = tokio_util::sync::CancellationToken::new();
        let ctx = ToolContext {
            cwd: cwd.clone(),
            registry: registry.clone(),
            agent_id: None,
            turn_cancel: Some(turn_cancel.clone()),
            file_state: Arc::new(crate::tool::file_state::FileStateTracker::default()),
        };

        let tool = SubagentTool;
        let run = tokio::spawn(async move {
            tool.execute(json!({"agent": "hangs", "task": "go"}), &ctx)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        turn_cancel.cancel();

        let out = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("subagent must stop within 2s of cancellation")
            .unwrap()
            .unwrap();
        assert!(out.is_error);
        let meta = out.metadata.unwrap();
        assert_eq!(meta["status"], "cancelled");
        assert_eq!(registry.live_count(), 0);

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn max_live_reached_refuses_without_spawning() {
        let _h = crate::TempNanopiHome::new();
        let home = _h.path().to_path_buf();
        write_user_agent(&home, "any", None);

        let cwd = tmpdir("maxlive");
        let mut cfg = default_cfg();
        cfg.max_live = 1;
        let registry = Arc::new(SubagentRegistry::new(cfg));
        registry.set_template(template(
            &cwd,
            Arc::new(|_model| Box::new(TextProvider("x".into()))),
        ));
        // Hold the only slot open.
        let _held = registry.reserve("held", true).unwrap();
        let ctx = ctx_for(cwd.clone(), registry);

        let tool = SubagentTool;
        let out = tool
            .execute(json!({"agent": "any", "task": "x"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("max_live"), "{:?}", out.content);

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn no_template_is_an_in_band_error_not_a_panic() {
        let cwd = tmpdir("notemplate");
        let registry = Arc::new(SubagentRegistry::new(default_cfg()));
        let ctx = ctx_for(cwd.clone(), registry);

        let tool = SubagentTool;
        let out = tool
            .execute(json!({"agent": "whatever", "task": "x"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("subagent runtime not initialised"), "{:?}", out.content);

        std::fs::remove_dir_all(&cwd).ok();
    }

    #[tokio::test]
    async fn parallel_rejects_over_cap() {
        let dir = tmpdir("cap");
        let registry = Arc::new(SubagentRegistry::new(default_cfg()));
        let ctx = ctx_for(dir.clone(), registry);
        let tool = SubagentTool;
        let tasks: Vec<Value> = (0..MAX_TASKS + 1)
            .map(|_| json!({"agent": "x", "task": "y"}))
            .collect();
        let err = tool
            .execute(json!({"tasks": tasks}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn unknown_agent_is_a_soft_error_listing_availability() {
        let dir = tmpdir("unknown");
        let registry = registry_with(
            &dir,
            default_cfg(),
            Arc::new(|_model| Box::new(TextProvider("unused".into()))),
        );
        let ctx = ctx_for(dir.clone(), registry);
        let tool = SubagentTool;
        let out = tool
            .execute(
                json!({"agent": "__definitely_not_an_agent__", "task": "x", "agent_scope": "project"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("Unknown agent"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

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

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::agent::agents::{discover_agents, AgentConfig, AgentScope, AgentSource};
use crate::agent::context::ToolSpec;
use crate::mode::print::JsonEnvelope;
use crate::tool::{ExecutionMode, Tool, ToolContext, ToolError, ToolOutput};

pub struct SubagentTool;

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
                "Delegate a task to a specialized agent that runs in an isolated ",
                "context window (a separate nanopi process). Use this to keep ",
                "large, self-contained subtasks (recon, planning, review) out of ",
                "your own context. Agents are defined as markdown files in ",
                "~/.nanopi/agents (user) or .nanopi/agents (project). The default ",
                "agent_scope is \"user\"; \"project\"/\"both\" require a trusted ",
                "project. Returns the agent's final answer."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "Name of the agent to invoke (its frontmatter `name`)."
                    },
                    "task": {
                        "type": "string",
                        "description": "The task to delegate. Be specific and self-contained: the agent shares none of your context."
                    },
                    "agent_scope": {
                        "type": "string",
                        "enum": ["user", "project", "both"],
                        "description": "Which agent directories to search. Default \"user\"."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the agent process. Defaults to the current directory."
                    }
                },
                "required": ["agent", "task"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("agent must be a string".into()))?
            .to_string();
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("task must be a string".into()))?
            .to_string();
        let scope = parse_scope(&args).map_err(ToolError::InvalidArgs)?;

        // A `cwd` override lets an agent be pointed at a subdirectory,
        // matching PI. Resolve it against the session cwd.
        let run_cwd = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(rel) => {
                let p = PathBuf::from(rel);
                if p.is_absolute() {
                    p
                } else {
                    ctx.cwd.join(rel)
                }
            }
            None => ctx.cwd.clone(),
        };

        let discovery = discover_agents(&run_cwd, scope);
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
            return Ok(ToolOutput {
                content: format!(
                    "Unknown agent {agent_name:?}. Available agents: {available}."
                ),
                is_error: true,
                metadata: None,
                images: Vec::new(),
            });
        };

        // Trust gate: project-sourced agents are repo-controlled prompts
        // that can instruct the model to run bash. Refuse them unless the
        // project is already trusted. User agents are always fine.
        if agent.source == AgentSource::Project
            && !matches!(
                crate::trust::check_trust_status(&run_cwd),
                crate::trust::TrustStatus::AlreadyTrusted
            )
        {
            return Ok(ToolOutput {
                content: format!(
                    "Refusing to run project-local agent {agent_name:?}: this project is not trusted. \
                     Approve it (nanopi -a) or use a user-level agent."
                ),
                is_error: true,
                metadata: None,
                images: Vec::new(),
            });
        }

        run_single(&agent, &task, &run_cwd).await
    }
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
}

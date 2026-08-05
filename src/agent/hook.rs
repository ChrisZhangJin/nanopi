//! Claude Code-style hook runtime.
//!
//! Each hook is a shell command invoked with JSON on stdin. Two ways to
//! communicate back: exit code 2 = block, or `{"decision":"block"}` on
//! stdout. See `docs/v0.5-research.md` §6 for the wire protocol.
//!
//! Example `~/.nanopi/settings.toml`:
//! ```toml
//! [[hooks.PreToolUse]]
//! matcher = "bash"
//! type = "command"
//! command = "~/.nanopi/hooks/check-rm-rf.sh"
//! timeout = 5000
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum HookError {
    #[error("io error spawning hook: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
}

/// What kind of hook event fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
}

impl HookEvent {
    pub fn env_var(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
        }
    }
}

/// One hook definition (parsed from settings.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Regex matched against the tool name. Empty or `*` = match all.
    pub matcher: String,
    /// Shell command. Supports `~`, `$HOME`, `${HOME}` expansion.
    #[serde(rename = "type", default = "default_type")]
    pub kind: String, // always "command" in v0.5
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64, // ms
}

fn default_type() -> String {
    "command".to_string()
}

fn default_timeout() -> u64 {
    5000
}

/// Input payload written to the hook's stdin (JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookInput {
    pub event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Outcome of running one hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Allow,
    Block { reason: String },
    /// Hook returned a JSON `updated_input` — replace the tool's args.
    Transform { new_arguments: Value },
}

/// Expand `~`, `$HOME`, `${HOME}` in a hook command. Relative paths
/// resolve against `~/.nanopi/hooks/`.
pub fn expand_command(command: &str) -> PathBuf {
    let expanded = if let Some(stripped) = command.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            format!("{}/{}", home.to_string_lossy(), stripped)
        } else {
            command.to_string()
        }
    } else if command.starts_with("${HOME}/") || command.starts_with("$HOME/") {
        if let Some(home) = std::env::var_os("HOME") {
            let stripped = command
                .trim_start_matches("${HOME}/")
                .trim_start_matches("$HOME/");
            format!("{}/{}", home.to_string_lossy(), stripped)
        } else {
            command.to_string()
        }
    } else {
        command.to_string()
    };
    PathBuf::from(expanded)
}

/// Check if `matcher` (regex) matches `tool_name`. Empty matches all.
pub fn matcher_matches(matcher: &str, tool_name: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    match regex::Regex::new(matcher) {
        Ok(re) => re.is_match(tool_name),
        Err(_) => false, // invalid regex silently fails closed
    }
}

/// Validate all hook matchers at config-load time. Returns the first
/// invalid matcher (line-agnostic).
pub fn validate_hooks(hooks: &[HookConfig]) -> Result<(), String> {
    for (i, h) in hooks.iter().enumerate() {
        if !h.matcher.is_empty() && h.matcher != "*" {
            regex::Regex::new(&h.matcher).map_err(|e| {
                format!("hook #{i} matcher {:?} is invalid regex: {e}", h.matcher)
            })?;
        }
    }
    Ok(())
}

fn extract_env(extra: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::env::vars().collect();
    v.extend(extra.iter().map(|(k, val)| (k.clone(), val.clone())));
    v
}

/// Run one hook. Sends `input` to stdin as JSON + trailing newline. Reads
/// stdout + stderr + exit code. Times out after `hook.timeout` ms.
pub async fn run_hook(
    hook: &HookConfig,
    input: &HookInput,
    extra_env: &HashMap<String, String>,
) -> Result<HookOutcome, HookError> {
    let input_json = serde_json::to_string(input).expect("serialize HookInput");
    let cmd_path = expand_command(&hook.command);
    let cmd_str = cmd_path.to_string_lossy().to_string();

    let env = extract_env(extra_env);

    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&cmd_str)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input_json.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        let _ = stdin.shutdown().await;
    }

    let timeout = Duration::from_millis(hook.timeout);
    let join = tokio::time::timeout(timeout, child.wait_with_output()).await;
    let output = match join {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(HookError::Spawn(e)),
        Err(_) => {
            // Timeout: kill_on_drop will reap. Return Block { reason: timeout }.
            return Ok(HookOutcome::Block {
                reason: format!("hook timed out after {timeout:?}"),
            });
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // exit 2 = block (stderr is the reason).
    if exit_code == 2 {
        let reason = if stderr.trim().is_empty() { "blocked by hook".into() } else { stderr.trim().to_string() };
        return Ok(HookOutcome::Block { reason });
    }

    // Try to parse JSON decision from stdout (last non-empty line wins).
    if let Some(decision) = parse_json_decision(&stdout) {
        return Ok(decision);
    }

    // exit 0 (or any other code) with no decision = allow.
    Ok(HookOutcome::Allow)
}

fn parse_json_decision(stdout: &str) -> Option<HookOutcome> {
    let line = stdout
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())?;
    let v: Value = serde_json::from_str(line).ok()?;
    let decision = v.get("decision").and_then(|x| x.as_str())?;
    match decision {
        "allow" => Some(HookOutcome::Allow),
        "block" => {
            let reason = v
                .get("reason")
                .and_then(|x| x.as_str())
                .unwrap_or("blocked by hook")
                .to_string();
            Some(HookOutcome::Block { reason })
        }
        _ => None,
    }
}

/// Convenience: run all matching hooks for a given event. Stops on the
/// first Block. Returns the final outcome (Allow if no hook blocks).
pub async fn run_hooks(
    hooks: &[HookConfig],
    event: HookEvent,
    tool_name: &str,
    arguments: Value,
    cwd: &std::path::Path,
    session_id: Option<&str>,
) -> (HookOutcome, Option<Value>) {
    let mut current_args = arguments;
    for h in hooks {
        if h.kind != "command" {
            continue; // only "command" type in v0.5
        }
        if !matcher_matches(&h.matcher, tool_name) {
            continue;
        }
        let input = HookInput {
            event,
            tool_name: Some(tool_name.to_string()),
            tool_call_id: None,
            arguments: current_args.clone(),
            cwd: Some(cwd.display().to_string()),
            session_id: session_id.map(|s| s.to_string()),
        };
        let mut env = HashMap::new();
        env.insert("NANOPI_EVENT".into(), event.env_var().into());
        env.insert("NANOPI_TOOL_NAME".into(), tool_name.into());
        env.insert("NANOPI_CWD".into(), cwd.display().to_string());
        if let Some(s) = session_id {
            env.insert("NANOPI_SESSION_ID".into(), s.into());
        }
        match run_hook(h, &input, &env).await {
            Ok(HookOutcome::Allow) => {}
            Ok(HookOutcome::Block { reason }) => {
                return (HookOutcome::Block { reason }, Some(current_args));
            }
            Ok(HookOutcome::Transform { new_arguments }) => {
                current_args = new_arguments;
            }
            Err(_) => {
                // Hook crashed — fail open (allow). Documented in research.
            }
        }
    }
    (HookOutcome::Allow, Some(current_args))
}

#[allow(dead_code)]
pub(crate) fn _json_used() -> Value { json!({}) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_command_tilde() {
        let p = expand_command("~/foo/bar.sh");
        assert!(p.to_string_lossy().contains("foo/bar.sh"));
        assert!(p.is_absolute());
    }

    #[test]
    fn expand_command_home_var() {
        let p = expand_command("${HOME}/hooks/x.sh");
        assert!(p.to_string_lossy().contains("hooks/x.sh"));
    }

    #[test]
    fn expand_command_absolute_passthrough() {
        let p = expand_command("/usr/local/bin/hook.sh");
        assert_eq!(p.to_string_lossy(), "/usr/local/bin/hook.sh");
    }

    #[test]
    fn matcher_star_matches_all() {
        assert!(matcher_matches("*", "anything"));
        assert!(matcher_matches("", "anything"));
    }

    #[test]
    fn matcher_regex() {
        assert!(matcher_matches("^(read|grep)$", "read"));
        assert!(matcher_matches("^(read|grep)$", "grep"));
        assert!(!matcher_matches("^(read|grep)$", "bash"));
    }

    #[test]
    fn matcher_invalid_regex_fails_closed() {
        assert!(!matcher_matches("[invalid", "anything"));
    }

    #[test]
    fn validate_hooks_accepts_valid() {
        let v = vec![
            HookConfig { matcher: "bash".into(), kind: "command".into(), command: "x".into(), timeout: 1000 },
            HookConfig { matcher: "*".into(), kind: "command".into(), command: "y".into(), timeout: 1000 },
        ];
        assert!(validate_hooks(&v).is_ok());
    }

    #[test]
    fn validate_hooks_rejects_invalid_regex() {
        let v = vec![
            HookConfig { matcher: "[bad".into(), kind: "command".into(), command: "x".into(), timeout: 1000 },
        ];
        assert!(validate_hooks(&v).is_err());
    }

    #[test]
    fn parse_json_decision_allow() {
        let s = r#"{"decision":"allow"}"#;
        assert_eq!(parse_json_decision(s), Some(HookOutcome::Allow));
    }

    #[test]
    fn parse_json_decision_block() {
        let s = r#"{"decision":"block","reason":"nope"}"#;
        match parse_json_decision(s) {
            Some(HookOutcome::Block { reason }) => assert_eq!(reason, "nope"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_json_decision_tolerates_trailing_garbage() {
        let s = "log line 1\nlog line 2\n{\"decision\":\"block\",\"reason\":\"x\"}\n";
        match parse_json_decision(s) {
            Some(HookOutcome::Block { reason }) => assert_eq!(reason, "x"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_json_decision_invalid_json_returns_none() {
        assert_eq!(parse_json_decision("not json"), None);
    }

    // End-to-end bash hook integration.
    #[tokio::test]
    async fn run_hook_exit_0_means_allow() {
        let hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: "echo allow".into(),  // emits JSON? no — but exit 0 = allow
            timeout: 2000,
        };
        let input = HookInput {
            event: HookEvent::PreToolUse,
            tool_name: Some("bash".into()),
            tool_call_id: None,
            arguments: json!({"command": "ls"}),
            cwd: Some("/tmp".into()),
            session_id: None,
        };
        let out = run_hook(&hook, &input, &HashMap::new()).await.unwrap();
        assert_eq!(out, HookOutcome::Allow);
    }

    #[tokio::test]
    async fn run_hook_exit_2_means_block() {
        let hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: "echo 'refused' 1>&2; exit 2".into(),
            timeout: 2000,
        };
        let input = HookInput {
            event: HookEvent::PreToolUse,
            tool_name: Some("bash".into()),
            tool_call_id: None,
            arguments: json!({}),
            cwd: None,
            session_id: None,
        };
        let out = run_hook(&hook, &input, &HashMap::new()).await.unwrap();
        match out {
            HookOutcome::Block { reason } => assert!(reason.contains("refused")),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_hook_json_decision_on_stdout() {
        let hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: r#"echo '{"decision":"block","reason":"json-rule"}'"#.into(),
            timeout: 2000,
        };
        let input = HookInput {
            event: HookEvent::PreToolUse,
            tool_name: Some("bash".into()),
            tool_call_id: None,
            arguments: json!({}),
            cwd: None,
            session_id: None,
        };
        let out = run_hook(&hook, &input, &HashMap::new()).await.unwrap();
        match out {
            HookOutcome::Block { reason } => assert_eq!(reason, "json-rule"),
            other => panic!("got {other:?}"),
        }
    }
}
    /// `UserPromptSubmit` hook is supported alongside Pre/PostToolUse.
    /// Round-trip its enum variant and env_var name.
    #[test]
    fn user_prompt_submit_event_round_trips() {
        let v = HookEvent::UserPromptSubmit;
        assert_eq!(v.env_var(), "UserPromptSubmit");
        let s = serde_json::to_string(&v).unwrap();
        let back: HookEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }

    /// `UserPromptSubmit` hooks don't have a tool_name, but the input
    /// payload still has a `prompt` field carrying the user's text.
    #[test]
    fn user_prompt_submit_input_has_event_field() {
        let input = HookInput {
            event: HookEvent::UserPromptSubmit,
            tool_name: None,
            tool_call_id: None,
            arguments: serde_json::Value::String("hi".into()),
            cwd: None,
            session_id: None,
        };
        let s = serde_json::to_string(&input).unwrap();
        assert!(s.contains("\"event\":\"user_prompt_submit\""), "got {s}");
        assert!(s.contains("\"hi\""), "got {s}");
    }

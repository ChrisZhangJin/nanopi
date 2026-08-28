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
use serde_json::{json, Value};
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
    SessionStart,
    SessionEnd,
    /// Fired once at the top of `run_turn`, BEFORE the user message is
    /// pushed to context. The only new-hook variant that supports
    /// Block (early return) and Transform (rewrite the prompt).
    BeforeAgentStart,
    /// Fired at the top of each agent-loop iteration (the `for` body
    /// in `run_turn`). Advisory only — Block is logged, not enforced.
    TurnStart,
    /// Fired at the bottom of each agent-loop iteration, after tool
    /// calls have been processed. Advisory only.
    TurnEnd,
    /// Fired once after the for-loop completes (all tool rounds done),
    /// just before post-turn compaction. Advisory only.
    MessageEnd,
}

impl HookEvent {
    pub fn env_var(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::BeforeAgentStart => "BeforeAgentStart",
            HookEvent::TurnStart => "TurnStart",
            HookEvent::TurnEnd => "TurnEnd",
            HookEvent::MessageEnd => "MessageEnd",
        }
    }
}

/// One hook definition (parsed from settings.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Regex matched against the tool name (or session_id for session_*).
    /// Empty or `*` = match all. Default when omitted is `"*"`, which is
    /// the useful case for session_start / session_end where there's no
    /// tool name to match.
    #[serde(default = "default_matcher")]
    pub matcher: String,
    /// Shell command. Supports `~`, `$HOME`, `${HOME}` expansion.
    #[serde(rename = "type", default = "default_type")]
    pub kind: String, // always "command" in v0.5
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64, // ms
}

fn default_matcher() -> String {
    "*".to_string()
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
    Block {
        reason: String,
    },
    /// Hook returned a JSON `updated_input` — replace the tool's args.
    Transform {
        new_arguments: Value,
    },
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
            regex::Regex::new(&h.matcher)
                .map_err(|e| format!("hook #{i} matcher {:?} is invalid regex: {e}", h.matcher))?;
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
        let reason = if stderr.trim().is_empty() {
            "blocked by hook".into()
        } else {
            stderr.trim().to_string()
        };
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

    // `decision: "block"` always wins — even if `updated_input` is
    // present, a block hook must refuse the call outright.
    let decision = v.get("decision").and_then(|x| x.as_str());
    if decision == Some("block") {
        let reason = v
            .get("reason")
            .and_then(|x| x.as_str())
            .unwrap_or("blocked by hook")
            .to_string();
        return Some(HookOutcome::Block { reason });
    }

    // `updated_input` (object) → Transform, whether `decision` is
    // "allow", omitted, or unrecognized. v0.9.1 fix: the previous
    // parser only handled "allow" / "block" strings and threw away
    // `updated_input` entirely, so `HookOutcome::Transform` was
    // unreachable from any real hook. Also accept `hookSpecificOutput`
    // as an alias since some Claude-Code-style hooks emit that.
    let updated = v
        .get("updated_input")
        .or_else(|| v.get("hookSpecificOutput"))
        .cloned();
    if let Some(new_arguments) = updated {
        if new_arguments.is_object() {
            return Some(HookOutcome::Transform { new_arguments });
        }
    }

    match decision {
        Some("allow") => Some(HookOutcome::Allow),
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
            Err(e) => {
                // Hook crashed — fail open (allow). Documented in
                // research.md, matches PI's advisory behavior. Say so on
                // stderr: silently degrading to unhooked behavior once
                // cost a CI run with no signal about *why*.
                //
                // This was a `tracing::warn!`, but nothing ever
                // initialized a tracing subscriber, so the message the
                // comment promises went nowhere for its whole life.
                eprintln!(
                    "nanopi: hook errored; failing open (allow) \
                     [tool={tool_name} matcher={} error={e}]",
                    h.matcher
                );
            }
        }
    }
    (HookOutcome::Allow, Some(current_args))
}

/// Run all `session_start` or `session_end` hooks. These don't have a
/// tool_name and their outcome (allow/block) is advisory: a Block just
/// gets logged, not enforced (a session start/end always proceeds).
/// `matcher` on session hooks is applied against the session_id so users
/// can scope by prefix; empty/"*" matches all.
pub async fn run_session_hooks(
    hooks: &[HookConfig],
    event: HookEvent,
    session_id: &str,
    cwd: &std::path::Path,
) {
    debug_assert!(matches!(
        event,
        HookEvent::SessionStart | HookEvent::SessionEnd
    ));
    for h in hooks {
        if h.kind != "command" {
            continue;
        }
        if !matcher_matches(&h.matcher, session_id) {
            continue;
        }
        let input = HookInput {
            event,
            tool_name: None,
            tool_call_id: None,
            arguments: json!({}),
            cwd: Some(cwd.display().to_string()),
            session_id: Some(session_id.to_string()),
        };
        let mut env = HashMap::new();
        env.insert("NANOPI_EVENT".into(), event.env_var().into());
        env.insert("NANOPI_SESSION_ID".into(), session_id.into());
        env.insert("NANOPI_CWD".into(), cwd.display().to_string());
        // Fire and forget: outcome is advisory. Errors are swallowed.
        let _ = run_hook(h, &input, &env).await;
    }
}

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
            HookConfig {
                matcher: "bash".into(),
                kind: "command".into(),
                command: "x".into(),
                timeout: 1000,
            },
            HookConfig {
                matcher: "*".into(),
                kind: "command".into(),
                command: "y".into(),
                timeout: 1000,
            },
        ];
        assert!(validate_hooks(&v).is_ok());
    }

    #[test]
    fn validate_hooks_rejects_invalid_regex() {
        let v = vec![HookConfig {
            matcher: "[bad".into(),
            kind: "command".into(),
            command: "x".into(),
            timeout: 1000,
        }];
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

    /// Regression: pre-v0.9.1 dropped `updated_input` entirely, so a
    /// hook that returned `{"decision":"allow","updated_input":{...}}`
    /// silently produced no Transform. Fixed by extending the parser
    /// to recognize `updated_input` (and `hookSpecificOutput` as an
    /// alias).
    #[test]
    fn parse_json_decision_transform_via_updated_input() {
        let s = r#"{"decision":"allow","updated_input":{"command":"echo TRANSFORMED"}}"#;
        match parse_json_decision(s) {
            Some(HookOutcome::Transform { new_arguments }) => {
                assert_eq!(
                    new_arguments.get("command").and_then(|v| v.as_str()),
                    Some("echo TRANSFORMED")
                );
            }
            other => panic!("expected Transform, got {other:?}"),
        }
    }

    /// `updated_input` without an explicit `decision` still transforms.
    /// Reasonable ergonomics — hook authors shouldn't have to write
    /// `decision:"allow"` alongside a rewrite.
    #[test]
    fn parse_json_decision_updated_input_alone_transforms() {
        let s = r#"{"updated_input":{"command":"safer"}}"#;
        match parse_json_decision(s) {
            Some(HookOutcome::Transform { new_arguments }) => {
                assert_eq!(
                    new_arguments.get("command").and_then(|v| v.as_str()),
                    Some("safer")
                );
            }
            other => panic!("expected Transform, got {other:?}"),
        }
    }

    /// `decision: "block"` overrides even when `updated_input` is
    /// present — a blocking hook must refuse the call outright, not
    /// silently rewrite and allow.
    #[test]
    fn parse_json_decision_block_beats_updated_input() {
        let s = r#"{"decision":"block","reason":"nope","updated_input":{"x":1}}"#;
        match parse_json_decision(s) {
            Some(HookOutcome::Block { reason }) => assert_eq!(reason, "nope"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    /// Non-object `updated_input` (e.g. a string or null) is ignored —
    /// tool arguments are always JSON objects in nanopi's schema.
    #[test]
    fn parse_json_decision_non_object_updated_input_is_ignored() {
        let s = r#"{"updated_input":"oops"}"#;
        assert_eq!(parse_json_decision(s), None);
    }

    /// `hookSpecificOutput` is an alias used by Claude-Code-style
    /// hooks. Same effect as `updated_input`.
    #[test]
    fn parse_json_decision_hook_specific_output_alias() {
        let s = r#"{"hookSpecificOutput":{"command":"aliased"}}"#;
        match parse_json_decision(s) {
            Some(HookOutcome::Transform { new_arguments }) => {
                assert_eq!(
                    new_arguments.get("command").and_then(|v| v.as_str()),
                    Some("aliased")
                );
            }
            other => panic!("expected Transform, got {other:?}"),
        }
    }

    // End-to-end bash hook integration.
    #[tokio::test]
    async fn run_hook_exit_0_means_allow() {
        let hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: "echo allow".into(), // emits JSON? no — but exit 0 = allow
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

    #[test]
    fn session_events_env_var_names() {
        assert_eq!(HookEvent::SessionStart.env_var(), "SessionStart");
        assert_eq!(HookEvent::SessionEnd.env_var(), "SessionEnd");
    }

    #[test]
    fn session_events_serialize_snake_case() {
        let s = serde_json::to_string(&HookEvent::SessionStart).unwrap();
        assert_eq!(s, "\"session_start\"");
        let s = serde_json::to_string(&HookEvent::SessionEnd).unwrap();
        assert_eq!(s, "\"session_end\"");
        // Round-trip.
        let back: HookEvent = serde_json::from_str("\"session_start\"").unwrap();
        assert_eq!(back, HookEvent::SessionStart);
    }

    /// v0.11.0 lifecycle hook env_var names. These are exported as
    /// process env vars (NANOPI_EVENT=<name>) so hook scripts can
    /// branch on event without parsing stdin JSON.
    #[test]
    fn lifecycle_events_env_var_names() {
        assert_eq!(HookEvent::BeforeAgentStart.env_var(), "BeforeAgentStart");
        assert_eq!(HookEvent::TurnStart.env_var(), "TurnStart");
        assert_eq!(HookEvent::TurnEnd.env_var(), "TurnEnd");
        assert_eq!(HookEvent::MessageEnd.env_var(), "MessageEnd");
    }

    /// v0.11.0 lifecycle hooks serialize as snake_case (matching the
    /// `serde(rename_all = "snake_case")` on the enum) and round-trip
    /// through JSON.
    #[test]
    fn lifecycle_events_serialize_snake_case() {
        for (v, expected) in [
            (HookEvent::BeforeAgentStart, "before_agent_start"),
            (HookEvent::TurnStart, "turn_start"),
            (HookEvent::TurnEnd, "turn_end"),
            (HookEvent::MessageEnd, "message_end"),
        ] {
            let s = serde_json::to_string(&v).unwrap();
            assert_eq!(s, format!("\"{expected}\""));
            let back: HookEvent = serde_json::from_str(&s).unwrap();
            assert_eq!(back, v);
        }
    }

    #[tokio::test]
    async fn run_session_hooks_fires_matching_hook() {
        // Use a temp file as a side-channel: the hook writes to it.
        let mut marker = std::env::temp_dir();
        marker.push(format!("nanopi-session-hook-{}", crate::util::uuid::v7()));
        let hook = HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: format!("touch '{}'", marker.display()),
            timeout: 2000,
        };
        run_session_hooks(
            &[hook],
            HookEvent::SessionStart,
            "test-session-id",
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(marker.exists(), "session_start hook should have run");
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn run_session_hooks_matcher_filters_by_session_id() {
        // Matcher "^prod-" should skip a "dev-*" session.
        let mut marker = std::env::temp_dir();
        marker.push(format!(
            "nanopi-session-nomatch-{}",
            crate::util::uuid::v7()
        ));
        let hook = HookConfig {
            matcher: "^prod-".into(),
            kind: "command".into(),
            command: format!("touch '{}'", marker.display()),
            timeout: 2000,
        };
        run_session_hooks(
            &[hook],
            HookEvent::SessionEnd,
            "dev-1234",
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(
            !marker.exists(),
            "hook should NOT fire for non-matching session id"
        );
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

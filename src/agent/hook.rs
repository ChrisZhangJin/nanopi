//! Claude Code-style hook runtime.
//!
//! Each hook is a shell command invoked with JSON on stdin. Two ways to
//! communicate back: exit code 2 = block, or `{"decision":"block"}` on
//! stdout. See `docs/v0.5-research.md` §6 for the wire protocol.
//!
//! Example `~/.nanopi/config.toml`:
//! ```toml
//! [[hooks.tool_execution_start]]
//! matcher = "bash"
//! type = "command"
//! command = "~/.nanopi/hooks/check-rm-rf.sh"
//! timeout = 5000
//! ```
//!
//! The table keys are snake_case (`tool_execution_start`,
//! `tool_execution_end`, `session_start`, ...) — they are `HooksSection`'s
//! field names, and there is no serde rename or alias. A CamelCase
//! `[[hooks.ToolExecutionStart]]` parses as an unrelated key and silently
//! registers nothing; this doc comment claimed otherwise until v0.11.0.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
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
    ToolExecutionStart,
    ToolExecutionEnd,
    Input,
    SessionStart,
    SessionShutdown,
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
    /// Fired inside `compact_now` BEFORE compaction runs.
    /// Mirrors Pi's `session_before_compact`. Advisory — can log or
    /// observe; cannot cancel the compaction (since v0.11; future
    /// versions may add a Cancel hook arm).
    SessionBeforeCompact,
    /// Fired after compaction runs successfully. Mirrors Pi's
    /// `session_compact`. Advisory only.
    SessionCompact,
}

impl HookEvent {
    pub fn env_var(self) -> &'static str {
        match self {
            HookEvent::ToolExecutionStart => "ToolExecutionStart",
            HookEvent::ToolExecutionEnd => "ToolExecutionEnd",
            HookEvent::Input => "Input",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionShutdown => "SessionShutdown",
            HookEvent::BeforeAgentStart => "BeforeAgentStart",
            HookEvent::TurnStart => "TurnStart",
            HookEvent::TurnEnd => "TurnEnd",
            HookEvent::MessageEnd => "MessageEnd",
            HookEvent::SessionBeforeCompact => "SessionBeforeCompact",
            HookEvent::SessionCompact => "SessionCompact",
        }
    }
}

/// One hook definition (parsed from settings.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Regex matched against the tool name (or session_id for session_*).
    /// Empty or `*` = match all. Default when omitted is `"*"`, which is
    /// the useful case for session_start / session_shutdown where there's no
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

/// Expand `~`, `$HOME`, `${HOME}` in a hook command. Anything else —
/// including a relative path — is passed to the shell unchanged, so it
/// resolves against the process cwd.
pub fn expand_command(command: &str) -> PathBuf {
    crate::paths::expand_home(command)
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

/// v0.12.0 §2.3: the four hook keys retired in favor of PI's names have
/// no alias and no dual-key parsing — a config using one of them is a
/// hard load error. `HooksSection` carries `#[serde(deny_unknown_fields)]`
/// so a retired (or simply misspelled) key surfaces as a
/// `toml::de::Error` whose rendered text names the field. This function
/// scans that rendered text for one of the four retired names and, if
/// found, rewrites it into a message that names BOTH the retired key and
/// its replacement, so the user does not have to cross-reference
/// `docs/v0.12-events.md` §2.1 by hand. Any other unknown-field error
/// (e.g. a genuine typo like `turn_startt`) returns `None` — the caller
/// keeps serde's original message, which already lists the valid keys.
pub fn retired_hook_key_error(err: &str) -> Option<String> {
    const RETIRED: [(&str, &str); 4] = [
        ("pre_tool_use", "tool_execution_start"),
        ("post_tool_use", "tool_execution_end"),
        ("user_prompt_submit", "input"),
        ("session_end", "session_shutdown"),
    ];
    for (old, new) in RETIRED {
        if err.contains(&format!("`{old}`")) {
            return Some(format!(
                "unknown hook event \"{old}\" — renamed to \"{new}\" in v0.12 (see docs/v0.12-events.md §2.1)"
            ));
        }
    }
    None
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

    // A hook that never reads its stdin — `exit 0`, a bare `touch`,
    // anything that just wants the NANOPI_* env vars — exits while we
    // are still writing, and the write lands on a closed pipe. That is
    // the hook working exactly as intended, so EPIPE is not an error
    // here: swallow it and go on to collect the exit code, which is
    // what actually decides allow vs block.
    //
    // Propagating it (the `?` this replaced) made every such hook a
    // coin flip between running and "failing open" on a spawn error,
    // depending on whether the child won the race. It reproduced as
    // ~30% flake across the hook tests and would have been far worse
    // in the field, where a blocking guard silently degrading to allow
    // is the whole thing you installed it to prevent.
    if let Some(mut stdin) = child.stdin.take() {
        let wrote = async {
            stdin.write_all(input_json.as_bytes()).await?;
            stdin.write_all(b"\n").await
        }
        .await;
        match wrote {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(HookError::Spawn(e)),
        }
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
///
/// `tool_call_id` is the provider's id for the call in flight, and is
/// `None` for the events that aren't about a tool. It was hardcoded to
/// `None` at both call sites until v0.11.0, which made the payload
/// field permanently null and left `tool_execution_end` hooks unable to
/// correlate a result with the `tool_execution_start` that preceded it.
pub async fn run_hooks(
    hooks: &[HookConfig],
    event: HookEvent,
    tool_name: &str,
    tool_call_id: Option<&str>,
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
            tool_call_id: tool_call_id.map(|s| s.to_string()),
            arguments: current_args.clone(),
            cwd: Some(cwd.display().to_string()),
            session_id: session_id.map(|s| s.to_string()),
        };
        let mut env = HashMap::new();
        env.insert("NANOPI_EVENT".into(), event.env_var().into());
        env.insert("NANOPI_TOOL_NAME".into(), tool_name.into());
        env.insert("NANOPI_CWD".into(), cwd.display().to_string());
        if let Some(id) = tool_call_id {
            env.insert("NANOPI_TOOL_CALL_ID".into(), id.into());
        }
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

/// Run all session-lifecycle hooks. These don't have a tool_name and
/// their outcome is advisory: a Block is reported on stderr, never
/// enforced (a session start/shutdown/compaction always proceeds).
///
/// Three separate things went into one field before v0.12 (`subject`
/// doubling as `session_id`); after v0.12 they are three separate
/// parameters, so no payload field lies:
///
/// | field | value |
/// |---|---|
/// | `session_id` (param) → payload `session_id` + `NANOPI_SESSION_ID` | the real session id, always |
/// | `arguments` (param) → payload `arguments` | `{"reason": ...}` for all four session events |
/// | `subject` → what `matcher` is tested against | session id for `SessionStart` / `SessionShutdown`; compaction reason (`"threshold"`/`"manual"`) for `SessionBeforeCompact` / `SessionCompact` — deliberately still overloaded |
///
/// A hook that used to read `session_id` to get `"threshold"` /
/// `"manual"` for the compaction events must now read
/// `arguments.reason` instead — `session_id` carries the real id there
/// too now.
pub async fn run_session_hooks(
    hooks: &[HookConfig],
    event: HookEvent,
    arguments: Value,
    subject: &str,
    session_id: &str,
    cwd: &std::path::Path,
) {
    // v0.11.0 added the compaction events, which route through here;
    // the old assert covered only start/end and would have panicked a
    // debug build the first time a `session_before_compact` hook fired.
    debug_assert!(matches!(
        event,
        HookEvent::SessionStart
            | HookEvent::SessionShutdown
            | HookEvent::SessionBeforeCompact
            | HookEvent::SessionCompact
    ));
    for h in hooks {
        if h.kind != "command" {
            continue;
        }
        if !matcher_matches(&h.matcher, subject) {
            continue;
        }
        let input = HookInput {
            event,
            tool_name: None,
            tool_call_id: None,
            arguments: arguments.clone(),
            cwd: Some(cwd.display().to_string()),
            session_id: Some(session_id.to_string()),
        };
        let mut env = HashMap::new();
        env.insert("NANOPI_EVENT".into(), event.env_var().into());
        env.insert("NANOPI_SESSION_ID".into(), session_id.into());
        env.insert("NANOPI_CWD".into(), cwd.display().to_string());
        report_advisory(event, &h.matcher, run_hook(h, &input, &env).await);
    }
}

/// Say on stderr that an advisory hook wanted to block, or errored.
///
/// Advisory call sites used to drop the outcome on the floor while
/// their comments promised "Block is logged, not enforced" — nothing
/// logged it, so a hook that blocked (or timed out, which surfaces as
/// Block) was indistinguishable from one that ran clean. Note that a
/// timeout still costs its full `timeout` in wall clock before landing
/// here.
///
/// `eprintln!` rather than `tracing::warn!` on purpose: nothing in this
/// binary initializes a tracing subscriber, so a `warn!` would go
/// nowhere — the same trap `run_hooks`'s error arm already documents.
pub(crate) fn report_advisory(
    event: HookEvent,
    matcher: &str,
    outcome: Result<HookOutcome, HookError>,
) {
    match outcome {
        Ok(o) => report_advisory_outcome(event, o),
        Err(e) => eprintln!(
            "nanopi: {} hook errored [matcher={matcher} error={e}]",
            event.env_var()
        ),
    }
}

/// `report_advisory` for the call sites that go through `run_hooks`,
/// which folds errors into `Allow` itself and hands back one outcome
/// for the whole chain.
pub(crate) fn report_advisory_outcome(event: HookEvent, outcome: HookOutcome) {
    if let HookOutcome::Block { reason } = outcome {
        eprintln!(
            "nanopi: {} hook asked to block; ignored (advisory event) \
             [reason={reason}]",
            event.env_var()
        );
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
    fn retired_hook_key_error_maps_all_four_retired_keys() {
        let cases = [
            ("pre_tool_use", "tool_execution_start"),
            ("post_tool_use", "tool_execution_end"),
            ("user_prompt_submit", "input"),
            ("session_end", "session_shutdown"),
        ];
        for (old, new) in cases {
            let raw = format!("unknown field `{old}`, expected one of `tool_execution_start`, `tool_execution_end`, `input`, `session_start`, `session_shutdown`");
            let mapped = retired_hook_key_error(&raw)
                .unwrap_or_else(|| panic!("expected a mapping for {old}"));
            assert!(mapped.contains(old), "message should name the retired key: {mapped}");
            assert!(mapped.contains(new), "message should name the replacement: {mapped}");
            assert!(
                mapped.contains("v0.12-events.md"),
                "message should point at the spec: {mapped}"
            );
        }
    }

    #[test]
    fn retired_hook_key_error_ignores_unrelated_errors() {
        let raw = "unknown field `turn_startt`, expected one of `tool_execution_start`, `turn_start`, `turn_end`";
        assert_eq!(retired_hook_key_error(raw), None);
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
            event: HookEvent::ToolExecutionStart,
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
            event: HookEvent::ToolExecutionStart,
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
            event: HookEvent::ToolExecutionStart,
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
        assert_eq!(HookEvent::SessionShutdown.env_var(), "SessionShutdown");
    }

    #[test]
    fn session_events_serialize_snake_case() {
        let s = serde_json::to_string(&HookEvent::SessionStart).unwrap();
        assert_eq!(s, "\"session_start\"");
        let s = serde_json::to_string(&HookEvent::SessionShutdown).unwrap();
        assert_eq!(s, "\"session_shutdown\"");
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
            json!({"reason": "startup"}),
            "test-session-id",
            "test-session-id",
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(marker.exists(), "session_start hook should have run");
        let _ = std::fs::remove_file(&marker);
    }

    /// The compaction events route through `run_session_hooks` too, and
    /// its `debug_assert!` originally listed only SessionStart/SessionShutdown
    /// — so the first `session_before_compact` hook to fire would have
    /// panicked any debug build. `matcher` here is tested against the
    /// compaction reason, not a session id.
    #[tokio::test]
    async fn compaction_events_are_accepted_and_match_on_reason() {
        let mut marker = std::env::temp_dir();
        marker.push(format!("nanopi-compact-hook-{}", crate::util::uuid::v7()));
        let hook = HookConfig {
            matcher: "^threshold$".into(),
            kind: "command".into(),
            command: format!("touch '{}'", marker.display()),
            timeout: 2000,
        };
        run_session_hooks(
            &[hook.clone()],
            HookEvent::SessionBeforeCompact,
            json!({"reason": "threshold"}),
            "threshold",
            "real-session-id",
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(marker.exists(), "reason should have matched the matcher");
        let _ = std::fs::remove_file(&marker);

        run_session_hooks(
            &[hook],
            HookEvent::SessionCompact,
            json!({"reason": "manual"}),
            "manual",
            "real-session-id",
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(!marker.exists(), "a different reason must not match");
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
            HookEvent::SessionShutdown,
            json!({"reason": "quit"}),
            "dev-1234",
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
/// `Input` hook is supported alongside ToolExecutionStart/ToolExecutionEnd.
/// Round-trip its enum variant and env_var name.
#[test]
fn input_event_round_trips() {
    let v = HookEvent::Input;
    assert_eq!(v.env_var(), "Input");
    let s = serde_json::to_string(&v).unwrap();
    let back: HookEvent = serde_json::from_str(&s).unwrap();
    assert_eq!(back, v);
}

/// `Input` hooks don't have a tool_name, but the input
/// payload still has a `prompt` field carrying the user's text.
#[test]
fn input_hook_input_has_event_field() {
    let input = HookInput {
        event: HookEvent::Input,
        tool_name: None,
        tool_call_id: None,
        arguments: serde_json::Value::String("hi".into()),
        cwd: None,
        session_id: None,
    };
    let s = serde_json::to_string(&input).unwrap();
    assert!(s.contains("\"event\":\"input\""), "got {s}");
    assert!(s.contains("\"hi\""), "got {s}");
}

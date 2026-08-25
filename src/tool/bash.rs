//! `bash` tool — runs a shell command via `bash -c "<command>"`.
//!
//! Output truncation:
//!   - cap at `max_bytes` (default 30 KB)
//!   - cap at `max_lines` (default 2000 lines)
//!   - overflow is stored at `os::tmpdir()/nanopi-bash-<id>.log` and the
//!     tool output references the path so the model can read it.
//!
//! An overflow file is written ONLY when one of those caps actually
//! fires. Output that fits is returned verbatim and leaves nothing
//! behind in the tmpdir.
//!
//! Timeout: 30 seconds (configurable). On timeout, returns partial output
//! with `is_error: true`.
//!
//! v0.5: bash output goes through stdout+stderr (merged). The full cmd
//! is `bash -c "<command>"` with no shell expansion safeguards.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_BYTES: usize = 30_000;
const DEFAULT_MAX_LINES: usize = 2000;

pub struct BashTool {
    timeout: Duration,
    max_bytes: usize,
    max_lines: usize,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
        }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command via `bash -c`. Returns combined stdout+stderr. Output is truncated at 30 KB / 2000 lines; overflow is saved to a temp file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let cmd = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("command must be a string".into()))?
            .to_string();

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&ctx.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Execution(format!("failed to spawn bash: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("bash stdout not captured".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("bash stderr not captured".into()))?;

        let timeout = self.timeout;
        let max_bytes = self.max_bytes;
        let max_lines = self.max_lines;

        // Race the child against a timeout. Read stdout + stderr + wait
        // INLINE (no `tokio::spawn` for the reader) — a spawned inner
        // task would survive after our future is dropped mid-cancel,
        // leaving `child` alive until the process naturally exited. By
        // keeping `child` on THIS future's stack, a drop propagates:
        // `child` is dropped → `kill_on_drop = true` → SIGKILL. That's
        // what makes Esc cancel a long-running bash command
        // immediately (see `agent/loop_.rs::execute_tool_calls`).
        let (out, err, status) = {
            let mut out = String::new();
            let mut err = String::new();
            let mut stdout_reader = BufReader::new(stdout);
            let mut stderr_reader = BufReader::new(stderr);
            let read_and_wait = async {
                let (_, _, status) = tokio::join!(
                    stdout_reader.read_to_string(&mut out),
                    stderr_reader.read_to_string(&mut err),
                    child.wait(),
                );
                status
            };
            match tokio::time::timeout(timeout, read_and_wait).await {
                Ok(status) => (out, err, status),
                Err(_) => {
                    // Timed out; child dropped when this scope exits →
                    // kill_on_drop terminates it.
                    return Ok(ToolOutput {
                        content: format!("command timed out after {timeout:?}"),
                        is_error: true,
                        images: Vec::new(),
                        metadata: None,
                    });
                }
            }
        };

        let status = status.map_err(|e| ToolError::Execution(format!("waitpid: {e}")))?;
        let mut combined = String::new();
        if !out.is_empty() {
            combined.push_str(&out);
        }
        if !err.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&err);
        }

        // Truncate by lines first, then by bytes, tracking whether
        // either cap actually fired.
        //
        // Do NOT infer truncation by rebuilding the string and comparing
        // lengths against `combined`. `lines()` drops the trailing
        // newline that virtually every command emits, so `join("\n")`
        // comes back at least one byte short even when nothing was
        // dropped — which made `echo hi` report itself as truncated,
        // spill a 3-byte `nanopi-bash-*.log` into the tmpdir, and tell
        // the model to go read a file holding the output it already had.
        let mut lines_iter = combined.lines();
        let head: Vec<&str> = lines_iter.by_ref().take(max_lines).collect();
        let line_capped = lines_iter.next().is_some();

        let mut truncated = if line_capped {
            head.join("\n")
        } else {
            // Nothing dropped — hand back the output verbatim so the
            // trailing newline and any \r\n survive intact.
            combined.clone()
        };

        let byte_capped = truncated.len() > max_bytes;
        if byte_capped {
            // Walk back to a char boundary first: `String::truncate`
            // panics when the index splits a multi-byte character, so
            // >30 KB of CJK or emoji output would take down the agent.
            let mut cut = max_bytes;
            while cut > 0 && !truncated.is_char_boundary(cut) {
                cut -= 1;
            }
            truncated.truncate(cut);
        }

        let overflow_path = if line_capped || byte_capped {
            Some(write_overflow(&combined).await)
        } else {
            None
        };

        let exit_code = status.code().unwrap_or(-1);
        let is_error = !status.success();

        let content = if let Some(path) = &overflow_path {
            format!("{truncated}\n[output truncated; full output at {path}]")
        } else {
            truncated
        };

        Ok(ToolOutput {
            content,
            is_error,
            images: Vec::new(),
            metadata: Some(json!({
                "exit_code": exit_code,
                "stdout_bytes": out.len(),
                "stderr_bytes": err.len(),
                "overflow_path": overflow_path,
            })),
        })
    }
}

async fn write_overflow(content: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!("nanopi-bash-{}.log", crate::util::uuid::v7()));
    let path_str = path.display().to_string();
    let _ = tokio::fs::write(&path, content).await;
    path_str
}

// Ensure Path import doesn't go stale.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-bash-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn runs_simple_command() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = BashTool::new()
            .execute(json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_zero_exit_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = BashTool::new()
            .execute(json!({"command": "exit 7"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn captures_stderr() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = BashTool::new()
            .execute(json!({"command": "echo err 1>&2"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("err"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn truncates_large_output() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = BashTool::new()
            .execute(json!({"command": "yes line | head -n 10000"}), &ctx)
            .await
            .unwrap();
        assert!(
            out.content.len() <= 30_000 + 200,
            "got len {}",
            out.content.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Small output must NOT spill an overflow file, and must not tell
    /// the model its output was truncated. Regression for the tmpdir
    /// filling up with 3-byte `nanopi-bash-*.log` files.
    #[tokio::test]
    async fn small_output_writes_no_overflow_file() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        for cmd in ["echo hi", "printf 'no trailing newline'", "echo 你好世界"] {
            let out = BashTool::new()
                .execute(json!({ "command": cmd }), &ctx)
                .await
                .unwrap();
            let overflow = out
                .metadata
                .as_ref()
                .and_then(|m| m.get("overflow_path").cloned())
                .unwrap_or(Value::Null);
            assert_eq!(
                overflow,
                Value::Null,
                "{cmd:?} spilled an overflow file: {overflow:?}"
            );
            assert!(
                !out.content.contains("[output truncated"),
                "{cmd:?} claimed truncation: {:?}",
                out.content
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Byte-capping must not split a multi-byte character. `String::
    /// truncate` panics when the index is not on a char boundary, so
    /// >30 KB of non-ASCII output could take the whole agent down.
    #[tokio::test]
    async fn byte_cap_does_not_split_utf8() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        // One ASCII byte then 20k x 3-byte chars = 60 KB on a single
        // line: blows the byte cap without hitting the line cap. The
        // leading byte shifts the 30_000 cut off a char boundary — with
        // no offset it lands exactly on one (30000 = 3 x 10000) and the
        // bug hides.
        let out = BashTool::new()
            .execute(
                json!({"command": "printf x; printf '\u{4f60}%.0s' $(seq 1 20000)"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.len() <= 30_000 + 200);
        assert!(!out.content.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn runs_in_ctx_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = BashTool::new()
            .execute(json!({"command": "pwd"}), &ctx)
            .await
            .unwrap();
        assert!(
            out.content.contains(dir.to_str().unwrap()),
            "bash ran in {} but ctx.cwd was {}",
            out.content.trim(),
            dir.display(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_command_arg_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = BashTool::new().execute(json!({}), &ctx).await;
        assert!(matches!(r, Err(ToolError::InvalidArgs(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

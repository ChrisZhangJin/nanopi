//! `bash` tool — runs a shell command via `bash -c "<command>"`.
//!
//! Output truncation:
//!   - cap at `max_bytes` (default 30 KB)
//!   - cap at `max_lines` (default 2000 lines)
//!   - overflow is stored at `os::tmpdir()/nanopi-bash-<id>.log` and the
//!     tool output references the path so the model can read it.
//!
//! Timeout: 30 seconds (configurable). On timeout, returns partial output
//! with `is_error: true`.
//!
//! v0.5: bash output goes through stdout+stderr (merged). The full cmd
//! is `bash -c "<command>"` with no shell expansion safeguards.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
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

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let cmd = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("command must be a string".into()))?
            .to_string();

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Execution(format!("failed to spawn bash: {e}")))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            ToolError::Execution("bash stdout not captured".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ToolError::Execution("bash stderr not captured".into())
        })?;

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

        // Truncate by lines first, then by bytes.
        let truncated_lines = combined
            .lines()
            .take(max_lines)
            .collect::<Vec<_>>()
            .join("\n");
        let mut truncated = truncated_lines;
        if truncated.len() > max_bytes {
            truncated.truncate(max_bytes);
        }

        let overflow_path = if truncated.len() < combined.len() {
            Some(write_overflow(&combined).await)
        } else {
            None
        };

        let exit_code = status.code().unwrap_or(-1);
        let is_error = !status.success();

        let content = if let Some(path) = &overflow_path {
            format!(
                "{truncated}\n[output truncated; full output at {path}]"
            )
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
#[allow(dead_code)]
fn _path_marker(_: &Path) {}

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
        assert!(out.content.len() <= 30_000 + 200, "got len {}", out.content.len());
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
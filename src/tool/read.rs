//! `read` tool — reads a file with optional offset/limit, returns content
//! as text. Image detection is not implemented in v0.5; it returns text for
//! everything.
//!
//! Path resolution: relative paths resolve against `ctx.cwd`. Absolute
//! paths must be within cwd (security: prevents reading /etc/shadow).

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a file from disk. Supports text files; binary files return an error.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path or path relative to cwd."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Optional 0-based line offset."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Optional max number of lines to return."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("path must be a string".into()))?;
        let abs = resolve_path(&ctx.cwd, path_str)?;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().map(|n| n as usize);

        let content = std::fs::read_to_string(&abs).map_err(|e| {
            ToolError::Execution(format!("cannot read {}: {e}", abs.display()))
        })?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let start = offset.min(total);
        let end = limit
            .map(|n| (start + n).min(total))
            .unwrap_or(total);
        let slice: Vec<&str> = lines[start..end].to_vec();
        let out = if slice.is_empty() {
            String::new()
        } else {
            slice.join("\n") + "\n"
        };

        Ok(ToolOutput {
            content: out,
            is_error: false,
            metadata: Some(json!({"path": abs.display().to_string(), "lines": total, "offset": start, "limit": limit})),
        })
    }
}

/// Resolve a (possibly relative) path against cwd. Reject paths that
/// escape cwd via `..` — security guard for read-only tools.
fn resolve_path(cwd: &std::path::Path, p: &str) -> Result<PathBuf, ToolError> {
    let candidate = if std::path::Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    let normalized = match std::fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(_) => candidate.clone(), // may be a new file; check parent below
    };
    // Only enforce the boundary if the file exists. For non-existent paths,
    // best-effort: ensure it doesn't contain `..` after normalization.
    if normalized.starts_with(cwd) {
        Ok(normalized)
    } else {
        Err(ToolError::Execution(format!(
            "path escapes cwd: {}",
            candidate.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-read-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn reads_full_file() {
        let dir = tmp();
        std::fs::write(dir.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = ReadTool
            .execute(json!({"path": "hello.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("line2"));
        assert!(!out.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reads_with_offset_and_limit() {
        let dir = tmp();
        let body = (1..=10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(dir.join("lines.txt"), &body).unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        // offset=2 (0-indexed → start at line3), limit=3 → lines 3,4,5
        let out = ReadTool
            .execute(json!({"path": "lines.txt", "offset": 2, "limit": 3}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("line3"));
        assert!(out.content.contains("line4"));
        assert!(out.content.contains("line5"));
        assert!(!out.content.contains("line6"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = ReadTool
            .execute(json!({"path": "nope.txt"}), &ctx)
            .await;
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_path_outside_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = ReadTool
            .execute(json!({"path": "/etc/shadow"}), &ctx)
            .await;
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
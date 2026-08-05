//! `write` tool — overwrites or creates a file with the given content.
//!
//! Same cwd-boundary check as `read`. Creates parent dirs as needed.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Write content to a file, overwriting if it exists. Creates parent directories as needed.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path or path relative to cwd."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("path must be a string".into()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("content must be a string".into()))?;

        // Reject absolute paths outside cwd.
        let abs = if std::path::Path::new(path_str).is_absolute() {
            if !std::path::Path::new(path_str).starts_with(&ctx.cwd) {
                return Err(ToolError::Execution(format!(
                    "absolute path escapes cwd: {path_str}"
                )));
            }
            PathBuf::from(path_str)
        } else {
            ctx.cwd.join(path_str)
        };

        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ToolError::Execution(format!("cannot create parent {}: {e}", parent.display()))
            })?;
        }

        std::fs::write(&abs, content).map_err(|e| {
            ToolError::Execution(format!("cannot write {}: {e}", abs.display()))
        })?;

        Ok(ToolOutput {
            content: format!("wrote {} bytes to {}", content.len(), abs.display()),
            is_error: false,
            metadata: Some(json!({"path": abs.display().to_string(), "bytes": content.len()})),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-write-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn creates_new_file() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        WriteTool
            .execute(json!({"path": "out.txt", "content": "hello"}), &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn overwrites_existing() {
        let dir = tmp();
        std::fs::write(dir.join("x.txt"), "old").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        WriteTool
            .execute(json!({"path": "x.txt", "content": "new"}), &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("x.txt")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        WriteTool
            .execute(json!({"path": "a/b/c.txt", "content": "x"}), &ctx)
            .await
            .unwrap();
        assert!(dir.join("a/b/c.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_absolute_outside_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = WriteTool
            .execute(json!({"path": "/tmp/nope.txt", "content": "x"}), &ctx)
            .await;
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_path_arg_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = WriteTool.execute(json!({"content": "x"}), &ctx).await;
        assert!(matches!(r, Err(ToolError::InvalidArgs(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
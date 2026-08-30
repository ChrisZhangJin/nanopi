//! `write` tool — overwrites or creates a file with the given content.
//!
//! Refuses to write outside cwd (`tool::resolve_in_cwd`). `read` has no
//! such guard on purpose — it is the mutation that needs bounding, not
//! the reading. Creates parent dirs as needed, but only after the path
//! has been accepted.

use async_trait::async_trait;
use serde_json::{json, Value};

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

        // Reject anything that resolves outside cwd. Checked before the
        // `create_dir_all` below — a rejected path must not leave
        // directories behind outside the tree on its way to being
        // refused.
        let abs = crate::tool::resolve_in_cwd(&ctx.cwd, path_str)
            .map_err(ToolError::Execution)?;

        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ToolError::Execution(format!("cannot create parent {}: {e}", parent.display()))
            })?;
        }

        std::fs::write(&abs, content)
            .map_err(|e| ToolError::Execution(format!("cannot write {}: {e}", abs.display())))?;

        Ok(ToolOutput {
            content: format!("wrote {} bytes to {}", content.len(), abs.display()),
            is_error: false,
            images: Vec::new(),
            metadata: Some(json!({"path": abs.display().to_string(), "bytes": content.len()})),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap(),
            "hello"
        );
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

    /// Regression: the old guard compared raw paths, so an absolute
    /// path that is *textually* prefixed by cwd but climbs out of it
    /// with `..` was accepted and written.
    #[tokio::test]
    async fn rejects_absolute_traversal_out_of_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        // Unique per run: a fixed name would be satisfied by a
        // leftover from an earlier failing run, turning the assertion
        // below into a false pass — or, worse, a false failure.
        let name = format!("escaped-abs-{}.txt", crate::util::uuid::v7());
        let escape = dir.join("..").join(&name);
        let r = WriteTool
            .execute(
                json!({"path": escape.display().to_string(), "content": "x"}),
                &ctx,
            )
            .await;
        assert!(r.is_err(), "traversal via `..` must be refused");
        assert!(
            !dir.parent().unwrap().join(&name).exists(),
            "nothing may be written outside cwd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: relative paths skipped the guard entirely — it only
    /// ran on the absolute branch — so this went straight through
    /// `cwd.join(..)` and wrote outside the tree.
    #[tokio::test]
    async fn rejects_relative_traversal_out_of_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let name = format!("escaped-rel-{}.txt", crate::util::uuid::v7());
        let r = WriteTool
            .execute(json!({"path": format!("../{name}"), "content": "x"}), &ctx)
            .await;
        assert!(r.is_err(), "relative `..` must be refused too");
        assert!(
            !dir.parent().unwrap().join(&name).exists(),
            "nothing may be written outside cwd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlinked directory inside cwd pointing out of it is the case
    /// lexical normalization alone cannot see — the deepest existing
    /// ancestor gets canonicalized precisely to catch this.
    #[tokio::test]
    #[cfg(unix)]
    async fn rejects_write_through_symlinked_dir() {
        let dir = tmp();
        let outside = tmp();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = WriteTool
            .execute(json!({"path": "link/pwned.txt", "content": "x"}), &ctx)
            .await;
        assert!(r.is_err(), "a symlink out of cwd must be refused");
        assert!(
            !outside.join("pwned.txt").exists(),
            "nothing may be written through the symlink"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// The guard must run before `create_dir_all`, or a refused path
    /// still litters directories outside the tree on its way out.
    #[tokio::test]
    async fn refusal_creates_no_directories_outside_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let name = format!("sibling-{}", crate::util::uuid::v7());
        let r = WriteTool
            .execute(
                json!({"path": format!("../{name}/deep/f.txt"), "content": "x"}),
                &ctx,
            )
            .await;
        assert!(r.is_err());
        assert!(
            !dir.parent().unwrap().join(&name).exists(),
            "a refused write must not create directories outside cwd"
        );
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

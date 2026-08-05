//! `edit` tool — replaces `oldText` with `newText` in a file.
//!
//! Errors if `oldText` is not found or matches more than once (ambiguous).

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace exact text in a file. Errors if oldText is not found or appears more than once.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "oldText": {"type": "string", "description": "Exact text to replace."},
                    "newText": {"type": "string", "description": "Replacement text."}
                },
                "required": ["path", "oldText", "newText"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("path must be a string".into()))?;
        let old = args["oldText"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("oldText must be a string".into()))?;
        let new = args["newText"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("newText must be a string".into()))?;

        // Resolve path with cwd boundary check.
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

        let content = std::fs::read_to_string(&abs).map_err(|e| {
            ToolError::Execution(format!("cannot read {}: {e}", abs.display()))
        })?;

        let count = content.matches(old).count();
        if count == 0 {
            return Err(ToolError::Execution("oldText not found in file".into()));
        }
        if count > 1 {
            return Err(ToolError::Execution(format!(
                "oldText is ambiguous: matches {count} locations"
            )));
        }

        let updated = content.replacen(old, new, 1);
        std::fs::write(&abs, &updated).map_err(|e| {
            ToolError::Execution(format!("cannot write {}: {e}", abs.display()))
        })?;

        // Compute a tiny diff metadata (count of removed/added lines).
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();

        Ok(ToolOutput {
            content: format!("edited {}: -{} +{} lines", abs.display(), old_lines.len(), new_lines.len()),
            is_error: false,
            metadata: Some(json!({
                "path": abs.display().to_string(),
                "removed_lines": old_lines.len(),
                "added_lines": new_lines.len(),
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-edit-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn replaces_unique_text() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "hello world\nfoo bar\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        EditTool
            .execute(json!({"path": "f.txt", "oldText": "foo bar", "newText": "baz qux"}), &ctx)
            .await
            .unwrap();
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert!(content.contains("baz qux"));
        assert!(!content.contains("foo bar"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn errors_when_not_found() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = EditTool
            .execute(json!({"path": "f.txt", "oldText": "missing", "newText": "x"}), &ctx)
            .await;
        assert!(matches!(r, Err(ToolError::Execution(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn errors_when_ambiguous() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "foo\nfoo\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = EditTool
            .execute(json!({"path": "f.txt", "oldText": "foo", "newText": "bar"}), &ctx)
            .await;
        match r {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("ambiguous")),
            _ => panic!("expected ambiguous error"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preserves_unrelated_content() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "AAA\nkeep me\nCCC\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        EditTool
            .execute(json!({"path": "f.txt", "oldText": "AAA", "newText": "BBB"}), &ctx)
            .await
            .unwrap();
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(content, "BBB\nkeep me\nCCC\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
//! `edit` tool — replaces `oldText` with `newText` in a file.
//!
//! Errors if `oldText` is not found or matches more than once (ambiguous).
//!
//! Refuses to edit outside cwd (`tool::resolve_in_cwd`), same boundary
//! as `write`.

use async_trait::async_trait;
use serde_json::{json, Value};

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
        let abs = crate::tool::resolve_in_cwd(&ctx.cwd, path_str)
            .map_err(ToolError::Execution)?;

        let content = std::fs::read_to_string(&abs)
            .map_err(|e| ToolError::Execution(format!("cannot read {}: {e}", abs.display())))?;

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
        std::fs::write(&abs, &updated)
            .map_err(|e| ToolError::Execution(format!("cannot write {}: {e}", abs.display())))?;

        // Compute a tiny diff metadata (count of removed/added lines).
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();

        Ok(ToolOutput {
            content: format!(
                "edited {}: -{} +{} lines",
                abs.display(),
                old_lines.len(),
                new_lines.len()
            ),
            is_error: false,
            images: Vec::new(),
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
    use std::path::PathBuf;

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
            .execute(
                json!({"path": "f.txt", "oldText": "foo bar", "newText": "baz qux"}),
                &ctx,
            )
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
            .execute(
                json!({"path": "f.txt", "oldText": "missing", "newText": "x"}),
                &ctx,
            )
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
            .execute(
                json!({"path": "f.txt", "oldText": "foo", "newText": "bar"}),
                &ctx,
            )
            .await;
        match r {
            Err(ToolError::Execution(msg)) => assert!(msg.contains("ambiguous")),
            _ => panic!("expected ambiguous error"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: `edit` carried the same broken guard as `write` —
    /// raw `starts_with`, and only on the absolute branch. Both shapes
    /// of escape are checked here against a real file outside cwd, so
    /// a pass means the edit was refused rather than merely erroring
    /// on a missing file.
    #[tokio::test]
    async fn rejects_traversal_out_of_cwd() {
        let dir = tmp();
        let outside = tmp();
        let target = outside.join("victim.txt");
        std::fs::write(&target, "original\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };

        for path in [
            dir.join("..").join(outside.file_name().unwrap()).join("victim.txt")
                .display()
                .to_string(),
            format!(
                "../{}/victim.txt",
                outside.file_name().unwrap().to_string_lossy()
            ),
        ] {
            let r = EditTool
                .execute(
                    json!({"path": path, "oldText": "original", "newText": "pwned"}),
                    &ctx,
                )
                .await;
            assert!(r.is_err(), "{path} must be refused");
        }

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "original\n",
            "the file outside cwd must be untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn preserves_unrelated_content() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "AAA\nkeep me\nCCC\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        EditTool
            .execute(
                json!({"path": "f.txt", "oldText": "AAA", "newText": "BBB"}),
                &ctx,
            )
            .await
            .unwrap();
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(content, "BBB\nkeep me\nCCC\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

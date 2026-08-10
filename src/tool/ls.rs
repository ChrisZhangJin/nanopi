//! `ls` tool — list directory entries. Read-only, cwd-bounded.
//!
//! Output: one line per entry, dirs suffixed with `/`. Sorted alphabetically
//! with dirs before files. Capped at 1000 entries; overflow noted in
//! metadata.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_ENTRIES: usize = 1000;

pub struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ls".into(),
            description: "List directory contents. Read-only; must be within cwd. Sorted (dirs first). Hidden entries excluded unless all=true.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path (absolute or relative to cwd). Defaults to cwd."
                    },
                    "all": {
                        "type": "boolean",
                        "description": "Include entries starting with '.'"
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"].as_str().unwrap_or(".");
        let show_hidden = args["all"].as_bool().unwrap_or(false);
        let abs = resolve_dir(&ctx.cwd, path_str)?;

        let mut entries: Vec<(bool, String)> = Vec::new();
        let read = std::fs::read_dir(&abs).map_err(|e| {
            ToolError::Execution(format!("cannot list {}: {e}", abs.display()))
        })?;
        for e in read {
            let e = e.map_err(ToolError::Io)?;
            let name = e.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push((is_dir, name));
        }
        // Dirs first, then files. Both alpha-sorted.
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let total = entries.len();
        let truncated = total > MAX_ENTRIES;
        entries.truncate(MAX_ENTRIES);

        let out: String = entries
            .into_iter()
            .map(|(is_dir, n)| if is_dir { format!("{n}/") } else { n })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput {
            content: if out.is_empty() { String::new() } else { out + "\n" },
            is_error: false,
            images: Vec::new(),
            metadata: Some(json!({
                "path": abs.display().to_string(),
                "entries": total,
                "truncated": truncated,
            })),
        })
    }
}

fn resolve_dir(cwd: &Path, p: &str) -> Result<PathBuf, ToolError> {
    let candidate = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    let normalized = std::fs::canonicalize(&candidate).map_err(|e| {
        ToolError::Execution(format!("cannot resolve {}: {e}", candidate.display()))
    })?;
    // v0.9.2: no cwd-escape guard on read-only tools (see tool/read.rs
    // for the rationale — PI / Claude Code don't sandbox these, and
    // bash bypasses them anyway).
    if !normalized.is_dir() {
        return Err(ToolError::Execution(format!(
            "not a directory: {}",
            normalized.display()
        )));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-ls-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    #[tokio::test]
    async fn lists_files_and_dirs_sorted() {
        let dir = tmp();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = LsTool.execute(json!({}), &ctx).await.unwrap();
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines, vec!["subdir/", "a.txt", "b.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hides_dotfiles_by_default() {
        let dir = tmp();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        std::fs::write(dir.join("visible.txt"), "").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = LsTool.execute(json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("visible.txt"));
        assert!(!out.content.contains(".hidden"));
        let out2 = LsTool.execute(json!({"all": true}), &ctx).await.unwrap();
        assert!(out2.content.contains(".hidden"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.9.2 relaxed the read-only sandbox to match PI / Claude Code.
    /// Absolute paths outside cwd now succeed when the target exists
    /// and is a directory.
    #[tokio::test]
    async fn absolute_path_outside_cwd_is_allowed() {
        let cwd = tmp();
        let other = tmp();
        std::fs::write(other.join("marker.txt"), "").unwrap();
        let ctx = ToolContext { cwd: cwd.clone() };
        let out = LsTool
            .execute(json!({"path": other.display().to_string()}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("marker.txt"));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[tokio::test]
    async fn errors_on_non_directory() {
        let dir = tmp();
        std::fs::write(dir.join("file.txt"), "x").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = LsTool.execute(json!({"path": "file.txt"}), &ctx).await;
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

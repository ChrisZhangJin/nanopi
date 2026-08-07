//! `find` tool — recursively find paths whose relative form matches a regex.
//!
//! Read-only, cwd-bounded. Skips a small ignore list (`.git`, `node_modules`,
//! `target`, `.venv`, `dist`, `build`) plus dotfiles at any depth (unless
//! `all=true`). Caps at 1000 results.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_RESULTS: usize = 1000;
const MAX_DEPTH: usize = 32;
const IGNORE_DIRS: &[&str] = &[
    ".git", "node_modules", "target", ".venv", "dist", "build", ".direnv",
];

pub struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "find".into(),
            description: "Recursively find files/dirs whose relative path (from `path`) matches a regex. Read-only; skips .git/node_modules/target and dotfiles by default.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex applied to each relative path (e.g. `\\.rs$` for Rust files)."
                    },
                    "path": {
                        "type": "string",
                        "description": "Base directory (absolute or relative to cwd). Defaults to cwd."
                    },
                    "all": {
                        "type": "boolean",
                        "description": "Include dotfiles/dotdirs and ignored dirs."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("pattern must be a string".into()))?;
        let re = Regex::new(pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
        let base_str = args["path"].as_str().unwrap_or(".");
        let all = args["all"].as_bool().unwrap_or(false);
        let base = resolve_dir(&ctx.cwd, base_str)?;

        let mut results: Vec<String> = Vec::new();
        let mut truncated = false;
        walk(&base, &base, &re, all, 0, &mut results, &mut truncated);

        results.sort();
        let out = if results.is_empty() {
            String::new()
        } else {
            results.join("\n") + "\n"
        };

        Ok(ToolOutput {
            content: out,
            is_error: false,
            images: Vec::new(),
            metadata: Some(json!({
                "base": base.display().to_string(),
                "matches": results.len(),
                "truncated": truncated,
            })),
        })
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    re: &Regex,
    all: bool,
    depth: usize,
    out: &mut Vec<String>,
    truncated: &mut bool,
) {
    if *truncated || depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        if out.len() >= MAX_RESULTS {
            *truncated = true;
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !all {
            if name.starts_with('.') {
                continue;
            }
            if is_dir && IGNORE_DIRS.contains(&name.as_str()) {
                continue;
            }
        }
        let full = e.path();
        let rel = full.strip_prefix(root).unwrap_or(&full);
        let rel_str = rel.to_string_lossy();
        if re.is_match(&rel_str) {
            out.push(rel_str.into_owned());
        }
        if is_dir {
            walk(root, &full, re, all, depth + 1, out, truncated);
        }
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
    let cwd_canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if !normalized.starts_with(&cwd_canon) {
        return Err(ToolError::Execution(format!(
            "path escapes cwd: {}",
            candidate.display()
        )));
    }
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
        p.push(format!("nanopi-find-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    #[tokio::test]
    async fn finds_files_by_extension() {
        let dir = tmp();
        std::fs::write(dir.join("a.rs"), "").unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("c.rs"), "").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = FindTool
            .execute(json!({"pattern": "\\.rs$"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("a.rs"));
        assert!(out.content.contains(&format!("sub{}c.rs", std::path::MAIN_SEPARATOR)));
        assert!(!out.content.contains("b.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_ignored_dirs() {
        let dir = tmp();
        std::fs::create_dir(dir.join("target")).unwrap();
        std::fs::write(dir.join("target").join("junk.rs"), "").unwrap();
        std::fs::write(dir.join("keep.rs"), "").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = FindTool
            .execute(json!({"pattern": "\\.rs$"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("keep.rs"));
        assert!(!out.content.contains("junk.rs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_regex_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = FindTool.execute(json!({"pattern": "["}), &ctx).await;
        assert!(matches!(r, Err(ToolError::InvalidArgs(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn all_true_includes_dotfiles() {
        let dir = tmp();
        std::fs::write(dir.join(".hidden.rs"), "").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = FindTool
            .execute(json!({"pattern": "\\.rs$", "all": true}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains(".hidden.rs"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

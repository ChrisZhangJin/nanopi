//! `grep` tool — recursively search file contents for a regex.
//!
//! Read-only, cwd-bounded. Skips binary files (detected by NUL byte in the
//! first 4 KB). Same ignore list as `find`. Output is `path:line:content`
//! per match, capped at 500 matches; overflow noted in metadata.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::{Value, json};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_MATCHES: usize = 500;
const MAX_DEPTH: usize = 32;
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024; // skip files > 5 MB
const IGNORE_DIRS: &[&str] = &[
    ".git", "node_modules", "target", ".venv", "dist", "build", ".direnv",
];

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Recursively search file contents for a regex. Read-only; skips binary files, .git/node_modules/target, and files > 5 MB. Output format: path:line:content.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex to match against each line."
                    },
                    "path": {
                        "type": "string",
                        "description": "Base directory or single file (absolute or relative to cwd). Defaults to cwd."
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Case-insensitive match."
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
        let ci = args["case_insensitive"].as_bool().unwrap_or(false);
        let all = args["all"].as_bool().unwrap_or(false);
        // `unicode(false)` uses ASCII case-folding, which is enough for
        // our line-oriented use case and doesn't require the `regex`
        // crate's optional `unicode-case` feature.
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ci)
            .unicode(false)
            .build()
            .map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
        let base_str = args["path"].as_str().unwrap_or(".");
        let base = resolve_path_within(&ctx.cwd, base_str)?;
        let root = if base.is_dir() { base.clone() } else {
            base.parent().unwrap_or(Path::new(".")).to_path_buf()
        };

        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut files_scanned = 0usize;

        if base.is_file() {
            grep_file(&root, &base, &re, &mut matches, &mut truncated, &mut files_scanned);
        } else {
            walk(&root, &base, &re, all, 0, &mut matches, &mut truncated, &mut files_scanned);
        }

        let out = if matches.is_empty() {
            String::new()
        } else {
            matches.join("\n") + "\n"
        };

        Ok(ToolOutput {
            content: out,
            is_error: false,
            images: Vec::new(),
            metadata: Some(json!({
                "base": base.display().to_string(),
                "matches": matches.len(),
                "files_scanned": files_scanned,
                "truncated": truncated,
            })),
        })
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    re: &regex::Regex,
    all: bool,
    depth: usize,
    out: &mut Vec<String>,
    truncated: &mut bool,
    files_scanned: &mut usize,
) {
    if *truncated || depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        if *truncated {
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
        if is_dir {
            walk(root, &full, re, all, depth + 1, out, truncated, files_scanned);
        } else {
            grep_file(root, &full, re, out, truncated, files_scanned);
        }
    }
}

fn grep_file(
    root: &Path,
    file: &Path,
    re: &regex::Regex,
    out: &mut Vec<String>,
    truncated: &mut bool,
    files_scanned: &mut usize,
) {
    if *truncated {
        return;
    }
    let Ok(meta) = file.metadata() else { return };
    if meta.len() > MAX_FILE_BYTES {
        return;
    }
    let Ok(bytes) = std::fs::read(file) else { return };
    if is_binary(&bytes) {
        return;
    }
    *files_scanned += 1;
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return,
    };
    let rel = file.strip_prefix(root).unwrap_or(file);
    let rel_str = rel.to_string_lossy();
    for (i, line) in text.lines().enumerate() {
        if re.is_match(line) {
            if out.len() >= MAX_MATCHES {
                *truncated = true;
                return;
            }
            out.push(format!("{}:{}:{}", rel_str, i + 1, line));
        }
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    let n = bytes.len().min(4096);
    bytes[..n].contains(&0)
}

fn resolve_path_within(cwd: &Path, p: &str) -> Result<PathBuf, ToolError> {
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
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-grep-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    #[tokio::test]
    async fn finds_matches_in_files() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "hello world\nfoo bar\nHELLO again\n").unwrap();
        std::fs::write(dir.join("b.txt"), "nothing here\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "hello"}), &ctx)
            .await
            .unwrap();
        // Case-sensitive: only line 1 matches.
        assert!(out.content.contains("a.txt:1:hello world"));
        assert!(!out.content.contains("HELLO"));
        assert!(!out.content.contains("b.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn case_insensitive_matches_both() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "hello\nHELLO\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "hello", "case_insensitive": true}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("a.txt:1:hello"));
        assert!(out.content.contains("a.txt:2:HELLO"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = tmp();
        // NUL byte in first 4KB → treated as binary.
        std::fs::write(dir.join("bin.dat"), b"hello\x00world").unwrap();
        std::fs::write(dir.join("txt.txt"), "hello world").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "hello"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("txt.txt"));
        assert!(!out.content.contains("bin.dat"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_regex_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = GrepTool.execute(json!({"pattern": "["}), &ctx).await;
        assert!(matches!(r, Err(ToolError::InvalidArgs(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn single_file_target() {
        let dir = tmp();
        std::fs::write(dir.join("single.txt"), "foo\nbar\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "bar", "path": "single.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("single.txt:2:bar"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

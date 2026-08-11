//! AGENTS.md / CLAUDE.md context-file loading.
//!
//! Ported from PI's `loadProjectContextFiles`
//! (`pi/packages/coding-agent/src/core/resource-loader.ts:70-156`) and
//! the `<project_context>` injection in `buildSystemPrompt`
//! (`.../core/system-prompt.ts:144-152`).
//!
//! Behavior (matching PI):
//! - Per directory, the first existing file among
//!   `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD` wins.
//! - The GLOBAL context file (in the nanopi home dir, `~/.nanopi`) is
//!   loaded first, then every ancestor from filesystem-root down to the
//!   cwd. Ordering is `[global, root-most, …, cwd]` so more-specific
//!   (closer-to-cwd) instructions appear last and take precedence in the
//!   model's reading.
//! - Duplicate paths are loaded once.
//!
//! Deliberately deferred vs PI: the git-worktree "shadowed file" dedup
//! (`findShadowedContextFile`) that avoids double-loading the SAME
//! tracked file when a linked worktree shadows the main repo's copy.
//! That path needs git plumbing and only causes a duplicate load, never
//! wrong content — see the memory roadmap. Path-based dedup here already
//! collapses the common cases.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Candidate filenames, tried in order within a directory. On
/// case-insensitive filesystems the `.MD` variants collapse onto the
/// `.md` ones; on Linux they're distinct, so we keep PI's full list.
const CANDIDATES: [&str; 4] = ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

/// One loaded context file: its absolute path and full text.
#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// Load the first existing context file in `dir`, or `None`.
/// Symlinks-to-files are fine; directories named `AGENTS.md` are skipped.
fn load_from_dir(dir: &Path) -> Option<ContextFile> {
    for name in CANDIDATES {
        let path = dir.join(name);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    return Some(ContextFile { path, content });
                }
            }
            _ => {}
        }
    }
    None
}

/// Load global + ancestor context files for `cwd`.
///
/// `agent_dir` is the nanopi home directory (`~/.nanopi`) whose context
/// file, if any, is the global one loaded first. Pass `None` to skip the
/// global scope (e.g. home dir unresolvable).
///
/// Returns `[global?, root-most-ancestor, …, cwd]` with duplicate paths
/// removed, matching PI's order.
pub fn load_project_context_files(cwd: &Path, agent_dir: Option<&Path>) -> Vec<ContextFile> {
    let mut files: Vec<ContextFile> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // Global scope first.
    if let Some(dir) = agent_dir {
        if let Some(cf) = load_from_dir(dir) {
            seen.insert(cf.path.clone());
            files.push(cf);
        }
    }

    // Walk cwd → root, collecting; then reverse so root-most is first and
    // the cwd's own file lands last (most specific, read last).
    let mut ancestors: Vec<ContextFile> = Vec::new();
    let mut current = Some(cwd.to_path_buf());
    while let Some(dir) = current {
        if let Some(cf) = load_from_dir(&dir) {
            if !seen.contains(&cf.path) {
                seen.insert(cf.path.clone());
                ancestors.push(cf);
            }
        }
        current = dir.parent().map(Path::to_path_buf);
    }
    ancestors.reverse();
    files.extend(ancestors);

    files
}

/// Render the loaded context files as the `<project_context>` block that
/// gets appended to the system prompt. Returns an empty string when there
/// are no files (so callers can append unconditionally).
///
/// Format matches PI (`system-prompt.ts:144-152`).
pub fn format_context_files(files: &[ContextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut s =
        String::from("\n\n<project_context>\n\nProject-specific instructions and guidelines:\n\n");
    for cf in files {
        s.push_str(&format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
            cf.path.display(),
            cf.content
        ));
    }
    s.push_str("</project_context>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-ctx-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn none_when_no_context_file() {
        let dir = tmpdir();
        assert!(load_from_dir(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agents_md_wins_over_claude_md() {
        let dir = tmpdir();
        std::fs::write(dir.join("AGENTS.md"), "from agents").unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "from claude").unwrap();
        let cf = load_from_dir(&dir).unwrap();
        assert_eq!(cf.content, "from agents");
        assert!(cf.path.ends_with("AGENTS.md"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claude_md_used_when_no_agents_md() {
        let dir = tmpdir();
        std::fs::write(dir.join("CLAUDE.md"), "from claude").unwrap();
        let cf = load_from_dir(&dir).unwrap();
        assert_eq!(cf.content, "from claude");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ancestor_order_is_root_first_cwd_last() {
        // <root>/AGENTS.md and <root>/sub/AGENTS.md — cwd = <root>/sub.
        let root = tmpdir();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root-level").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "sub-level").unwrap();

        let files = load_project_context_files(&sub, None);
        assert_eq!(files.len(), 2);
        // root-most first, cwd last.
        assert_eq!(files[0].content, "root-level");
        assert_eq!(files[1].content, "sub-level");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn global_scope_loads_first() {
        let home = tmpdir();
        let proj = tmpdir();
        std::fs::write(home.join("AGENTS.md"), "global").unwrap();
        std::fs::write(proj.join("AGENTS.md"), "project").unwrap();

        let files = load_project_context_files(&proj, Some(&home));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].content, "global");
        assert_eq!(files[1].content, "project");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&proj).ok();
    }

    #[test]
    fn same_path_loaded_once() {
        // agent_dir == cwd: the one file must not appear twice.
        let dir = tmpdir();
        std::fs::write(dir.join("AGENTS.md"), "only-once").unwrap();
        let files = load_project_context_files(&dir, Some(&dir));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "only-once");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn format_empty_is_empty_string() {
        assert_eq!(format_context_files(&[]), "");
    }

    #[test]
    fn format_wraps_each_file_with_path() {
        let files = vec![
            ContextFile {
                path: PathBuf::from("/a/AGENTS.md"),
                content: "hello".into(),
            },
            ContextFile {
                path: PathBuf::from("/b/CLAUDE.md"),
                content: "world".into(),
            },
        ];
        let out = format_context_files(&files);
        assert!(out.contains("<project_context>"));
        assert!(out.contains("</project_context>"));
        assert!(out.contains(
            "<project_instructions path=\"/a/AGENTS.md\">\nhello\n</project_instructions>"
        ));
        assert!(out.contains(
            "<project_instructions path=\"/b/CLAUDE.md\">\nworld\n</project_instructions>"
        ));
    }
}

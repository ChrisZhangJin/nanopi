//! Best-effort git branch name lookup — no libgit2 dep, just read HEAD.
//!
//! Walks upward from `cwd` looking for a `.git` directory. If found,
//! reads `.git/HEAD`:
//!   - `ref: refs/heads/<branch>`  → returns the branch
//!   - a raw 40-char hex sha       → detached; returns Some("HEAD@<sha7>")
//! Returns None if we're not in a git repo or the file is unreadable.
//!
//! This is called on each status-line render, so keep it cheap — no
//! subprocess, single small file read.

use std::path::Path;

pub fn branch_of(cwd: &Path) -> Option<String> {
    let git_dir = find_git_dir(cwd)?;
    let head_path = git_dir.join("HEAD");
    let contents = std::fs::read_to_string(&head_path).ok()?;
    let line = contents.trim();
    if let Some(refname) = line.strip_prefix("ref: refs/heads/") {
        Some(refname.to_string())
    } else if line.len() >= 7 && line.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("HEAD@{}", &line[..7]))
    } else {
        None
    }
}

/// Walk upward from `start` looking for `.git`. Handles both a normal
/// repo (`.git` is a directory) and a worktree (`.git` is a text file
/// containing `gitdir: <path>`).
fn find_git_dir(start: &Path) -> Option<std::path::PathBuf> {
    let mut cur = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    loop {
        let candidate = cur.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            // .git is a text pointer (worktree). Read the redirect.
            let s = std::fs::read_to_string(&candidate).ok()?;
            let target = s.trim().strip_prefix("gitdir: ")?;
            return Some(std::path::PathBuf::from(target));
        }
        if !cur.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("nanopi-git-{}", crate::util::uuid::v7()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_head(dir: &Path, contents: &str) {
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let mut f = std::fs::File::create(git.join("HEAD")).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn reads_branch_name() {
        let td = TempDir::new();
        write_head(&td.0, "ref: refs/heads/feature/xyz\n");
        assert_eq!(branch_of(&td.0).as_deref(), Some("feature/xyz"));
    }

    #[test]
    fn detached_head_returns_short_sha() {
        let td = TempDir::new();
        write_head(&td.0, "1234567abcdef1234567abcdef1234567abcdef0\n");
        let b = branch_of(&td.0);
        assert_eq!(b.as_deref(), Some("HEAD@1234567"));
    }

    #[test]
    fn no_repo_returns_none() {
        let td = TempDir::new();
        assert!(branch_of(&td.0).is_none());
    }

    #[test]
    fn walks_up_to_parent_repo() {
        let td = TempDir::new();
        write_head(&td.0, "ref: refs/heads/main\n");
        let sub = td.0.join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(branch_of(&sub).as_deref(), Some("main"));
    }
}

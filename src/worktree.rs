//! Git worktree primitive for isolated background agent runs (ISO-01, ISO-02).
//!
//! Self-contained: no dependency on `agent_registry` or dispatch. Shells
//! out to the `git` CLI only (no new crates), per CONTEXT's discretion.
//! All arguments are passed as argv to `std::process::Command`, never
//! through a shell, so there is no injection surface even though
//! `run`/`id` ultimately come from the registry (T-04-04).

use std::path::{Path, PathBuf};
use std::process::Command;

/// A created worktree: its path, branch, the base commit it was created
/// from, and the root of the repo it belongs to.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub repo_root: PathBuf,
}

/// Outcome of [`finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeOutcome {
    /// No changes relative to base: worktree and branch removed.
    Removed,
    /// Changes merged cleanly into the main tree; worktree and branch removed.
    Merged { branch: String },
    /// Merge conflicted (or the main tree was dirty and blocked the
    /// merge); the merge was aborted, and the worktree + branch are
    /// kept so nothing is lost.
    Conflict {
        branch: String,
        path: PathBuf,
        detail: String,
    },
    /// A git command failed in a way that isn't a merge conflict.
    Error(String),
}

impl Worktree {
    /// A line for the agent's report describing what happened.
    pub fn report_line(&self, outcome: &WorktreeOutcome) -> String {
        match outcome {
            WorktreeOutcome::Removed => {
                format!("worktree: no changes, removed {}", self.branch)
            }
            WorktreeOutcome::Merged { branch } => {
                format!("worktree: merged {branch} into main tree")
            }
            WorktreeOutcome::Conflict {
                branch,
                path,
                detail,
            } => {
                format!(
                    "worktree: CONFLICT merging {branch} — merge aborted, branch and worktree kept at {} ({detail}); ask the user how to proceed",
                    path.display()
                )
            }
            WorktreeOutcome::Error(e) => format!("worktree: error — {e}"),
        }
    }
}

/// Detect whether `dir` is inside a git repo with `git` available on
/// PATH. Returns the repo root, or `None` if `git` is missing or `dir`
/// is not inside a repo — callers should warn and ignore isolation in
/// that case (D-08).
pub fn detect(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Reject run/id components that could escape the intended worktree
/// path or be used to inject extra git arguments (T-04-04). Ids are
/// registry-generated, but this is defence in depth.
fn validate_component(s: &str, label: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if s.contains('/') || s.contains("..") || s.chars().any(char::is_whitespace) {
        return Err(format!(
            "{label} {s:?} must not contain '/', '..' or whitespace"
        ));
    }
    Ok(())
}

fn git_stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

/// Create a worktree at `root/.nanopi/worktrees/<run>-<id>` on branch
/// `nanopi/<run>/<id>`, based on current HEAD (D-09).
pub fn create(root: &Path, run: &str, id: &str) -> Result<Worktree, String> {
    validate_component(run, "run")?;
    validate_component(id, "id")?;

    let base_output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if !base_output.status.success() {
        return Err(format!(
            "git rev-parse HEAD failed: {}",
            git_stderr(&base_output)
        ));
    }
    let base = String::from_utf8_lossy(&base_output.stdout).trim().to_string();

    let branch = format!("nanopi/{run}/{id}");
    let dir_name = format!("{run}-{id}");
    let path = root.join(".nanopi").join("worktrees").join(&dir_name);

    let add_output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&branch)
        .arg(&path)
        .arg(&base)
        .output()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if !add_output.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            git_stderr(&add_output)
        ));
    }

    Ok(Worktree {
        path,
        branch,
        base,
        repo_root: root.to_path_buf(),
    })
}

/// Finish a worktree: commit any outstanding changes, then either
/// remove it (unchanged), merge it into the main tree (clean merge),
/// or keep it with a conflict report (D-10, D-11, Addendum 3).
pub fn finish(wt: &Worktree, agent_id: &str) -> WorktreeOutcome {
    // Step 1: commit any outstanding changes in the worktree.
    let status_output = match Command::new("git")
        .arg("-C")
        .arg(&wt.path)
        .arg("status")
        .arg("--porcelain")
        .output()
    {
        Ok(o) => o,
        Err(e) => return WorktreeOutcome::Error(format!("failed to spawn git: {e}")),
    };
    if !status_output.status.success() {
        return WorktreeOutcome::Error(format!(
            "git status failed: {}",
            git_stderr(&status_output)
        ));
    }
    let dirty = !String::from_utf8_lossy(&status_output.stdout)
        .trim()
        .is_empty();

    if dirty {
        let add = Command::new("git")
            .arg("-C")
            .arg(&wt.path)
            .arg("add")
            .arg("-A")
            .output();
        match add {
            Ok(o) if o.status.success() => {}
            Ok(o) => return WorktreeOutcome::Error(format!("git add failed: {}", git_stderr(&o))),
            Err(e) => return WorktreeOutcome::Error(format!("failed to spawn git: {e}")),
        }

        let commit = Command::new("git")
            .arg("-C")
            .arg(&wt.path)
            .arg("commit")
            .arg("-m")
            .arg(format!("nanopi agent {agent_id}"))
            .output();
        match commit {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return WorktreeOutcome::Error(format!("git commit failed: {}", git_stderr(&o)))
            }
            Err(e) => return WorktreeOutcome::Error(format!("failed to spawn git: {e}")),
        }
    }

    // Step 2: count commits ahead of base.
    let rev_list = match Command::new("git")
        .arg("-C")
        .arg(&wt.path)
        .arg("rev-list")
        .arg(format!("{}..{}", wt.base, wt.branch))
        .arg("--count")
        .output()
    {
        Ok(o) => o,
        Err(e) => return WorktreeOutcome::Error(format!("failed to spawn git: {e}")),
    };
    if !rev_list.status.success() {
        return WorktreeOutcome::Error(format!(
            "git rev-list failed: {}",
            git_stderr(&rev_list)
        ));
    }
    let ahead: u64 = String::from_utf8_lossy(&rev_list.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    if ahead == 0 {
        return remove_worktree_and_branch(wt);
    }

    // Step 3: attempt merge into the main tree.
    let merge = Command::new("git")
        .arg("-C")
        .arg(&wt.repo_root)
        .arg("merge")
        .arg("--no-edit")
        .arg(&wt.branch)
        .output();
    match merge {
        Ok(o) if o.status.success() => match remove_worktree_and_branch(wt) {
            WorktreeOutcome::Removed | WorktreeOutcome::Error(_) => WorktreeOutcome::Merged {
                branch: wt.branch.clone(),
            },
            other => other,
        },
        Ok(o) => {
            let detail = git_stderr(&o);
            // Abort whatever merge state resulted (ignore error if no
            // merge was actually started, e.g. dirty-main-tree case).
            let _ = Command::new("git")
                .arg("-C")
                .arg(&wt.repo_root)
                .arg("merge")
                .arg("--abort")
                .output();
            WorktreeOutcome::Conflict {
                branch: wt.branch.clone(),
                path: wt.path.clone(),
                detail,
            }
        }
        Err(e) => WorktreeOutcome::Error(format!("failed to spawn git: {e}")),
    }
}

fn remove_worktree_and_branch(wt: &Worktree) -> WorktreeOutcome {
    let remove = Command::new("git")
        .arg("-C")
        .arg(&wt.repo_root)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(&wt.path)
        .output();
    match remove {
        Ok(o) if o.status.success() => {}
        Ok(o) => return WorktreeOutcome::Error(format!("worktree remove failed: {}", git_stderr(&o))),
        Err(e) => return WorktreeOutcome::Error(format!("failed to spawn git: {e}")),
    }

    let branch_del = Command::new("git")
        .arg("-C")
        .arg(&wt.repo_root)
        .arg("branch")
        .arg("-D")
        .arg(&wt.branch)
        .output();
    match branch_del {
        Ok(o) if o.status.success() => WorktreeOutcome::Removed,
        Ok(o) => WorktreeOutcome::Error(format!("branch delete failed: {}", git_stderr(&o))),
        Err(e) => WorktreeOutcome::Error(format!("failed to spawn git: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// tempdir, `git init`, local user.name/email, one commit.
    fn init_fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "nanopi-test"]);
        run(&["config", "user.email", "nanopi-test@example.com"]);
        fs::write(dir.path().join("README.md"), "init\n").expect("write README");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn detect_returns_root_inside_repo() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        let root = detect(repo.path()).expect("should detect repo");
        // Resolve symlinks (macOS /tmp -> /private/tmp) before comparing.
        let expected = fs::canonicalize(repo.path()).expect("canonicalize");
        let actual = fs::canonicalize(&root).expect("canonicalize");
        assert_eq!(actual, expected);
    }

    #[test]
    fn detect_returns_none_outside_repo() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let non_repo = tempfile::tempdir().expect("tempdir");
        assert_eq!(detect(non_repo.path()), None);
    }

    #[test]
    fn create_makes_path_and_branch() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        let wt = create(repo.path(), "r1", "a1").expect("create worktree");
        assert_eq!(
            wt.path,
            repo.path().join(".nanopi").join("worktrees").join("r1-a1")
        );
        assert_eq!(wt.branch, "nanopi/r1/a1");
        assert!(wt.path.exists());

        let branch_list = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("branch")
            .arg("--list")
            .arg("nanopi/r1/a1")
            .output()
            .expect("git branch --list");
        assert!(!String::from_utf8_lossy(&branch_list.stdout).trim().is_empty());
    }

    #[test]
    fn create_rejects_bad_run_or_id() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        assert!(create(repo.path(), "r/1", "a1").is_err());
        assert!(create(repo.path(), "r1", "..").is_err());
        assert!(create(repo.path(), "r1", "a 1").is_err());
    }

    #[test]
    fn unchanged_worktree_removed_with_branch() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        let wt = create(repo.path(), "r2", "a1").expect("create worktree");
        let outcome = finish(&wt, "a1");
        assert_eq!(outcome, WorktreeOutcome::Removed);
        assert!(!wt.path.exists());
        let branch_list = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("branch")
            .arg("--list")
            .arg(&wt.branch)
            .output()
            .expect("git branch --list");
        assert!(String::from_utf8_lossy(&branch_list.stdout).trim().is_empty());
    }

    #[test]
    fn committed_then_reverted_counts_as_changed() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        let wt = create(repo.path(), "r3", "a1").expect("create worktree");

        // Add a file and commit it.
        fs::write(wt.path.join("new.txt"), "temp\n").expect("write file");
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&wt.path)
                .args(args)
                .output()
                .expect("spawn git")
        };
        run(&["add", "-A"]);
        let out = run(&["commit", "-q", "-m", "add"]);
        assert!(out.status.success());

        // Revert the tree to net-empty while keeping the commit.
        fs::remove_file(wt.path.join("new.txt")).expect("remove file");
        run(&["add", "-A"]);
        let out = run(&["commit", "-q", "-m", "revert"]);
        assert!(out.status.success());

        // Net-empty tree, but 1+ commits ahead of base => treated as
        // changed (Addendum 3): finish merges rather than removing.
        let outcome = finish(&wt, "a1");
        match outcome {
            WorktreeOutcome::Merged { .. } => {}
            other => panic!("expected Merged, got {other:?}"),
        }
    }

    #[test]
    fn merge_no_conflict() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        let wt = create(repo.path(), "r4", "a1").expect("create worktree");

        fs::write(wt.path.join("feature.txt"), "feature\n").expect("write file");

        let outcome = finish(&wt, "a1");
        assert_eq!(
            outcome,
            WorktreeOutcome::Merged {
                branch: wt.branch.clone()
            }
        );
        assert!(repo.path().join("feature.txt").exists());
        assert!(!wt.path.exists());
        let branch_list = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("branch")
            .arg("--list")
            .arg(&wt.branch)
            .output()
            .expect("git branch --list");
        assert!(String::from_utf8_lossy(&branch_list.stdout).trim().is_empty());
    }

    #[test]
    fn merge_conflict_keeps_both() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        let wt = create(repo.path(), "r5", "a1").expect("create worktree");

        // Edit the same line in the worktree.
        fs::write(wt.path.join("README.md"), "worktree change\n").expect("write file");

        // Edit + commit the same line in main tree.
        fs::write(repo.path().join("README.md"), "main change\n").expect("write file");
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .expect("spawn git")
        };
        run(&["add", "-A"]);
        let out = run(&["commit", "-q", "-m", "main edit"]);
        assert!(out.status.success());

        let outcome = finish(&wt, "a1");
        match &outcome {
            WorktreeOutcome::Conflict { branch, path, .. } => {
                assert_eq!(branch, &wt.branch);
                assert_eq!(path, &wt.path);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        // No merge in progress in main tree.
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("status")
            .output()
            .expect("git status");
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(!status_text.contains("You have unmerged paths"));
        // Worktree and branch still exist.
        assert!(wt.path.exists());
        let branch_list = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("branch")
            .arg("--list")
            .arg(&wt.branch)
            .output()
            .expect("git branch --list");
        assert!(!String::from_utf8_lossy(&branch_list.stdout).trim().is_empty());

        let report = wt.report_line(&outcome);
        assert!(report.contains(&wt.branch));
        assert!(report.contains(&wt.path.display().to_string()));
    }

    #[test]
    fn dirty_main_tree_blocks_merge_treated_as_conflict() {
        if !git_available() {
            eprintln!("skip: git not available");
            return;
        }
        let repo = init_fixture_repo();
        let wt = create(repo.path(), "r6", "a1").expect("create worktree");

        // Touch the same tracked file the dirty main tree has uncommitted,
        // so git refuses to merge rather than silently merging an
        // unrelated addition.
        fs::write(wt.path.join("README.md"), "worktree change\n").expect("write file");

        // Dirty, uncommitted change in main tree that blocks the merge.
        fs::write(repo.path().join("README.md"), "dirty uncommitted\n").expect("write file");

        let outcome = finish(&wt, "a1");
        match &outcome {
            WorktreeOutcome::Conflict { branch, .. } => {
                assert_eq!(branch, &wt.branch);
            }
            other => panic!("expected Conflict (dirty main tree), got {other:?}"),
        }
        assert!(wt.path.exists());
    }
}

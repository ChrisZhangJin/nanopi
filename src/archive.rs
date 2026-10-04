//! On-disk side of the agent archive (ARC-01, ARC-03, ARC-04): run-id
//! format, durable state in brief.md front-matter, derived index.md,
//! startup interrupted-marking, run liveness, and `.gitignore`
//! registration (D-01, D-05, D-06, D-07).

use std::io;
use std::path::{Path, PathBuf};

use crate::agent::brief::{brief_write_lock, front_matter_get, set_front_matter_field};
use crate::tool::file_state::atomic_write;

/// States that no longer count as "live" (D-05). `waiting_permission` is
/// accepted by the parser and treated as live, but nothing sets it in the
/// child-process architecture because RT-07 resolves permissions at
/// dispatch time and a child process never prompts interactively.
pub const TERMINAL_STATES: &[&str] = &["done", "failed", "stopped", "limit_reached", "interrupted"];

/// True if `s` is one of [`TERMINAL_STATES`].
pub fn is_terminal_state(s: &str) -> bool {
    TERMINAL_STATES.contains(&s)
}

/// New run id: `YYYYMMDD-HHMMSS-<8 hex>`. The 8 hex chars are the LAST 8
/// hex digits of a UUIDv7 (its leading chars are timestamp-derived, so the
/// random tail is what gives us uniqueness here).
pub fn new_run_id() -> String {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let full = uuid::Uuid::now_v7().simple().to_string();
    let tail: String = full.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{ts}-{tail}")
}

/// Parse the `YYYYMMDD-HHMMSS` prefix of a run id into a local naive
/// datetime. `None` if the prefix doesn't parse.
pub fn run_started(run_id: &str) -> Option<chrono::NaiveDateTime> {
    let prefix: String = run_id.chars().take(15).collect();
    chrono::NaiveDateTime::parse_from_str(&prefix, "%Y%m%d-%H%M%S").ok()
}

/// Rewrite only the `state` front-matter field of `agent_dir/brief.md`,
/// atomically, under `brief_write_lock` so it can never race a concurrent
/// amendment append. Missing brief.md is not an error (nothing to
/// update). After the rewrite, the parent run dir's `index.md` is
/// regenerated.
pub fn set_agent_state(agent_dir: &Path, state: &str) -> io::Result<()> {
    let brief_path = agent_dir.join("brief.md");
    {
        let _guard = brief_write_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let content = match std::fs::read_to_string(&brief_path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if let Some(updated) = set_front_matter_field(&content, "state", state) {
            atomic_write(&brief_path, updated.as_bytes())?;
        }
    }
    if let Some(run_dir) = agent_dir.parent() {
        regenerate_index(run_dir)?;
    }
    Ok(())
}

fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Numeric suffix of an agent dir name like `a12` -> `12`, used only for
/// sort ordering; non-matching names sort last, in lexical order among
/// themselves.
fn agent_sort_key(name: &str) -> (u64, String) {
    if let Some(rest) = name.strip_prefix('a') {
        if let Ok(n) = rest.parse::<u64>() {
            return (n, String::new());
        }
    }
    (u64::MAX, name.to_string())
}

/// Rebuild `run_dir/index.md` from the `brief.md` front-matter of every
/// child agent directory. Fully derived — never read before being
/// overwritten. Created mode 0600 on unix.
pub fn regenerate_index(run_dir: &Path) -> io::Result<()> {
    let run_id = run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort_by_key(|n| agent_sort_key(n));
        for name in names {
            let brief_path = run_dir.join(&name).join("brief.md");
            let Ok(content) = std::fs::read_to_string(&brief_path) else {
                continue;
            };
            // Phase 1 legacy briefs without front-matter are skipped, not
            // crashed on.
            let id = match front_matter_get(&content, "id") {
                Some(v) => v,
                None => continue,
            };
            let state = front_matter_get(&content, "state").unwrap_or_default();
            let role = front_matter_get(&content, "role").unwrap_or_default();
            let started = front_matter_get(&content, "started").unwrap_or_default();
            let _ = &name;
            rows.push((id, state, role, started));
        }
    }

    let mut out = format!("# Run {run_id}\n\n| id | state | role | started |\n| --- | --- | --- | --- |\n");
    for (id, state, role, started) in &rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            escape_cell(id),
            escape_cell(state),
            escape_cell(role),
            escape_cell(started)
        ));
    }

    let index_path = run_dir.join("index.md");
    if !index_path.exists() {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        // Create empty so atomic_write's perm-copy picks up 0600; the
        // content is replaced immediately below.
        let _ = opts.open(&index_path)?;
    }
    atomic_write(&index_path, out.as_bytes())
}

/// Write `run.pid` in `run_dir` with this process's pid.
pub fn write_run_pid(run_dir: &Path) -> io::Result<()> {
    let pid = std::process::id();
    atomic_or_create(&run_dir.join("run.pid"), pid.to_string().as_bytes())
}

fn atomic_or_create(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if !path.exists() {
        std::fs::write(path, bytes)?;
        return Ok(());
    }
    atomic_write(path, bytes)
}

/// Whether `run_dir/run.pid` names a live process. Missing or garbled
/// file -> false.
pub fn run_is_live(run_dir: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(run_dir.join("run.pid")) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<u32>() else {
        return false;
    };
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        // Signal 0: existence check only, no signal delivered.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Mark every non-terminal agent brief in every run other than
/// `current_run` (and not currently live) as `interrupted` (D-06). Never
/// spawns anything. Returns the count rewritten. Missing `agents_root` ->
/// `Ok(0)`.
pub fn mark_interrupted(agents_root: &Path, current_run: &str) -> io::Result<usize> {
    let mut count = 0usize;
    let entries = match std::fs::read_dir(agents_root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let run_dir = entry.path();
        if !run_dir.is_dir() {
            continue;
        }
        let run_name = entry.file_name().to_string_lossy().into_owned();
        if run_name == current_run {
            continue;
        }
        if run_is_live(&run_dir) {
            continue;
        }
        let agent_entries = match std::fs::read_dir(&run_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for agent_entry in agent_entries.filter_map(|e| e.ok()) {
            let agent_dir = agent_entry.path();
            if !agent_dir.is_dir() {
                continue;
            }
            let brief_path = agent_dir.join("brief.md");
            let Ok(content) = std::fs::read_to_string(&brief_path) else {
                continue;
            };
            let Some(state) = front_matter_get(&content, "state") else {
                // Legacy brief without front-matter: skip, not crash.
                continue;
            };
            if is_terminal_state(&state) {
                continue;
            }
            set_agent_state(&agent_dir, "interrupted")?;
            count += 1;
        }
    }
    Ok(count)
}

/// The `.nanopi/agents/` entry forms that already count as "covered" in a
/// `.gitignore`, keyed by the relative prefix (empty at the repo root).
fn covered_forms(rel_prefix: &str) -> Vec<String> {
    let base = if rel_prefix.is_empty() {
        ".nanopi/agents".to_string()
    } else {
        format!("{rel_prefix}/.nanopi/agents")
    };
    let parent = if rel_prefix.is_empty() {
        ".nanopi".to_string()
    } else {
        format!("{rel_prefix}/.nanopi")
    };
    vec![
        format!("{base}/"),
        base.clone(),
        format!("/{base}/"),
        format!("/{base}"),
        format!("{parent}/"),
        parent.clone(),
        format!("/{parent}/"),
        format!("/{parent}"),
    ]
}

/// Find the git repo root (dir containing a `.git` dir or file) by
/// walking up from `cwd`, at most 64 ancestors. Never uses
/// `std::env::current_dir`; the caller supplies `cwd`.
fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    for _ in 0..64 {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => return None,
        }
    }
    None
}

/// Register `<rel>/.nanopi/agents/` in the enclosing git repo's
/// `.gitignore` (D-07). No-op outside a git repo or when an equivalent
/// entry is already present. Append-only — never truncates an existing
/// file. Returns `Ok(true)` iff a line was appended.
pub fn ensure_gitignore(cwd: &Path) -> io::Result<bool> {
    let Some(repo_root) = find_repo_root(cwd) else {
        return Ok(false);
    };
    let rel_prefix = match cwd.strip_prefix(&repo_root) {
        Ok(rel) if rel.as_os_str().is_empty() => String::new(),
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => String::new(),
    };
    let entry = if rel_prefix.is_empty() {
        ".nanopi/agents/".to_string()
    } else {
        format!("{rel_prefix}/.nanopi/agents/")
    };

    let gitignore_path = repo_root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    let covered = covered_forms(&rel_prefix);
    let already = existing
        .lines()
        .any(|line| covered.iter().any(|c| line.trim() == c));
    if already {
        return Ok(false);
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o644);
    }
    use std::io::Write;
    let mut f = opts.open(&gitignore_path)?;
    let needs_leading_newline = !existing.is_empty() && !existing.ends_with('\n');
    if needs_leading_newline {
        f.write_all(b"\n")?;
    }
    f.write_all(entry.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::brief::{render_brief_with_meta, BriefMeta, BriefSpec};
    use std::path::Path;

    fn write_brief(dir: &Path, state: &str, id: &str, role: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let spec = BriefSpec {
            task: "t".into(),
            role: Some(role.into()),
            tools: vec![],
            model: None,
        };
        let meta = BriefMeta {
            id: id.into(),
            state: state.into(),
            started: "2026-10-04T10:00:00+08:00".into(),
            parent: "run".into(),
        };
        std::fs::write(dir.join("brief.md"), render_brief_with_meta(&spec, &meta)).unwrap();
    }

    #[test]
    fn new_run_id_matches_format() {
        let id = new_run_id();
        let re_ok = {
            let parts: Vec<&str> = id.splitn(3, '-').collect();
            parts.len() == 3
                && parts[0].len() == 8
                && parts[0].chars().all(|c| c.is_ascii_digit())
                && parts[1].len() == 6
                && parts[1].chars().all(|c| c.is_ascii_digit())
                && parts[2].len() == 8
                && parts[2].chars().all(|c| c.is_ascii_hexdigit())
        };
        assert!(re_ok, "{id}");
        assert!(run_started(&id).is_some());
    }

    #[test]
    fn terminal_states_classification() {
        for s in TERMINAL_STATES {
            assert!(is_terminal_state(s));
        }
        assert!(!is_terminal_state("queued"));
        assert!(!is_terminal_state("running"));
        assert!(!is_terminal_state("waiting_permission"));
    }

    #[test]
    fn set_agent_state_rewrites_state_and_regenerates_index() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run1");
        let agent_dir = run_dir.join("a1");
        write_brief(&agent_dir, "queued", "a1", "reviewer");

        set_agent_state(&agent_dir, "running").unwrap();

        let content = std::fs::read_to_string(agent_dir.join("brief.md")).unwrap();
        assert_eq!(front_matter_get(&content, "state"), Some("running".to_string()));
        // rest of brief unchanged
        assert!(front_matter_get(&content, "role") == Some("reviewer".to_string()));

        let index = std::fs::read_to_string(run_dir.join("index.md")).unwrap();
        assert!(index.contains("a1"));
        assert!(index.contains("running"));
    }

    #[test]
    fn set_agent_state_missing_brief_is_ok_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("run1").join("a1");
        std::fs::create_dir_all(&agent_dir).unwrap();
        assert!(set_agent_state(&agent_dir, "running").is_ok());
        assert!(!agent_dir.join("brief.md").exists());
    }

    #[test]
    fn index_mode_is_0600_on_unix_and_fully_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run1");
        write_brief(&run_dir.join("a1"), "running", "a1", "r1");
        write_brief(&run_dir.join("a2"), "done", "a2", "r2");
        regenerate_index(&run_dir).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(run_dir.join("index.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        // delete a2's dir, regenerate: its row must be gone
        std::fs::remove_dir_all(run_dir.join("a2")).unwrap();
        regenerate_index(&run_dir).unwrap();
        let index = std::fs::read_to_string(run_dir.join("index.md")).unwrap();
        assert!(index.contains("a1"));
        assert!(!index.contains("a2"));
    }

    #[test]
    fn legacy_brief_without_front_matter_is_skipped_not_crashed() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run1");
        let legacy_dir = run_dir.join("a1");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("brief.md"), "# Brief\n\nno front matter\n").unwrap();
        // must not panic
        regenerate_index(&run_dir).unwrap();
        let index = std::fs::read_to_string(run_dir.join("index.md")).unwrap();
        assert!(!index.contains("no front matter"));
    }

    #[test]
    fn write_and_check_run_pid_liveness() {
        let tmp = tempfile::tempdir().unwrap();
        write_run_pid(tmp.path()).unwrap();
        assert!(run_is_live(tmp.path()));
    }

    #[test]
    fn run_is_live_false_for_missing_or_garbled() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!run_is_live(tmp.path()));
        std::fs::write(tmp.path().join("run.pid"), "not-a-pid").unwrap();
        assert!(!run_is_live(tmp.path()));
    }

    #[test]
    fn mark_interrupted_rewrites_non_terminal_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let run_a = root.join("run-a");
        write_brief(&run_a.join("a1"), "queued", "a1", "r");
        write_brief(&run_a.join("a2"), "running", "a2", "r");
        write_brief(&run_a.join("a3"), "waiting_permission", "a3", "r");
        write_brief(&run_a.join("a4"), "done", "a4", "r");

        let current_run = "run-current";
        std::fs::create_dir_all(root.join(current_run)).unwrap();
        write_brief(&root.join(current_run).join("a1"), "running", "a1", "r");

        let count = mark_interrupted(root, current_run).unwrap();
        assert_eq!(count, 3);

        for id in ["a1", "a2", "a3"] {
            let c = std::fs::read_to_string(run_a.join(id).join("brief.md")).unwrap();
            assert_eq!(front_matter_get(&c, "state"), Some("interrupted".to_string()));
        }
        let done = std::fs::read_to_string(run_a.join("a4").join("brief.md")).unwrap();
        assert_eq!(front_matter_get(&done, "state"), Some("done".to_string()));

        // current run untouched
        let cur = std::fs::read_to_string(root.join(current_run).join("a1").join("brief.md")).unwrap();
        assert_eq!(front_matter_get(&cur, "state"), Some("running".to_string()));

        // index of run-a regenerated
        assert!(run_a.join("index.md").exists());
    }

    #[test]
    fn mark_interrupted_skips_live_run() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let run_b = root.join("run-b");
        write_brief(&run_b.join("a1"), "running", "a1", "r");
        write_run_pid(&run_b).unwrap();

        let count = mark_interrupted(root, "run-current").unwrap();
        assert_eq!(count, 0);
        let c = std::fs::read_to_string(run_b.join("a1").join("brief.md")).unwrap();
        assert_eq!(front_matter_get(&c, "state"), Some("running".to_string()));
    }

    #[test]
    fn mark_interrupted_missing_root_is_ok_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(mark_interrupted(&missing, "run").unwrap(), 0);
    }

    #[test]
    fn mark_interrupted_skips_legacy_briefs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let run_a = root.join("run-a");
        let legacy = run_a.join("a1");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("brief.md"), "no front matter here\n").unwrap();
        // must not panic/crash
        let count = mark_interrupted(root, "run-current").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn gitignore_noop_outside_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let result = ensure_gitignore(tmp.path()).unwrap();
        assert!(!result);
        assert!(!tmp.path().join(".gitignore").exists());
    }

    #[test]
    fn gitignore_appends_once_then_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        // existing file with no trailing newline
        std::fs::write(tmp.path().join(".gitignore"), "node_modules").unwrap();

        let first = ensure_gitignore(tmp.path()).unwrap();
        assert!(first);
        let content = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(content.contains("node_modules\n.nanopi/agents/\n"), "{content}");

        let second = ensure_gitignore(tmp.path()).unwrap();
        assert!(!second);
        let content2 = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(content, content2);
    }

    #[test]
    fn gitignore_existing_covering_lines_count_as_present() {
        for existing in ["/.nanopi/agents/", ".nanopi/agents", ".nanopi/", "/.nanopi/"] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
            std::fs::write(tmp.path().join(".gitignore"), format!("{existing}\n")).unwrap();
            let result = ensure_gitignore(tmp.path()).unwrap();
            assert!(!result, "existing form {existing} should count as covered");
        }
    }

    #[test]
    fn gitignore_subdir_appends_relative_entry_to_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let result = ensure_gitignore(&sub).unwrap();
        assert!(result);
        assert!(!sub.join(".gitignore").exists());
        let content = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(content.contains("sub/.nanopi/agents/"), "{content}");
    }

    #[test]
    fn gitignore_git_as_file_counts_as_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        // worktree/submodule: .git is a file, not a dir
        std::fs::write(tmp.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        let result = ensure_gitignore(tmp.path()).unwrap();
        assert!(result);
        assert!(tmp.path().join(".gitignore").exists());
    }
}

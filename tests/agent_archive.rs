//! Phase 02 end-to-end proof of the archive lifecycle (ARC-04/05, D-06,
//! D-07, D-09, D-10) using only the public `nanopi` lib API and tempfile
//! dirs — no real `nanopi -p` process is spawned here (see
//! `tests/agent_runtime.rs` for that style).

use nanopi::agent::brief::{render_brief_with_meta, BriefMeta, BriefSpec};
use nanopi::archive;

/// Build a `YYYYMMDD-HHMMSS-<8hex>` run id whose timestamp is `days` days
/// in the past (D-01 format, required by `is_run_id_shaped`/`run_started`).
fn run_id_days_ago(days: i64) -> String {
    let dt = chrono::Local::now() - chrono::Duration::days(days);
    format!("{}-deadbeef", dt.format("%Y%m%d-%H%M%S"))
}

fn write_brief(agent_dir: &std::path::Path, id: &str, state: &str, run_id: &str) {
    std::fs::create_dir_all(agent_dir).unwrap();
    let spec = BriefSpec {
        task: "do the thing".into(),
        role: Some("worker".into()),
        tools: vec![],
        model: None,
    };
    let meta = BriefMeta {
        id: id.into(),
        state: state.into(),
        started: "2026-10-04T10:00:00+08:00".into(),
        parent: run_id.into(),
    };
    std::fs::write(agent_dir.join("brief.md"), render_brief_with_meta(&spec, &meta)).unwrap();
}

/// D-06: a run left `running` on disk (no `run.pid`, so it's not live) is
/// rewritten `interrupted` on the next startup scan and is never re-run
/// (no report.md or transcript.jsonl ever gets created by `mark_interrupted`
/// — it only ever touches brief.md/index.md). A second call is a no-op.
#[test]
fn stale_running_agent_marked_interrupted_not_rerun() {
    let tmp = tempfile::tempdir().unwrap();
    let agents_root = tmp.path().join("agents");
    let stale_run = "20200101-000000-deadbeef";
    let agent_dir = agents_root.join(stale_run).join("agent1");
    write_brief(&agent_dir, "agent1", "running", stale_run);

    let current_run = "20991231-235959-cafef00d";
    let count = archive::mark_interrupted(&agents_root, current_run).unwrap();
    assert_eq!(count, 1);

    let brief = std::fs::read_to_string(agent_dir.join("brief.md")).unwrap();
    assert_eq!(
        nanopi::agent::brief::front_matter_get(&brief, "state"),
        Some("interrupted".to_string())
    );

    let index = std::fs::read_to_string(agents_root.join(stale_run).join("index.md")).unwrap();
    assert!(index.contains("interrupted"), "index.md: {index}");

    // Never re-run: mark_interrupted only ever touches brief.md/index.md.
    assert!(!agent_dir.join("report.md").exists());
    assert!(!agent_dir.join("transcript.jsonl").exists());

    // Idempotent: the state is now terminal, so a second pass rewrites
    // nothing.
    let count2 = archive::mark_interrupted(&agents_root, current_run).unwrap();
    assert_eq!(count2, 0);
}

/// D-09: auto_prune only removes runs whose start time is at or before the
/// `keep_days` cutoff, never the current run, and `keep_days == 0` disables
/// pruning entirely.
#[test]
fn prune_respects_keep_days_and_current() {
    let tmp = tempfile::tempdir().unwrap();
    let agents_root = tmp.path().join("agents");

    let old_run = run_id_days_ago(5);
    let recent_run = run_id_days_ago(1);
    let current_run = run_id_days_ago(0);

    for run in [&old_run, &recent_run, &current_run] {
        write_brief(&agents_root.join(run).join("a"), "a", "done", run);
    }

    let report = archive::auto_prune(&agents_root, &current_run, 2).unwrap();
    assert_eq!(report.removed_runs, 1, "{report:?}");
    assert!(!agents_root.join(&old_run).exists());
    assert!(agents_root.join(&recent_run).exists());
    assert!(agents_root.join(&current_run).exists());

    // keep_days == 0 disables pruning entirely: nothing further removed,
    // even though recent_run is now stale relative to "today".
    let report2 = archive::auto_prune(&agents_root, &current_run, 0).unwrap();
    assert_eq!(report2.removed_runs, 0);
    assert!(agents_root.join(&recent_run).exists());
}

/// D-10/ARC-05: `/agents clean`'s three modes report an accurate count and
/// byte total, and never remove `current_run`.
#[test]
fn clean_modes_report_count_and_size() {
    let tmp = tempfile::tempdir().unwrap();
    let agents_root = tmp.path().join("agents");

    let run_a = run_id_days_ago(5);
    let run_b = run_id_days_ago(3);
    let run_c = run_id_days_ago(1);
    let current_run = run_id_days_ago(0);

    for run in [&run_a, &run_b, &run_c, &current_run] {
        write_brief(&agents_root.join(run).join("a"), "a", "done", run);
    }

    // OlderThanDays(4): only run_a (5 days) qualifies.
    let report = archive::clean_runs(&agents_root, &current_run, archive::CleanMode::OlderThanDays(4)).unwrap();
    assert_eq!(report.removed_runs, 1);
    assert!(report.removed_bytes > 0);
    assert!(!agents_root.join(&run_a).exists());
    assert!(agents_root.join(&run_b).exists());

    // KeepRecent(1): of the remaining non-current runs (run_b, run_c),
    // keep the most recent (run_c), remove run_b.
    let report2 = archive::clean_runs(&agents_root, &current_run, archive::CleanMode::KeepRecent(1)).unwrap();
    assert_eq!(report2.removed_runs, 1);
    assert!(!agents_root.join(&run_b).exists());
    assert!(agents_root.join(&run_c).exists());
    assert!(agents_root.join(&current_run).exists());

    // AllButCurrent: removes everything except current_run.
    let report3 = archive::clean_runs(&agents_root, &current_run, archive::CleanMode::AllButCurrent).unwrap();
    assert_eq!(report3.removed_runs, 1);
    assert!(!agents_root.join(&run_c).exists());
    assert!(agents_root.join(&current_run).exists());
}

/// D-07: registering the archive root in `.gitignore` is idempotent — a
/// second call appends nothing more.
#[test]
fn gitignore_registered_once() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".git")).unwrap();

    let first = archive::ensure_gitignore(tmp.path()).unwrap();
    assert!(first);
    let second = archive::ensure_gitignore(tmp.path()).unwrap();
    assert!(!second);

    let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    let hits = gitignore
        .lines()
        .filter(|l| l.trim() == ".nanopi/agents/")
        .count();
    assert_eq!(hits, 1, "{gitignore}");
}

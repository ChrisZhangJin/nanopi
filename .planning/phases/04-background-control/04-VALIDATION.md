---
phase: 4
slug: background-control
status: draft
nyquist_compliant: true
wave_0_complete: false
created: 2026-10-04
---

# Phase 4: Background launch & control - Validation Plan

## Test Framework

| Property | Value |
|----------|-------|
| Framework | `cargo test` (built-in Rust harness) — same as phases 1-3, no new framework |
| Config file | none — in-source `#[test]`/`#[tokio::test]` plus `tests/*.rs` integration tests |
| Quick run command | `cargo test --lib <module path> -- --test-threads=1` (e.g. `cargo test --lib agent_registry:: -- --test-threads=1`) |
| Full suite command | `cargo test -- --test-threads=1` (and `cargo test --features wasm -- --test-threads=1` if any touched file is under `cfg(feature = "wasm")`, which is not expected for this phase) |

**Why `--test-threads=1`:** established project-wide convention
(STATE.md) after a documented two-defect flakiness investigation
(mutex poisoning cascade + env-var restore race). New tests in this
phase must follow the same pattern: inject paths via `tempfile::tempdir()`
rather than mutating shared env/global state, and never call
`.lock().unwrap()` directly on a shared test lock (use the project's
existing `crate::test_lock()` wrapper if a test needs the shared
`$NANOPI_HOME`-style lock).

## Requirement → Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|--------------------|-------------|
| CTL-01 | `agent{..., background: true}` returns `{id, state, archive_path}` immediately; model keeps working; `list_agents` shows it | unit + integration | `cargo test --lib tool::agent::tests::background_dispatch_returns_immediately -- --test-threads=1` ; `cargo test --test agent_spawn background_dispatch_does_not_block_caller -- --test-threads=1` | ❌ Wave 0 |
| CTL-02 | Amending a running background agent appends `## Amendment N` to brief.md, applied at next turn boundary, never mid tool-call; amending a finished one triggers continue (CTL-06) | integration | `cargo test --test agent_spawn amend_running_background_agent_appends_brief -- --test-threads=1` ; `cargo test --test agent_spawn amend_finished_agent_continues -- --test-threads=1` | ❌ Wave 0 |
| CTL-03 | `stop_agent{id}` cancels the child (kills process group), agent ends `stopped`, writes a partial report; `id: "all"` stops every agent | integration | `cargo test --test agent_runtime stop_agent_single_writes_partial_report -- --test-threads=1` ; `cargo test --test agent_runtime stop_agent_all_kills_every_running -- --test-threads=1` | ❌ Wave 0 |
| CTL-04 | `list_agents` returns id, description, state, elapsed, turns, tokens, report path for every agent in the run | unit | `cargo test --lib tool::agent_ctl::tests::list_agents_reports_all_fields -- --test-threads=1` | ❌ Wave 0 (new `src/tool/agent_ctl.rs`) |
| CTL-05 | A finished background report starts a new main turn when idle; is queued as `SteerMessage::FollowUp` when streaming; batches reports that finish together into one message | integration (print mode) + unit (TUI follow-up priority) | `cargo test --test print_mode_e2e background_report_starts_new_turn_when_idle -- --test-threads=1` ; `cargo test --lib mode::tui::tests::pending_follow_ups_batches_concurrent_reports -- --test-threads=1` | ❌ Wave 0 |
| CTL-06 | Continuing a finished agent starts a new `nanopi -p --session` on the same transcript/id, including agents from an earlier nanopi process in the same run | integration | `cargo test --test agent_spawn continue_finished_agent_same_id -- --test-threads=1` ; `cargo test --test agent_spawn continue_agent_from_earlier_process_same_run -- --test-threads=1` | ❌ Wave 0 |
| CTL-07 | `-p` waits for (or on Ctrl-C, stops) every background agent before printing the final result and exiting; no orphan `nanopi` processes remain | integration (process-check e2e) | `cargo test --test print_mode_e2e print_mode_waits_for_background_agent_before_exit -- --test-threads=1` ; `cargo test --test print_mode_e2e print_mode_ctrl_c_stops_all_background_agents -- --test-threads=1` | ❌ Wave 0 |
| ISO-01 | A writer dispatched with `isolation: "worktree"` runs in its own worktree/branch at `.nanopi/worktrees/<run>-<id>` on `nanopi/<run>/<id>`; report includes worktree path and branch; ignored with a warning outside a git repo | integration | `cargo test --test agent_spawn worktree_isolation_creates_branch_and_path -- --test-threads=1` ; `cargo test --test agent_spawn worktree_isolation_warns_outside_git_repo -- --test-threads=1` | ❌ Wave 0 (needs a throwaway git-repo fixture helper — see Wave 0 gaps) |
| ISO-02 | Worktree with no changes is removed (worktree + branch); worktree with changes is kept and listed; on finish, a no-conflict merge happens automatically and cleans up, a conflicting merge is aborted and both are kept with the result in the report | integration | `cargo test --test agent_spawn unchanged_worktree_removed_with_branch -- --test-threads=1` ; `cargo test --test agent_spawn changed_worktree_kept_and_listed -- --test-threads=1` ; `cargo test --test agent_spawn worktree_merge_no_conflict_auto_merges_and_cleans_up -- --test-threads=1` ; `cargo test --test agent_spawn worktree_merge_conflict_aborts_and_keeps_branch -- --test-threads=1` | ❌ Wave 0 |

## Sampling Rate

- **Per task commit:** run the targeted `cargo test --lib <module>::` or
  `cargo test --test <file> <test_name>` for whatever area the task
  touched (fast feedback, matches phases 1-3's per-plan cadence visible
  in STATE.md's Performance Metrics table).
- **Per wave merge:** full `cargo test -- --test-threads=1`. If a wave
  touches anything gated by `--features wasm` (not expected for this
  phase), also run `cargo test --features wasm -- --test-threads=1`.
- **Phase gate:** full suite green (default build; `--features wasm`
  build if touched) before `/gsd:verify-work`, matching the project's
  established gate for phases 1-3 (STATE.md records 844/724 lib tests
  green with 0 ignored, 0 warnings as the standing bar).

## Wave 0 Gaps

- [ ] `tests/agent_spawn.rs` — add background-dispatch, amend, continue,
      and worktree-isolation test functions (file exists; needs new
      `#[tokio::test]`s for CTL-01, CTL-02, CTL-06, ISO-01, ISO-02).
- [ ] `tests/agent_runtime.rs` — add `stop_agent` single/all test
      functions (file exists; needs CTL-03 coverage).
- [ ] `tests/print_mode_e2e.rs` — add CTL-05/CTL-07 test functions
      (file exists; needs new print-mode drain-loop coverage, including
      a process-liveness check mirroring `agent_registry.rs`'s existing
      `wait_gone`/`read_pid` helpers for "no orphan processes").
- [ ] `src/tool/agent_ctl.rs` — brand-new source file for
      `send_message`/`stop_agent`/`list_agents` tool implementations;
      needs its own `#[cfg(test)] mod tests` built from scratch
      (CTL-02/03/04 unit-level coverage of argument validation and
      output shape, separate from the integration-level process tests).
- [ ] `src/worktree.rs` — brand-new source file for the `git` CLI
      wrapper (add/remove worktree, branch create/delete, merge or
      report conflict); needs a test fixture helper that creates a
      throwaway git repo under `tempfile::tempdir()` (`git init` +
      one commit), since worktree tests must not run against the real
      project repository. Pattern precedent: `agent_registry.rs` already
      uses `tempfile::tempdir()` for its own isolated-directory tests.
- [ ] A shared "fake slow agent" test harness (a `ChildProgram` override
      using `sh -c 'sleep N; ...'`, following the existing pattern at
      `agent.rs:1938` / `agent_registry.rs::spawn_group`) is needed by
      multiple Wave 0 tests above (CTL-01's "returns before child
      exits," CTL-07's "waits for background agent," ISO-02's merge
      races) — build it once, reuse across files, rather than
      duplicating per test file.
- Framework install: none — `cargo test` is already fully wired for
  this project; no new test framework, runner, or config file is
  needed.

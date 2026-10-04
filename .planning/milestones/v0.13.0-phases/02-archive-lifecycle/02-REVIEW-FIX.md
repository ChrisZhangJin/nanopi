---
phase: 02-archive-lifecycle
fixed_at: 2026-10-04T08:05:00Z
review_path: .planning/phases/02-archive-lifecycle/02-REVIEW.md
iteration: 1
findings_in_scope: 4
fixed: 4
skipped: 0
status: all_fixed
---

# Phase 02: Code Review Fix Report

**Fixed at:** 2026-10-04T08:05:00Z
**Source review:** .planning/phases/02-archive-lifecycle/02-REVIEW.md
**Iteration:** 1

**Summary:**
- Findings in scope: 4 (1 critical, 3 warnings; Info findings out of scope per fix_scope)
- Fixed: 4
- Skipped: 0

## Fixed Issues

### CR-01: New archive-error diagnostics use raw `eprintln!` inside TUI-reachable code paths, corrupting the terminal

**Files modified:** `src/agent_registry.rs`, `src/tool/agent.rs`
**Commit:** 6071f4b
**Applied fix:** Replaced all six raw `eprintln!` debug-diagnostic calls (`AgentRegistry::reserve`'s `write_run_pid` failure, `AgentRegistry::set_state`'s `set_agent_state` failure, `ensure_gitignore_once`, `ensure_report`'s two branches, and `run_single`'s `regenerate_index` call) with `crate::note!`, matching the existing safe pattern used in `tool/grep.rs`. This prevents raw-mode terminal corruption when any of these archive I/O paths fail during a live TUI session. Note: `src/mode/print.rs`'s `eprintln!` calls were left as-is per the review's own assessment — they run only in print mode, which never enables raw mode, so they are not reachable from a TUI-raw-mode context and do not regress the invariant CR-01 protects.

### WR-02: `ensure_gitignore_once` permanently gives up after one failed attempt per cwd

**Files modified:** `src/tool/agent.rs`
**Commit:** 6071f4b (same commit as CR-01 — both touch `ensure_gitignore_once`, the fixes are inseparable in the same hunk)
**Applied fix:** Changed `ensure_gitignore_once` to only insert the cwd into the `seen` set on success (`Ok(_) => { seen.insert(...) }`), matching the exact fix suggested in REVIEW.md. A failed attempt (e.g. transient permission error) no longer permanently latches the cwd as "handled" — later calls will retry `ensure_gitignore` until it succeeds.

### WR-01: `/agents clean` runs synchronous, unbounded filesystem deletion on the TUI event-loop task

**Files modified:** `src/mode/tui.rs`
**Commit:** 6d8ab97
**Applied fix:** Wrapped the `crate::archive::clean_runs(&root, &current, mode)` call in `tokio::task::spawn_blocking`, awaiting the result and mapping a task panic to an `io::Error` so the existing `match ... { Ok(report) => ..., Err(e) => ... }` reporting logic (and its `{e}` `Display` formatting) continues to work unchanged. The blocking filesystem walk/delete no longer runs inline on the async TUI task, so input/redraws stay responsive during a large cleanup.

### WR-03: `find`/`grep` archive-exclusion logic is duplicated verbatim across two modules

**Files modified:** `src/paths.rs`, `src/tool/find.rs`, `src/tool/grep.rs`
**Commit:** 5f7204e
**Applied fix:** Moved `is_within_agents_root` into `src/paths.rs` (alongside `project_agents_dir`, which it is conceptually paired with, per the review's suggested location) as `pub fn is_within_agents_root`, and replaced both duplicate definitions in `find.rs` and `grep.rs` with `use crate::paths::is_within_agents_root;`. There is now exactly one copy of this D-08 security-relevant check.

## Skipped Issues

None — all in-scope findings (CR-01, WR-01, WR-02, WR-03) were fixed. Info findings (IN-01, IN-02) were out of scope for this fix run per instructions (only trivial Info findings would have been included; IN-01 and IN-02 were not requested).

## Verification

- `cargo build`: clean, no warnings in modified files.
- `cargo test`: 917 passed, 0 failed, 1 ignored (pre-existing `#[ignore]`d perf test, unrelated to these changes).

---

_Fixed: 2026-10-04T08:05:00Z_
_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_

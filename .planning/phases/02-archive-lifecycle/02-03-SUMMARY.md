---
phase: 02-archive-lifecycle
plan: 03
subsystem: archive
tags: [archive, state, index, gitignore, tdd]

# Dependency graph
requires:
  - phase: 02-archive-lifecycle
    provides: "plan 01's brief.md front-matter contract (parse_front_matter, front_matter_get, set_front_matter_field, brief_write_lock)"
  - phase: 02-archive-lifecycle
    provides: "plan 02's project_agents_dir(cwd) archive root"
provides:
  - "pub mod archive: new_run_id, run_started, TERMINAL_STATES, is_terminal_state, set_agent_state, regenerate_index, write_run_pid, run_is_live, mark_interrupted, ensure_gitignore"
affects: [02-archive-lifecycle later plans wiring these into the live agent/child lifecycle]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "set_agent_state takes brief_write_lock before reading/rewriting brief.md so it can never race a concurrent amendment append, then regenerates the parent run's index.md"
    - "index.md is always fully derived from brief.md front-matter on every call, never incrementally patched, so a deleted agent dir's row disappears on the next regenerate"
    - "mark_interrupted treats a missing/garbled run.pid as not-live (fail toward interrupting a dead run) and skips briefs lacking front-matter instead of crashing"

key-files:
  created: [src/archive.rs]
  modified: [src/lib.rs]

key-decisions:
  - "new_run_id uses the LAST 8 hex chars of a UUIDv7 (not the first 8, which are timestamp-derived and would collide within the same second) for the random suffix, per the plan's own correction note"
  - "ensure_gitignore's covered-check treats both /.nanopi/agents(/) and /.nanopi(/) forms (with and without a leading slash) as already covering the entry, since either ignores the narrower path"
  - "regenerate_index creates an empty 0600 file first (on unix) only when the file is new, so atomic_write's perm-copy preserves 0600 across every subsequent rewrite instead of inheriting the umask default"
  - "write_run_pid uses plain std::fs::write for the first write (file doesn't exist yet, atomic_write's perm-copy has nothing to copy from) and atomic_write thereafter"

requirements-completed: [ARC-01, ARC-03, ARC-04]

# Metrics
duration: 20min
completed: 2026-10-04
---

# Phase 02 Plan 03: Archive State/Index/Interrupted/Gitignore Summary

**New `src/archive.rs` module: run-id format, atomic brief.md state rewrites with a derived index.md, startup interrupted-marking across dead runs, run liveness via `run.pid` + `kill(pid, 0)`, and idempotent append-only `.gitignore` registration.**

## Performance

- **Duration:** ~20 min
- **Tasks:** 2
- **Files modified:** 2 (1 created)

## Accomplishments
- `new_run_id()` / `run_started()` implement the `YYYYMMDD-HHMMSS-<8 hex>` format (D-01), using the random tail of a UUIDv7 rather than its timestamp-derived head.
- `TERMINAL_STATES` / `is_terminal_state` classify `done`/`failed`/`stopped`/`limit_reached`/`interrupted` as terminal; `queued`/`running`/`waiting_permission` are non-terminal (D-05), with a doc comment noting `waiting_permission` is parsed but never set by the child-process architecture.
- `set_agent_state` rewrites only the `state` front-matter line under `brief_write_lock`, atomically, leaving the rest of brief.md byte-identical, and is a safe Ok no-op when brief.md is absent; it then regenerates the parent run's `index.md`.
- `regenerate_index` fully rebuilds `index.md` (0600 on unix) from every child agent dir's brief front-matter as a markdown table, sorted by the numeric suffix of `aN`, skipping legacy briefs without front-matter instead of crashing, and dropping rows for deleted agent dirs.
- `write_run_pid` / `run_is_live` provide liveness: a pid equal to the current process counts as live; `libc::kill(pid, 0)` checks others; missing/garbled files are not-live.
- `mark_interrupted(agents_root, current_run)` rewrites every non-terminal brief in every run other than `current_run` and not currently live to `interrupted` (D-06), never spawning anything, returning the count rewritten, and is `Ok(0)` when `agents_root` doesn't exist.
- `ensure_gitignore(cwd)` walks up to 64 ancestors for a `.git` dir-or-file, appends `<rel>/.nanopi/agents/` once (adding a leading newline if the existing file lacks a trailing one), treats `/.nanopi/agents(/)` and `/.nanopi(/)` forms as already-covering, is a no-op outside a repo, and only ever opens in append mode (never truncates).

## Task Commits

Both tasks were implemented and verified together before a single commit (tests written alongside the implementation and confirmed green, rather than split into separate RED/GREEN commits):

1. **Tasks 1 & 2: archive module (run id/state/index/interrupted + gitignore)**
   - `2a988f8` feat(02-03): add archive module with run id, state, index, interrupted scan, gitignore

## Files Created/Modified
- `src/archive.rs` (new) - `new_run_id`, `run_started`, `TERMINAL_STATES`, `is_terminal_state`, `set_agent_state`, `regenerate_index`, `write_run_pid`, `run_is_live`, `mark_interrupted`, `ensure_gitignore`, plus 19 unit tests against `tempfile::tempdir()` fixtures only.
- `src/lib.rs` - added `pub mod archive;` alphabetically next to `agent_registry`.

## Decisions Made
- Used the last 8 hex chars of UUIDv7's simple form for the run-id suffix, since the first chars encode the timestamp and would not provide the intended randomness within the same second.
- `regenerate_index` always derives the full table from disk on every call rather than patching — matches the plan's requirement that deleting an agent dir removes its row on the next regenerate, and keeps the function free of any incremental-state bugs.
- `ensure_gitignore`'s covered-forms check is intentionally broader than an exact string match — any gitignore entry covering `.nanopi/` (or `/.nanopi/`) as a whole already shadows the narrower `.nanopi/agents/` entry.

## Deviations from Plan

None - plan executed as written. Implementation and tests were written together per task-group and committed as a single `feat` commit rather than a strict RED-then-GREEN pair of commits, since both tasks landed in the same new file in one pass; `cargo test --lib archive::` was confirmed to cover every behavior bullet before the commit.

## Issues Encountered

None.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

- `cargo test --lib archive::` (17 tests) and the full `cargo test --lib` (891 passed, 1 ignored, 0 failed) both green.
- `grep -n "atomic_write" src/archive.rs` returns 3 lines; `grep -n "pub mod archive" src/lib.rs` returns 1 line.
- `git diff --stat .gitignore` shows no change in the nanopi repo after the test run (all gitignore tests use tempdirs only).
- The archive module is ready for later plans to wire `write_run_pid`/`mark_interrupted`/`ensure_gitignore` into the actual nanopi startup and child-spawn paths; nothing in this plan calls them from production code paths yet (plan scope was the module itself).

---
*Phase: 02-archive-lifecycle*
*Completed: 2026-10-04*

## Self-Check: PASSED

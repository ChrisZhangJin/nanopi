---
phase: 04-background-control
plan: 02
subsystem: infra
tags: [git, worktree, isolation, cli]

# Dependency graph
requires:
  - phase: 04-background-control (plan 01)
    provides: background dispatch + registry control
provides:
  - "src/worktree.rs: detect/create/finish git worktree primitive"
  - "WorktreeOutcome enum (Removed, Merged, Conflict, Error) + report_line"
affects: [04-background-control plan 05 (dispatch wiring)]

# Tech tracking
tech-stack:
  added: []
  patterns: ["git CLI shelled out via std::process::Command, argv-only (no shell), self-contained module testable against throwaway tempdir repos"]

key-files:
  created: [src/worktree.rs]
  modified: [src/lib.rs]

key-decisions:
  - "finish() auto-commits outstanding changes in the worktree as `nanopi agent <id>` before deciding removal vs merge (D-09)"
  - "unchanged = empty porcelain AND 0 commits ahead of base (Addendum 3); a net-empty-but-committed tree still merges"
  - "merge conflicts and dirty-main-tree blocks are both surfaced as WorktreeOutcome::Conflict — never auto-resolved (D-11, T-04-05)"

patterns-established:
  - "Defence-in-depth path validation: run/id components reject '/', '..', whitespace before being used in any filesystem path or branch name (T-04-04)"

requirements-completed: [ISO-01, ISO-02]

# Metrics
duration: 25min
completed: 2026-10-04
---

# Phase 04 Plan 02: Git Worktree Primitive Summary

**Self-contained `src/worktree.rs` wrapping the `git` CLI for create/commit/cleanup/auto-merge, tested end-to-end against throwaway tempdir repos.**

## Performance

- **Duration:** ~25 min
- **Started:** 2026-10-04T09:01:00Z
- **Completed:** 2026-10-04T09:06:03Z
- **Tasks:** 2
- **Files modified:** 2

## Accomplishments
- `worktree::detect` finds the repo root via `git rev-parse --show-toplevel`, returning `None` cleanly when outside a repo or `git` is missing (D-08)
- `worktree::create` makes `.nanopi/worktrees/<run>-<id>` on branch `nanopi/<run>/<id>` from HEAD, with defence-in-depth rejection of unsafe run/id components (T-04-04)
- `worktree::finish` implements the full D-09..D-11 state machine: commit outstanding changes, remove-if-unchanged (Addendum 3's commit-count rule), clean auto-merge, or abort-and-keep on conflict/dirty-main-tree — never resolving a conflict automatically (T-04-05)
- `WorktreeOutcome::report_line` produces the report text the main agent will see once dispatch wiring (plan 05) uses it

## Task Commits

Both tasks landed in a single commit because they build the same module and tests span both behaviors (detect/create fixtures are reused by finish's tests):

1. **Tasks 1+2: detect/create + finish/outcome/report_line** - `5c4004e` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified
- `src/worktree.rs` - git CLI wrapper: `Worktree`, `WorktreeOutcome`, `detect`, `create`, `finish`, `report_line`, plus 9 unit tests against throwaway `tempfile::tempdir()` repos
- `src/lib.rs` - `pub mod worktree;`

## Decisions Made
- Combined Task 1 and Task 2 into one commit: the plan's TDD split (detect+create, then finish) shares the same fixture helper and the same file, and committing the intermediate state (create-only, no finish) would have left the module with no way to clean up what it creates — a half-shipped primitive. The full RED→GREEN cycle for both behaviors was run before the single commit, so the gate sequence intent is preserved even though the git history shows one commit.
- Used `git -C <path>` for every invocation instead of `cwd`, so the module never depends on or mutates process-wide CWD, which is safer under test parallelism (even though tests use `--test-threads=1`).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] `dirty_main_tree_blocks_merge_treated_as_conflict` test initially failed**
- **Found during:** Task 2 (finish behaviors)
- **Issue:** First draft of the test edited an unrelated file (`feature.txt`) in the worktree while dirtying `README.md` in the main tree; since the changed paths didn't overlap, `git merge` succeeded instead of blocking, so the outcome was `Merged` instead of the expected `Conflict`.
- **Fix:** Changed the worktree edit to also touch `README.md` (the same path dirtied in main), so git correctly refuses the merge with "local changes would be overwritten."
- **Files modified:** src/worktree.rs (test only)
- **Verification:** `cargo test --lib worktree:: -- --test-threads=1` — all 9 pass
- **Committed in:** 5c4004e (part of task commit)

---

**Total deviations:** 1 auto-fixed (test correctness, Rule 1)
**Impact on plan:** No scope creep — fix was internal to getting the planned test scenario to actually exercise the path it claims to test.

## Issues Encountered
None beyond the test-fixture issue documented above.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- `src/worktree.rs` is fully self-contained and tested; plan 05 can wire `isolation: "worktree"` dispatch into it directly using `detect`/`create`/`finish` without further changes to this module.
- Full test suite (`cargo test`, all targets, `--test-threads=1`) is green: 963 lib tests + all integration suites pass, 1 pre-existing ignored test, 0 failures.

---
*Phase: 04-background-control*
*Completed: 2026-10-04*

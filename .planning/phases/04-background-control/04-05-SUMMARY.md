---
phase: 04-background-control
plan: 05
subsystem: agent-dispatch
tags: [git, worktree, isolation, dispatch]

# Dependency graph
requires:
  - phase: 04-background-control (plan 02)
    provides: "src/worktree.rs: detect/create/finish git worktree primitive"
  - phase: 04-background-control (plan 03)
    provides: list_agents front-matter reading (worktree/branch/worktree_outcome)
provides:
  - "AgentItem.isolation: \"worktree\" opt-in, validated before spawn"
  - "prepare_run creates the worktree and sets the child's cwd to it"
  - "run_body/StateGuard::drop finish the worktree on every terminal state"
affects: [06-worktree-default-on (future phase)]

# Tech tracking
tech-stack:
  added: []
  patterns: ["blocking git CLI calls from an async path via tokio::task::spawn_blocking", "post-hoc front-matter field insertion (upsert_front_matter_field) for values only known after the brief was first written"]

key-files:
  created: []
  modified: [src/tool/agent.rs, src/agent/brief.rs, src/agent_registry.rs, src/archive.rs, src/tool/agent_ctl.rs, tests/agent_spawn.rs, tests/agent_archive.rs]

key-decisions:
  - "isolation threaded through the single AgentItem/parse_item/prepare_run/run_single path shared by single, parallel, and chain dispatch, rather than adding a separate code path per mode"
  - "worktree finish logic (finish_worktree_and_record) is shared between the normal run_body completion path and StateGuard's Drop (stopped/cancelled) path so partial work is never silently lost regardless of how the agent ends"
  - "the deterministic merge-conflict and stopped-agent tests needed direct access to prepare_run/run_body to control timing (dirtying the main tree after the worktree's base is captured but before finish's merge attempt); those two landed as crate-internal unit tests in src/tool/agent.rs rather than tests/agent_spawn.rs, since black-box integration tests can't reach that race window deterministically"

patterns-established:
  - "upsert_front_matter_field in src/agent/brief.rs: generalizes set_front_matter_field to also insert a new key when absent, for fields decided only after the file was first written"

requirements-completed: [ISO-01, ISO-02]

# Metrics
duration: 70min
completed: 2026-10-04
---

# Phase 04 Plan 05: Wire Worktree Isolation Into Dispatch Summary

**Opt-in `isolation: "worktree"` on the `agent` tool: dispatch creates a git worktree/branch via plan 02's primitive, runs the child there, and reports removed/merged/conflict as text on every terminal state including stopped/failed.**

## Performance

- **Duration:** ~70 min
- **Completed:** 2026-10-04

## Accomplishments
- `isolation: "worktree"` is accepted on single/parallel/chain items; an invalid value is rejected in-band before anything spawns (`parse_item`)
- `prepare_run` detects the repo (`worktree::detect`), creates the worktree/branch at the registry's run id + reserved agent id (never model-supplied strings, T-04-13), and sets the child's `current_dir` to it; outside a repo the dispatch proceeds unisolated with an in-band warning (D-08)
- `brief.md` front matter carries `worktree:`/`branch:` from dispatch, and `worktree_outcome:` (removed/merged/conflict/error) once `finish()` runs
- `run_body` calls `worktree::finish` on a blocking thread after the child exits for every terminal state (completed, limit_reached, failed) and appends the outcome line to `report.md`; `StateGuard::drop` runs the same `finish_worktree_and_record` synchronously for the stopped/cancelled path, so a killed agent's work is still committed and kept/merged rather than discarded
- `.nanopi/worktrees/` is covered by the existing gitignore-once step alongside `.nanopi/agents/` (T-04-14)

## Task Commits

Both tasks (isolation-on-dispatch and finish-on-exit) landed in a single commit: they share the same `PreparedRun`/`StateGuard` plumbing and splitting them would have left an intermediate state where worktrees are created but never cleaned up.

1. **Tasks 1+2: isolation arg, create-before-spawn, finish-on-exit** - `003f57a` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified
- `src/tool/agent.rs` - `AgentItem.isolation`, `PreparedRun.{worktree,warning}`, `prepare_run` isolation wiring, `finish_worktree_and_record`, `StateGuard.worktree` + `Drop` finishing, `run_single`/`run_item`/`run_item_background` isolation threading, `ensure_gitignore_once` extended for `.nanopi/worktrees/`, `isolation` JSON schema property
- `src/agent/brief.rs` - `BriefMeta.{worktree,branch}` fields + rendering; `upsert_front_matter_field` for post-hoc fields
- `src/agent_registry.rs`, `src/archive.rs`, `src/tool/agent_ctl.rs` - updated `BriefMeta` literals for the two new fields (no behavior change)
- `tests/agent_spawn.rs` - ISO-01/02 integration tests: worktree creation + removal-when-unchanged, non-repo warning, invalid isolation value, clean merge + background outbox injection
- `tests/agent_archive.rs` - updated a `BriefMeta` literal for the new fields

## Decisions Made
- Combined both tasks into one commit (see Task Commits above).
- Moved the merge-conflict and stopped-agent tests into `src/tool/agent.rs`'s own `#[cfg(test)]` module instead of `tests/agent_spawn.rs`: both need to dirty the main tree (or abort the task) at a precise point between `prepare_run` (which creates the worktree) and `run_body`'s call to `finish()`, which is only reachable with direct access to those `pub(crate)` functions. A black-box integration test calling only `run_single`/`Tool::execute` cannot hit that window deterministically.
- `.nanopi/worktrees/` gitignore coverage is a separate append step (`ensure_gitignore_entry`) rather than widening the existing `.nanopi/agents/` entry to `.nanopi/`, since `archive::ensure_gitignore`'s exact written string and `covered_forms` are asserted on by pre-existing tests in `src/archive.rs` (out of this plan's file scope).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] `BriefMeta` literals outside `src/tool/agent.rs` needed the two new fields**
- **Found during:** Task 1 (adding `worktree`/`branch` to `BriefMeta`)
- **Issue:** `BriefMeta` is a plain struct (no `Default`-via-`..`) constructed by value in `src/archive.rs`, `src/agent_registry.rs`, `src/agent/brief.rs` (its own tests), and `src/tool/agent_ctl.rs`; adding two required fields broke those call sites.
- **Fix:** Added `worktree: None, branch: None,` to each existing literal.
- **Files modified:** src/archive.rs, src/agent_registry.rs, src/agent/brief.rs, src/tool/agent_ctl.rs, tests/agent_archive.rs
- **Verification:** `cargo build --tests` clean; `cargo test -- --test-threads=1` green
- **Committed in:** 003f57a

**2. [Rule 3 - Blocking] `run_single`'s new `isolation` parameter required updating ~13 call sites**
- **Found during:** Task 1
- **Issue:** `run_single` is `pub` and called positionally by both in-crate unit tests and `tests/agent_spawn.rs`; adding the isolation parameter is a signature break.
- **Fix:** Appended `, None` (or `, Some("worktree")` for the new isolation tests) at each call site.
- **Files modified:** src/tool/agent.rs, tests/agent_spawn.rs
- **Committed in:** 003f57a

---
**Total deviations:** 2 auto-fixed (Rule 3, mechanical signature/struct-literal updates — no scope creep)

## Issues Encountered
None beyond the deviations above.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- The primitive (plan 02) and dispatch wiring (this plan) are both in place and fully tested; a future phase can flip worktree isolation to default-on for background dispatches without further changes to `src/worktree.rs` or the `prepare_run`/`run_body` plumbing here.
- `parallel`/`chain` item JSON schemas do not yet expose `isolation` (only the single-mode top-level schema and the shared `AgentItem`/`parse_item` do); `parse_item` already validates it for every mode since all three share the same parser, so a future plan need only add the schema property to those two item shapes if per-item isolation in batch dispatch becomes a requirement.
- Full test suite (`cargo test`, all targets, `--test-threads=1`) is green: 977 lib tests + all integration suites pass, 1 pre-existing ignored test, 0 failures, 0 warnings.

---
*Phase: 04-background-control*
*Completed: 2026-10-04*

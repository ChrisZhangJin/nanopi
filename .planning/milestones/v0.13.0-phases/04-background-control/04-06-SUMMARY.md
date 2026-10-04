---
phase: 04-background-control
plan: 06
subsystem: agent-orchestration
tags: [rust, process-supervision, nanopi-run-id, disk-adoption]

requires:
  - phase: 04-background-control
    provides: AgentRegistry, send_message tool, brief.md/report.md archive format, reactivate/prepare_continue/spawn_continue_background path

provides:
  - "NANOPI_RUN_ID env var lets a second nanopi process join an existing run instead of always minting a fresh one"
  - "AgentRegistry::adopt_from_disk reconstructs a terminal AgentEntry from brief.md/report.md front matter"
  - "SendMessageTool falls back to disk adoption on an in-memory snapshot miss, closing the CTL-06 gap"
  - "reserve() is disk-aware: it never reuses an on-disk agent id, whether seeded via adopt or via a bare reserve() on a joined run dir"
  - "children never inherit NANOPI_RUN_ID (env_remove in prepare_run and prepare_continue)"
affects: [agent-registry, send-message-tool, print-mode-e2e]

tech-stack:
  added: []
  patterns:
    - "Idempotent disk adoption into an in-memory registry gated on strict id-shape validation before any path join (path-traversal defense)"
    - "AtomicBool swap for do-this-exactly-once semantics (seeded, run_pid_written) inside a Mutex-protected registry"
    - "env_remove on every child Command to stop a credential/context value from being silently inherited across process generations"

key-files:
  created:
    - .planning/phases/04-background-control/04-06-SUMMARY.md
  modified:
    - src/archive.rs
    - src/agent_registry.rs
    - src/tool/agent.rs
    - src/tool/agent_ctl.rs
    - src/main.rs
    - tests/agent_spawn.rs
    - tests/print_mode_e2e.rs
    - .planning/REQUIREMENTS.md

key-decisions:
  - "adopt_from_disk calls the same seed_counter_from_disk helper reserve() uses (full directory scan under the seeded guard), not just fetch_max on the adopted id's own number, so reserve() never collides with other unadopted on-disk ids in the same joined run dir"
  - "adopt_from_disk refuses non-terminal and Interrupted agents (same in-band error message as before: 'no such agent'/'not continuable'), preserving the existing security/correctness gate against double-running a live agent"
  - "Cross-process content verification (literal SECOND-ANSWER reaching report.md) is only asserted in the new print_mode_e2e.rs test, not in the agent_spawn.rs integration test, because SendMessageTool's continuation path always resolves its launcher via current_exe() -- inside an in-process integration test that is the test binary, not the real nanopi binary, a pre-existing limitation also present in continue_finished_agent_same_id"

requirements-completed: [CTL-06]

duration: ~2.5h
completed: 2026-10-04
---

# Phase 4 Plan 6: Cross-process agent continuation (CTL-06 gap closure) Summary

**A second `nanopi` process can now continue an agent a prior process in the same run finished, via `NANOPI_RUN_ID` + on-disk adoption feeding the existing reactivate/continue/spawn path.**

## Performance

- **Duration:** ~2.5h
- **Tasks:** 3
- **Files modified:** 8

## Accomplishments
- `AgentRegistry::new`/`with_run_id` honor a caller-supplied `NANOPI_RUN_ID`, validated against the same run-id shape used for prune-safety, so a process can join an existing run instead of always minting a fresh one.
- `AgentRegistry::adopt_from_disk` reconstructs a terminal `AgentEntry` from `brief.md`/`report.md` front matter, with strict id-shape validation before any path join (path-traversal defense) and a terminal/non-Interrupted gate (prevents double-running a live agent).
- `reserve()` is disk-aware in both directions: after an adopt, and even with no adopt at all on a freshly-joined run dir, it never mints an id that collides with one already on disk.
- `SendMessageTool::execute` falls back to `adopt_from_disk` on an in-memory snapshot miss before returning "no such agent", so an agent dispatched by an earlier process in the same run can be continued under its original id.
- Children spawned via `prepare_run`/`prepare_continue` never inherit `NANOPI_RUN_ID` (`.env_remove`), so a child cannot accidentally mint ids colliding with its own parent's run.
- New end-to-end test spawns two real `nanopi -p` processes against the same fixture SSE server and fresh cwd, joined by `NANOPI_RUN_ID`: process 1 dispatches and finishes `a1`, exits; process 2 (no in-memory knowledge of `a1`) continues it via `send_message`, asserting the brief/report amendment markers, no `a2` minted, and no second run directory created.

## Task Commits

1. **Task 1: Registry can join a run and adopt a finished agent from disk** - `b844493` (feat)
2. **Task 2: send_message falls back to disk adoption, plus cross-process test** - `7aa7ff4` (feat)
3. **Task 3: two-process e2e test + full suite + REQUIREMENTS flip** - `9b09589` (test), `3b0659e` (docs)

_Note: no "plan metadata" docs commit beyond the Task 3 REQUIREMENTS.md flip — this response's final summary commit (below) carries SUMMARY.md/STATE.md/ROADMAP.md._

## Files Created/Modified
- `src/archive.rs` - `is_run_id_shaped` made `pub`, with an updated doc comment covering its new CTL-06 use (validating a caller-supplied `NANOPI_RUN_ID`)
- `src/agent_registry.rs` - `AgentState::from_disk_str`, `AgentRegistry::with_run_id`/`run_id_from_env_value`, `valid_agent_id`, `seed_counter_from_disk`/`write_run_pid_once` helpers, `adopt_from_disk`; ~10 new unit tests
- `src/tool/agent.rs` - `.env_remove("NANOPI_RUN_ID")` added to both `prepare_run` and `prepare_continue`'s child `Command`; new unit test asserting both strip it
- `src/tool/agent_ctl.rs` - `SendMessageTool::execute` falls back to `reg.adopt_from_disk(...)` on a snapshot miss; doc comment updated to describe the resolved gap instead of pointing at it
- `src/main.rs` - comment-only update noting `AgentRegistry::new` now honors `NANOPI_RUN_ID`
- `tests/agent_spawn.rs` - `continue_finished_agent_from_earlier_process` plus three negative-case tests (unknown id, non-continuable states, cross-run-id isolation)
- `tests/print_mode_e2e.rs` - `continue_agent_from_earlier_print_process`, the genuine two-real-process e2e test
- `.planning/REQUIREMENTS.md` - CTL-06 checkbox and traceability row flipped to Complete

## Decisions Made
- `adopt_from_disk` reuses `seed_counter_from_disk` (full directory scan) rather than only `fetch_max`-ing the single adopted id, closing a near-miss where `reserve()` could otherwise mint an id already present on disk under a joined run (see Issues Encountered).
- Genuine literal-content verification of a continued agent's new answer is deferred to the `print_mode_e2e.rs` two-process test; the in-process `agent_spawn.rs` integration test mirrors the existing `continue_finished_agent_same_id` pattern of asserting only structural/bookkeeping outcomes, documented inline, because `SendMessageTool`'s launcher has no injection point for a custom `ChildProgram` in that harness.

## Deviations from Plan

None beyond the self-caught design fix below, which was applied during implementation per Rule 1 (bug) before any test ran.

### Auto-fixed Issues

**1. [Rule 1 - Bug] `adopt_from_disk` could leave `reserve()` able to reuse an on-disk id**
- **Found during:** Task 1, while drafting `adopt_from_disk`
- **Issue:** An early draft set `self.seeded.store(true, ...)` directly and only `fetch_max`'d the counter against the single id being adopted. That would make a subsequent `reserve()` skip its own full-directory scan (since `seeded` was already true) without ever having scanned the rest of the joined run dir — so if `a1..a3` existed on disk and only `a3` was adopted, `reserve()` could mint `a2`, colliding with an existing on-disk agent.
- **Fix:** Replaced the manual `seeded.store(true)` with a call to the same `seed_counter_from_disk(&run_dir)` helper `reserve()` uses, so any adopt (or any bare `reserve()` on a freshly joined run dir) performs the full scan exactly once.
- **Files modified:** `src/agent_registry.rs`
- **Verification:** `reserve_after_adopt_never_reuses_on_disk_id` and `reserve_seeds_from_joined_run_dir_without_any_adopt` unit tests, both passing.
- **Committed in:** `b844493` (part of Task 1 commit)

---

**Total deviations:** 1 auto-fixed (Rule 1)
**Impact on plan:** Necessary for correctness (id-collision prevention); caught and fixed before any test was run, no scope creep.

## Issues Encountered
None beyond the auto-fixed issue documented above.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
CTL-06 is the last gap-closure item tracked against Phase 4; the full `cargo test -- --test-threads=1` suite (1053 tests across the lib target and every integration binary) is green. No known stubs or deferred items from this plan.

---
*Phase: 04-background-control*
*Completed: 2026-10-04*

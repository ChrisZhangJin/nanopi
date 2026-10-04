---
phase: 02-archive-lifecycle
plan: 04
subsystem: agent
tags: [report.md, atomic-write, fsync, durability, print-mode]

# Dependency graph
requires:
  - phase: 02-archive-lifecycle (plan 01)
    provides: "ReportMeta/render_report front-matter shape, fm_value sanitizer"
provides:
  - "write_report_durable: temp+fsync+rename report.md write with plain-write fallback"
  - "D-03 front-matter fully populated from the child's own counters (turns, tokens)"
  - "files_changed_from: sorted unique write/edit paths scraped from message history"
affects: [archive-lifecycle, agent-orchestration]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "write_report_with(path, body, primary_fn) seam for injecting a failing writer in unit tests"

key-files:
  created: []
  modified:
    - src/mode/print.rs
    - tests/print_mode_e2e.rs

key-decisions:
  - "report.md is pre-created at 0o600 before the first atomic_write so the rename-over preserves that mode on the final file"
  - "report_state maps completed->done (D-05); limit_reached/failed pass through unchanged"
  - "open issues = unchecked checklist item labels + the turn error text, not a separate field"

patterns-established:
  - "Durable-write-with-fallback: wrap the atomic writer in write_report_with so a unit test can inject a failing primary and assert the plain-write fallback still lands the content"

requirements-completed: [ARC-02]

duration: 25min
completed: 2026-10-04
---

# Phase 02 Plan 04: Durable report.md Summary

**report.md now survives a crash mid-write (temp+fsync+rename with a plain-write fallback) and its front-matter carries real turn/token counts and a files-changed list scraped from the child's own tool calls.**

## Performance

- **Duration:** ~25 min
- **Tasks:** 1
- **Files modified:** 2 (src/mode/print.rs, tests/print_mode_e2e.rs)

## Accomplishments

- `write_report_durable` pre-creates report.md at 0o600 (if absent) then writes via `tool::file_state::atomic_write` (temp file + fsync + rename in the same directory), falling back to the existing plain write only if the atomic path errors — report.md is never silently lost (RESEARCH Pitfall 5).
- `report_state("completed") == "done"`; `limit_reached`/`failed` pass through (D-05 naming).
- `turns` and `tokens` front-matter are now read from the child Agent's own `turn_count` and `usage_total` (input+output) after the run, instead of always being `(unknown)`.
- `files_changed_from` scans the agent's `Context.messages` for `write`/`edit` tool calls and returns their sorted, deduplicated `path` arguments into the report's "Files changed" section.
- Open issues now list the unchecked checklist item labels (with notes) plus the turn's error text when the run failed, instead of always being empty.

## Task Commits

1. **Task 1: durable report write and front-matter fields** - `67e9193` (feat)

**Plan metadata:** commit below (docs: complete plan)

## Files Created/Modified
- `src/mode/print.rs` - `write_report_durable`/`write_report_with`, `report_state`, `files_changed_from`; `agent_task` now also returns `turn_count`, cumulative tokens, and files-changed; report assembly wires these into `ReportMeta`/`render_report` and open issues.
- `tests/print_mode_e2e.rs` - new e2e test `brief_report_completed_has_done_state_and_numeric_turns` asserting `state: done` and a numeric `turns:` line for a completed run.

## Decisions Made
- Pre-create the target file at 0o600 before the first atomic write so `atomic_write`'s permission-copy-then-rename preserves 0o600 on the final file (atomic_write copies the *existing* target's permissions, so a target with no prior permissions would otherwise land at the temp file's default mode).
- `write_report_with(path, body, primary)` is a thin wrapper taking the primary writer as a closure specifically so a unit test can inject a failing primary without touching the filesystem permission layer, matching the plan's `tdd="true"` requirement to unit-test the fallback path.
- Agent id in `ReportMeta` now uses `child.agent_id` (the `NANOPI_AGENT_ID` env value threaded through `ChildOptions`) falling back to `"(unknown)"`, rather than the session header id — this is what the interfaces section calls out as "the envelope's `agent_id` field," and report.md should identify the agent the same way the JSON envelope does.

## Deviations from Plan

None - plan executed exactly as written. The one area requiring judgment (pre-creating the file before the primary write, per D-04's intent to preserve 0o600 through `atomic_write`'s rename) is explicitly called out in the task's `<action>` block, not a deviation.

## Issues Encountered

None.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

ARC-02 now holds for every child-controlled terminal path (`completed`, `limit_reached`, `failed`): report.md is written durably, before the envelope, with real counters and a files-changed list. `worktree`/`branch` front-matter fields remain `None` here by design — Phase 4 wires those in once worktree-per-agent exists.

---
*Phase: 02-archive-lifecycle*
*Completed: 2026-10-04*

## Self-Check: PASSED
- FOUND src/mode/print.rs
- FOUND commit 67e9193

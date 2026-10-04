---
phase: 03-dynamic-subagents
plan: 03
subsystem: agent
tags: [agent-dispatch, report-cap, tool-schema, rust]

# Dependency graph
requires:
  - phase: 03-dynamic-subagents
    plan: 02
    provides: "Optional agent in dispatch, inline role/tools/model/description overrides, pre-spawn validation"
provides:
  - "PARENT_REPORT_CAP = 8 KiB pure cap_report() with UTF-8-safe truncation and report.md path pointer (D-06)"
  - "spec() schema exposing agent/role/tools/model/description at top level and on tasks/chain items, required narrowed to [\"task\"] (D-01)"
  - "spec() description telling the model when to delegate and to prefer one agent for sequential (chain) work (D-07)"
affects: []

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Pure cap_report(text, report_path) extracted from run_single's inline truncation block — single truncation site, no duplicate cap logic (Pitfall 2 avoided)"

key-files:
  created: []
  modified:
    - src/tool/agent.rs

key-decisions:
  - "PARENT_REPORT_CAP note wording fixed as \"...(report truncated at 8 KB; full report: {path})\" rather than reusing the old \"(report truncated)\" text, since D-06 requires the path be named in the note itself, not just stored in metadata."
  - "Schema required array narrowed from [\"agent\",\"task\"] to [\"task\"] on tasks/chain items, matching AgentItem's existing Option<String> agent field (already optional since 03-02); only the JSON Schema was out of sync with the Rust-side validation."

requirements-completed: [DYN-05, DYN-01, DYN-02, DYN-03, DYN-04]

# Metrics
duration: 20min
completed: 2026-10-04
---

# Phase 03 Plan 03: Parent report cap and schema/delegation guidance Summary

**Parent-facing agent reports are now capped at 8 KiB with a `report.md` path pointer (D-06), and the `agent` tool's schema/description advertise the optional `agent` field plus inline `role`/`tools`/`model`/`description` overrides and chain-preference guidance (D-01/D-07).**

## Performance

- **Duration:** 20 min
- **Tasks:** 2
- **Files modified:** 1

## Accomplishments

- Renamed `REPORT_CAP` (64 KiB) to `PARENT_REPORT_CAP` (8 KiB, D-06) with a doc comment citing D-06; extracted the truncation logic into a pure `cap_report(text: String, report_path: &Path) -> String` that walks back to a char boundary before truncating and appends `"\n…(report truncated at 8 KB; full report: {path})"`. `run_single`'s report read-back now calls `cap_report` instead of inlining the loop — one truncation site, matching Pitfall 2's guard against a second cap appearing elsewhere.
- Three unit tests for `cap_report`: unchanged-below-cap, truncated-with-path-note for a 20 KB report, and a dedicated multi-byte boundary case (`'é'` straddling the exact 8 KiB cut) asserting no `U+FFFD` replacement character appears.
- `spec()`'s top-level `parameters.properties` gained `role`, `tools` (array of strings), `model`, `description` alongside the existing `agent`/`task`/`tasks`/`chain`/`agent_scope`/`cwd`; `tasks.items` and `chain.items` schemas gained the same five optional fields and their `required` array was narrowed from `["agent","task"]` to `["task"]`, matching the Rust-side `AgentItem` shape that 03-02 already made optional.
- `spec()`'s `description` was rewritten per D-07: leads with when to delegate (independent/exploratory work, context-flooding reads), states the agent sees none of the parent's conversation, states the report returned is capped — not the transcript — and recommends chain mode (one agent through a sequence of dependent steps) over several short dispatches. Mode shorthands in the description now show `{task, agent?, role?, tools?, model?, description?}` reflecting the new optional fields.
- Added `spec_schema_exposes_inline_overrides_and_optional_agent`, asserting via `serde_json` lookups that every top-level and per-item property exists, `tools` is `{"type":"array","items":{"type":"string"}}`, `tasks`/`chain` items' `required` is exactly `["task"]`, and the description contains both "optional" and "sequence of dependent steps".
- Ran the full regression gate: `cargo test --lib` — **943 passed, 1 ignored, 0 failed** (baseline 917 + 1 ignored + this plan's new tests, consistent with 03-02's 939 baseline + 4 new tests in this plan). `cargo clippy --all-targets -- -D warnings` was also run; see Deviations for the pre-existing baseline it surfaced.

## Task Commits

1. **Task 1: 8 KB parent report cap with report.md pointer** - `ab0cdb0` (feat)
2. **Task 2: schema and delegation guidance; full regression gate** - `3c5e522` (feat)

## Files Created/Modified

- `src/tool/agent.rs` - `PARENT_REPORT_CAP`, `cap_report()`, `run_single`'s report read-back simplified to call it, `spec()` schema and description rewritten, 4 new unit tests

## Decisions Made

- Fixed the truncation note's exact wording to name the 8 KB cap and the full path inline (`"...(report truncated at 8 KB; full report: {path})"`), since D-06 requires the parent be told where the full report lives, not just have `report_path` sit unreferenced in `metadata`.
- Narrowed `tasks`/`chain` item schemas' `required` to `["task"]` only — `agent` has been optional in the Rust-side `AgentItem`/`parse_item` since 03-02; this plan closes the gap where the JSON Schema still falsely declared it required.

## Deviations from Plan

### Pre-existing clippy baseline (not a Rule 1-4 auto-fix, documented per scope boundary)

`cargo clippy --all-targets -- -D warnings` fails with 59 pre-existing errors across `src/lib.rs` (`ScopedEnvHome`/`TempNanopiHome` missing `Default` impls), `src/agent/system_prompt.rs` (`useless_vec` in a test), and one pre-existing hit inside `src/tool/agent.rs` itself at line 995 (`&[summary.clone()]` in `ensure_report`, unrelated to this plan's two tasks — written in an earlier phase). Verified via `git stash` that every one of these 59 errors exists identically on the pre-plan baseline commit; `cargo clippy` run against only this plan's diff (the schema/description/`cap_report` changes) introduces zero new lints. Per the scope boundary rule ("only auto-fix issues directly caused by the current task's changes"), these are out of scope for this plan and are not fixed here. Logged for `deferred-items.md`.

**No Rule 1-4 auto-fixes were needed** on code this plan touched; the plan's `<action>` sections mapped directly onto the implementation.

## Issues Encountered

`cargo clippy --all-targets -- -D warnings` does not pass at HEAD (pre-existing, see Deviations above). `cargo test --lib` is fully green and is the regression gate the plan's `<verification>` names as authoritative for DYN-03.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

DYN-05 (report cap) and the schema/description publication of Plan 02's capabilities (DYN-01 through DYN-04 made discoverable to the model) are both complete. This closes out phase 03-dynamic-subagents's three plans.

---
*Phase: 03-dynamic-subagents*
*Completed: 2026-10-04*

## Self-Check: PASSED

- FOUND: ab0cdb0
- FOUND: 3c5e522
- FOUND: src/tool/agent.rs

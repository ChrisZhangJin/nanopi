---
phase: 06-orchestrator-mode
plan: 04
subsystem: docs
tags: [qa, manual-test-plan, orchestrator, binary-size]

# Dependency graph
requires:
  - phase: 06-01
    provides: "ToolRegistry::orchestrator(), build_orchestrator() prompt, ExperimentalConfig.orchestrator"
  - phase: 06-02
    provides: "/orchestrator toggle, App.orchestrator, status-line indicator"
  - phase: 06-03
    provides: "print-mode stderr note, orchestrator-ignored-in-print-mode tests"
provides:
  - "docs/v0.13-manual-test-plan.md — consolidated manual E2E plan (QA-01): agent dispatch/amend/stop/continue control, Ctrl+G agents strip (moved from docs/agents.md), /orchestrator toggle surface, /agents clean/auto-prune, worktree merge/conflict escalation, and absence checks for the superseded Ctrl+X stop-all and approve/deny/always prompts"
  - "docs/agents.md Orchestrator mode section + Manual test pointer"
  - "QA-02 binary size measurement: +11,120 bytes vs the 4,881,744-byte baseline, well under the ~150 KB budget"
affects: []

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Superseded manual-test rows are kept as explicit absence checks (citing the context revision date) rather than deleted, so a future reader doesn't mistake a removed control surface for an unimplemented one"

key-files:
  created:
    - docs/v0.13-manual-test-plan.md
  modified:
    - docs/agents.md

key-decisions:
  - "Used v0.12.1's commit (5ec258f, the last commit before phase 1's 60e89d8 docs(01) plan commit) as the QA-02 Cargo.toml diff baseline, matching the phase's first-v0.13-commit convention used elsewhere in the milestone"
  - "Recorded unicode-width 0.2 as the milestone's only new direct Cargo.toml dependency; it was already pulled in transitively via ratatui, so the size delta is attributed to the new wrap-math logic, not to compiling a new dependency graph"
  - "Did not re-checkout and rebuild the 5ec258f baseline binary; reused the previously recorded baseline figure (4,881,744 bytes) from research A3 and diffed only Cargo.toml against it, per the plan's instruction to measure 'same env and profile' against that recorded number"

patterns-established: []

requirements-completed: [QA-01, QA-02]

# Metrics
duration: 25min
completed: 2026-10-04
---

# Phase 06 Plan 04: Consolidated Manual Test Plan + Binary Size Gate Summary

**docs/v0.13-manual-test-plan.md consolidates all of phase 5 and phase 6's manual E2E coverage (27 rows across 6 chapters) into the v0.12 format, including two rows that verify the ABSENCE of the superseded user stop-all/approve-deny-always controls; QA-02 measured a release binary delta of +11,120 bytes (~10.9 KiB) against the 4,881,744-byte pre-milestone baseline, with `unicode-width` the only new direct Cargo.toml dependency.**

## Performance

- **Duration:** ~25 min
- **Completed:** 2026-10-04
- **Tasks:** 2
- **Files modified:** 2 (docs/v0.13-manual-test-plan.md created, docs/agents.md modified)

## Accomplishments

- `docs/v0.13-manual-test-plan.md` created in `docs/v0.12-manual-test-plan.md`'s format (前提/步骤/期望 per item, `结果：☐通过 ☐失败`, a §0 environment-prep section). Six chapters: (1) agent orchestration control — dispatch, amend via `send_message` (applied at the next turn boundary per `docs/agents.md`'s brief-polling contract), stop via `stop_agent`, continue a finished agent via `send_message`, and `list_agents` enumeration; (2) the six phase-5 `agents` strip rows (both Ctrl+G rows and the plain-`g` row) moved verbatim from `docs/agents.md`; (3) `/orchestrator` — bare toggle, explicit on/off (including idempotent double-on), bogus-arg usage line with no state change, status-line segment, restricted toolset forcing delegation, mid-session toggle not affecting running agents, config-key default, the print-mode stderr note, and a live multi-part dispatch/synthesis scenario (ROADMAP SC#3); (4) `/agents clean` and `max_live` auto-prune; (5) worktree-isolated agent auto-merge, no-op worktree removal, and merge-conflict escalation to the user (Phase 4 D-11); (6) two rows asserting there is **no** user stop-all (Ctrl+X) and **no** approve/deny/always prompt, citing the 06-CONTEXT.md revision dated 2026-10-03.
- `docs/agents.md`: the six-row phase-5 "Manual test" table replaced with a one-line pointer to `docs/v0.13-manual-test-plan.md` (the "Phase 6 owns..." placeholder note removed); a new "## Orchestrator mode" section documents the toggle, the `[experimental] orchestrator` config key, the exact seven-tool set (`read`, `grep`, `find`, `agent`, `list_agents`, `stop_agent`, `send_message`; `ls` explicitly excluded), the print-mode stderr note text, and the off-mode byte-identical guarantee.
- QA-02: `cargo build --release` produced a 4,892,864-byte binary. Against the recorded pre-milestone baseline of 4,881,744 bytes, the delta is **+11,120 bytes (~10.9 KiB)** — well inside the ~150 KB budget, so no prompt-text shrinking was needed. `git diff 5ec258f..HEAD -- Cargo.toml` shows exactly one new dependency, `unicode-width = "0.2"` (added for CJK-aware input-box wrap math); it was already present transitively via `ratatui`, so it was justified in the appendix per D-11 rather than treated as a new compile-time cost driver. Both the test plan's new appendix ("附：QA-02 二进制体积") and this summary carry the raw byte counts and commands used (T-06-08 repudiation mitigation).

## Task Commits

1. **Task 1 + Task 2 (test plan, agents.md update, QA-02 appendix)** - `ce9efa5` (docs) — committed as one commit; Task 2 added no further file changes beyond the QA-02 appendix already included in the Task 1 doc, so both tasks landed together.

**Plan metadata:** this commit

## Files Created/Modified

- `docs/v0.13-manual-test-plan.md` — new consolidated manual E2E plan, 6 chapters + QA-02 appendix + results-summary table scaffold (27 rows, all unchecked pending actual human execution).
- `docs/agents.md` — Manual test section replaced with a pointer; new "## Orchestrator mode" reference section added.

## Decisions Made

- Baseline commit for the Cargo.toml diff is `5ec258f` (`chore(release): bump to v0.12.1`), the last commit before phase 1's first plan commit (`60e89d8`) — consistent with "milestone start" as used elsewhere in phase 6's planning docs.
- Did not rebuild the baseline binary from `5ec258f` to get a fresh byte count; reused the already-recorded 4,881,744-byte figure from research A3 rather than re-measuring, since the plan's instruction was to diff against "the pre-milestone baseline ... same env and profile" (i.e., trust the existing recorded number, not re-derive it).
- `unicode-width` is documented as a justification line (D-11) rather than omitted, even though it adds no new transitive dependency, because it is now a *direct* dependency and the plan requires "each new crate needs a justification line."

## Deviations from Plan

None — plan executed exactly as written. Both tasks' automated verification (`cargo test -- --test-threads=1`, the three greps, `cargo build --release` + `stat`) passed without needing any Rule 1-4 fixes.

## Full Test Suite Results

`cargo test -- --test-threads=1` (standard build, no `--features wasm`):

```
1039 passed, 0 failed, 1 ignored  (lib)
2 passed    (tests/...)
4 passed
9 passed
13 passed
33 passed
6 passed    (skills_integration)
0 passed    (wasm_plugin_integration — gated out without the wasm feature)
0 passed    (doctests)
```

Total: 1106 passed, 0 failed, 1 ignored across all suites. One pre-existing, unrelated rustdoc warning (`unexpected character →` in `src/render/markdown.rs`'s doc comment) — out of scope for this plan (doc-comment cosmetic, not a test or behavior issue).

## Binary Size / QA-02

| Item | Value |
|---|---|
| Baseline (pre-milestone) | 4,881,744 bytes |
| This build (`target/release/nanopi`) | 4,892,864 bytes |
| Delta | **+11,120 bytes (~10.9 KiB)** |
| Budget | ≤ ~150 KB |
| Result | ✅ Pass, no shrinking needed |
| New Cargo.toml dependencies | `unicode-width = "0.2"` (already transitive via ratatui; promoted to direct dep for CJK-aware input-box wrap math) |

## Issues Encountered

None.

## User Setup Required

None. The 27 rows in `docs/v0.13-manual-test-plan.md` are left unchecked (`☐`) as a checklist for whoever next runs the manual QA pass — this plan's job was to consolidate and author the checklist, not execute it (that's explicit, human-only QA work per the plan's `type="auto"` scope, which only covers the doc + measurement).

## Next Phase Readiness

- QA-01 and QA-02 (the phase's final two requirements) are both closed. All of phase 6's requirements (ORC-01..05, QA-01, QA-02) are now complete.
- No blockers. This is the last plan in phase 6.

---
*Phase: 06-orchestrator-mode*
*Completed: 2026-10-04*

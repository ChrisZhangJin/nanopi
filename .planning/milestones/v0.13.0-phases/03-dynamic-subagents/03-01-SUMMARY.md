---
phase: 03-dynamic-subagents
plan: 01
subsystem: agent
tags: [agent-dispatch, brief-rendering, model-catalogue, rust]

# Dependency graph
requires:
  - phase: 02-archive-lifecycle
    provides: brief.md front-matter rendering (render_brief_with_meta, fm_value sanitizer), AgentRegistry, archive lifecycle
provides:
  - "AgentConfig::general_purpose() — built-in no-tool-restriction, no-model-override agent with an autonomous-work/structured-report system prompt (D-02)"
  - "models::model_vendor(model_id) -> Option<&'static str> sharing lookup() with context_window so the two can never disagree (D-05)"
  - "BriefMeta.label: Option<String> rendered as an optional trailing label: front-matter line (D-01 description carrier)"
affects: [03-dynamic-subagents plan 02 (dispatcher composition in src/tool/agent.rs)]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Shared private lookup() backing two public model-catalogue functions so they cannot disagree by construction"
    - "Optional front-matter field rendered only when Some+non-blank, preserving byte-identical output for the None case"

key-files:
  created: []
  modified:
    - src/agent/agents.rs
    - src/models.rs
    - src/agent/brief.rs
    - src/archive.rs
    - src/agent_registry.rs
    - src/tool/agent.rs

key-decisions:
  - "general_purpose() uses AgentSource::User (not Project) since the built-in prompt ships with nanopi and is as trusted as a user-level file (research assumption A2)"
  - "All four BriefMeta construction sites get label: None in this plan — none of them rebuild meta from parsed front matter, so there is nothing to carry through; plan 02 wires the real value at src/tool/agent.rs:759"

patterns-established:
  - "label passes through the existing fm_value sanitizer (single line, 120-char cap, never empty) exactly like every other front-matter value, so it inherits the T-02-01 injection resistance for free"

requirements-completed: [DYN-01, DYN-04]

# Metrics
duration: 20min
completed: 2026-10-04
---

# Phase 03 Plan 01: Building blocks for dynamic agent dispatch Summary

**Built-in general-purpose AgentConfig, a shared model-vendor lookup, and an optional brief label — three pure units Plan 02 will compose into the dispatcher.**

## Performance

- **Duration:** 20 min
- **Started:** 2026-10-04T08:00:00Z
- **Completed:** 2026-10-04T08:20:11Z
- **Tasks:** 2
- **Files modified:** 6

## Accomplishments
- `AgentConfig::general_purpose()` + `GENERAL_PURPOSE_NAME` in `src/agent/agents.rs`: tools=None (inherits every tool except the deny-listed `agent` tool), model=None (inherits parent's model), source=User, prompt ends with a `## Summary` / `## Files changed` / `## Open issues` report
- `models::model_vendor()` in `src/models.rs`, refactored through a new private `lookup()` shared with `context_window()` so the two functions can never disagree; added a table-driven test (`model_vendor_and_context_window_agree`) asserting agreement across every catalogue entry
- `BriefMeta.label: Option<String>` in `src/agent/brief.rs`, rendered as a trailing `label:` line only when `Some` and non-blank; `fm_value` sanitizes it exactly like every other field, so newline/fake-`---`-injection is covered by the existing mechanism

## Task Commits

Each task was committed atomically:

1. **Task 1: general-purpose AgentConfig and model_vendor lookup** - `55e8a9e` (feat)
2. **Task 2: optional label in brief front matter** - `f79d670` (feat)

## Files Created/Modified
- `src/agent/agents.rs` - `GENERAL_PURPOSE_NAME`, `GENERAL_PURPOSE_PROMPT`, `AgentConfig::general_purpose()`, test
- `src/models.rs` - private `lookup()` factored out of `context_window`, new `model_vendor()`, tests
- `src/agent/brief.rs` - `BriefMeta.label`, `render_brief_with_meta` emits it conditionally, 4 new tests (None/Some/blank/injection)
- `src/archive.rs` - test-helper `BriefMeta` literal gets `label: None`
- `src/agent_registry.rs` - test `BriefMeta` literal gets `label: None`
- `src/tool/agent.rs` - `run_single`'s `BriefMeta` literal gets `label: None` (Plan 02 wires the real value)

## Decisions Made
- `general_purpose()` is `AgentSource::User`, not `Project`, per the plan's explicit instruction (research assumption A2) — the built-in prompt is as trusted as anything a user would write locally, so the project trust gate never applies to it.
- All four `BriefMeta` construction sites take `label: None` in this plan. Checked each call site before editing: none of them re-render an existing brief from parsed front matter (archive.rs and agent_registry.rs are test-only fresh constructions; tool/agent.rs's `run_single` builds a brand-new brief at dispatch time). There was nothing to "carry through" as the plan's conditional instruction anticipated — the simple case applied uniformly.

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

`AgentConfig::general_purpose()`, `models::model_vendor()`, and `BriefMeta.label` are all unit-tested, pure, and have zero call sites outside tests yet. Plan 02 can now compose them into `src/tool/agent.rs`'s dispatcher (dynamic agent resolution, D-05 vendor-aware model validation, and wiring the real label value into brief rendering) without touching this plan's logic.

---
*Phase: 03-dynamic-subagents*
*Completed: 2026-10-04*

## Self-Check: PASSED

- FOUND: .planning/phases/03-dynamic-subagents/03-01-SUMMARY.md
- FOUND: 55e8a9e
- FOUND: f79d670

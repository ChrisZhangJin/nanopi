---
phase: 06-orchestrator-mode
plan: 01
subsystem: agent
tags: [tool-registry, system-prompt, config, orchestrator]

# Dependency graph
requires: []
provides:
  - "ToolRegistry::orchestrator() — restricted tool set for orchestrator mode (read, grep, find, agent, list_agents, stop_agent, send_message)"
  - "system_prompt::build_orchestrator() — coordinator prompt encoding the understand/plan/dispatch/monitor/verify/report workflow"
  - "build::compose_system_prompt_mode(.., orchestrator: bool) — mode-aware prompt composer; compose_system_prompt delegates with false"
  - "config::ExperimentalConfig { orchestrator: bool } — [experimental] config section, defaults false"
affects: [06-02, 06-03]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Byte-identical snapshot tests captured BEFORE any change, to pin default behavior while adding a new mode"
    - "Restricted tool registries are hand-registered constructors, never derived from a broader set by filtering"

key-files:
  created: []
  modified:
    - src/tool/mod.rs
    - src/agent/system_prompt.rs
    - src/agent/build.rs
    - src/config.rs

key-decisions:
  - "ls is deliberately excluded from ToolRegistry::orchestrator() (research open question 1, resolved conservatively per D-03/ROADMAP SC#2); adding it later is a one-line change"
  - "compose_system_prompt is now a one-line delegation to compose_system_prompt_mode(.., false), so every existing caller is untouched by construction (ORC-04)"
  - "ExperimentalConfig is TUI-only by convention; print mode's one-line stderr note is deferred to 06-02/06-03 since no UI wiring happens in this plan"

patterns-established:
  - "New prompt variants are new sibling functions (build_orchestrator next to build), never edits to the existing default-path function"

requirements-completed: [ORC-02, ORC-03, ORC-04]

# Metrics
duration: 35min
completed: 2026-10-04
---

# Phase 06 Plan 01: Orchestrator Mode Building Blocks Summary

**Restricted ToolRegistry::orchestrator() (7 tools, no write/edit/bash/ls), a coordinator system prompt encoding the plan-dispatch-monitor-verify-report workflow, a mode-aware compose_system_prompt_mode() that leaves the default path byte-identical, and an `[experimental] orchestrator` config flag defaulting to false.**

## Performance

- **Duration:** ~35 min
- **Completed:** 2026-10-04
- **Tasks:** 2
- **Files modified:** 4 (src/tool/mod.rs, src/agent/system_prompt.rs, src/agent/build.rs, src/config.rs)

## Accomplishments
- `ToolRegistry::orchestrator()` registers exactly `agent, find, grep, list_agents, read, send_message, stop_agent` by hand (never derived from `standard()`/`standard_with_control()` by filtering), with a test pinning the exact name list and `all_specs().len() == 7`.
- Two ORC-04 byte-identical baseline snapshot tests were written and committed green *before* any non-test code changed: `standard_with_control_specs_byte_identical_to_baseline` (sorted `all_specs()` JSON) and `default_prompt_byte_identical_to_v0_12_baseline` (default `compose_system_prompt` output, cwd substituted with `{CWD}`).
- `system_prompt::build_orchestrator(cwd, tool_names)` is a new sibling of `build()` encoding: understand → plan → dispatch → monitor → verify → report workflow (D-08), plan-confirmation rule for ambiguous/risky work (D-05), fewest-agents judgment and `max_concurrency` as a ceiling (D-06), worktree isolation for parallel code-writing agents (D-07), self-contained briefs (D-09), and an explicit statement that write/edit/bash are unavailable.
- `build::compose_system_prompt_mode(cwd, tool_names, skills, no_context_files, overrides, orchestrator)` now owns the prompt-selection match; `compose_system_prompt` is a one-line call with `orchestrator: false`, so all existing callers (`build.rs:355`, `build.rs:489`, and every existing test) are untouched.
- `config::ExperimentalConfig { orchestrator: bool }` added under `Config.experimental`, wired through `builtin_defaults()` and `merge()` (project section wins if it differs from default, matching the `AgentConfig` pattern), defaulting to `false`.

## Task Commits

Each task was committed atomically (TDD: RED-then-GREEN split further into "snapshot tests first" then "implementation"):

1. **Task 1 — snapshot tests (RED, pre-change baseline)** - `6b5078d` (test)
2. **Task 1 — ToolRegistry::orchestrator() + exclusion test (GREEN)** - `ab43225` (feat)
3. **Task 2 — build_orchestrator prompt, compose_system_prompt_mode, ExperimentalConfig** - `9011e4e` (feat)

**Plan metadata:** pending (this commit)

## Files Created/Modified
- `src/tool/mod.rs` - `ToolRegistry::orchestrator()` constructor + baseline snapshot test + exclusion test
- `src/agent/system_prompt.rs` - `build_orchestrator()` coordinator prompt + its tests
- `src/agent/build.rs` - `compose_system_prompt_mode()`, `compose_system_prompt` thin delegation, snapshot test, mode tests
- `src/config.rs` - `ExperimentalConfig`, `Config.experimental`, `builtin_defaults`/`merge` wiring, default-flag test

## Decisions Made
- Spec ordering from `all_specs()` is non-deterministic (backed by a `HashMap`), so both snapshot tests sort before serializing/comparing, as anticipated by the plan.
- Kept `build_orchestrator`'s wording at Claude's discretion per the plan, verified against the required behavior list via two targeted tests rather than prescribing exact copy.

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered
None.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- The pure building blocks (restricted registry, coordinator prompt, mode-aware composer, config flag) are in place and unit-tested; 06-02/06-03 can now wire TUI/print-mode UI on top of them without touching the default (non-orchestrator) path, which is pinned by the two baseline snapshot tests.
- No blockers.

---
*Phase: 06-orchestrator-mode*
*Completed: 2026-10-04*

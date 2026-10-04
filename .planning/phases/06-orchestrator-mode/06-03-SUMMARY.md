---
phase: 06-orchestrator-mode
plan: 03
subsystem: print-mode
tags: [orchestrator, print-mode, config, integration-tests]

# Dependency graph
requires: ["06-01"]
provides:
  - "print mode stderr note when experimental.orchestrator is set and ignored (D-01)"
  - "real-binary integration tests pinning the note's exact shape and the untouched registry"
affects: []

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Config flags read for diagnostics must never gate registry/prompt construction; the read and the branch are kept textually separate so a reviewer can see the flag is inert"
    - "Proving a tool is still available is done by having the fake model actually call it and checking the side effect on disk, not by inspecting the request body — resilient to request-shape changes"

key-files:
  created: []
  modified:
    - src/mode/print.rs
    - tests/agent_spawn.rs

key-decisions:
  - "The note is unconditional (not gated on verbose/quiet), per D-01's conservative resolution of research open question 2 — a scripted -p run must never be silently restricted without a trace"
  - "No CLI flag was added, as the plan specified; the only control is the config key itself"
  - "Tests live in tests/agent_spawn.rs (per plan), reusing print_mode_e2e.rs's SSE-server/tool-call-delta helper shapes rather than importing cross-file, since integration test binaries are independent crates"

patterns-established: []

requirements-completed: [ORC-01, ORC-04]

# Metrics
duration: 25min
completed: 2026-10-04
---

# Phase 06 Plan 03: Print Mode Ignores Orchestrator Summary

**`-p` now emits exactly one unconditional stderr line — `note: experimental.orchestrator is set but ignored in print mode (-p)` — when `[experimental] orchestrator = true` is configured, while never branching its tool registry or system prompt on that flag; proven against the real binary by having a fake model call `write` and checking the file lands on disk.**

## Performance

- **Duration:** ~25 min
- **Completed:** 2026-10-04
- **Tasks:** 1
- **Files modified:** 2 (src/mode/print.rs, tests/agent_spawn.rs)

## Accomplishments

- `src/mode/print.rs`: right after `cfg_for_build` is loaded (the one place `-p` re-reads `config.toml` for flags not threaded through the function signature), a single `if !child.agent_mode && cfg_for_build.experimental.orchestrator` guard emits the note via `eprintln!`. It is read-only — nothing downstream of it (the `ToolRegistry` construction just above, or the prompt composition inside `AgentBuildInputs`) consults `cfg_for_build.experimental.orchestrator` at all, so the flag has zero effect on an actual `-p` run beyond this one line. `child.agent_mode` (agent children) are excluded, matching the plan's "Agent children never print the note" truth.
- `tests/agent_spawn.rs` gained two real-binary integration tests:
  - `print_mode_warns_when_orchestrator_config_set`: runs `nanopi -p` twice against a fake SSE endpoint, once with `.nanopi/config.toml` containing `[experimental]\norchestrator = true` and once without. Asserts the stderr line count matching the note text is exactly 1 in the first run and 0 in the second.
  - `print_mode_ignores_orchestrator_config`: with the same config set, scripts a two-round SSE exchange where the fake model calls the `write` tool, then asserts `proof.txt` actually lands on disk with the expected content — direct proof that `write` (not in `ToolRegistry::orchestrator()`'s 7-tool set) is still registered and executable in print mode regardless of the config flag.
  - Both tests also added two small test-only helpers to `tests/agent_spawn.rs` mirroring `print_mode_e2e.rs`'s style: `spawn_sse_server_seq` (scripted multi-response fake server) and `tool_call_delta` (one streamed tool-call delta in `WireToolCall` shape), since `agent_spawn.rs` previously only had the single-response `spawn_sse_server`.

## Task Commits

1. **Task 1 — print.rs note + two integration tests** - `a4cdef3` (feat)

**Plan metadata:** this commit

## Files Created/Modified

- `src/mode/print.rs` — one-line stderr note, gated only on `!child.agent_mode && cfg_for_build.experimental.orchestrator`, placed after the existing `cfg_for_build` load and before registry/prompt-affecting code.
- `tests/agent_spawn.rs` — `spawn_sse_server_seq`, `tool_call_delta`, `run_p_capture` helpers; `print_mode_warns_when_orchestrator_config_set`; `print_mode_ignores_orchestrator_config`.

## Decisions Made

- Kept the note unconditional rather than gating it behind an existing verbose/quiet flag — `-p` already calls `crate::render::notice::set_quiet(true)` to suppress TUI-style startup chrome, but this note is a user-config contradiction warning, not decorative chrome, so it bypasses that suppression by construction (it never goes through the `notice` module at all).
- Verified "still available" via an actual `write` tool call completing (file on disk) rather than capturing/parsing the outbound HTTP request body for `tools: [...]` — simpler, more resilient to wire-format changes, and directly observable as the plan's fallback option suggested.

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None.

## User Setup Required

None.

## Next Phase Readiness

- D-01 (print mode never restricted by orchestrator config) is now implemented and pinned by real-binary tests. 06-04 (remaining orchestrator surface) can proceed without any further changes to `mode/print.rs`.
- No blockers.

---
*Phase: 06-orchestrator-mode*
*Completed: 2026-10-04*

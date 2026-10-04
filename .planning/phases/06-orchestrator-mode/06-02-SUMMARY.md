---
phase: 06-orchestrator-mode
plan: 02
subsystem: tui
tags: [orchestrator, tool-registry, system-prompt, status-line, slash-command]

# Dependency graph
requires: ["06-01"]
provides:
  - "SlashCmd::Orchestrator / KeyAction::SetOrchestrator(Option<bool>) / KeyAction::OrchestratorUsage(String) — /orchestrator toggle, on/off, usage-on-bad-arg"
  - "App.orchestrator: bool — single source of truth for whether orchestrator mode is live, seeded from config.experimental.orchestrator at startup"
  - "apply_orchestrator_mode(app, agent, on) — in-place swap of agent.registry + agent.context.tools + system prompt base between the standing registry and ToolRegistry::orchestrator()"
  - "Status-line ' · ⎈ orchestrator' segment, shown iff App.orchestrator"
affects: [06-03, 06-04]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Lightweight in-place mode swap (registry + context.tools + system_base) instead of a full Agent rebuild, since provider/session/cwd don't change"
    - "App.saved_registry stashes the pre-swap registry only on the OFF->ON edge (never re-stashed on ON->ON), so repeated toggling never loses the real original"
    - "Rebuild sites reset saved_registry to None before re-applying the mode, so /new, /resume, /import, /fork rebuild against their freshly-built registry, not a stale pre-rebuild one"

key-files:
  created: []
  modified:
    - src/mode/tui.rs
    - src/command.rs

key-decisions:
  - "ToolRegistry already derives Clone (HashMap<String, Arc<dyn Tool>> + Option<HashSet<String>> are both Clone-safe since Arc<dyn Tool> clones the Rc, not the trait object) — no need for the plan's fallback of storing tools_allow and rebuilding"
  - "Added \"orchestrator\" to command::RESERVED_COMMAND_NAMES (Rule 1/2 fix): the hand-maintained reserved list has its own sync-guard test (reserved_command_names_match_the_builtin_palette) and would have let a plugin shadow the new built-in command"
  - "/model's SwapModel handler never rebuilds the registry (only provider+model), so orchestrator mode already persists across /model with no extra wiring"

patterns-established:
  - "A mode toggle that must survive Agent rebuilds re-applies itself at every rebuild site rather than being threaded through AgentBuildInputs, keeping the mode concern out of the build path entirely"

requirements-completed: [ORC-01, ORC-05]

# Metrics
duration: 40min
completed: 2026-10-04
---

# Phase 06 Plan 02: Orchestrator Mode TUI Wiring Summary

**`/orchestrator` slash command (toggle / on / off / usage-on-bad-arg) wired to an in-place registry+prompt swap helper that never touches `AgentRegistry`, applied at startup from config and re-applied at all four Agent-rebuild sites, plus a status-line `⎈ orchestrator` indicator.**

## Performance

- **Duration:** ~40 min
- **Completed:** 2026-10-04
- **Tasks:** 2
- **Files modified:** 2 (src/mode/tui.rs, src/command.rs)

## Accomplishments

- `SlashCmd::Orchestrator` added to the palette (`/orchestrator`), dispatched via `parse_orchestrator_args`: bare → `KeyAction::SetOrchestrator(None)` (toggle), `on`/`off` → `Some(true)`/`Some(false)`, anything else → `KeyAction::OrchestratorUsage(msg)` with zero state change (T-06-05).
- `App.orchestrator: bool` is the single source of truth (T-06-06): seeded at startup from `cfg_for_build.experimental.orchestrator`, read by both the toggle handler and the status-line segment, and swapped into the initial agent before the first turn when on by default.
- `apply_orchestrator_mode(app, agent, on)` is the one place that mutates `agent.registry`, `agent.context.tools`, and the system prompt base (via `agent.set_system_base` → `refresh_system_prompt`, the same path `build_fresh`/`hydrate_resumed` use). It never touches `AgentRegistry` — background/child agents are structurally unreachable from this function. `App.saved_registry: Option<ToolRegistry>` stashes the pre-swap registry only on the off→on edge (guarded by `is_none()`), so toggling on twice in a row does not overwrite the real original with the orchestrator set — verified by `orchestrator_toggle_on_twice_is_idempotent`.
- The four rebuild sites (`/new` at `Agent::build_fresh`, `/resume`/`/import`/`/fork` at `hydrate_resumed`) reset `app.saved_registry = None` and re-apply the mode to the freshly built registry when `app.orchestrator` is true, so the mode survives every path that can replace the live Agent. `/model`'s `SwapModel` handler never rebuilds the registry, so it needs no extra wiring.
- Status line (`draw_dock`'s line 2) gains a ` · ⎈ orchestrator` segment next to `think:`/`vendor:`, present iff `app.orchestrator`.
- `command::RESERVED_COMMAND_NAMES` gained `"orchestrator"` so a WASM plugin cannot register a command that shadows the new built-in — caught immediately by the pre-existing sync-guard test `reserved_command_names_match_the_builtin_palette`, which is designed to fail exactly this way when a built-in command is added without updating the list.

## Task Commits

1. **Task 1 + Task 2 (toggle, swap helper, rebuild sites, status segment, tests)** - `6ca27dd` (feat) — committed as one atomic commit; both tasks touch the same small set of anchors in `src/mode/tui.rs` and were implemented and verified together.

**Plan metadata:** pending (this commit)

## Files Created/Modified

- `src/mode/tui.rs` — `SlashCmd::Orchestrator`, `parse_orchestrator_args`/`ORCHESTRATOR_USAGE`, `KeyAction::SetOrchestrator`/`OrchestratorUsage`, `App.orchestrator`/`App.saved_registry` fields + constructor init, `apply_orchestrator_mode` helper, startup seeding, `handle_action` arms, orchestrator re-application at the 4 rebuild sites, status-line segment, 6 new tests.
- `src/command.rs` — `"orchestrator"` added to `RESERVED_COMMAND_NAMES`.

## Decisions Made

- `ToolRegistry` turned out to already derive `Clone` (confirmed by reading `src/tool/mod.rs`), so the plan's fallback ("if ToolRegistry is not Clone, store the inputs needed to rebuild it") was unnecessary — the helper simply clones the live registry into `App.saved_registry`.
- Test assertions on `registry.names()` sort both sides before comparing, since the registry is `HashMap`-backed and iteration order is not stable (same pattern 06-01 used for its snapshot tests).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1/2 - missing correctness guard] Added "orchestrator" to `command::RESERVED_COMMAND_NAMES`**
- **Found during:** full-suite verification after Task 2
- **Issue:** The new `/orchestrator` built-in command was not in the hand-maintained reserved-name list, so a WASM plugin could have registered a command named `orchestrator` and shadowed the built-in. The pre-existing test `reserved_command_names_match_the_builtin_palette` exists specifically to catch this and failed immediately.
- **Fix:** Added `"orchestrator"` to `RESERVED_COMMAND_NAMES` in `src/command.rs`.
- **Files modified:** `src/command.rs`
- **Commit:** `6ca27dd`

## Issues Encountered

None beyond the reserved-name gap above, which the existing test caught immediately.

## User Setup Required

None.

## Next Phase Readiness

- ORC-01 (toggle) and ORC-05 (status line) are now live in the TUI; ORC-02 (restricted tools) was already covered structurally by 06-01 and is now exercised live through the toggle.
- 06-03/06-04 (print-mode wiring, remaining orchestrator surface) can build on `apply_orchestrator_mode` and `App.orchestrator` without further changes to the registry/prompt composition path.
- No blockers.

---
*Phase: 06-orchestrator-mode*
*Completed: 2026-10-04*

## Self-Check: PASSED

---
phase: 05-tui-agents-strip
plan: 02
subsystem: ui
tags: [keybindings, tui, agents, docs]

requires:
  - phase: 05-tui-agents-strip
    plan: 01
    provides: AgentsView cache, glyph_for, collapsed_rows, strip_height, draw_agents_strip, expanded_detail_lines (pure, IO-free agents-strip renderer)
provides:
  - ActionId::ToggleAgentsStrip registered in src/keys.rs with default binding Ctrl+G, label "Toggle agents strip", and settings_toml persisted-name mapping ("toggle_agents_strip")
  - docs/agents.md "Agents strip (TUI)" section plus a Chinese/English manual test row set for Ctrl+G
affects: [05-03-tui-dock-wiring, phase-6-manual-test-plan]

tech-stack:
  added: []
  patterns:
    - "New ActionId variants require matching entries in three places: keys.rs (all()/label()/default bindings) and settings_toml.rs's action_toml_key() persisted-name match — grep for an existing variant like ExpandLastTool to find every site"

key-files:
  created: []
  modified:
    - src/keys.rs
    - src/settings_toml.rs
    - docs/agents.md
    - tests/agent_runtime.rs

key-decisions:
  - "sc_no_agent_ui_controls (a pre-Phase-5 invariant test asserting src/keys.rs never mentions \"agent\") was narrowed rather than deleted: it now permits only ToggleAgentsStrip-related lines and still fails on any other agent-control keybinding, preserving its original intent (no stop/approve/deny/message controls) per the 05-CONTEXT.md revision that scopes the strip to display-only"
  - "settings_toml.rs's action_toml_key() got a 'toggle_agents_strip' arm alongside the enum change, since it is the persisted-name mapping path the interfaces section flagged as needing a matching entry"

requirements-completed: [UI-02]

duration: 25min
completed: 2026-10-04
---

# Phase 5 Plan 2: Ctrl+G keybinding and docs Summary

**Registered `ActionId::ToggleAgentsStrip` with a default Ctrl+G binding (rebindable through the existing settings/keybindings path) and documented the agents strip with a bilingual manual test row for Ctrl+G.**

## Performance

- **Duration:** ~25 min
- **Completed:** 2026-10-04T16:27:09Z
- **Tasks:** 2
- **Files modified:** 4

## Accomplishments

- `ActionId::ToggleAgentsStrip` added to `src/keys.rs`: included in `all()`, labelled "Toggle agents strip", bound to `Ctrl+G` in `KeyBindings::default()`
- `src/settings_toml.rs::action_toml_key()` gained the `toggle_agents_strip` arm so the persisted-name round trip works the same way `ExpandLastTool` does
- Three new unit tests in `src/keys.rs`: default Ctrl+G matches and Ctrl+O still only matches `ExpandLastTool`; no two default bindings share a `KeySpec` (T-05-04 DoS mitigation); rebind round-trip via `overrides()`/`from_overrides()` proving the old Ctrl+G stops matching once rebound
- `docs/agents.md` gained an "Agents strip (TUI)" section (placement, glyph table minus a "waiting for permission" row since children never prompt, collapsed/expanded behavior, narrow/short-terminal behavior, configurability) and a bilingual (前提/步骤/期望/结果) manual test table with 6 rows covering dispatch-and-appear, expand, collapse, persistence after finish, no idle flicker, and typing `g` without Ctrl
- Updated the stale top-of-file claim in `docs/agents.md` ("no keybinding ... for agents") to reflect the new display-only toggle

## Task Commits

1. **Task 1: Add ActionId::ToggleAgentsStrip (default Ctrl+G)** - `cd7d3b0` (feat)
2. **Task 2: Document the strip and add the manual test row** - `94cc519` (docs)
3. **Deviation fix: narrow sc_no_agent_ui_controls** - `a62b0d9` (fix)

## Files Created/Modified

- `src/keys.rs` - new `ActionId::ToggleAgentsStrip` variant, default Ctrl+G binding, 3 new tests
- `src/settings_toml.rs` - `action_toml_key()` arm for the new variant
- `docs/agents.md` - new "Agents strip (TUI)" section + manual test table; corrected stale "no keybinding" sentence
- `tests/agent_runtime.rs` - narrowed `sc_no_agent_ui_controls` to permit the new display-only toggle while still rejecting any other agent-control keybinding

## Decisions Made

- `sc_no_agent_ui_controls` narrowed, not deleted or weakened wholesale: it now parses `src/keys.rs` line-by-line and only tolerates lines mentioning "agent" that also mention `toggleagentsstrip`/`toggle_agents_strip`. Any other agent-control keybinding (stop/approve/deny/message) added later will still fail this test, preserving its original guard.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Pre-existing invariant test went stale against this plan's explicit change**
- **Found during:** full-suite verification after Task 1
- **Issue:** `tests/agent_runtime.rs::sc_no_agent_ui_controls` asserted `src/keys.rs` never contains the substring "agent" (case-insensitive) — a pre-Phase-5 invariant from when agents had zero TUI surface. Adding `ActionId::ToggleAgentsStrip` (required by this plan and by 05-CONTEXT.md's D-04/UI-02) necessarily made that assertion fail.
- **Fix:** Narrowed the test to tolerate only lines referencing `ToggleAgentsStrip`/`toggle_agents_strip`, keeping the test's actual intent (no *control* keybinding for agents — no stop/approve/deny/message) intact, per the 05-CONTEXT.md revision that scopes the strip to display-only.
- **Files modified:** `tests/agent_runtime.rs`
- **Commit:** `a62b0d9`

## Issues Encountered

None beyond the test-invariant deviation above.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

- `ActionId::ToggleAgentsStrip` and its default Ctrl+G binding are ready for 05-03 to wire into `src/mode/tui.rs`'s key-handling and `draw_dock`, calling the 05-01 renderer (`draw_agents_strip`, `strip_height`, `expanded_detail_lines`).
- The manual test row in `docs/agents.md` is ready for Phase 6 to fold into the consolidated manual test plan (QA-01).
- No blockers.

---
*Phase: 05-tui-agents-strip*
*Completed: 2026-10-04*

## Self-Check: PASSED

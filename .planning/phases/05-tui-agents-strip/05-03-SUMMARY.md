---
phase: 05-tui-agents-strip
plan: 03
subsystem: ui
tags: [ratatui, tui, agents, keybindings]

requires:
  - phase: 05-tui-agents-strip
    plan: 01
    provides: AgentsView, StripOpts, draw_agents_strip, strip_height, expanded_detail_lines (pure, IO-free agents-strip renderer)
  - phase: 05-tui-agents-strip
    plan: 02
    provides: ActionId::ToggleAgentsStrip registered with default Ctrl+G binding
provides:
  - Agents strip wired into the live TUI dock between the status line and the input box
  - App.agents_view/agents_strip_expanded/term_rows, tick-only refresh (D-07/UI-04), Ctrl+G toggle, Esc collapse
  - agents_detail_block pure helper feeding the Ctrl+G scrollback detail print
affects: [phase-6-manual-test-plan]

tech-stack:
  added: []
  patterns:
    - "Strip rows come out of MAX_INPUT_LINES' own budget, not DOCK_HEIGHT: ratatui 0.29's Viewport::Inline(height) is fixed at Terminal construction and Terminal::resize recomputes with the ORIGINAL height, so no code path can grow the 10-row inline dock per frame"
    - "Registry reads happen ONLY in the tick arm of the main select! loop — on_agent_event and interpret_key never touch AgentsView::refresh, keeping the strip provably off the hot key-handling and event paths (T-05-07)"

key-files:
  created: []
  modified:
    - src/mode/tui.rs

key-decisions:
  - "D-04 deviation (plan-level, pre-approved by the owner in 05-03-PLAN.md's rendering_decision before this plan executed): the '~40% of screen' expanded view cannot grow the live dock under the pinned ratatui 0.29 Viewport::Inline. Ctrl+G instead prints the full per-agent detail block into scrollback (same insert_line mechanism as Ctrl+O's ExpandLastTool) and switches the in-dock rows to each agent's latest activity line; the terminal's own scrollback makes the detail view unbounded and scrollable rather than capped at 40% of the screen."
  - "Esc-collapse check is gated on turn_started_at/tool_started_at being None (not app.status), placed BEFORE the streaming-interrupt and double-Esc checks so it consumes the key without arming the fork-picker timer, exactly per the plan's interface note."

requirements-completed: [UI-01, UI-02, UI-03, UI-04]

duration: 45min
completed: 2026-10-04
---

# Phase 5 Plan 3: TUI dock wiring (strip, Ctrl+G, Esc collapse) Summary

**Wired the pure Phase-5 agents-strip renderer into the live TUI dock: the strip appears between the status line and the input box only when agents exist, refreshes exclusively on the 120ms tick, and Ctrl+G/Esc give a display-only expand (into scrollback)/collapse with zero agent-control reachability from any strip key.**

## Performance

- **Duration:** ~45 min
- **Completed:** 2026-10-04T16:50:00Z
- **Tasks:** 2
- **Files modified:** 1

## Accomplishments

- `App` gained `agents_view: AgentsView`, `agents_strip_expanded: bool`, `term_rows: u16` (defaults to 24, kept current each loop iteration via `term.size()`)
- `draw_dock` computes `strip_h` from `agents_strip::strip_height` (0 when empty, header-only under an open overlay or a short terminal), reduces the input box's content-line budget by that amount (never below 1), and renders the strip in its own `Length(strip_h)` row between the status line and the input box — all downstream chunk indices shifted accordingly
- The main loop's tick arm is the ONLY call site of `AgentsView::refresh`, reading `crate::agent_registry::global().snapshot()`; the redraw condition now also fires on a non-empty view and once when the view transitions from non-empty to empty (so the strip disappears cleanly instead of leaving a stale frame)
- `ActionId::ToggleAgentsStrip` (Ctrl+G, from 05-02) now flips `agents_strip_expanded` (no-op with zero agents) and, when becoming expanded, prints a dim `── agents ──` rule plus `agents_detail_block`'s lines into scrollback via the same `insert_line` path `ExpandLastTool` uses
- Esc collapses the expanded strip when idle (`turn_started_at`/`tool_started_at` both `None`), consuming the key before it can arm the double-Esc fork-picker timer; with a turn or tool running, Esc keeps its existing cancel/interrupt behavior unchanged
- `agents_detail_block(&App, width) -> Vec<String>` is a pure wrapper over `agents_strip::expanded_detail_lines`, reusing that module's own control-character sanitization (T-05-05)
- 21 new tests in `mode::tui::tests` (strip presence/absence, input-budget shrinkage, overlay precedence, short-terminal header-only, deterministic re-render, Ctrl+G toggle incl. no-op and input-buffer safety, Esc collapse-vs-cancel, expanded detail content, and a real-`AgentRegistry`-backed negative test proving no strip key mutates agent state)

## Task Commits

1. **Task 1 + Task 2 (App state/dock layout/tick refresh, and Ctrl+G/Esc handling)** — `c9fc1ff` (feat)

_Note: both tasks landed in one commit. They share the same file (`src/mode/tui.rs`) and Task 2's behavior tests build directly on Task 1's new `App` fields and `draw_dock` layout, so splitting the diff into two commits would have required re-deriving which already-applied hunks belonged to which task rather than writing them as two independent patches. Both tasks' behavior tests and the full suite were verified green before committing._

## Files Created/Modified

- `src/mode/tui.rs` — `App.agents_view`/`agents_strip_expanded`/`term_rows`; `draw_dock` strip row + shrunk input budget; tick-arm-only `AgentsView::refresh`; `KeyAction::ToggleAgentsStrip`/`CollapseAgentsStrip`; `interpret_key` Ctrl+G and Esc-collapse checks; `handle_action` arms; `agents_detail_block` helper; 21 new tests

## Decisions Made

- See `key-decisions` in frontmatter for the D-04 deviation (pre-approved at planning time, recorded here as the owner-visible note the plan asked for) and the Esc-collapse ordering rationale.
- `toggle_key_label` passed to `StripOpts` comes from `app.bindings.get(ActionId::ToggleAgentsStrip)` (falls back to the literal string `"Ctrl+G"` only if unbound), so a user rebind is reflected in the strip's own header text, not just in the keybindings menu.

## Deviations from Plan

### Pre-approved (D-04, plan-level)

**1. Expanded view prints to scrollback instead of growing a 40%-of-screen live pane**
- **Found during:** planning (recorded in 05-03-PLAN.md's `rendering_decision` before this plan executed); re-confirmed true during Task 2 implementation.
- **Issue:** ratatui 0.29 (pinned in Cargo.lock) fixes `Viewport::Inline(height)` at `Terminal::with_options` construction; `Terminal::resize` recomputes the inline area using the ORIGINAL height (ratatui-0.29.0/src/terminal/terminal.rs:212-240), and `set_viewport_area` is private. No code path in the pinned version can grow the live dock past `DOCK_HEIGHT` per frame.
- **Fix:** Ctrl+G toggles `agents_strip_expanded` and prints `agents_detail_block`'s lines into scrollback through the existing `insert_line` path (same mechanism as Ctrl+O's `ExpandLastTool`); the in-dock rows switch to each agent's latest activity line instead of growing the dock. Scrollback is naturally unbounded and scrollable, which the plan's owner accepted as the substitute for "~40% of screen."
- **Files modified:** `src/mode/tui.rs`
- **Commit:** `c9fc1ff`

No other deviations — Task 1 and Task 2 otherwise executed as written.

## Issues Encountered

None beyond the ratatui 0.29 viewport constraint already anticipated and resolved in the plan itself.

## User Setup Required

None — no external service configuration required.

## Next Phase Readiness

- Phase 5 (UI-01..04) is now fully wired end to end: pure model (05-01) + keybinding (05-02) + live dock (05-03).
- The `<human-check>` manual pass from 05-03-PLAN.md's `<verification>` (dispatch 1 then 5 background agents; strip appears, "+K more" shows, Ctrl+G prints detail and toggles, Esc collapses, no idle flicker, finished agents show ✓) is ready for the owner to run; it folds into the bilingual manual test table 05-02 already added to `docs/agents.md`.
- No blockers.

---
*Phase: 05-tui-agents-strip*
*Completed: 2026-10-04*

## Self-Check: PASSED

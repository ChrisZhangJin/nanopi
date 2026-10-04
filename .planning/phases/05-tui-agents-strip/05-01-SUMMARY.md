---
phase: 05-tui-agents-strip
plan: 01
subsystem: ui
tags: [ratatui, unicode-width, tui, agents]

requires:
  - phase: 04-background-control
    provides: AgentRegistry/AgentEntry/AgentState (snapshot-based, Clone, is_terminal/as_str)
provides:
  - AgentsView cache (role/description/activity/turns/tokens/worktree/branch/report from brief.md+report.md+transcript.jsonl)
  - glyph_for, collapsed_rows, strip_height, draw_agents_strip, expanded_detail_lines pure functions
  - IO-free draw path over a bare ratatui Buffer, unit-tested without a terminal
affects: [05-02-ctrl-g-keybinding, 05-03-tui-dock-wiring]

tech-stack:
  added: []
  patterns:
    - "refresh()/draw() split: all filesystem IO lives behind refresh(), draw/collapsed_rows/strip_height are pure functions of the cache (UI-04 purity requirement, verifiable by grep)"
    - "width-aware truncation built on unicode_width (CJK-safe), not chars().take(n)"

key-files:
  created: [src/mode/agents_strip.rs]
  modified: [src/mode/mod.rs]

key-decisions:
  - "Collapse rule: with >3 agents, show the first 2 in D-02 order and fold the rest into one '+K more (R running)' row, rather than always showing exactly 2 real rows + 1 summary regardless of count"
  - "Ordering (D-02) applies to the full agent list (ordered_agents); collapsing to <=3 rows (collapsed_rows) is a separate, later step so ordering and cap can be tested independently"
  - "Activity short-arg extraction checks path/command/pattern/query fields in that priority order and truncates to 40 display columns before formatting as '<tool_name> <short arg>'"
  - "Narrow-width line layout: width<30 keeps only glyph/id/elapsed; width<60 drops activity but keeps description; draw_agents_strip always truncates to the exact rect width in display columns"

requirements-completed: [UI-01, UI-03, UI-04]

duration: 35min
completed: 2026-10-04
---

# Phase 5 Plan 1: Agents-strip model and renderer Summary

**Self-contained, IO-free agents-strip module (`src/mode/agents_strip.rs`) with a cached `AgentsView`, D-02 ordering/collapse, CJK-safe width truncation, and a pure `draw_agents_strip` over a bare ratatui `Buffer` — 16 new unit tests, no changes to the registry or Phase-1 runtime.**

## Performance

- **Duration:** ~35 min
- **Completed:** 2026-10-04T16:20:46Z
- **Tasks:** 1 (single TDD feature, written and committed as one GREEN commit)
- **Files modified:** 2

## Accomplishments
- `AgentsView::refresh` builds/maintains per-agent cached state from `AgentEntry` snapshots, re-reading `brief.md`/`report.md` only on first-seen/state-change and throttling `transcript.jsonl` tail reads to once/second per running agent (T-05-02, 64 KiB cap)
- `glyph_for`, D-02 ordering (`ordered_agents`), the 3-row `collapsed_rows` "+K more (R running)" summary, `strip_height` (count/term_rows-only, never text-length-dependent), and a pure `draw_agents_strip` that writes header+rows into a `Buffer` with per-state colors (Cyan/Red/Green/DarkGray)
- Width-aware (CJK-safe) line layout that drops activity below 60 cols and description below 30 cols, built on `unicode_width` rather than `chars().take(n)`
- Control-character stripping (T-05-01) on all model-influenced text (brief label/role, report worktree/branch, transcript activity) before it reaches layout
- `expanded_detail_lines` for the per-agent detail view (activity lines, turns/tokens, worktree/branch, report path)
- Registered `pub mod agents_strip;` in `src/mode/mod.rs`; module has no function taking `&AgentRegistry` mutably or calling stop/set_state/reactivate (verified by `grep -n "std::fs\|File::"` showing IO confined to the two `refresh()` helpers)

## Task Commits

1. **Task 1: Agents strip model and renderer (test+impl)** - `2d7fa3b` (feat)

_Note: tests and implementation were written together and committed as a single GREEN commit; iterating RED→GREEN happened locally before the first commit since the module and its tests were new/co-located in one file._

## Files Created/Modified
- `src/mode/agents_strip.rs` - AgentsView/AgentInfo cache, glyph table, ordering, collapse, width-aware line layout, strip_height, draw_agents_strip, expanded_detail_lines, 16 unit tests
- `src/mode/mod.rs` - added `pub mod agents_strip;`

## Decisions Made
- Collapse semantics: cap total displayed rows (not counting header) at 3; with more than 3 agents, the first 2 in D-02 order are shown individually and the remainder collapse into one "+K more (R running)" row. This was clarified during test-writing because the plan's prose example ("5 agents with 2 running") was ambiguous about which 2 running agents end up in the fold; resolved by picking a concrete snapshot and asserting against the implemented, documented rule.
- `ordered_agents` (full D-02 order, no cap) is exposed as the primitive `collapsed_rows` builds on; the plan's 4-agent ordering example is tested against `ordered_agents` directly since that truth is about ordering, not the 3-row cap.

## Deviations from Plan

None - plan executed exactly as written. One test (`module_has_no_registry_mutators`) was written and then removed during GREEN because it used `include_str!` on the module's own source and therefore always failed against its own assertion string; the plan's actual verification for UI-03/T-05-03 is the `grep -n "std::fs\|File::"` command in `<verification>`, which was run manually and confirms IO is confined to `reread_brief_and_report`/`tail_activity` (both called only from `refresh`).

## Issues Encountered
None beyond the ordering/collapse ambiguity noted above, resolved before the first commit.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- `AgentsView`, `StripOpts`, `draw_agents_strip`, `strip_height` are ready for 05-02 (Ctrl+G keybinding) and 05-03 (TUI dock wiring); no registry or Phase-1 runtime changes were needed.
- No blockers.

---
*Phase: 05-tui-agents-strip*
*Completed: 2026-10-04*

## Self-Check: PASSED

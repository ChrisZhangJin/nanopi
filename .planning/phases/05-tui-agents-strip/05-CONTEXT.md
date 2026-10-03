# Phase 5: TUI agents strip - Context

**Gathered:** 2026-10-03
**Status:** Ready for planning

<domain>
## Phase Boundary

A bottom strip in the TUI shows every subagent at a glance. It expands
with a shortcut to show details, and the user approves or denies
queued permission requests there, replacing the interim prompt from
Phase 1. Covers UI-01..04.

</domain>

<decisions>
## Implementation Decisions

### Layout (owner-chosen design)
- **D-01:** The strip sits between the status line and the input box,
  as a new row in `draw_dock` (`src/mode/tui.rs:5041-5098`). It is
  hidden when the current run has no agents.
- **D-02:** Collapsed, it is 1–3 lines: a header
  `agents (N) · Ctrl+G expand`, then one line per agent, at most 3.
  When there are more than 3 agents, the last line becomes
  `+K more (R running)`. Agents that need attention (waiting for
  permission, failed) come first, then running ones, then the most
  recently finished.
- **D-03:** Each agent line shows: state glyph, `#id`, description,
  the current activity or result, and the elapsed time on the right.
  The glyphs are:
  - `●` running, in colour
  - `◐` queued
  - `⚠` waiting for permission
  - `✓` done
  - `✗` failed
  - `■` stopped
  - `⏱` limit reached
  - `?` interrupted

  Lines are truncated to the terminal width.
- **D-04:** Expanded (Ctrl+G toggles it), the strip grows up to about
  40% of the screen height and lists every agent with:
  - its last 2–3 activity lines (tool calls)
  - its turns and tokens
  - its worktree and branch, if any
  - its report path

  It scrolls when the list is longer. Esc or Ctrl+G collapses it.
  Ctrl+G is free in the current keybindings; it is configurable.
- **D-05:** No stop or amend controls in the strip. The owner put them
  out of scope; agents are controlled through the main agent and
  tools.

### Permission approval (UI-03)
- **D-06:** When a request is pending, the strip shows
  `⚠ #a3 wants: bash "cargo test" — y allow · n deny · a always`. The
  oldest request is shown first. The keys work only while the input box
  is empty, or after focusing the strip with Ctrl+G, so typing is never
  hijacked. This replaces the Phase 1 interim inline prompt.

### Rendering
- **D-07:** The strip is drawn from a registry snapshot taken on the
  existing 120 ms tick, never once per event. Agent activity does not
  trigger redraws by itself.
- **D-08:** Narrow terminals (fewer than about 60 columns) drop the
  activity text first, then the description. Short terminals (fewer
  than about 15 rows) keep the strip at its header line only.

### Claude's Discretion
- Exact colours (follow the existing theme), and the activity-text
  format.

</decisions>

<specifics>
## Specific Ideas

The owner chose this mockup:

```
├ agents (3) ──────── Ctrl+G 展开 ──┤
│ ● #1 explore  读取 src/config  12s │
│ ● #2 coder    改写 loader     34s │
│ ✓ #3 tester   完成 · 报告已收到    │
```

Reference: Claude Code's `CoordinatorAgentStatus.tsx`.

</specifics>

<canonical_refs>
## Canonical References

- `.planning/REQUIREMENTS.md` — UI-01..04
- `src/mode/tui.rs` — `draw_dock` (5041-5098), the tick and
  `select!` loop (1828)
- `src/keys.rs`, `src/settings.rs` — keybindings
- `src/render/panel.rs`

</canonical_refs>

<code_context>
## Existing Code Insights

Ctrl+O is already used to expand tool output, and Ctrl+G is unused.
Lessons from v0.12: keybindings and controls must be reachable, so
every new key needs a manual end-to-end test row (QA-01).

</code_context>

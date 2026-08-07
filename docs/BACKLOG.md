# Deferred work

Things that are worth doing but explicitly deferred. Each entry
should describe (a) what it is, (b) why we deferred, (c) what
would trigger us to pick it up.

---

## Ctrl+O toggle (collapse back after expanding a tool card)

**Current state (v0.7)**: `Ctrl+O` expands the last tool result's
full output *once* into scrollback, styled with the same green/red
bg as the collapsed card. Pressing `Ctrl+O` again does nothing —
the expansion cannot be un-drawn.

**PI's behavior**: `setExpanded(true)` flips a boolean on a live
`ToolExecutionComponent`; the next render cycle re-materializes
the transcript in memory and PI's TUI framework (`tui-main-screen.ts`)
computes a line-level diff, emitting only ANSI cursor moves +
line rewrites for the changed rows. Collapse ↔ expand happens in
place with no scroll disruption.

**Why we can't just mirror it**: nanopi uses ratatui's inline
viewport with `insert_before(N, |buf| …)`, which is fundamentally
append-only against terminal scrollback. Once lines are in the
scrollback they belong to the terminal emulator, not us —
scrolling, selection, copy/paste all depend on that.

**What it would take**: move tool cards (and, ideally, all
per-turn assistant content) out of scrollback and into a
redrawable region owned by ratatui — a `Vec<TranscriptBlock>` in
`App` that `draw_dock` fully re-renders each frame. Estimated
~400 LOC + a rewrite of `on_agent_event` handlers. Not free.

**Trigger to revisit**:
- A user request that specifically needs toggle (not just "view
  full output" — that's already covered by expanding once and
  scrolling in the terminal).
- Or an unrelated refactor that already puts the transcript into
  a reactive component tree.

See conversation on 2026-08-07 for the full analysis.

---

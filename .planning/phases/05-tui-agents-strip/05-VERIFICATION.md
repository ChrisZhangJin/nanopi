---
phase: 05-tui-agents-strip
verified: 2026-10-04T17:10:00Z
status: human_needed
score: 4/4 must-haves verified
overrides_applied: 1
overrides:
  - must_have: "Expanded view grows the live dock to ~40% of screen height (D-04)"
    reason: "ratatui 0.29.0 (pinned in Cargo.lock) fixes Viewport::Inline(height) at Terminal::with_options construction; Terminal::resize recomputes the inline area using the ORIGINAL height (ratatui-0.29.0/src/terminal/terminal.rs:212-240) and Terminal::set_viewport_area is private — no code path in the pinned version can grow the live dock per frame. Substitute: Ctrl+G prints the full detail block into scrollback (same insert_line mechanism as Ctrl+O ExpandLastTool) and switches in-dock rows to each agent's latest activity line. User-approved per task instructions."
    accepted_by: "user (task instructions: 'D-04 deviation ... is user-approved — not a gap')"
    accepted_at: "2026-10-04T16:50:00Z"
human_verification:
  - test: "Run nanopi interactively, dispatch 1 agent then 4+ background agents via the `agent` tool"
    expected: "Strip appears between the status line and the input box; with 4+ agents it caps at 3 rows plus a '+K more (R running)' row"
    why_human: "Real terminal rendering and live agent dispatch cannot be exercised by unit tests over a bare ratatui Buffer"
  - test: "Press Ctrl+G while the strip is visible"
    expected: "Strip expands: a dim '── agents ──' rule plus the full per-agent detail block (activity, turns/tokens, worktree/branch, report path) is printed into scrollback; in-dock rows switch to showing latest activity per agent"
    why_human: "Scrollback insertion and visual detail-block formatting require a real terminal"
  - test: "Press Ctrl+G again, or Esc while idle, with the strip expanded"
    expected: "Strip collapses back to the 1-3 line summary"
    why_human: "Requires live key input against a running TUI session"
  - test: "Let all dispatched agents finish, then watch the strip for several seconds while idle"
    expected: "Strip remains visible showing ✓ for finished agents (does not vanish immediately); no flicker or redraw churn while idle"
    why_human: "Flicker/redraw-storm perception cannot be observed via Buffer-diffing (explicitly called out as manual-only in 05-VALIDATION.md); requires real frame timing"
  - test: "With the input box focused and no modifier held, type the letter 'g'"
    expected: "'g' is inserted into the input as a normal character; the strip does not toggle"
    why_human: "End-to-end keyboard-to-input-buffer behavior in a live terminal"
---

# Phase 5: TUI agents strip Verification Report

**Phase Goal:** The user can see every agent's state at a glance without leaving the conversation (display-only).
**Verified:** 2026-10-04T17:10:00Z
**Status:** human_needed
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths (ROADMAP Success Criteria, UI-01..04)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | With agents present, a 1-3 line strip above the input shows id, role, short task, state and elapsed time; disappears when none exist | VERIFIED | `src/mode/agents_strip.rs` `collapsed_rows`/`draw_agents_strip`/`strip_height` (0 when empty); wired at `src/mode/tui.rs:5369-5446` (`draw_dock` computes `strip_h` from `agents_view`, draws into `chunks[2]`); tests `agents_strip_hidden_when_empty`, `agents_strip_shows_entries` pass (`cargo test` run, all green) |
| 2 | Ctrl+G (or chosen free key) expands/collapses; expanded view shows latest activity and report path | VERIFIED (with user-approved deviation, see override) | `ActionId::ToggleAgentsStrip` registered `src/keys.rs:19,31,43,173` with default Ctrl+G; rebindable via `src/settings_toml.rs:172` `action_toml_key()`; wired in `src/mode/tui.rs:1317-1318` (interpret_key), `2317-2336` (handle_action flips `agents_strip_expanded`, prints `agents_detail_block` into scrollback on expand); `expanded_detail_lines` (`src/mode/agents_strip.rs:491`) includes activity, turns/tokens, worktree/branch, report path. Expanded view substitutes scrollback print for a live 40%-height pane (D-04, user-approved per task instructions) |
| 3 | The strip is display-only: no approve/stop/message actions | VERIFIED | `src/mode/agents_strip.rs` IO confined to `refresh()` helpers (grep `std::fs\|File::` shows only lines 138/150/199, all inside cache-refresh code, plus test-only writes ≥717); `sc_no_agent_ui_controls` test in `tests/agent_runtime.rs` narrowed (not weakened) to reject any agent-control keybinding other than the display-only toggle — passes; `strip_is_display_only_no_mutation`-style test in `src/mode/tui.rs` (~line 7837) feeds y/n/a/Delete/Enter/Ctrl+G/Esc against a real `AgentRegistry` and asserts no mutation — passes |
| 4 | The strip updates on the TUI tick from a registry snapshot, with no flicker or redraw storm under many agent events | PARTIALLY AUTOMATED / human needed for flicker | `AgentsView::refresh` is called only in the tick arm (`src/mode/tui.rs:2101-2112`), never in `on_agent_event` or `interpret_key` (grep confirms no other call sites); `strip_render_deterministic_across_ticks`-style test proves same-input same-output determinism. Actual "no visible flicker" is explicitly called out in `05-VALIDATION.md` as manual-only (ratatui `TestBackend` buffer-diffing cannot assert flicker) — routed to human verification |

**Score:** 4/4 must-haves verified at the code level (one carries a user-approved deviation; one has a residual manual-only sub-claim per the phase's own validation strategy, routed to human verification rather than treated as a gap)

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `src/mode/agents_strip.rs` | AgentsView cache, StripRow model, glyph table, ordering, layout, height calc, pure draw fn | VERIFIED | 33KB file; exports `glyph_for`, `AgentInfo`, `AgentsView`, `StripRow`, `collapsed_rows`, `strip_height`, `StripOpts`, `expanded_detail_lines`, `draw_agents_strip` all present (grep confirmed) |
| `src/mode/mod.rs` | `pub mod agents_strip;` | VERIFIED | line 3 |
| `src/keys.rs` | `ActionId::ToggleAgentsStrip` default Ctrl+G | VERIFIED | enum variant, `all()`, `label()`, default binding, 3+ tests present |
| `src/settings_toml.rs` | persisted-name mapping | VERIFIED | `action_toml_key()` arm `"toggle_agents_strip"` at line 172 |
| `docs/agents.md` | Agents strip section + manual test row(s) | VERIFIED | "Agents strip (TUI)" section at line 91; 6-row bilingual manual test table lines 135-140 with `☐通过 ☐失败` markers |
| `src/mode/tui.rs` | App state, dock layout, tick refresh, Ctrl+G/Esc handling, scrollback detail | VERIFIED | `agents_view`/`agents_strip_expanded`/`term_rows` fields (954-967); draw_dock wiring (5369-5446); tick-only refresh (2101-2112); Ctrl+G/Esc handling (1317-1318, 1381, 2317-2336) |

### Key Link Verification

| From | To | Via | Status | Details |
|------|-----|-----|--------|---------|
| tick arm of select! loop | `AgentsView::refresh` | `crate::agent_registry::global().snapshot()` on tick | WIRED | `src/mode/tui.rs:2101-2112`; confirmed no other refresh call sites via grep across the file |
| `draw_dock` | `crate::mode::agents_strip::draw_agents_strip` | layout row between status and input | WIRED | `src/mode/tui.rs:5446` |
| `interpret_key` | `ActionId::ToggleAgentsStrip` | `app.bindings.matches` | WIRED | `src/mode/tui.rs:1317-1318` |
| `handle_action` | `agents_detail_block` / scrollback `insert_line` | Ctrl+G expand | WIRED | `src/mode/tui.rs:2321-2336`, `5350` |

### Behavioral Spot-Checks / Automated Test Run

```
cargo test -- --test-threads=1
```

Result: **all green**. Totals across the run: lib suite `1020 passed; 0 failed; 1 ignored`; plus `tests/agent_runtime.rs` (9 passed incl. `sc_no_agent_ui_controls`), `tests/agent_spawn.rs` (11 passed), `tests/print_mode_e2e.rs` (33 passed), `tests/skills_integration.rs` (6 passed), `tests/wasm_plugin_integration.rs` (0, no tests), doc-tests (0, 1 unrelated rustdoc warning in `src/render/markdown.rs`, pre-existing, not phase-5 code). No failures anywhere in the suite.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|------------|-------------|--------|----------|
| UI-01 | 05-01, 05-03 | 1-3 line strip with id/role/task/state/elapsed, hidden when empty | SATISFIED | See Truth 1 |
| UI-02 | 05-01, 05-02, 05-03 | Ctrl+G expand/collapse, configurable, shows activity+report path | SATISFIED (with approved D-04 deviation) | See Truth 2 |
| UI-03 | 05-01, 05-03 | Display-only, no approve/stop/message | SATISFIED | See Truth 3 |
| UI-04 | 05-01, 05-03 | Tick-driven redraw from snapshot, no flicker | SATISFIED at code level; flicker claim needs human confirmation | See Truth 4 |
| QA-01 | (Phase 6, not this phase) | Manual E2E row per new control | N/A for Phase 5 — REQUIREMENTS.md and ROADMAP.md both scope QA-01 to Phase 6; this phase nonetheless pre-added the Ctrl+G manual test row to `docs/agents.md` for Phase 6 to fold in | Not a Phase 5 gap |

No orphaned requirements found: REQUIREMENTS.md maps only UI-01..04 to Phase 5, and all four are claimed and satisfied across the three plans.

### Anti-Patterns Found

None in phase-5-modified files (`src/mode/agents_strip.rs`, `src/keys.rs`, `src/settings_toml.rs`, `src/mode/tui.rs` strip-related sections, `docs/agents.md`). One pre-existing, unrelated `TODO` was found at `src/mode/tui.rs:5780` inside `draw_menu` (model picker), dated before this phase and unconnected to the agents strip — not a Phase 5 artifact, not flagged as a gap.

### Human Verification Required

See `human_verification` in frontmatter — these are the phase's own documented manual-only items (05-VALIDATION.md "Manual-Only Justification" + 05-03-PLAN.md `<human-check>`), not newly discovered gaps. They cover: live dispatch/strip-appearance, Ctrl+G expand-to-scrollback, Ctrl+G/Esc collapse, post-finish persistence with ✓, idle-flicker absence, and non-Ctrl `g` typing safety. All are already captured as the manual test row set in `docs/agents.md` lines 135-140.

### Gaps Summary

No gaps. All four ROADMAP success criteria for Phase 5 are backed by code that exists, is substantive (not a stub), is wired into the live TUI (tick-only refresh, draw_dock layout, key handling), and is covered by passing automated tests (`cargo test -- --test-threads=1`: 0 failures). The D-04 "~40% live pane" deviation is explicitly user-approved per the task's own framing and is recorded as an override, not a gap. The remaining item (visual flicker) was never claimed to be automatable — the phase's own 05-VALIDATION.md calls it out as inherently manual, and 05-03-PLAN.md's `<human-check>` block already exists for it — so this routes to `human_needed` rather than `gaps_found`, per the status decision tree (Step 9: human verification items present → human_needed, even with full automated pass).

---

_Verified: 2026-10-04T17:10:00Z_
_Verifier: Claude (gsd-verifier)_

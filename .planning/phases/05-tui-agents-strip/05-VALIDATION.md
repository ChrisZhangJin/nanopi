# Phase 5: TUI agents strip - Validation Strategy (Nyquist)

**Prepared:** 2026-10-04
**Companion to:** `05-RESEARCH.md`
**Principle:** Sample verification often enough (every task commit) to catch
regressions at the moment they're introduced, not just at the phase gate —
analogous to sampling above the Nyquist rate so no defect "aliases" past
unnoticed between a cheap per-commit check and the expensive full-suite
check.

## Test Framework

| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` / `cargo test` |
| Config file | None (no custom test runner) |
| Quick run command | `cargo test --lib mode::tui:: agent_registry:: keys::` |
| Full suite command | `cargo test` |
| Build sanity (QA-02 size check) | `cargo build --release && ls -la target/release/nanopi` (compare binary size to the pre-phase baseline, must grow ≤ ~150 KB) |

All tests for this phase are pure unit tests operating on in-memory
`ratatui::buffer::Buffer` / `Rect` / `App` values — no terminal emulation,
no subprocess spawning, no filesystem fixtures beyond temp dirs already used
by existing `agent_registry.rs` tests. Every test below runs in well under
100ms; the full set should add well under 2 seconds to `cargo test`.

## Per-Requirement Test Map

| Req ID | Behavior | Test Type | Automated Command | File Status |
|--------|----------|-----------|-------------------|-------------|
| UI-01 | Strip present with id/role/task/state/elapsed per live agent (1-3 lines) | unit | `cargo test --lib mode::tui::tests::agents_strip_shows_entries` | Wave 0: new |
| UI-01 | Strip absent when `registry.snapshot()` is empty | unit | `cargo test --lib mode::tui::tests::agents_strip_hidden_when_empty` | Wave 0: new |
| UI-01 | State glyphs render correctly for each of the 8 states (running/queued/waiting-for-permission is no longer applicable post-revision, but done/failed/stopped/limit_reached/interrupted/running/queued must each map to the D-03 glyph table) | unit (table-driven) | `cargo test --lib mode::tui::tests::agents_strip_glyph_per_state` | Wave 0: new |
| UI-02 | Ctrl+G toggles `app.agents_strip_expanded` true→false→true | unit | `cargo test --lib mode::tui::tests::ctrl_g_toggles_strip` | Wave 0: new |
| UI-02 | Expanded view renders activity lines (last 2-3), turns/tokens, worktree/branch if present, report path | unit | `cargo test --lib mode::tui::tests::expanded_strip_shows_detail` | Wave 0: new (blocked on Open Question 3 — activity source) |
| UI-02 | Esc collapses the expanded strip | unit | `cargo test --lib mode::tui::tests::esc_collapses_strip` | Wave 0: new |
| UI-02 | Ctrl+G is present in `ActionId::all()` and rebindable via settings.toml, same mechanism as `ExpandLastTool` | unit | `cargo test --lib keys::tests::toggle_agents_strip_registered_and_rebindable` | Wave 0: new |
| UI-03 | No key sequence reachable while the strip is focused mutates `AgentRegistry` state (negative test against stop/approve/message-style keys) | unit | `cargo test --lib mode::tui::tests::strip_is_display_only_no_mutation` | Wave 0: new |
| UI-03 | Strip rendering never calls any registry mutator (`stop`, `set_state`, `reactivate`) — grep-based static check as a supplement, not a replacement | static / `cargo test` combo | `! grep -n "reg\.\(stop\|set_state\|reactivate\)" src/mode/tui.rs \| grep -i "agents_strip\|draw_agents"` (manual review note, not a CI gate) | Wave 0: documented as a code-review checklist item, not an automated test — grep false-positive risk is high |
| UI-04 | Calling `draw_dock` twice with an unchanged snapshot produces byte-identical `Buffer` (proves no per-event hidden mutation / non-determinism) | unit | `cargo test --lib mode::tui::tests::strip_render_deterministic_across_ticks` | Wave 0: new |
| UI-04 | Strip caps at 3 agent lines + "+K more (R running)" when agent count > 3; ordering is attention-needed → running → recently-finished per D-02 | unit | `cargo test --lib mode::tui::tests::strip_caps_and_orders_entries` | Wave 0: new |
| UI-04 | No panic/overflow with agent count at `max_live` cap (Phase 1 config) | unit | `cargo test --lib mode::tui::tests::strip_handles_max_live_agents` | Wave 0: new |
| D-07 | Registry cache refresh (description/activity) happens on state-transition call sites (`set_state`, `push_report`), not from the render path — verified by asserting `draw_agents_strip` performs zero `std::fs` calls (structural: function takes only in-memory data, no `Path`/`dir` access inside the draw fn signature) | unit (type-signature-level guarantee) + review | `cargo test --lib mode::tui::tests::agents_strip_render_is_pure` (asserts the draw function signature takes no filesystem-capable args — a smoke test calling it with a `Buffer`+cached data and confirming no panics/IO even with a nonexistent `dir` path) | Wave 0: new |
| D-08 | Narrow terminal (<60 cols) drops activity text first, then description, keeping id/state/elapsed | unit (parametrized widths) | `cargo test --lib mode::tui::tests::strip_narrow_terminal_drops_activity_then_description` | Wave 0: new |
| D-08 | Short terminal (<15 rows) collapses strip to header-only regardless of `expanded` flag | unit (parametrized heights) | `cargo test --lib mode::tui::tests::strip_short_terminal_header_only` | Wave 0: new |
| QA-01 | Ctrl+G has a row in the manual E2E test plan | manual (doc check) | N/A — verify `.planning/` or project manual-test-plan doc has a "Ctrl+G / expand agents strip" row added during this phase, not deferred entirely to Phase 6 | Wave 0: add the row as part of this phase's plan, even though QA-01 is formally tracked under Phase 6 |
| QA-02 | Release binary size delta ≤ ~150 KB, no new crates | build check | `cargo build --release` then compare `ls -la target/release/nanopi` before/after; `git diff Cargo.lock` must show no new `[[package]]` entries | Phase-gate only (not per-commit — release builds are slow) |

## Sampling Rate

- **Per task commit** (every commit inside this phase's plans):
  ```bash
  cargo test --lib mode::tui:: agent_registry:: keys::
  ```
  Fast (<5s), catches regressions the moment a task introduces them —
  this is the "above Nyquist" sampling: frequent enough that no broken
  state survives more than one commit undetected.

- **Per wave merge** (when a wave of parallel plans converges):
  ```bash
  cargo test
  ```
  Full suite, including pre-existing Phase 1-4 regression tests (e.g.
  `AgentRegistry` stop/reactivate/kill_all tests already present at
  `src/agent_registry.rs` lines ~636-1100) to catch cross-module
  interference (e.g. a cache-refresh hook added to `set_state` breaking
  an existing `set_state` test).

- **Phase gate** (before `/gsd:verify-work`):
  1. `cargo test` — full suite green.
  2. `cargo build --release` — binary size check (QA-02).
  3. Manual smoke: run `nanopi` interactively, dispatch 1 and then 4+
     background agents (via existing `agent` tool), observe the strip
     appear/update/collapse/expand/disappear live. This is the one step
     that cannot be automated (real terminal rendering, human-perceived
     flicker) — explicitly called out as manual-only with justification:
     ratatui `TestBackend` buffer-diffing cannot assert "no visible
     flicker," only "buffer content is as expected for a given snapshot."

## Manual-Only Justification

One behavior in this phase is inherently manual-only and should NOT be
treated as a gap to "fix" with more automation:

- **"No flicker or redraw storm under many agent events" (UI-04, D-07).**
  Unit tests can prove determinism (same snapshot → same buffer) and can
  prove the tick-driven call path doesn't touch the registry, but they
  cannot observe real terminal-emulator-level flicker, since that depends
  on actual frame timing and terminal rendering behavior outside the
  `Buffer` abstraction. This is covered by the phase-gate manual smoke
  test above, not by an automated command.

## Wave 0 Gaps (must exist before task work starts)

- [ ] No new test *files* are needed — all new tests slot into the
  existing `mod tests` blocks in `src/mode/tui.rs` (line ~5830),
  `src/agent_registry.rs`, and `src/keys.rs`.
- [ ] Confirm during Wave 0 whether `src/keys.rs` has a generic
  "all `ActionId` variants are rebindable" test (if so,
  `ToggleAgentsStrip` is covered automatically by adding it to
  `ActionId::all()` — no new test needed for that sub-case).
- [ ] Resolve 05-RESEARCH.md's Open Question 2 (ratatui dynamic-viewport
  resize API for the pinned version) before planning the height-growth
  implementation task — this determines whether
  `strip_render_deterministic_across_ticks`-style tests need to also
  cover a viewport-resize code path, or whether the scrollback-insert
  fallback (no resize API touched) is used instead.
- [ ] Resolve Open Question 3 (source of "latest activity" lines for the
  expanded view) before writing `expanded_strip_shows_detail` — if a new
  activity-log file must be added to the child runtime (Phase 1 code),
  that is itself a task with its own tests, not just a TUI-side read.
- [ ] Resolve Open Question 1 (visibility semantics: live-only vs.
  live+recently-finished) before writing
  `agents_strip_hidden_when_empty` and `strip_caps_and_orders_entries`,
  since the definition of "empty" and "caps/orders" both depend on it.

**If any of the three open questions remain unresolved when planning
starts:** the planner should insert a `checkpoint:human-verify` or a
discussion step before the affected tasks, per GSD convention — these are
product-semantics decisions, not implementation details Claude should
silently decide.

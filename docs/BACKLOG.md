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

## Test suite is flaky under parallel execution (env-var race)

**Current state (2026-08-25)**: `cargo test` fails roughly half the
time on a clean tree, with a *different* test failing each run.
Observed so far: `agent::hook::tests::run_hook_exit_0_means_allow`,
`agent::hook::tests::run_hook_exit_2_means_block`,
`agent::hook::tests::run_hook_json_decision_on_stdout`,
`paths::tests::nanopi_home_honors_env`,
`agent::loop_::tests::load_session_replays_tool_calls`,
`provider::openai::tests::resumed_session_outgoing_request_has_tools`.
Measured 3 failures in 6 runs at `3254017`, i.e. this predates the
2026-08-25 bug-fix batch and is not caused by it.

**Cause**: 57 `std::env::set_var("NANOPI_HOME", …)` /
`remove_var` calls live in tests across 9 modules — `settings.rs`,
`session.rs`, `agent/loop_.rs`, `agent/build.rs`, `settings_toml.rs`,
`config.rs`, `paths.rs`, `agent/permission.rs`,
`provider/openai.rs`. The environment is process-global but cargo
runs tests as parallel threads in one process, so any test that
*sets* `NANOPI_HOME` races every test that *reads* it. Whichever
loses picks up another test's temp dir and fails on a path that
isn't there.

**Confirmation**: `cargo test -- --test-threads=1` passes 409/409
consistently. The failures are pure interference, not real defects.

**Why deferred**: this is CI hygiene, not a user-facing bug, and
the fix is a real chunk of work rather than a one-liner.

**What it would take**, roughly in order of preference:
1. Thread the home directory through as a parameter (e.g. widen
   `paths::nanopi_home()` to take an override, or put it on a
   context struct) so tests never touch the environment. Cleanest,
   biggest diff.
2. Serialize the affected tests behind a shared `Mutex` — small
   diff, but easy for a future test to forget the lock and silently
   reintroduce the flake.
3. `--test-threads=1` in CI. Cheapest, but hides the race rather
   than fixing it and slows the suite.

**Trigger to revisit**: CI going green is a prerequisite for
trusting it as a merge gate. Worth doing before (or alongside)
clearing the pre-existing `cargo fmt --check` and
`cargo clippy -D warnings` failures, which are red on `main` for
unrelated reasons — all three want one "make CI green" pass.

---

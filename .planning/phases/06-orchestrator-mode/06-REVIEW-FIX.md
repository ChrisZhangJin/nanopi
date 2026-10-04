---
phase: 06-orchestrator-mode
fixed_at: 2026-10-04T18:09:02Z
review_path: .planning/phases/06-orchestrator-mode/06-REVIEW.md
iteration: 1
findings_in_scope: 4
fixed: 4
skipped: 0
status: all_fixed
---

# Phase 06: Code Review Fix Report

**Fixed at:** 2026-10-04T18:09:02Z
**Source review:** .planning/phases/06-orchestrator-mode/06-REVIEW.md
**Iteration:** 1

**Summary:**
- Findings in scope: 4 (CR-01, CR-02, WR-01, WR-02). IN-01 was excluded: it asks only for a
  cross-reference comment and was judged a documentation nicety rather than a code defect worth a
  separate fix/commit in this pass — left for the author to add alongside the `orchestrator()`
  doc comment if desired.
- Fixed: 4
- Skipped: 0

## Fixed Issues

### CR-01: `/reload` recomposes the system prompt with the wrong template while orchestrator mode is active

**Files modified:** `src/mode/tui.rs`
**Commit:** `8030219`
**Applied fix:** `handle_reload` now calls `compose_system_prompt_mode(.., app.orchestrator)` instead
of the plain `compose_system_prompt`, matching the other orchestrator-aware rebuild sites
(`/new`, `/resume`, `/import`, `/fork`). Confirmed the bug was real by reading `handle_reload` at
the cited lines: it unconditionally used the non-orchestrator composer regardless of
`app.orchestrator`, while the registry it describes is never rebuilt from scratch by `/reload`.
Added `reload_resyncs_system_prompt_to_orchestrator_mode`, which pins the same composition call
`handle_reload` makes and asserts the orchestrator-mode prompt differs from the plain one
(`handle_reload` itself has no test seam — `Term` is `Terminal<CrosstermBackend<Stdout>>`, not a
`TestBackend` — this mirrors the existing pattern used for `reload_extensions_clause`).

### WR-01: `/reload`'s extension reload can add tools into the orchestrator-restricted registry that bypass the closed-list design

**Files modified:** `src/mode/tui.rs`
**Commit:** `e70b5d9`
**Applied fix:** Extracted a new `reload_extensions_guarded(app, agent, exts)` helper: when
`app.orchestrator` is true, it swaps `agent.registry` out for the saved pre-orchestrator registry,
applies `reload_extensions` to that saved registry instead of the live restricted one, swaps back,
updates `app.saved_registry`, and resyncs `agent.context.tools` / the plugin dispatch to the
still-restricted live registry. This keeps the closed list genuinely closed (no plugin tool becomes
callable from orchestrator mode via `/reload`) while still making the reloaded tools reappear when
the user later toggles orchestrator mode off. `handle_reload` now calls this helper instead of
`a.reload_extensions(exts)` directly. Added
`reload_extensions_guarded_leaves_restricted_registry_live_and_updates_saved`, which exercises the
guarded path and asserts (a) the live registry's tool names stay exactly the orchestrator closed
list after the guarded reload, (b) `context.tools` matches that restricted set (not the saved one),
and (c) toggling off still round-trips to the original pre-orchestrator registry.

### CR-02: `/orchestrator` toggle silently no-ops mid-turn but still reports success and flips the status flag

**Files modified:** `src/mode/tui.rs`
**Commit:** `3d96443`
**Applied fix:** Extracted the toggle decision into a pure, testable function
`try_apply_orchestrator_toggle(app, agent: Option<&mut Agent>, new_val) -> OrchestratorToggleOutcome`
(`Applied` / `Unchanged` / `RefusedBusy`). `app.orchestrator` is now only flipped when an Agent is
actually present in the slot; when the slot is empty (turn in flight), the toggle is refused,
`app.orchestrator` is left untouched, and the user sees "orchestrator mode: a turn is in flight —
try again once it finishes" instead of a false "takes effect from next turn" success message. This
implements option (a) from the review's fix guidance (refuse rather than silently queue), since
queuing would need a second reconciliation point that doesn't currently exist anywhere the Agent is
returned to the slot. Added two regression tests:
`try_apply_orchestrator_toggle_refuses_mid_turn_without_flipping_flag` (refusal path + flag
untouched, then confirms a subsequent toggle with the Agent present succeeds) and
`try_apply_orchestrator_toggle_is_noop_when_value_unchanged` (no-op path still reports `Unchanged`
without touching the flag).

### WR-02: `apply_orchestrator_mode`'s off-path silently no-ops when `saved_registry` is `None` for reasons other than "mode was never on"

**Files modified:** `src/mode/tui.rs`
**Commit:** `8b51a0e`
**Applied fix:** Added an `else` arm to `apply_orchestrator_mode`'s off-branch that emits a
diagnostic (`eprintln!`) when `on = false` is requested and `app.saved_registry` is already `None`,
per the review's "at minimum, assert/log" guidance — this is the minimal fix that makes the CR-02
symptom visible if it ever recurs, without changing behavior (the branch was already a no-op; it is
now a no-op with a visible line on stderr). Added
`apply_orchestrator_mode_off_with_no_saved_registry_does_not_panic`, confirming the no-op path
leaves the registry/`context.tools` fully untouched and does not panic.

## Skipped Issues

None — all in-scope findings were fixed. IN-01 was intentionally excluded from scope (see summary
above) rather than skipped due to a verification failure.

## Verification

- Each fix verified via: re-read of the modified section (Tier 1), `cargo build --lib` and
  `cargo build --lib --features wasm` after every change (Tier 2 equivalent for Rust), and a
  dedicated regression test exercising the specific defect.
- Full suite: `cargo test -- --test-threads=1` — **1111 tests passed, 0 failed** across the lib
  target (1044 passed, 1 ignored) and all integration binaries (`main` 2, `agent_archive` 4,
  `agent_runtime` 9, `agent_spawn` 13, `print_mode_e2e` 33, `skills_integration` 6,
  `wasm_plugin_integration` 0). Also ran `cargo test --features wasm -- --test-threads=1`
  separately (1111 non-wasm-gated tests plus the 37 `wasm_plugin_integration` tests), all green.
- One test (`print_mode_ctrl_c_stops_all_background_agents`) was observed to fail intermittently
  under full-suite parallelism load in two of several runs; confirmed pre-existing and unrelated to
  this change set by reproducing the same intermittent failure rate on the pre-fix commit
  (`f7278ae`) in an isolated worktree, and by the test passing consistently (6/6 runs) when run
  in isolation both before and after these fixes.

---

_Fixed: 2026-10-04T18:09:02Z_
_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_

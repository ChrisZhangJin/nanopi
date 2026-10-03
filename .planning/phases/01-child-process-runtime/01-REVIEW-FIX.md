---
phase: 01-child-process-runtime
fixed_at: 2026-10-03T00:00:00Z
review_path: .planning/phases/01-child-process-runtime/01-REVIEW.md
iteration: 1
findings_in_scope: 8
fixed: 8
skipped: 0
status: all_fixed
---

# Phase 1: Code Review Fix Report

**Source review:** .planning/phases/01-child-process-runtime/01-REVIEW.md
**Iteration:** 1
**Scope:** Critical + Warning (IN-01..IN-03 out of scope, not attempted)

**Summary:**
- Findings in scope: 8
- Fixed: 8
- Skipped: 0

Verification: `cargo build`, full `cargo test` (all suites green), and
`cargo test --release --test subagent_runtime` (8/8) pass. Clippy on the touched
files reports the same 4 pre-existing warnings as the baseline (c592542), so no
new warnings.

## Fixed Issues

### CR-01: token budget / max turns reset on every self-check turn

**Files modified:** `src/agent/loop_.rs`, `src/mode/print.rs`
**Commit:** ed143b2
**Status:** fixed: requires human verification (logic change)
**Applied fix:** `TurnLimits` gains an opt-in run-scoped mode
(`Agent::set_run_scoped_limits`). In this mode, `run_tokens` and `run_turns`
accumulate across `run_turn` calls, and each call only gets the allowance that
is left (it returns at once with `last_limit_hit` set if nothing is left). Print
mode turns this on whenever `--brief`, `--max-turns` or `--token-budget` is set.
TUI per-turn semantics are unchanged. Regression test:
`run_scoped_limits_span_run_turn_calls`, a unit test that calls `run_turn`
repeatedly against a provider that keeps requesting tools.

### CR-02: JSON-mode child failure exits 1, parent drops the envelope

**Files modified:** `src/tool/subagent.rs`
**Commit:** c2babcb
**Applied fix:** On a non-signal non-zero exit, the parent now parses the
untruncated stdout. If it holds a `status: failed` envelope, the parent routes
it through `envelope_output`, so the error text, `agent_id` and `report_path`
reach the result. Signals and stdout that is not an envelope still map to the
exit code. Regression test: `failed_envelope_with_exit_1_surfaces_error`. Its
fake child prints a failed envelope and exits 1, and the test asserts that
"provider down: 503" appears in the result.

### WR-01: unreadable brief silently runs an empty task

**Files modified:** `src/mode/print.rs`, `tests/print_mode_e2e.rs`
**Commit:** 5f3a342
**Applied fix:** An unreadable brief is now fatal. JSON mode prints a
`status: failed` envelope and exits 1. Text mode bails. The model is never
called. The existing e2e test
`tools_allowlist_agent_strips_subagent_and_denies_unlisted_in_band` was passing
a brief path that did not exist, so it now writes the brief first. New e2e test:
`unreadable_brief_fails_without_calling_model`.

### WR-02: brief role/model not escaped (amendment injection)

**Files modified:** `src/agent/brief.rs`
**Commit:** ff21363
**Applied fix:** `render_brief` now runs `escape_body` on role, model and tool
names. `parse_amendments_with` accepts the marker only as a whole line, so
inline or escaped copies are ignored. Test:
`role_and_model_cannot_inject_amendments`.

### WR-03: ChildGuard SIGKILLs the pgid after reap (pid reuse)

**Files modified:** `src/tool/subagent.rs`
**Commit:** 9d88e42
**Applied fix:** The new `wait_exited` uses `waitid(P_PID, WEXITED | WNOWAIT)`
on a blocking thread. The leader is detected as exited but not reaped, the group
sweep (`drop(guard)`) runs while the zombie still holds the pgid, and only then
does `child.wait()` reap it. Non-unix falls back to `child.wait()`. Test:
`normal_exit_sweeps_grandchild_before_reap`.

### WR-04: grandchild holding pipes turns finished child into timeout

**Files modified:** `src/tool/subagent.rs`
**Commit:** bc17f01
**Applied fix:** Leader exit and pipe drains are now awaited with `select!`
instead of `join!`. Once the leader exits, the drains get `DRAIN_GRACE` (2s).
Then the group is swept and the drains get one more grace period. Whatever is
buffered is then parsed. Test:
`grandchild_holding_stdout_does_not_cause_timeout`.

### WR-05: amendments lost between watcher restarts

**Files modified:** `src/mode/print.rs`
**Commit:** d99dd90
**Status:** fixed: requires human verification (race fix, no deterministic test)
**Applied fix:** `start_brief_watch` now takes the brief content that was
already read. The initial task and each self-check prompt come from the same
read that sets the watcher's `initial_last`, so no amendment can fall between
the two reads. The review's sub-point is not addressed: amendments appended
after the final self-check turn ends are still not recorded in report.md.

### WR-06: zero `[subagent]` caps silently disable subagents

**Files modified:** `src/config.rs`
**Commit:** 26cc63a
**Applied fix:** `load_config` now runs `validate_subagent`. It rejects 0 for
`max_live`, `max_concurrency`, `max_turns`, `token_budget` and `timeout_secs`
with an error that names each key, consistent with
`validate_tool_exec_overrides`. Test: `subagent_zero_caps_rejected_at_load`.

---

_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_

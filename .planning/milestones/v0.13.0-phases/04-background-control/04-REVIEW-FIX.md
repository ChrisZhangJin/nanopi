---
phase: 04-background-control
fixed_at: 2026-10-04T11:30:00Z
review_path: .planning/phases/04-background-control/04-REVIEW.md
iteration: 1
findings_in_scope: 4
fixed: 4
skipped: 0
status: all_fixed
---

# Phase 04: Code Review Fix Report

**Fixed at:** 2026-10-04T11:30:00Z
**Source review:** .planning/phases/04-background-control/04-REVIEW.md
**Iteration:** 1

**Summary:**
- Findings in scope: 4 (CR-01, WR-01, WR-02, WR-03)
- Fixed: 4
- Skipped: 0

## Fixed Issues

### CR-01: Top-level process only trapped SIGTERM, not SIGINT, despite the comment claiming both

**Files modified:** `src/mode/print.rs`
**Commits:** `58699fe`, `e1d70a5`
**Applied fix:** Verified the gap was real: `main.rs`'s own comments (added since the reviewed snapshot) confirm the top-level `select!` deliberately dropped SIGINT handling in print mode to avoid racing the whole `print_fut` against `ctrl_c()` (that race always won and tore the in-drain Ctrl-C handling down before it could run), but this left a documented gap — "no ctrl_c listener is installed for the ordinary main-turn case, only for this drain window." A Ctrl-C during the main turn (while a `background: true` agent was already dispatched and alive) had no handler at all.

Fixed by installing a persistent `tokio::signal::ctrl_c()` listener *inside* `run_print_mode` (not as a sibling branch racing the whole future in `main.rs`, which is what caused the original regression this design was worked around). The listener runs for the entire duration of the print-mode run, flips a shared `AtomicBool` flag, and calls `reg.stop_all()` on every Ctrl-C, regardless of whether the main turn, the self-check loop, the drain window, or the bounded extra turn is currently running. Both `wait_background()` call sites were changed from a racing `select!` (previously installed only around the first call) to plain unconditional awaits, since the persistent listener already requests cancellation independently of which wait is pending.

During verification a real regression surfaced: a single `stop_all()` call fired the instant SIGINT arrives can race a background dispatch that is still mid-registration (it only sees already-registered, non-terminal entries), making `stop_all()` a no-op for that agent, which then ran to completion untouched. This reproduced in roughly 1/6 runs of `print_mode_ctrl_c_stops_all_background_agents` before the fix, confirmed via a stashed `cargo build` of the pre-fix commit alone. Closed by re-issuing `stop_all()` at each `has_background()`-gated checkpoint whenever the interrupted flag is already set — by the time a turn's own `run_turn()` call has returned, any background agent it dispatched is guaranteed to be registered, so the re-issued call always has a target. Verified clean across 22 additional back-to-back runs of the targeted test after the fix (12 isolated + 10 more), plus two full `cargo test -- --test-threads=1` passes.

### WR-01: `StateGuard::drop` ran blocking `git` subprocess calls synchronously on the async executor thread

**Files modified:** `src/tool/agent.rs`
**Commit:** `63eea08`
**Applied fix:** Confirmed real — `StateGuard::drop`'s `finish_worktree_and_record` call (several `git status`/`add`/`commit`/`merge`/`worktree remove`/`branch -D` subprocess calls) ran directly and synchronously whenever the cancellation branch of `spawn_background`'s `select!` dropped `run_body`'s future mid-flight, blocking whatever tokio worker thread was performing the drop for as long as those `git` commands took. Fixed per option (b) in the review: the drop path now hands the blocking work to `tokio::task::spawn_blocking` (fire-and-forget — nothing in `Drop` needs to observe completion, matching how `wait_background()`/tests already poll `report.md`/registry state rather than relying on synchronous completion), with a synchronous fallback only if no tokio runtime handle is available (e.g. a bare unit-test context outside any runtime). Updated the doc comment on `finish_worktree_and_record` and the `stopped_agent_still_finishes_worktree` test comment to reflect the new async-dispatch behavior. The existing test (which already polls with a retry loop, tolerant of async completion) passes unchanged.

### WR-02: Second `wait_background()` call after the bounded "extra" turn was not covered by the Ctrl-C drain race

**Files modified:** `src/mode/print.rs`
**Commit:** `58699fe`
**Applied fix:** Confirmed real and fixed as a direct consequence of the CR-01 restructuring above: because the persistent Ctrl-C listener now calls `stop_all()` independent of which `wait_background()` call is pending (first drain or the second one after the bounded extra turn), both call sites are now uniformly covered. No separate code change was needed beyond the CR-01 fix, since the original narrow `select!` wrapping only the first call was removed entirely in favor of the always-on listener plus unconditional awaits.

### WR-03: `ensure_gitignore_entry`'s line-matching allowed a prefix/typo collision to silently suppress re-adding the real entry... and vice versa — redundant entries accumulate

**Files modified:** `src/tool/agent.rs`
**Commit:** `36be76e`
**Applied fix:** Confirmed real (low-impact, as the review noted). Added a prefix-directory check so a broader existing ignore line (e.g. `.nanopi/`) is recognized as already covering a more specific one (`.nanopi/worktrees/`), with a `/`-boundary check to avoid false-positives on sibling names (e.g. `.nanopi-old/` must not be treated as covering `.nanopi/worktrees/`). Added two unit tests: `ensure_gitignore_entry_skips_when_prefix_dir_already_ignored` and `ensure_gitignore_entry_does_not_false_positive_on_sibling_prefix`, both passing.

## Skipped Issues

None — all four in-scope findings were confirmed real and fixed.

## Test Results

Full suite run twice at the end (`cargo test -- --test-threads=1`), both clean:

```
test result: ok. 990 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out (lib)
test result: ok. 2 passed; 0 failed (agent_archive)
test result: ok. 4 passed; 0 failed (agent_runtime)
test result: ok. 9 passed; 0 failed (agent_spawn... scenario suite)
test result: ok. 11 passed; 0 failed (agent_spawn)
test result: ok. 33 passed; 0 failed (print_mode_e2e)
test result: ok. 6 passed; 0 failed (skills_integration)
test result: ok. 0 passed (wasm_plugin_integration — no wasm feature)
test result: ok. 0 passed (doc-tests)
```

The CR-01-affected test, `print_mode_ctrl_c_stops_all_background_agents`, was additionally run 22 times back-to-back in isolation post-fix with zero failures, after reproducing a genuine ~1/6 flake rate on the intermediate (pre-race-fix) commit via bisection against the pre-existing baseline (which showed 0 failures in 13 comparable runs).

---

_Fixed: 2026-10-04T11:30:00Z_
_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_

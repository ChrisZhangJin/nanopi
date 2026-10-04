---
phase: 05-tui-agents-strip
fixed_at: 2026-10-04T17:30:00Z
review_path: .planning/phases/05-tui-agents-strip/05-REVIEW.md
iteration: 1
findings_in_scope: 4
fixed: 3
skipped: 1
status: partial
---

# Phase 5: Code Review Fix Report

**Fixed at:** 2026-10-04T17:30:00Z
**Source review:** .planning/phases/05-tui-agents-strip/05-REVIEW.md
**Iteration:** 1

**Summary:**
- Findings in scope: 4 (CR-01, WR-01, WR-02, IN-02 folded into CR-01; IN-01 considered per special instruction)
- Fixed: 3 (CR-01 + IN-02 combined, WR-01, WR-02)
- Skipped: 1 (IN-01)

## Fixed Issues

### CR-01 (and IN-02): `tool_name` from `transcript.jsonl` is never sanitized/bounded before reaching the render buffer

**Files modified:** `src/mode/agents_strip.rs`
**Commit:** `91cad8a`
**Applied fix:** Verified the gap by reading `tail_activity` directly: `tool_name` was pulled from untrusted `transcript.jsonl` via `v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("?")` and used raw (no `sanitize()`, no `truncate_to_width()`) in both the `Some(arg)` and `None` branches of the `match short_arg` block, unlike `arg` which already went through `sanitize()` + `truncate_to_width(.., 40)`. Applied the fix suggested in the review: `tool_name` is now run through `sanitize(..)` and `truncate_to_width(.., 40)` immediately after extraction, before being used in either branch. This closes the control-character/escape-sequence injection gap (T-05-01) and also addresses IN-02 (unbounded length) with the same change, since `truncate_to_width(.., 40)` bounds it identically to `arg`.
**Regression tests added:** `activity_sanitizes_and_bounds_tool_name_from_untrusted_transcript` (exercises the `Some(arg)` branch with a `tool_name` containing an ESC/CSI sequence plus 100 extra chars — asserts no ESC byte, no control chars, and bounded display width reach `AgentInfo::activity`) and `activity_sanitizes_tool_name_with_no_arg` (same but for the `None` branch, using a BEL control char). Both pass.

### WR-01: `agent_line`/`expanded_detail_lines` ignore `StripOpts.now`, breaking the documented purity/determinism contract

**Files modified:** `src/mode/agents_strip.rs`, `src/mode/tui.rs`
**Commit:** `ef3550a`
**Applied fix:** Confirmed via `grep -n "\.now\b" src/mode/agents_strip.rs` that `StripOpts.now` was indeed unread, and `agent_line` called `Instant::now()` directly for the elapsed-seconds column, while `expanded_detail_lines` had no `now` parameter at all (and transitively no way to pass one to `agent_line`). Applied the fix as specified: `agent_line` now takes a `now: Instant` parameter and uses it directly; `expanded_detail_lines` gained a `now: Instant` parameter threaded into its internal `agent_line` call; `draw_agents_strip` passes `opts.now` instead of letting `agent_line` call the clock itself. Updated both call sites: `draw_agents_strip`'s row loop, and `tui.rs::agents_detail_block` (now passes `std::time::Instant::now()` at the call boundary, preserving current external behavior while making the renderer itself pure). Updated all five existing unit-test call sites in `agents_strip.rs` to pass their already-in-scope `now` variable.
**Regression tests added:** `agent_line_elapsed_is_pinned_to_opts_now_not_wall_clock` (asserts identical output for two calls with the same `now`, even across a real `thread::sleep`, and different output for different `now` values with hand-verified `format_elapsed` output ("5s" vs "1m05s")) and `expanded_detail_lines_elapsed_uses_passed_now` (same invariant through the public `expanded_detail_lines` entry point). Both pass.

### WR-02: `agents_strip_expanded` is not reset when the agents view becomes empty

**Files modified:** `src/mode/tui.rs`
**Commit:** `3144318`
**Applied fix:** Confirmed the tick handler computes `became_empty` purely to force a final redraw but never reads/writes `app.agents_strip_expanded`. Rather than inlining the one-line fix directly in the async tick branch (which is awkward to unit-test without a live `Term`/event loop), extracted the decision into a small private helper `reset_strip_expanded_on_became_empty(app: &mut App, became_empty: bool)` and call it from the tick handler immediately after `became_empty` is computed. Behavior at the call site is identical to the review's suggested inline fix; the extraction only adds testability.
**Regression test added:** `strip_expanded_resets_when_agents_view_becomes_empty` — asserts the flag clears when `became_empty=true` and is left untouched when `became_empty=false`. Passes.

## Skipped Issues

### IN-01: Every tick now triggers a full dock redraw whenever any agent is present

**File:** `src/mode/tui.rs:2101-2113`
**Reason:** Per the task instructions, this was only to be fixed "if a trivial change-detection gate avoids idle redraws without risking UI-04 — otherwise skip." The review itself independently classifies this as "Out of scope as a performance defect per review instructions." After reviewing the tick handler and the elapsed-seconds display path (which, post-WR-01, still legitimately needs re-rendering roughly every second for any `Running`/`Queued` agent so the elapsed counter stays live), a safe change-detection gate would need a cheap signature covering: agent count, each agent's state, each agent's last activity line, and whether any visible elapsed-seconds value has ticked over — i.e., effectively re-deriving most of what `draw_agents_strip` already computes, just to decide whether to call it. That is not a trivial, low-risk change; a naive gate (e.g., only on `AgentsView` snapshot equality) would silently freeze the elapsed-time column for idle `Queued`/`Running` agents, which is a user-visible regression and arguably a new correctness bug, not a safe performance fix. Skipped and left for a dedicated follow-up/performance pass rather than risking UI-04 (pure/deterministic/IO-free render contract) or introducing a stale-elapsed-time bug under time pressure.
**Original issue:** The redraw condition gained `|| !app.agents_view.is_empty()`, so the dock is fully redrawn on every 120ms tick as long as any agent is tracked, even if nothing about it visibly changed since the last tick.

---

_Fixed: 2026-10-04T17:30:00Z_
_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_

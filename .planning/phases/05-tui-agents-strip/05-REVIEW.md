---
phase: 05-tui-agents-strip
reviewed: 2026-10-04T16:43:11Z
depth: standard
files_reviewed: 5
files_reviewed_list:
  - src/keys.rs
  - src/mode/agents_strip.rs
  - src/mode/mod.rs
  - src/mode/tui.rs
  - src/settings_toml.rs
  - tests/agent_runtime.rs
  - docs/agents.md
findings:
  critical: 1
  warning: 2
  info: 2
  total: 5
status: issues_found
---

# Phase 5: Code Review Report

**Reviewed:** 2026-10-04T16:43:11Z
**Depth:** standard
**Files Reviewed:** 7 (6 source/test, 1 doc)
**Status:** issues_found

## Summary

Reviewed the agents-strip data model/renderer (`src/mode/agents_strip.rs`), its
wiring into the dock (`src/mode/tui.rs`), the new `Ctrl+G` keybinding
(`src/keys.rs`, `src/settings_toml.rs`), and the related test/doc changes.
Width math (CJK truncation, narrow/short-terminal fallbacks, `u16`/`usize`
subtraction) is consistently done with `saturating_sub`/`.min()` and is sound
against the edge cases checked (zero width, zero height, oversized ids,
oversized elapsed strings). Key handling for `Ctrl+G`/`Esc` is correctly
ordered relative to existing modals and the double-Esc fork-picker timer, and
registry mutation is genuinely absent from the new code paths (matches the
`sc_no_agent_ui_controls` test intent). However, one control-character
sanitization gap was found in the transcript-tail parser that lets
model/tool-influenced text reach the terminal buffer unsanitized, which is
exactly the class of bug this phase's own T-05-01 invariant exists to
prevent. Two secondary correctness/determinism issues were also found in the
"pure" renderer.

## Critical Issues

### CR-01: `tool_name` from `transcript.jsonl` is never sanitized before reaching the render buffer

**File:** `src/mode/agents_strip.rs:243-260`
**Issue:** In `tail_activity`, `arg` (the tool argument) is correctly run through `sanitize()` (control-char stripping) and `truncate_to_width(..., 40)` before being used, but `tool_name` — pulled straight from the untrusted `transcript.jsonl` (`v.get("tool_name").and_then(|t| t.as_str())`) — is used raw in both branches:
```rust
let formatted = match short_arg {
    Some(arg) => {
        let arg = truncate_to_width(&sanitize(&arg), 40);
        format!("{tool_name} {arg}")   // tool_name unsanitized
    }
    None => tool_name.to_string(),      // tool_name unsanitized, unbounded
};
```
This string is pushed into `AgentInfo::activity`, which `draw_agents_strip`/`expanded_detail_lines` write directly into the `ratatui::Buffer` via `buf.set_string(...)` with no further sanitization pass. Since `transcript.jsonl` is written from tool-call data that is itself derived from model output (the entire premise of T-05-01's doc comment: "Strip control characters from model-influenced text... escape sequences in brief/report/transcript text must not reach the terminal"), a model/tool emitting a crafted `tool_name` containing ESC/CSI sequences will have those raw bytes land in cell content and ultimately get written to the real terminal by the backend, which is precisely the terminal-escape-injection risk this module claims to close. It is also unbounded in length (no `truncate_to_width` cap), unlike every other field.
**Fix:**
```rust
let tool_name = v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("?");
let tool_name = sanitize(tool_name);
...
let formatted = match short_arg {
    Some(arg) => {
        let arg = truncate_to_width(&sanitize(&arg), 40);
        format!("{tool_name} {arg}")
    }
    None => tool_name.clone(),
};
```
Also consider truncating `tool_name` itself to a bounded width, since it is independent, attacker-influenced text.

## Warnings

### WR-01: `agent_line`/`expanded_detail_lines` ignore `StripOpts.now`, breaking the documented purity/determinism contract

**File:** `src/mode/agents_strip.rs:421-433` (`elapsed_secs_for` call site), `StripOpts.now` field at `src/mode/agents_strip.rs:409`
**Issue:** `StripOpts` carries a `now: Instant` field specifically so the renderer can be pure/deterministic (the module doc says "Calling twice with the same `view`/`opts` yields an identical buffer"), but it is never read anywhere in the file (`grep -n "\.now\b" src/mode/agents_strip.rs` returns nothing). Instead, `agent_line` calls `Instant::now()` directly:
```rust
let elapsed = format_elapsed(elapsed_secs_for(info, Instant::now()));
```
This makes the "pure" draw function's output depend on wall-clock time at call time rather than on `opts`, violating the stated UI-04 determinism invariant and making the elapsed-seconds column untestable/unreproducible across a second boundary. The existing `draw_is_pure_and_deterministic` / `strip_render_deterministic_across_ticks` tests do not catch this because both draws happen within the same wall-clock second in practice.
**Fix:** Thread `opts.now` through instead of calling `Instant::now()`:
```rust
fn agent_line(info: &AgentInfo, width: usize, use_activity: bool, now: Instant) -> String {
    ...
    let elapsed = format_elapsed(elapsed_secs_for(info, now));
```
and pass `opts.now` from `draw_agents_strip`, and a caller-supplied `now` into `expanded_detail_lines` (currently it also has no `now` parameter and nothing to pass to `agent_line`).

### WR-02: `agents_strip_expanded` is not reset when the agents view becomes empty

**File:** `src/mode/tui.rs:2094-2113` (tick handler, `became_empty` computation)
**Issue:** The tick handler tracks `became_empty` purely to force one more redraw so the strip disappears cleanly, but it never clears `app.agents_strip_expanded`. If a user expands the strip (`Ctrl+G`) and all agents subsequently finish and age out of the view, `agents_strip_expanded` stays `true`. The next time any agent appears (even in an unrelated later run), the strip renders already-expanded and immediately dumps a fresh detail block into scrollback without the user pressing `Ctrl+G` again — surprising since D-05 implies the toggle is the only way to expand.
**Fix:**
```rust
let became_empty = !was_empty && app.agents_view.is_empty();
if became_empty {
    app.agents_strip_expanded = false;
}
```

## Info

### IN-01: Every tick now triggers a full dock redraw whenever any agent is present

**File:** `src/mode/tui.rs:2101-2113`
**Issue:** The redraw condition gained `|| !app.agents_view.is_empty()`, so as long as any agent is tracked (even an idle `Queued` one with no activity change), the dock is fully redrawn on every 120ms tick, versus only when a turn/tool/status-note was live before this change. This matches the doc's claim that redraws "come from the existing tick, not per-event," but it is a meaningful increase in steady-state redraw frequency (from "only while something visibly animates" to "continuously while any agent exists, however idle"). Out of scope as a performance defect per review instructions, but worth a note since it also affects the `docs/agents.md` manual-test claim of "no flicker or redraw churn while idle."
**Fix:** If desired, redraw the strip rows only every `TRANSCRIPT_THROTTLE_SECS` or when `collapsed_rows`/elapsed-second display actually changes, rather than unconditionally each tick.

### IN-02: Unbounded in-memory text for `AgentInfo::activity` entries

**File:** `src/mode/agents_strip.rs:253-260`
**Issue:** Once CR-01 is fixed for control characters, `tool_name` is still unbounded in length (unlike `arg`, which is capped to 40 display columns). A pathological `tool_name` value could make each of the (max 3) cached activity strings arbitrarily large in memory, though this is bounded by `TRANSCRIPT_TAIL_BYTES` (64 KiB) per read and clipped visually by `truncate_to_width` at draw time, so impact is low.
**Fix:** Cap `tool_name` with the same `truncate_to_width(..., N)` treatment applied to `arg`.

---

_Reviewed: 2026-10-04T16:43:11Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_

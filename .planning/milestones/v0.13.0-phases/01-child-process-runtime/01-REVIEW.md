---
phase: 01-child-process-runtime
reviewed: 2026-10-03T15:14:07Z
depth: standard
files_reviewed: 23
files_reviewed_list:
  - src/agent/brief.rs
  - src/agent/build.rs
  - src/agent/hook.rs
  - src/agent/loop_.rs
  - src/agent/mod.rs
  - src/config.rs
  - src/lib.rs
  - src/main.rs
  - src/mode/brief_watch.rs
  - src/mode/mod.rs
  - src/mode/print.rs
  - src/mode/tui.rs
  - src/session.rs
  - src/subagent_registry.rs
  - src/tool/edit.rs
  - src/tool/file_state.rs
  - src/tool/mod.rs
  - src/tool/read.rs
  - src/tool/subagent.rs
  - src/tool/write.rs
  - tests/print_mode_e2e.rs
  - tests/subagent_runtime.rs
  - tests/subagent_spawn.rs
findings:
  critical: 2
  warning: 6
  info: 3
  total: 11
status: issues_found
---

# Phase 1: Code Review Report

**Reviewed:** 2026-10-03T15:14:07Z
**Depth:** standard
**Files Reviewed:** 23
**Status:** issues_found

## Summary

I reviewed the changed hunks in 190d730..HEAD, covering the child `-p` CLI surface, the registry and ChildGuard, the parent dispatch in `tool/subagent.rs`, the brief, amendment, and report model, and the stale-write guard. Process-group handling and the in-band failure mapping hold up in isolation. Two contracts break once the pieces run end to end:

- A brief-driven child can spend several times its token budget.
- The JSON-mode child's in-band error envelope never reaches the parent.

The unit tests do not catch either problem. Each piece is tested alone: the limits test calls `run_turn` once, and the envelope test exits 0. As agreed, grandchild orphaning on a hard kill of nanopi is out of scope.

## Critical Issues

### CR-01: `--token-budget` / `--max-turns` reset on every self-check turn; a child can use up to 3x its budget

**File:** `src/agent/loop_.rs:1118-1125` (limits are per `run_turn`), `src/mode/print.rs` (self-check loop, `SELF_CHECK_TURNS = 2`)
**Issue:** `run_turn` sets `turn_tokens = 0` and clears `last_limit_hit` on every call, and `max_turns` bounds only the iterations inside that call. In brief mode, `run_print_mode` calls `run_turn` up to `1 + SELF_CHECK_TURNS` times. Every real child is launched with `--brief`. So a child can consume up to 3 x `token_budget` tokens and 3 x `max_turns` model turns. That breaks the CLI's documented promise ("Stop once this run has used K input+output tokens") and the RT-06 limit guarantee. The self-check prompt itself says "keep working on it first", which tells the model to start more tool loops. The self-check loop only skips the next pass if the *previous* `run_turn` hit a limit; it never deducts usage already spent.
**Fix:** Track usage for the whole run in `Agent`, and have `run_turn` check the remaining allowance:
```rust
// TurnLimits
pub run_tokens: u64,   // cumulative across run_turn calls
pub run_turns: u32,
// in run_turn: compare self.limits.run_tokens + turn_tokens against budget,
// and self.limits.run_turns + iteration_idx against max_turns; add both on exit.
```
Alternatively, `print.rs` can pass the remaining budget and turns into each self-check `run_turn`. Add an e2e test that uses `--brief` with a tool-looping provider and checks total usage stays within the budget.

### CR-02: JSON-mode child failure exits 1, so the parent drops the in-band error envelope

**File:** `src/mode/print.rs` (`Ok(if turn_error.is_some() { 1 } else { 0 })`), `src/tool/subagent.rs:938-954`
**Issue:** On a turn error in `--output json` mode, the child prints a `status: failed, error: "<reason>"` envelope and returns exit code 1. In the parent, `spawn_and_collect_with` checks `!status.success()` *before* it parses stdout, so it returns `"Subagent failed: exit code 1"` and discards the envelope. As a result:
- `envelope_output`'s `status == "failed"` branch is unreachable for real children.
- The provider error reason is lost. The parent only sees it if report.md happens to be readable.
- `agent_id` and `report_path` from the envelope are never surfaced.

`failure_envelope_statuses_map` hides this because its fake child exits 0.
**Fix:** If stdout parses as a `JsonEnvelope`, prefer it over the exit code:
```rust
let parsed: Option<JsonEnvelope> = (!out_truncated)
    .then(|| serde_json::from_str(String::from_utf8_lossy(&out_buf).trim()).ok())
    .flatten();
if let Some(env) = parsed { return envelope_output(env, &stderr_tail); }
// otherwise fall through to signal / exit-code mapping
```
Also add a test where the child prints a failed envelope and exits 1.

## Warnings

### WR-01: A brief that cannot be read silently runs an empty or partial task

**File:** `src/mode/print.rs` (`let task = match &brief_path { ... Err(e) => { eprintln!(...); message.to_string() } }`)
**Issue:** The parent passes `--brief` with no positional message, so `main.rs` sets `message = ""`. If the brief cannot be read (wrong path, permissions, deleted), the child logs to stderr and runs `run_turn("")`. That burns tokens on a turn with no task, writes a report, and may still report `completed`.
**Fix:** Treat an unreadable brief as a fatal error: `anyhow::bail!("cannot read brief {}: {e}", p.display())`. In JSON mode, emit a `status: failed` envelope instead.

### WR-02: Brief role and model are not escaped, so an agent's system prompt can inject amendments

**File:** `src/agent/brief.rs:42,52`
**Issue:** `render_brief` escapes `task` but appends `role` (the agent's `system_prompt`) and `model` verbatim. If a system prompt contains the line `<!-- nanopi:amendments -->`, `parse_amendments_with` (`content.find`) uses that first occurrence. Any `## Amendment N` lines after it are then parsed as amendments. `next_amendment_number` and the watcher's `initial_last` are calculated from those, so genuine amendments can be skipped as `n <= last`. Agent markdown from project scope is repo-controlled.
**Fix:** Run `escape_body` on `role` and `model` as well, or use `rfind` for the marker after making `escape_body` cover every interpolated field.

### WR-03: ChildGuard sends SIGKILL to the pgid after every normal reap (pid-reuse hazard)

**File:** `src/tool/subagent.rs:935-936`, `src/subagent_registry.rs:188-193`
**Issue:** On the success path, `drop(guard)` calls `killpg(pid, SIGKILL)` after `child.wait()` has already reaped the leader. If the child left no grandchildren, that pgid is free, and the kernel can reuse it for an unrelated new group. The research accepted the A1 window for exit-time `kill_all`. This path is different: it runs on *every* successful dispatch, not just on abnormal exit.
**Fix:** Sweep the group *before* reaping. Once the drains reach EOF, call `killpg` while the zombie leader still holds the pgid, then call `wait()`. Alternatively, check with `kill(-pgid, 0)` and accept the race only when grandchildren exist.

### WR-04: A grandchild holding the child's stdout/stderr turns a finished child into a "timed out" failure

**File:** `src/tool/subagent.rs:910-919`
**Issue:** `tokio::join!` waits for both pipes to reach EOF *and* for `child.wait()`. If the child exits normally but anything it spawned inherited its stdout or stderr (for example a backgrounded process launched by a hook or a plugin), the drains never reach EOF. The dispatch then waits the full `timeout` (1800s by default) and reports `timed out`, even though a valid envelope is already buffered.
**Fix:** After `child.wait()` completes, put a short deadline on the drains (for example, 2s). If they don't finish, kill the group first, then parse whatever was buffered.

### WR-05: Amendments can be lost between watcher restarts in the self-check loop

**File:** `src/mode/print.rs` (`start_brief_watch` / self-check loop)
**Issue:** Each `run_turn` gets a new watcher whose `last` comes from a *separate* read of brief.md. In the self-check loop, `current` is read for the prompt and then `start_brief_watch(p)` reads the file again. An amendment appended between those two reads is counted in `last`, but it is missing from the prompt text, so it is never delivered. The same window exists between the initial `task` read and the first watcher. Amendments appended during the last self-check turn after its final steer drain, or after it ends, are dropped with no record in report.md.
**Fix:** Read the brief once, then use that same content both for the prompt or task text and for the watcher's `initial_last`. For example, change `start_brief_watch` to accept the already-read content.

### WR-06: `[subagent]` config values are not validated, so `max_live = 0` or `timeout_secs = 0` silently disable subagents

**File:** `src/config.rs:174-192`, `src/subagent_registry.rs:79`, `src/tool/subagent.rs:910`
**Issue:** `max_concurrency` is clamped with `.max(1)`, but `max_live = 0` makes every `reserve` fail, and `timeout_secs = 0` makes every child time out immediately. Both errors are confusing, and neither names the misconfiguration.
**Fix:** Clamp `max_live` and `timeout_secs` to at least 1 (or reject 0 at config load with a clear message), the same way `max_concurrency` is handled.

## Info

### IN-01: Tool description hardcodes "4 at a time"

**File:** `src/tool/subagent.rs:380,399`
**Issue:** Parallelism is now `cfg.subagent.max_concurrency`, and `max_live` can turn excess parallel items into immediate "limit reached" failures rather than queuing them. The text shown to the model is stale.
**Fix:** Generate the description from the config, or remove the specific number.

### IN-02: Mode 0600 is ignored when report.md already exists

**File:** `src/mode/print.rs` (`write_private`), `src/tool/subagent.rs:653-661`
**Issue:** `OpenOptions::mode` only applies when the file is created, so an existing report.md keeps its mode. It also follows a symlink planted at that path. The child-side writer does not call `set_permissions`.
**Fix:** Call `set_permissions(0o600)` after writing, or write the file through `file_state::atomic_write`.

### IN-03: The brief escape round-trip is lossy for task lines that already start with `\## Amendment `

**File:** `src/agent/brief.rs:25-36,118-130`
**Issue:** `join_body` removes one leading backslash from any line matching `\## Amendment...`, so a line that legitimately started with that backslash loses it. The task section is never parsed back, so the effect is only on amendment text.
**Fix:** Also escape lines that start with `\` + a marker or prefix (double the escape).

---

_Reviewed: 2026-10-03T15:14:07Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_

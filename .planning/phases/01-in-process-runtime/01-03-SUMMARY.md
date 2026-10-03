---
phase: 01-in-process-runtime
plan: 03
subsystem: agent-runtime
tags: [tokio, hooks, permission-broker, stop-reason, deny-list]

requires:
  - "SubagentRegistry, PermissionBroker, FileStateTracker, widened ToolContext from 01-01 (src/agent/subagent_registry.rs, src/tool/mod.rs)"
provides:
  - "HookInput.agent_id (D-16) and HookOutcome::Ask (RT-07 trigger), threaded through build_hook_input/event_payload_json/run_hooks/run_session_hooks"
  - "ToolRegistry::for_subagent() / SUBAGENT_DENIED_TOOLS (D-10/D-17)"
  - "Agent.agent_id/limits/stop_reason/subagents/file_state + StopReason enum; run_turn enforces max_turns/token_budget every iteration (D-08/D-09)"
  - "ToolExecutionStart's Ask outcome routes through SubagentRegistry::permissions().request() (D-13/D-14); run_one_tool builds real ToolContext from threaded agent_id/subagents/file_state/turn_cancel instead of ToolContext::new(cwd)"
affects: [01-04, 01-05, 01-06]

tech-stack:
  added: []
  patterns:
    - "Ask does not short-circuit a hook chain the way Block does: run_hooks remembers the first Ask, keeps running later hooks, and a later Block still wins — only returned if nothing blocked"
    - "Lifecycle hooks (BeforeAgentStart/TurnStart/TurnEnd/MessageEnd/compaction) gated behind `self.agent_id.is_none()`; ToolExecutionStart/End always fire and carry agent_id"
    - "run_one_tool is a free fn, not an Agent method — agent_id/subagents/file_state/turn_cancel are explicit parameters captured from self before the tool-execution futures are spawned, same pattern as the pre-existing cwd/registry/session_path captures"

key-files:
  modified:
    - src/agent/hook.rs
    - src/tool/mod.rs
    - src/agent/loop_.rs
    - src/agent/build.rs
    - src/mode/tui.rs
    - src/plugin_tools.rs

key-decisions:
  - "Agent gets 5 flat fields (agent_id/limits/stop_reason/subagents/file_state) rather than one bundled sub-struct, matching the plan's literal artifact naming and keeping every construction site's diff a 5-line block anchored on the existing system_base: line"
  - "BeforeAgentStart and Input hooks (which support Block/Transform, unlike the purely advisory TurnStart/TurnEnd/MessageEnd) treat an Ask outcome as a no-op/Allow rather than routing through the broker — only ToolExecutionStart has a broker-backed Ask per D-13; the plan never specified ask-routing for these two sites"
  - "run_turn's natural loop-exhaustion only sets LimitReached{max_turns} when limits is Some; with limits None (today's main agent) stop_reason stays None on exhaustion, same as before this plan — preserves the 'main-agent behaviour unchanged' success criterion exactly rather than inventing a new Completed-on-exhaustion case nothing previously asserted"

requirements-completed: []

duration: ~90min
completed: 2026-10-03
---

# Phase 1 Plan 03: Subagent-Ready Agent Loop Summary

**Agent loop now carries per-agent identity/limits/stop-reason/registry/file-state, enforces max_turns and token_budget every iteration, routes a hook's `ask` decision through the shared PermissionBroker, and the tool registry has a deny-list-enforced `for_subagent()` — all with zero behavior change when `agent_id`/`limits` are `None`**

## Performance

- **Duration:** ~90 min
- **Tasks:** 2
- **Files modified:** 6 (2 new modules from 01-01 consumed, no new files created)

## Accomplishments
- `HookInput.agent_id: Option<String>` (`skip_serializing_if` when `None`) threaded through `build_hook_input`, `event_payload_json`, `run_hooks`, `run_session_hooks` — every one of the ~18 call sites across `hook.rs` and `loop_.rs` updated.
- `HookOutcome::Ask { reason }` parsed from `{"decision":"ask"}`; `run_hooks` remembers the first `Ask` but keeps running later hooks (a later `Block` still wins); `report_advisory_outcome` logs-and-allows `Ask` for the purely advisory lifecycle events, same treatment it already gives `Block`.
- `ToolRegistry::for_subagent()` builds `standard()` then strips every `SUBAGENT_DENIED_TOOLS` name (`subagent`, `send_message`, `stop`, `list` — D-10) and every `ToolSource::Plugin` entry (D-17), enforced at construction rather than by hiding names from the subagent's prompt.
- `Agent` gains `agent_id: Option<String>`, `limits: Option<AgentLimits>`, `stop_reason: Option<StopReason>`, `subagents: Arc<SubagentRegistry>`, `file_state: Arc<FileStateTracker>`; new `StopReason { Completed, Cancelled, LimitReached { limit: &'static str } }` enum. All ~29 pre-existing struct-literal construction sites (`loop_.rs`, `build.rs`, `mode/tui.rs`) updated to the main-agent defaults.
- `run_turn`'s loop bound is `limits.max_turns` when `Some`, else the unchanged `MAX_ITERATIONS` (50); a per-iteration token-budget check (after the existing cancel check) stops with `LimitReached{"token_budget"}` the moment cumulative `usage_total` meets the budget; every existing `break` site now records `Completed`; natural exhaustion records `LimitReached{"max_turns"}` only when `limits` is `Some`; cancel now also records `Cancelled`. Steer/cancel plumbing itself (D-07) is untouched.
- D-16: `BeforeAgentStart`/`TurnStart`/`TurnEnd`/`MessageEnd`/compaction hooks are skipped entirely when `agent_id` is `Some`; `ToolExecutionStart`/`End` still fire for every agent and carry `agent_id` in their payload.
- `run_one_tool` (a free function, not an `Agent` method) now takes `agent_id`/`subagents`/`file_state`/`turn_cancel` as explicit parameters and builds `ToolContext` from them, replacing the previous `ToolContext::new(cwd.clone())`. All three call sites (the two in `loop_.rs`'s `execute_tool_calls` + tests, and the plugin-dispatch call in `plugin_tools.rs`) updated.
- `ToolExecutionStart`'s `Ask` outcome routes through `subagents.permissions().request(PermissionRequest { agent_id, summary }, &cancel_token)`: marks the agent `WaitingPermission` (when `agent_id` is `Some`), awaits the broker (Deny mode's non-interactive default resolves `false`; a cancelled token also resolves `false`), restores `Running` afterward, and a `false` result is treated exactly like `Block` with reason `"permission denied by user"`.

## Task Commits

1. **Task 1: Hook agent_id + Ask outcome, subagent tool deny-list** - `8bb73b2` (feat)
2. **Task 2: Per-agent limits, StopReason, Ask routing, ToolContext wiring** - `5214bb4` (feat)

## Files Created/Modified
- `src/agent/hook.rs` — `HookInput.agent_id`, `HookOutcome::Ask`, `run_hooks` Ask-aggregation, `report_advisory_outcome` Ask arm, `parse_json_decision` ask branch; 6 new/updated tests
- `src/tool/mod.rs` — `SUBAGENT_DENIED_TOOLS`, `ToolRegistry::for_subagent()`; `subagent_registry_denies_control_tools`, `subagent_registry_denies_plugin_tools` tests
- `src/agent/loop_.rs` — `Agent` struct fields, `StopReason` enum, loop-limit logic, lifecycle-hook D-16 gating, `run_one_tool` parameter threading + Ask/broker routing + real `ToolContext` construction; `turn_limit_yields_partial_report`, `token_budget_yields_partial_report`, `main_agent_without_limits_completes_normally` tests
- `src/agent/build.rs` — both `Agent` literal construction sites (`build_fresh`, `prompt_agent` test helper) given the 5 new field defaults
- `src/mode/tui.rs` — `agent_with_id` test helper given the 5 new field defaults
- `src/plugin_tools.rs` — 4 `run_one_tool` call sites (prod `call_blocking` + 3 tests) updated for the new parameters, passing a standalone registry and no `agent_id` (plugin-originated calls have no agent identity)

## Decisions Made
- Flat fields on `Agent` rather than a bundled sub-struct — matches the plan's literal artifact naming (`Agent.agent_id/limits/stop_reason/subagents/file_state`) and keeps every one of the ~29 construction-site diffs a uniform 5-line block anchored on the pre-existing `system_base:` line.
- `BeforeAgentStart`/`Input` hooks (the two that support `Block`/`Transform` directly in `run_turn`, unlike the four purely-advisory lifecycle events) treat `Ask` as a no-op (same as `Allow`) rather than routing through the broker — the plan's D-13 broker routing is scoped to `ToolExecutionStart` specifically; these two sites have no natural "tool call" to attach a permission summary to.
- Natural loop-exhaustion with `limits: None` leaves `stop_reason` as `None`, not `Completed` — preserves "main-agent behaviour unchanged" literally: nothing in the pre-existing code ever asserted a stop reason for the main agent's 50-iteration cap, and inventing one here would be new unrequested behavior rather than a requirement of this plan.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking issue] `run_one_tool` call sites outside `loop_.rs` needed the same parameter updates**
- **Found during:** Task 2, after changing `run_one_tool`'s signature
- **Issue:** `src/plugin_tools.rs` calls `crate::agent::loop_::run_one_tool` directly from the plugin `host-call-tool` dispatch path (1 production call site, 3 test call sites) — not mentioned in the plan's `<files>` list for either task, but required for the crate to compile once the signature gained 4 new parameters.
- **Fix:** Updated all 4 call sites to pass `None` for `agent_id` (plugin-originated calls have no agent identity) and a fresh `standalone()` registry / default `FileStateTracker` / `None` cancel token, matching the existing `ToolCallOrigin::Plugin` semantics (no turn-level cancel token exists for a plugin-initiated call).
- **Files modified:** `src/plugin_tools.rs`
- **Commit:** `5214bb4`

**2. [Rule 3 - Blocking issue] `src/mode/tui.rs`'s `agent_with_id` test helper needed the 5 new Agent fields**
- **Found during:** Task 2
- **Issue:** One `Agent { ... }` struct literal in `mode/tui.rs`'s test module was missed by the mechanical `system_base:`-anchored field-insertion pass (that pass only touched `loop_.rs` and `build.rs`), causing an `E0063` missing-fields compile error.
- **Fix:** Added the same 5-field default block (`agent_id: None`, `limits: None`, `stop_reason: None`, `subagents: standalone()`, `file_state: default()`) immediately after its `system_base: None,` line.
- **Files modified:** `src/mode/tui.rs`
- **Commit:** `5214bb4`

None of these changed behavior outside what the plan specified — both are mechanical consequences of widening `Agent`/`run_one_tool`'s signatures that the plan's `<files>` lists didn't enumerate (they list only `loop_.rs`/`build.rs`/`hook.rs`/`tool/mod.rs`), not new functionality.

## Issues Encountered
None blocking. The largest mechanical cost was threading the new trailing `agent_id: Option<&str>` parameter through `hook.rs`'s three public functions (`build_hook_input`, `event_payload_json`, `run_hooks`) across their ~18 call sites in `loop_.rs` — done via a bracket-aware Python script per call site (with one hand-fixed site where an inline comment's comma confused the arg-counting heuristic).

## User Setup Required
None.

## Requirements Completed

None marked complete by this plan specifically. RT-05/RT-06/RT-07 were already marked `[x]` in `REQUIREMENTS.md` from `01-01` (registry-level deny-list, limits struct, and broker respectively) and this plan implements the loop-level consumption of those same interfaces (D-10 enforced at `ToolRegistry::for_subagent()`, D-08/D-09 enforced every iteration in `run_turn`, D-13/D-14 enforced at the `ToolExecutionStart` ask site) — no new requirement line items flip here.

RT-02 ("Each subagent has its own cancel token... foreground stops on Esc, background is unaffected") remains `[ ]` deliberately: this plan threads a `turn_cancel: Option<CancellationToken>` parameter through `run_one_tool`/`ToolContext` and lets `Agent.subagents` carry a real registry, but no subagent is actually spawned yet (no `tokio::spawn` of a child `Agent::run_turn`, no foreground/background distinction, no Esc wiring across multiple concurrent turns) — that is 01-04/01-05's job. Marking RT-02 complete here would be premature per the "only mark complete if the plan fully delivers it end-to-end" instruction.

## Next Phase Readiness
- `cargo test --lib -- --test-threads=1` green at 826 passed / 0 failed / 1 ignored (823 baseline from 01-02 + 3 new: `turn_limit_yields_partial_report`, `token_budget_yields_partial_report`, `main_agent_without_limits_completes_normally`; Task 1 alone added `subagent_registry_denies_plugin_tools` plus 5 hook tests, accounted in the same total).
- `cargo build --features wasm` green.
- `Agent.agent_id`/`limits`/`subagents`/`file_state`/`turn_cancel` are now real, consumed fields — 01-04 (actual subagent spawning) can set `agent_id: Some("a1")`, `limits: Some(cfg.into())`, and share one `Arc<SubagentRegistry>`/`Arc<FileStateTracker>` across parent and child `Agent` instances without further plumbing changes to `run_turn`/`run_one_tool`.
- No blockers for 01-04/01-05.

---
*Phase: 01-in-process-runtime*
*Completed: 2026-10-03*

## Self-Check: PASSED
Both task commits (`8bb73b2`, `5214bb4`) verified present in git log; `src/agent/hook.rs`, `src/tool/mod.rs`, `src/agent/loop_.rs`, `src/agent/build.rs`, `src/mode/tui.rs`, `src/plugin_tools.rs` all exist with the expected content; `cargo test --lib -- --test-threads=1` (826 passed) and `cargo build --features wasm` both green as of this summary.

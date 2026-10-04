---
phase: 06-orchestrator-mode
reviewed: 2026-10-04T17:42:05Z
depth: standard
files_reviewed: 8
files_reviewed_list:
  - docs/agents.md
  - docs/v0.13-manual-test-plan.md
  - src/agent/build.rs
  - src/agent/system_prompt.rs
  - src/command.rs
  - src/config.rs
  - src/mode/print.rs
  - src/mode/tui.rs
  - src/tool/mod.rs
  - tests/agent_spawn.rs
findings:
  critical: 2
  warning: 2
  info: 1
  total: 5
status: issues_found
---

# Phase 06: Code Review Report

**Reviewed:** 2026-10-04T17:42:05Z
**Depth:** standard
**Files Reviewed:** 10
**Status:** issues_found

## Summary

Reviewed the orchestrator-mode toggle feature: restricted `ToolRegistry::orchestrator()`, the coordinator system prompt (`build_orchestrator`), the `/orchestrator` slash command, the `[experimental] orchestrator` config flag, re-application of the mode across Agent-rebuild sites (`/new`, `/resume`, `/import`, `/fork`, startup), the status-line indicator, and print-mode's warn-but-ignore behavior. The byte-identical-by-default guarantee is well tested (snapshot tests pin both the default system prompt and the default tool spec list), the `/model` path correctly never touches the registry/prompt so no re-application is needed there, and print-mode's gating is correctly scoped to the parent process only (not propagated to spawned children) and is covered by an end-to-end test that proves `write` still actually works under `-p` despite the flag.

However, two real synchronization gaps were found that violate the stated invariant ("registry and `context.tools`/system prompt stay in sync," T-06-04/T-06-06): `/reload` recomposes the system prompt unconditionally with the non-orchestrator template even when orchestrator mode is active, and toggling `/orchestrator` while a turn is in flight silently fails to apply (because the Agent is temporarily moved out of `agent_slot`) while still reporting success and flipping the status-line flag.

## Critical Issues

### CR-01: `/reload` recomposes the system prompt with the wrong template while orchestrator mode is active

**File:** `src/mode/tui.rs:4469-4482`
**Issue:** `handle_reload` rebuilds the live Agent's system prompt base unconditionally via `crate::agent::build::compose_system_prompt(...)` (the plain, non-orchestrator variant), regardless of `app.orchestrator`. All four rebuild sites that are orchestrator-aware (`/new`, `/resume`, `/import`, `/fork`) call `compose_system_prompt_mode(.., app.orchestrator)` through `apply_orchestrator_mode`, but `/reload` was missed. Since `/reload` does not rebuild `a.registry` from scratch (it only calls `a.reload_extensions(exts)`, which adds to whatever registry is currently installed — the orchestrator-restricted one, if orchestrator mode is on), the result after `/reload` while orchestrator mode is active is: `context.tools` still reflects the restricted 7-tool registry (plus any newly loaded plugin tools), but the system prompt reverts to the generic "You help the user by reading files, running shell commands, editing files, and writing new files" prompt that describes `write`/`edit`/`bash` as available and recommends bash-first guidelines — none of which exist in the registry anymore, and the orchestrator workflow instructions (plan/dispatch/monitor/verify) are lost. This breaks the registry/prompt sync invariant the feature is built around, and silently downgrades orchestrator mode to "restricted tools with a prompt that contradicts the restriction" until the user does `/new`/`/resume`/`/fork`/`/import` or toggles `/orchestrator` off and back on.
**Fix:**
```rust
let tool_names = a.registry.names();
let base = crate::agent::build::compose_system_prompt_mode(
    &a.cwd,
    &tool_names,
    &a.skills,
    a.no_context_files,
    &a.prompt_overrides,
    app.orchestrator,
);
a.set_system_base(base);
```

### CR-02: `/orchestrator` toggle silently no-ops mid-turn but still reports success and flips the status flag

**File:** `src/mode/tui.rs:3058-3068`
**Issue:** `KeyAction::SetOrchestrator` is reachable from the command palette regardless of `app.status` (the palette's key handling in the Enter/nav branch, `src/mode/tui.rs:1498-1521`, does not check `Status::Streaming` the way free-text `Enter` submission does at `tui.rs:1804`). While a turn is running, the Agent is moved out of `agent_slot` for the duration of the turn (documented explicitly in the `/reload` comment at `tui.rs:4445-4452`). `SetOrchestrator`'s handler does `let mut g = agent_slot.lock().await; if let Some(a) = g.as_mut() { apply_orchestrator_mode(app, a, new_val); }` — when the slot is empty (turn in flight), the swap is silently skipped, yet `app.orchestrator` has already been set to `new_val` *before* the lock is taken, and the handler unconditionally prints `"orchestrator mode on (from next turn)"` / `"...off (from next turn)"`. The status-line segment (`draw_dock`, reads only `app.orchestrator`) will show orchestrator mode as ON immediately, while the actual Agent registry/prompt remain unchanged and stay that way indefinitely — unlike `/new`/`/resume`/`/fork`/`/import`, nothing re-applies the pending toggle once the turn completes and the Agent is returned to the slot. The user is told the toggle "will take effect from next turn," but the next turn runs with the stale registry/prompt and no further toggle fires to reconcile it, directly contradicting T-06-06 (status-line must reflect the real source of truth).
**Fix:** Detect the busy case (mirroring `handle_reload`'s `agent_busy` flag) and either (a) refuse the toggle with a "turn in progress, try again after it finishes" message without changing `app.orchestrator`, or (b) queue the pending flip and apply it when the Agent is returned to the slot after the turn completes (e.g. check-and-apply at the point `StartTurn`'s task puts the Agent back).

## Warnings

### WR-01: `/reload`'s extension reload can add tools into the orchestrator-restricted registry that bypass the closed-list design

**File:** `src/mode/tui.rs:4466-4468`, `src/tool/mod.rs:438-448`
**Issue:** `ToolRegistry::orchestrator()`'s doc comment states it is deliberately "its own closed list" so that `write`/`edit`/`bash` can never leak in through a future change to the standard/control registries. But `/reload`'s `a.reload_extensions(exts)` registers plugin ("external") tools into whatever registry the Agent currently holds — including the orchestrator-restricted one, if active — with no orchestrator-specific guard. A plugin extension registering a `bash`-equivalent tool (or anything that shells out) would end up callable from orchestrator mode after a `/reload`, defeating the "no direct file/shell access in orchestrator mode" guarantee the feature exists to enforce. This is adjacent to CR-01 but is a distinct gap: even with CR-01 fixed (prompt resynced), the tool itself would still be live and callable.
**Fix:** When `app.orchestrator` is true, either skip plugin-tool registration on reload (require an explicit `/orchestrator off` first) or filter `reload_extensions` registrations through the same closed allowlist used to build `ToolRegistry::orchestrator()`, and document the decision next to the "closed list" comment in `src/tool/mod.rs`.

### WR-02: `apply_orchestrator_mode`'s off-path silently no-ops when `saved_registry` is `None` for reasons other than "mode was never on"

**File:** `src/mode/tui.rs:2947-2963` (`apply_orchestrator_mode`)
**Issue:** The off-branch is `else if let Some(saved) = app.saved_registry.take() { agent.registry = saved; }` — if `saved_registry` is `None` when `on=false` is requested (e.g., as a side effect of the CR-02 race: a toggle-on is lost mid-turn, then a later toggle-off is attempted against an Agent whose registry was never actually swapped), the function does nothing and returns without feedback, leaving `agent.context.tools`/prompt recomposed from whatever `agent.registry` already was. This is only reachable via the CR-02 race today, but it's a silent-failure branch with no diagnostic, which will make the CR-02 symptom harder to debug in the field (no error, no log line — just "off" doing nothing).
**Fix:** At minimum, assert/log when `on=false` is requested with `saved_registry` already `None`, so the inconsistency is visible rather than silently swallowed once CR-02 is triggered.

## Info

### IN-01: `ToolRegistry::orchestrator()` and the control tools (`agent_ctl::*`) use parameterless `::new()` constructors with no visible wiring check

**File:** `src/tool/mod.rs:438-448`
**Issue:** `ToolRegistry::orchestrator()` constructs fresh `AgentTool::new()`, `ListAgentsTool::new()`, `StopAgentTool::new()`, `SendMessageTool::new()` instances, identical in shape to how `standard_with_control()` builds them. This matches the existing pattern so it is not a new defect, but given this phase's explicit goal of "re-application... incl. plugin tools," it's worth a one-line comment (or a shared-state test) confirming these tools correctly bind to the single shared `AgentRegistry`/background-agent bookkeeping regardless of which `ToolRegistry` variant constructs them, since that wiring is implicit rather than passed explicitly into `orchestrator()`.
**Fix:** Add a short comment at `ToolRegistry::orchestrator()` cross-referencing wherever `AgentTool`'s shared state is actually injected (context/launcher), so a future reader doesn't have to chase this the way this review did.

---

_Reviewed: 2026-10-04T17:42:05Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_

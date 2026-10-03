# Research Summary — v0.13.0 Orchestrator & Dynamic Subagents

Sources: STACK.md, FEATURES.md, ARCHITECTURE.md, PITFALLS.md (this directory).

## Executive Summary

nanopi already has what in-process subagents need. `Agent` can be
spawned as a task. `run_turn` already takes a `CancellationToken`
(stop) and a steer receiver (amend). The TUI loop can read a shared
registry on its 120 ms tick. No new crates are needed.

The plan: replace the child-process `subagent` runtime with a
`SubagentRegistry` passed through `ToolContext`. Then add, in order:
`.md` archives, dynamic dispatch, background launch with control tools,
the TUI agents strip, and orchestrator mode. Orchestrator mode is built
from tool filtering plus a coordinator prompt.

Main risks:
- orphaned tasks
- `panic = "abort"`
- concurrent file edits
- recursion and cost
- permission prompts from background agents
- an orchestrator that does the work itself

## Stack

- No new crates. Use tokio 1.53, tokio-util 0.7 (`CancellationToken`),
  ratatui 0.29 and crossterm 0.29, which are already locked.
- Stop: a child `CancellationToken` per agent.
- Amend: the existing `SteerMessage` path.
- Events: a `std::sync::Mutex` registry snapshot plus `JoinSet` /
  `oneshot` for reports.
- Do not upgrade ratatui to 0.30 this milestone.
- Do not add flume, crossbeam, dashmap, actor frameworks, or YAML /
  front-matter crates.

## Features (reference: claude-code-haha-main)

**Table stakes:**
- Optional agent type, with a general-purpose default.
- Inline role prompt and toolset per call.
- Predefined agents and single / parallel / chain modes keep working.
- Subagents cannot spawn subagents.
- A capped summary is returned to the parent.
- Agent ids.
- Stop and amend.
- Background launch, with results injected as messages.
- Per-agent status.
- An opt-in orchestrator toggle with a restricted toolset.
- A concurrency cap.
- The bottom agents strip.

**Differentiators:**
- The `.md` brief / report archive.
- Continuing a finished agent.
- Recovery from the archive.
- Inline stop / amend from the expanded panel.
- A model override per agent.
- Worktree isolation.

**Anti-features:**
- Nested teams or swarms.
- Worker-to-worker messaging.
- Streaming full transcripts into the parent.
- An orchestrator that can edit.
- Orchestrator mode as the default.
- Hard-killing an agent mid-write.
- Forking the parent's context by default.

## Architecture

- **Integration points:**
  - `Agent` (src/agent/loop_.rs:76) and `Agent::build_fresh`
    (src/agent/build.rs:321).
  - The `run_turn` steer and cancel inputs (loop_.rs:1080-1130).
  - The TUI `select!` in `run_app` (src/mode/tui.rs:1828).
  - The `draw_dock` layout (tui.rs:5041-5098).
  - `ToolContext` (src/tool/mod.rs:309).
- **New:**
  - `SubagentRegistry`.
  - A subagent runner.
  - An `.md` archive writer.
  - Control tools: `send_message`, `stop_agent`, `list_agents`.
- **Modified:**
  - `subagent.rs` is rewritten, removing `run_single` and
    `spawn_and_collect`.
  - `event.rs` gets subagent events.
  - `Agent` gets an orchestrator flag.
  - The TUI gets the strip, the toggle and the shortcut.
  - Print mode drains or kills background agents before it exits.
- **Background reports** reuse the follow-up path: they start a new turn
  when the main agent is idle, and arrive as `FollowUp` while it is
  streaming.

## Top Pitfalls

1. **Orphaned tasks after Esc.** Today's child processes die when
   dropped (`kill_on_drop`); in-process tasks do not. Fix: a cancel
   token tree plus a registry of running tasks.
2. **`panic = "abort"`.** A panic in any subagent kills the whole
   process. Fix: error handling throughout, and a panic audit.
3. **Recursion and toolset escalation.** Fix: a deny-list of tools no
   subagent gets, a depth cap, a global cap on live agents, and a
   turn / token budget per agent.
4. **The orchestrator does the work itself.** Fix: do not register the
   mutating tools in orchestrator mode, and test that they are absent.
5. **Amend races.** Fix: apply amends only between turns, never during a
   tool call.
6. **Report loss.** Fix: write `report.md` before returning the result.
7. **Permission prompts from background agents.** Fix: auto-deny, or one
   queued approval surface.

Separately, each subagent needs its own session file.

## Reconciled Phase Order

1. **In-process runtime.**
   - Delivers: cancel token tree, a session file per agent, permission
     auto-deny, cloned file state plus a stale-edit check, and a global
     semaphore.
   - Everything else depends on this phase.
2. **Archive and lifecycle.**
   - Delivers: `brief.md`, and `report.md` written before the result is
     returned.
   - Agents move through a state machine and are marked `interrupted`
     on load.
3. **Dynamic subagents.**
   - Delivers: optional `agent`, inline prompt and tools, an allowlist
     plus deny-list, `max_turns` and a budget.
4. **Background launch and control tools.**
   - Delivers: `send_message`, `stop_agent`, `list_agents`, report
     injection, and a print-mode drain.
5. **TUI agents strip.**
   - Comes before orchestrator mode so that mode can be watched and
     tested by hand.
6. **Orchestrator mode.**
   - Built only from phases 3–5.
   - Mutating tools are not registered.
   - With the toggle off, prompts and tool specs stay byte-identical to
     v0.12.

Every new control (amend, expand key, toggle) needs a row in the manual
end-to-end test plan.

## Conflicts Resolved

- **Panics:** `abort` makes `catch_unwind` useless, so the fix is error
  handling.
- **Esc:** foreground children stop with the main turn. Background and
  orchestrated children are stopped only from the strip or with
  `stop_agent`.
- **Status:** the TUI reads a registry snapshot on each tick, rather
  than one watch channel per agent.
- **Continuing a finished agent:** deferred.

## Research Flags

- **Needs research:**
  - Phase 1: WASM extensions shared across agents, the panic audit, the
    file-state design, and a shared 429 backoff.
  - Phase 4: the injection path and print-mode exit.
  - Phase 6: prompt and cost evaluation.
- **Standard patterns:** phases 2, 3 and 5.

## Open Questions

1. Should Esc stop background agents?
2. Should hooks fire for subagents, or should there be a new
   `subagent_start` event?
3. Can WASM extensions be shared across concurrent agents?
4. Which key expands the strip? Ctrl+O is taken; candidates are Ctrl+G
   and F2.
5. Archive location, whether it is gitignored, and pruning.
6. Remove the child-process runtime, or keep it as a fallback?
7. Background permission requests: auto-deny, or a queued approval
   surface?
8. Defaults for `max_turns`, budget and concurrency, and whether they
   are configurable.
9. Should the orchestrator get read-only bash?
10. How is the toggle exposed: slash command, flag, env var, print
    mode?
11. Keep the release panic strategy as `abort`?
12. Is a per-path advisory lock needed across agents?

## Confidence

| Area | Level |
|------|-------|
| Stack | HIGH |
| Features | HIGH for Claude Code, MEDIUM for other tools |
| Architecture | HIGH on integration points, MEDIUM on design |
| Pitfalls | MEDIUM-HIGH |
| Overall feasibility | HIGH |

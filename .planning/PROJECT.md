# nanopi

## What This Is

nanopi is a tiny Rust port of the Pi coding-agent CLI — a ~4 MB static
binary with zero runtime dependencies, aimed at old and low-resource
Linux boxes. It has a TUI, a print mode (`-p`), sessions, hooks, WASM
extensions, and a built-in `subagent` tool.

## Core Value

A capable coding agent that fits in a tiny static binary and runs
anywhere.

## Current Milestone: v0.13.0 Orchestrator & Dynamic Subagents

**Goal:** Let nanopi dispatch subagents on its own, based on the task at
hand, and add an experimental orchestrator mode in which the main agent
only plans and delegates.

**Target features:**
- **Dynamic subagents** — the model dispatches a subagent by describing
  the task, optionally with an ad-hoc role prompt and toolset. Defining
  an agent file first is no longer required; predefined agents remain
  usable. Reference: `/root/workspace/claude-code-haha-main`.
- **Orchestrator mode (experimental toggle, TUI)** — the main agent
  analyses and splits the work, then assigns it to subagents and does no
  implementation itself. It can amend a running subagent's task or stop
  it mid-run, and it collects each subagent's report and summarises it
  for the user.
- **Orchestrator ↔ subagent communication** — subagents run in-process
  with channels for real-time control (amend / stop / report). Task
  briefs and final reports are also written as local `.md` files so
  people can inspect them and work can be recovered.
- **TUI agents panel** — a bottom status strip (1–3 lines) listing each
  subagent's state, task and elapsed time, with a shortcut to expand the
  details.

## Requirements

### Validated

- Single / parallel / chain `subagent` tool, child-process based,
  driven by predefined agent files (`src/tool/subagent.rs`) — v0.12.x
- TUI with steer / follow-up injection — v0.11.0
- Shell hooks, WASM extensions, plugin slash commands — v0.11.0

### Active

- See `.planning/REQUIREMENTS.md` for v0.13.0.

### Out of Scope

- (filled in during requirements definition)

## Key Decisions

| Decision | Rationale | Date |
|----------|-----------|------|
| Subagents move in-process, with `.md` archives | Channels give low-latency amend / stop; `.md` keeps the human-readable trail the owner asked for | 2026-10-03 |
| Orchestrator mode is an experimental, opt-in toggle | Changes how the main agent behaves; must not affect the default flow | 2026-10-03 |
| Agents panel is a collapsible bottom strip | Fits small terminals; detail only on demand | 2026-10-03 |

## Evolution

This document evolves at phase transitions and milestone boundaries.

**After each phase transition** (via `/gsd-transition`):
1. Requirements invalidated? → Move to Out of Scope with reason
2. Requirements validated? → Move to Validated with phase reference
3. New requirements emerged? → Add to Active
4. Decisions to log? → Add to Key Decisions
5. "What This Is" still accurate? → Update if drifted

**After each milestone** (via `/gsd:complete-milestone`):
1. Full review of all sections
2. Core Value check — still the right priority?
3. Audit Out of Scope — reasons still valid?
4. Update Context with current state

---
*Last updated: 2026-10-03 — milestone v0.13.0 started*

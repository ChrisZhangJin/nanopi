# nanopi

## What This Is

nanopi is a tiny Rust port of the Pi coding-agent CLI — a ~4 MB static
binary with zero runtime dependencies, aimed at old and low-resource
Linux boxes. It has a TUI, a print mode (`-p`), sessions, hooks, WASM
extensions, and a built-in `agent` tool.

## Core Value

A capable coding agent that fits in a tiny static binary and runs
anywhere.

## Current State

v0.13.0 Orchestrator & Dynamic Agents shipped 2026-10-04 (6 phases,
30 plans, 40/40 requirements). No milestone in progress — next step is
`/gsd:new-milestone`. History: `.planning/MILESTONES.md`,
`.planning/milestones/`.

## Requirements

### Validated

- Single / parallel / chain `agent` tool, child-process based,
  driven by predefined agent files (`src/tool/agent.rs`) — v0.12.x
- TUI with steer / follow-up injection — v0.11.0
- Shell hooks, WASM extensions, plugin slash commands — v0.11.0
- ✓ Child-process agent runtime (isolated `nanopi -p` children, caps,
  limits, brief amendments, self-check, stale-write guard) — v0.13.0
- ✓ `.md` archive & lifecycle (brief/report, interrupted marking,
  `/agents clean`, auto-prune) — v0.13.0
- ✓ Dynamic agents (optional agent name, inline role/tools/model,
  capped report) — v0.13.0
- ✓ Background launch & control (amend/stop/list/continue, report
  injection, `-p` drain, worktree isolation) — v0.13.0
- ✓ TUI agents strip (display-only, Ctrl+G) — v0.13.0
- ✓ Experimental orchestrator mode (`/orchestrator`, restricted tools,
  default path byte-identical) — v0.13.0

### Active

- (none — define with `/gsd:new-milestone`)
- Carry-over: human UAT for phases 01/04/05/06 and accepted tech debt
  listed in `milestones/v0.13.0-MILESTONE-AUDIT.md`
- Provider registration from plugins (blocked on owner decision, see
  `docs/BACKLOG.md`)

### Out of Scope

- In-process agents with channels — superseded by child processes for
  crash isolation (v0.13.0 phase 1 rollback)
- User-facing stop/approve controls in the agents strip — control goes
  through the orchestrator only
- Orchestrator mode in `-p` print mode — key is ignored with a note

## Context

Rust, ~4.9 MB release binary (v0.13.0 added ~11 KB). Known debt: 59
pre-existing `cargo clippy -D warnings` errors; several Nyquist
validation files partial.

## Key Decisions

| Decision | Rationale | Date |
|----------|-----------|------|
| Agents move in-process, with `.md` archives | Channels give low-latency amend / stop; `.md` keeps the human-readable trail the owner asked for | 2026-10-03 (superseded: reverted to child processes, `2bd0343`) |
| Agents are `nanopi -p` child processes controlled via brief-file amendments | Crash isolation; never leaks into parent session — ✓ Good | 2026-10-03 |
| Control tools registered only on main process (`standard_with_control`) | Children structurally cannot get them — ✓ Good | 2026-10-04 |
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
*Last updated: 2026-10-04 after v0.13.0 milestone*

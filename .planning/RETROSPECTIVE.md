# Project Retrospective

*A living document updated after each milestone. Lessons feed forward into future planning.*

## Milestone: v0.13.0 — Orchestrator & Dynamic Agents

**Shipped:** 2026-10-04
**Phases:** 6 | **Plans:** 30

### What Was Built
- Child-process agent runtime (`nanopi -p` children, process-group kill, caps, limits, brief amendments, self-check, cross-process stale-write guard)
- Loss-proof `.md` archive with lifecycle states, interrupted marking, auto-prune and `/agents clean`
- Dynamic agents with inline role/tools/model and an 8 KiB capped parent report
- Background launch and control tools (send_message / stop_agent / list_agents), report injection, `-p` drain, worktree isolation
- Display-only TUI agents strip (Ctrl+G)
- Experimental `/orchestrator` mode with a restricted 7-tool registry; default path byte-identical to v0.12

### What Worked
- Rolling back the in-process design early (`2bd0343`) and re-planning phase 1 around child processes kept isolation guarantees structural rather than conventional
- Keeping control tools out of `standard()` so children are structurally incapable of receiving them
- Byte-identical snapshot tests for the default prompt/tool specs protected the non-orchestrator path
- Binary size gate: +11 KB against a ~150 KB budget

### What Was Inefficient
- Phase 1 was executed twice (in-process, then child-process)
- Several requirements are verified at code level only; human UAT for phases 01/04/05/06 still pending
- Nyquist validation left partial on phases 01, 02, 05

### Patterns Established
- Brief-file amendments applied at turn boundaries as the parent→child control channel
- Atomic temp+fsync+rename writes for durable archive files
- Real-binary end-to-end tests for print-mode behaviour

### Key Lessons
1. Decide process vs in-process isolation before planning, not after executing.
2. Schedule human UAT inside the milestone rather than deferring it to close.
3. Track the pre-existing clippy baseline so it does not get reported as phase debt each time.

### Cost Observations
- Model mix: not tracked
- Sessions: not tracked
- Notable: whole milestone executed in ~2 days

---

## Cross-Milestone Trends

### Process Evolution

| Milestone | Sessions | Phases | Key Change |
|-----------|----------|--------|------------|
| v0.13.0 | n/a | 6 | First fully GSD-phased milestone (earlier work via quick tasks) |

### Cumulative Quality

| Milestone | Tests | Coverage | Zero-Dep Additions |
|-----------|-------|----------|-------------------|
| v0.13.0 | n/a | n/a | 1 (`unicode-width`, already transitive) |

### Top Lessons (Verified Across Milestones)

1. Manual acceptance passes find defects that unit tests next to correct code cannot (v0.12 manual run; v0.13 UAT still owed).

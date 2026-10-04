# Milestones

## v0.13.0 Orchestrator & Dynamic Agents (Shipped: 2026-10-04)

**Delivered:** nanopi can dispatch agents on its own (as isolated `nanopi -p`
child processes), control them in the background, show them in a TUI strip,
and run an experimental opt-in orchestrator mode.

**Phases completed:** 6 phases, 30 plans, 43 tasks
**Requirements:** 40/40 satisfied (audit status: tech_debt — accepted)
**Timeline:** 2026-10-03 → 2026-10-04
**Binary size delta:** +11,120 bytes vs 4,881,744-byte baseline; `unicode-width` the only new direct dependency

**Key accomplishments:**

- Child-process agent runtime: process-group kill, global live cap, turn/token limits, brief.md amendments applied at turn boundaries, final self-check, cross-process stale-write guard.
- Loss-proof archive: `.nanopi/agents/<run>/<id>/brief.md` + `report.md` with front-matter, durable state machine, startup `interrupted` marking, auto-prune and `/agents clean`.
- Dynamic agents: `agent` field optional (general-purpose fallback), inline role/tools/model validated before spawn, 8 KiB capped parent report.
- Background launch & control: `background: true`, `send_message` / `stop_agent` / `list_agents` (main process only), report injection via follow-up path, `-p` drain, opt-in git worktree isolation.
- TUI agents strip: display-only 1–3 line dock strip, Ctrl+G expand/collapse, tick-driven refresh.
- Orchestrator mode: `/orchestrator` toggle with restricted 7-tool registry and coordinator prompt; default path byte-identical to v0.12; ignored (with stderr note) in `-p`.

**Known deferred items at close:** 16 (see STATE.md Deferred Items) — 4 verification files awaiting human UAT (phases 01, 04, 05, 06) and 12 legacy quick tasks without completion markers. Tech debt detailed in `milestones/v0.13.0-MILESTONE-AUDIT.md`.

---

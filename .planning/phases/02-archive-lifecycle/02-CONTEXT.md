# Phase 2: Archive & lifecycle - Context

**Gathered:** 2026-10-03
**Status:** Ready for planning

<domain>
## Phase Boundary

Every agent leaves a durable `.md` record that a person can read.
Agents move through an explicit state machine. The archive is cleaned
up automatically and on demand. Covers ARC-01..05.

</domain>

<decisions>
## Implementation Decisions

### Layout
- **D-01:** Each agent gets a directory
  `.nanopi/agents/<run>/<id>/`, under the project root, containing
  `brief.md`, `report.md` and `transcript.jsonl` (from Phase 1).
  `<run>` is `YYYYMMDD-HHMMSS-<short-uuid>`, one per nanopi process.
  `.nanopi/agents/<run>/index.md` lists the agents in that run with
  their state.
- **D-02:** `brief.md` has a small hand-written front-matter block:
  id, role, model, tools, state, started, parent. The body holds the
  task text. Each amendment is appended as
  `## Amendment N (<time>)`. No YAML crate is used.
- **D-03:** `report.md` front-matter: id, final state, ended, turns,
  tokens, and the worktree / branch if there is one. The body is the
  agent's final summary, then files changed, then open issues.

### Durability
- **D-04:** Write `report.md` (fsync, write to a temp file then rename)
  before returning the result to the parent or emitting the done
  event. This applies to every terminal state: done, failed, stopped,
  limit_reached. On failure, the report is the error text plus whatever
  partial summary exists.

### State machine
- **D-05:** The states are `queued → running ⇄ waiting_permission →
  done | failed | stopped | limit_reached`, plus `interrupted`.
  `state` in the brief front-matter and `index.md` is kept current.
- **D-06:** At startup, every archived agent whose state is not
  terminal is rewritten as `interrupted`. It is never re-run.

### Git / search hygiene
- **D-07:** When an archive is first created, add `.nanopi/agents/` to
  the project `.gitignore` if it is a git repo and the entry is
  missing. This happens once and is idempotent.
- **D-08:** The built-in grep and glob tools skip `.nanopi/agents/`.

### Cleanup
- **D-09:** Auto-prune at startup: delete run directories older than
  **2 days**. This is configurable as `agent.archive_keep_days`
  (default 2; 0 disables auto-prune). Never delete the current run, or
  a run that still has live agents.
- **D-10:** `/agents clean` removes all runs except the current one.
  `/agents clean --older <days>` removes only older runs. It reports
  what it removed (count and size).

### Claude's Discretion
- Exact markdown wording and layout; temp-file naming.

</decisions>

<specifics>
## Specific Ideas

The owner wants the md files so a person can open them during and after
a run. Keep them readable, not machine dumps.

</specifics>

<canonical_refs>
## Canonical References

- `.planning/REQUIREMENTS.md` — ARC-01..05
- `.planning/phases/01-in-process-runtime/01-CONTEXT.md` — the
  registry and transcript path
- `.planning/research/PITFALLS.md` — report loss, resume
- `src/command.rs` — where slash commands are registered
- `src/paths.rs` — path helpers

</canonical_refs>

<code_context>
## Existing Code Insights

- `src/paths.rs` owns every nanopi path; add the archive-root helper
  there.
- Session-file corruption on cancel was fixed in v0.11. Reuse that
  temp-file-then-rename pattern.

</code_context>


## Revision 2026-10-03 (supersedes conflicting decisions above)

Phase 1 changed to a child-process runtime (see `01-child-process-runtime/01-CONTEXT.md`). Agents are `nanopi -p` children controlled only by the orchestrator; the user never controls them directly.
- brief.md is the amendment channel: the orchestrator appends `## Amendment N`; the child reads it between turns and self-checks it before writing report.md (P1 D-09..D-11). report.md carries a per-item checklist.

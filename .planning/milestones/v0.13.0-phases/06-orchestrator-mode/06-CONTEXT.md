# Phase 6: Orchestrator mode - Context

**Gathered:** 2026-10-03
**Status:** Ready for planning

<domain>
## Phase Boundary

An experimental, opt-in TUI mode in which the main agent only analyses,
splits the work, delegates it, monitors the agents, and synthesises
their results. It is built entirely from Phases 3–5. When the mode is
off, behaviour is unchanged. Also closes QA-01 and QA-02. Covers
ORC-01..05 and QA-01..02.

</domain>

<decisions>
## Implementation Decisions

### Toggle
- **D-01:** `/orchestrator` toggles the mode (`/orchestrator on|off`
  also works). The config key `experimental.orchestrator = false` sets
  the default at startup. The mode is TUI only: print mode ignores it,
  with a one-line warning when the key is set.
- **D-02:** Toggling in the middle of a session takes effect from the
  next turn. Running agents are not affected. The status line shows
  `⎈ orchestrator` while the mode is on.

### Toolset
- **D-03:** The orchestrator's tools are read, grep, glob, `agent`,
  `send_message`, `stop_agent` and `list_agents`. Write, edit and bash
  are not registered at all, not just discouraged. A test asserts they
  are absent.
- **D-04:** With the mode off, the system prompt and the tool specs
  sent to the provider are byte-identical to v0.12. A snapshot test
  checks this.

### Orchestrator behaviour (system prompt)
- **D-05 (owner decision): plan confirmation.** When the task is clear
  and nothing is ambiguous, the orchestrator states its plan briefly
  and dispatches immediately. When anything needs confirming (unclear
  scope, a choice between approaches, a risky action), it asks the user
  first and dispatches only after the answer.
- **D-06 (owner decision): use as few agents as possible.**
  - The orchestrator first judges whether parallelism is worth it.
  - Sequential or interdependent development work goes to **one**
    agent, which is the safer practice.
  - Several parallel agents are used only for genuinely independent
    work.
  - The concurrency cap stays `agent.max_concurrency` (default 4,
    configurable). That is a ceiling, not a target.
- **D-07:** For parallel code-writing agents the orchestrator sets
  `isolation: "worktree"`. Merging follows Phase 4 D-11: automatic,
  except that conflicts are escalated to the user.
- **D-08:** The prompt's workflow is: understand (using read-only tools
  or an explore agent), then plan, then dispatch, then monitor (react
  to reports, and amend or stop when the direction changes), then
  verify (a verify agent where worthwhile), then report a combined
  summary to the user covering what was done, changed files, open
  issues, and the archive path.
- **D-09:** The orchestrator writes clear, self-contained briefs, since
  agents do not inherit its context. A brief covers the goal,
  relevant files, constraints and the expected report.

### Quality
- **D-10 (QA-01):** Add manual end-to-end test rows for amend, stop,
  stop-all (Ctrl+X), expand (Ctrl+G), approve / deny / always, the
  `/orchestrator` toggle, `/agents clean`, auto-prune, worktree merge,
  and merge conflict. Put them in a new
  `docs/v0.13-manual-test-plan.md` that follows the v0.12 format.
- **D-11 (QA-02):** Measure the release binary size before and after
  the milestone. The growth must be no more than about 150 KB, with no
  new crates unless justified in the summary.

### Claude's Discretion
- Exact prompt wording, as long as it follows D-05 to D-09.

</decisions>

<specifics>
## Specific Ideas

Reference: Claude Code coordinator mode
(`claude-code-haha-main/src/coordinator/coordinatorMode.ts`) for the
research → synthesis → implement → verify structure, and its table of
when to continue a worker versus start a new one. Roo's Orchestrator
("Boomerang") is a second reference for "delegating mode cannot edit".

</specifics>

<canonical_refs>
## Canonical References

- `.planning/REQUIREMENTS.md` — ORC-01..05, QA-01..02
- All earlier phase CONTEXT files (01–05)
- `docs/v0.12-manual-test-plan.md` — the format to follow for the test
  plan
- `.planning/research/FEATURES.md` — coordinator mode findings

</canonical_refs>

<code_context>
## Existing Code Insights

Building tools happens in `Agent::build_fresh`. Filter there, based on
an `orchestrator` flag on `Agent`.

</code_context>


## Revision 2026-10-03 (supersedes conflicting decisions above)

Phase 1 changed to a child-process runtime (see `01-child-process-runtime/01-CONTEXT.md`). Agents are `nanopi -p` children controlled only by the orchestrator; the user never controls them directly.
- Control tools are dispatch/amend/stop/list/continue; there is no user stop-all or approval surface.

# Requirements — v0.13.0 Orchestrator & Dynamic Subagents

Source: `.planning/PROJECT.md`, `.planning/research/SUMMARY.md`, and
owner decisions made on 2026-10-03.

## v1 Requirements (this milestone)

### Runtime (RT) — in-process subagents

- [ ] **RT-01**: Subagents run as in-process tasks; the child-process
  runtime (`run_single` / `spawn_and_collect`) is removed.
- [ ] **RT-02**: Each subagent has its own cancel token. A foreground
  subagent stops when the user presses Esc on the main turn.
  Background subagents are not affected by Esc.
- [ ] **RT-03**: The user can stop all running subagents with one
  shortcut.
- [ ] **RT-04**: Each subagent has its own session transcript, and
  nothing leaks into the parent session.
- [ ] **RT-05**: Subagents cannot spawn subagents (deny-list). A global
  cap limits how many subagents are alive at once.
- [ ] **RT-06**: Each subagent has a turn limit and a token budget, with
  configurable defaults. When it hits either, it stops and reports
  partial work.
- [ ] **RT-07**: When a background subagent needs permission, the
  request is queued for the user rather than interrupting the main
  conversation. The subagent waits until the user decides.
- [ ] **RT-08**: A subagent failure or provider error never takes down
  the nanopi process. It is reported as a failed agent.

### Dynamic dispatch (DYN)

- [ ] **DYN-01**: The model can dispatch a subagent by describing the
  task only. Without an agent name, a general-purpose agent is used.
- [ ] **DYN-02**: The model can give an ad-hoc role prompt and a
  toolset per call. The toolset is checked against the allowlist and
  deny-list.
- [ ] **DYN-03**: Predefined agent files and single / parallel / chain
  modes keep working.
- [ ] **DYN-04**: The model can choose a model per subagent.
- [ ] **DYN-05**: The parent receives a capped summary report, not the
  subagent's full transcript.

### Control & communication (CTL)

- [ ] **CTL-01**: The model can launch a subagent in the background and
  keep working; every agent has an id.
- [ ] **CTL-02**: The model can amend a running subagent's task. The
  change is applied between the subagent's turns, never in the middle
  of a tool call.
- [ ] **CTL-03**: The model can stop a running subagent, and the
  subagent reports its partial work.
- [ ] **CTL-04**: The model can list subagents with their status.
- [ ] **CTL-05**: A finished background subagent's report is delivered
  to the main agent automatically. If the main agent is idle, the
  report starts a new turn; if it is streaming, the report is queued as
  a follow-up.
- [ ] **CTL-06**: The model can continue a finished subagent with a new
  message, and the subagent keeps its previous context.
- [ ] **CTL-07**: In print mode (`-p`), nanopi waits for background
  subagents (or stops them) before exiting, so no task is left
  orphaned.

### Archive (ARC)

- [ ] **ARC-01**: Each subagent writes
  `.nanopi/agents/<run>/<id>/brief.md` (task, role, tools, model) when
  it starts. Amendments are appended to that file.
- [ ] **ARC-02**: `report.md` is written before the result is returned
  to the parent, so a report cannot be lost.
- [ ] **ARC-03**: `.nanopi/agents/` is added to `.gitignore`
  automatically and is excluded from the agents' own searches.
- [ ] **ARC-04**: Subagents that were still running when nanopi exited
  are marked `interrupted` on the next start; they are not re-run.
- [ ] **ARC-05**: The user can clean up the archive with one command,
  for example `/agents clean`. It can keep the most recent N runs or
  remove everything.

### Isolation (ISO)

- [ ] **ISO-01**: A subagent that writes code can run in its own git
  worktree, either opt-in per dispatch or by default for parallel
  writers. The report includes the worktree path and branch.
- [ ] **ISO-02**: Worktrees with no changes are removed automatically;
  worktrees with changes are kept and listed.
- [ ] **ISO-03**: An edit is refused if the file was changed by another
  agent since this agent read it. This protects agents that share a
  working tree.

### TUI agents strip (UI)

- [ ] **UI-01**: When subagents exist, a 1–3 line strip appears above
  the input box. Each line shows the agent's id, role, short task,
  state (running / waiting for approval / done / failed / stopped /
  interrupted) and elapsed time.
- [ ] **UI-02**: The user can expand and collapse the strip with a
  shortcut (default **Ctrl+G**, if it is free in `keys.rs`). The
  expanded view shows each agent's latest activity and its report path.
- [ ] **UI-03**: Pending permission requests are shown in the strip,
  and the user can approve or deny them there.
- [ ] **UI-04**: The strip is redrawn from a registry snapshot on the
  TUI tick, not once per event.

### Orchestrator mode (ORC)

- [ ] **ORC-01**: The user can turn orchestrator mode on and off from
  the TUI with `/orchestrator`. It is also available as the config key
  `experimental.orchestrator`. It is off by default.
- [ ] **ORC-02**: In orchestrator mode, the main agent's tools are
  limited to: read / grep / glob, dispatch, amend, stop, list and
  continue. The write, edit and bash tools are not registered.
- [ ] **ORC-03**: The orchestrator's system prompt tells it to
  analyse, split the work, dispatch subagents, monitor them, and
  synthesise their reports for the user.
- [ ] **ORC-04**: When orchestrator mode is off, the prompts and tool
  specs are byte-identical to v0.12.
- [ ] **ORC-05**: The status line shows when orchestrator mode is
  active.

### Quality (QA)

- [ ] **QA-01**: Each new control (amend, stop, stop-all, expand,
  approve, toggle, clean) has a row in the manual end-to-end test plan.
- [ ] **QA-02**: The release binary grows by no more than about
  150 KB, and no new crates are added unless justified.

## Future Requirements (deferred)

- Shared 429 / backoff coordination across concurrent agents, unless
  phase 1 finds it necessary.
- Automatic recovery or re-run of interrupted agents from the archive.

## Out of Scope

| Item | Reason |
|------|--------|
| Stopping or messaging a subagent directly from the expanded panel | Owner chose to control agents through the orchestrator and tools only |
| Nested teams / swarms; workers messaging each other | Anti-feature: cost and complexity, and hard to observe |
| Orchestrator with write / edit / bash | It would end up doing the work itself |
| Orchestrator mode as the default | Experimental |
| Forking the parent's full context into subagents by default | Bloats context; the brief is the interface |
| Child-process subagent fallback | Owner chose to remove the old runtime |
| ratatui 0.30 upgrade | API split; not this milestone |

## Traceability

(filled by roadmap)

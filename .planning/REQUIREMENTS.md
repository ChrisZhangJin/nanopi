# Requirements — v0.13.0 Orchestrator & Dynamic Subagents

Source: `.planning/PROJECT.md`, `.planning/research/SUMMARY.md`, and
owner decisions made on 2026-10-03.

## v1 Requirements (this milestone)

### Runtime (RT) — child-process subagents (owner decision 2026-10-03)

- [ ] **RT-01**: Each subagent runs as an isolated `nanopi -p` child
  process. A crash, panic, stack overflow or OOM in a subagent never
  affects the main nanopi process.

- [ ] **RT-02**: Every child process is tracked by the orchestrator
  (id, pid, state) and is killed when it is stopped, when its parent
  turn is cancelled, or when nanopi exits. No orphaned processes.

- [ ] **RT-03**: Subagents are controlled only by the orchestrator
  (main agent) through tools. The user never stops, answers or messages
  a subagent directly; they talk only to the orchestrator.

- [ ] **RT-04**: Each subagent has its own session transcript in its
  agent directory, and nothing leaks into the parent session.

- [ ] **RT-05**: Subagents cannot spawn subagents (the child is started
  without the subagent/control tools). A global cap limits how many
  child processes are alive at once.

- [ ] **RT-06**: Each subagent has a turn limit and a token budget,
  passed to the child on its command line, with configurable defaults.
  When it hits either, it stops and reports partial work.

- [ ] **RT-07**: Permissions are decided by the orchestrator at dispatch
  time: the dispatch carries the allowed tool list, and the child runs
  with exactly those tools. A child never prompts; anything outside the
  list is denied in-band.

- [ ] **RT-08**: A child that exits non-zero, crashes, times out or
  produces unparseable output is reported as a failed agent with its
  error text; nanopi keeps running.

- [ ] **RT-09**: The child works from a brief file. While it runs, the
  orchestrator may only append `## Amendment N` sections to the brief;
  the child checks the brief between turns and injects new amendments
  as steering messages. Before finishing, the child re-reads the brief
  and checks every requirement and amendment is done (bounded to a few
  extra turns), then writes its report with a per-item checklist.

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

- [ ] **CTL-02**: The model can amend a running subagent's task by
  appending to its brief file (RT-09). The child picks it up between
  turns, never in the middle of a tool call. If the child has already
  finished, the amendment is handled by continuing it (CTL-06).

- [ ] **CTL-03**: The model can stop a running subagent, and the
  subagent reports its partial work.

- [ ] **CTL-04**: The model can list subagents with their status.
- [ ] **CTL-05**: A finished background subagent's report is delivered
  to the main agent automatically. If the main agent is idle, the
  report starts a new turn; if it is streaming, the report is queued as
  a follow-up.

- [ ] **CTL-06**: The model can continue a finished subagent with a new
  message: a new `nanopi -p` is started on the same session, so the
  subagent keeps its previous context.

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

- [x] **ISO-03**: An edit is refused if the file was changed by another
  agent (another process) since this agent read it, based on the file's
  on-disk mtime and content hash. This protects agents that share a
  working tree.

### TUI agents strip (UI)

- [ ] **UI-01**: When subagents exist, a 1–3 line strip appears above
  the input box. Each line shows the agent's id, role, short task,
  state (running / waiting for approval / done / failed / stopped /
  interrupted) and elapsed time.

- [ ] **UI-02**: The user can expand and collapse the strip with a
  shortcut (default **Ctrl+G**, if it is free in `keys.rs`). The
  expanded view shows each agent's latest activity and its report path.

- [ ] **UI-03**: The strip is display-only. It has no stop, approve or
  message actions; subagents are controlled through the orchestrator
  (RT-03).

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

- [ ] **QA-01**: Each new control (amend, stop, continue, expand,
  toggle, clean) has a row in the manual end-to-end test plan.

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

| Requirement | Phase | Status |
|-------------|-------|--------|
| RT-01 | Phase 1 | Pending |
| RT-02 | Phase 1 | Pending |
| RT-03 | Phase 1 | Pending |
| RT-04 | Phase 1 | Pending |
| RT-05 | Phase 1 | Pending |
| RT-06 | Phase 1 | Pending |
| RT-07 | Phase 1 | Pending |
| RT-08 | Phase 1 | Pending |
| RT-09 | Phase 1 | Pending |
| ISO-03 | Phase 1 | Complete |
| ARC-01 | Phase 2 | Pending |
| ARC-02 | Phase 2 | Pending |
| ARC-03 | Phase 2 | Pending |
| ARC-04 | Phase 2 | Pending |
| ARC-05 | Phase 2 | Pending |
| DYN-01 | Phase 3 | Pending |
| DYN-02 | Phase 3 | Pending |
| DYN-03 | Phase 3 | Pending |
| DYN-04 | Phase 3 | Pending |
| DYN-05 | Phase 3 | Pending |
| CTL-01 | Phase 4 | Pending |
| CTL-02 | Phase 4 | Pending |
| CTL-03 | Phase 4 | Pending |
| CTL-04 | Phase 4 | Pending |
| CTL-05 | Phase 4 | Pending |
| CTL-06 | Phase 4 | Pending |
| CTL-07 | Phase 4 | Pending |
| ISO-01 | Phase 4 | Pending |
| ISO-02 | Phase 4 | Pending |
| UI-01 | Phase 5 | Pending |
| UI-02 | Phase 5 | Pending |
| UI-03 | Phase 5 | Pending |
| UI-04 | Phase 5 | Pending |
| ORC-01 | Phase 6 | Pending |
| ORC-02 | Phase 6 | Pending |
| ORC-03 | Phase 6 | Pending |
| ORC-04 | Phase 6 | Pending |
| ORC-05 | Phase 6 | Pending |
| QA-01 | Phase 6 | Pending |
| QA-02 | Phase 6 | Pending |

Coverage: 39/39 v1 requirements mapped, no orphans, no duplicates.

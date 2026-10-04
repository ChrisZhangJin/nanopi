# Phase 4: Background launch & control - Context

**Gathered:** 2026-10-03
**Status:** Ready for planning

<domain>
## Phase Boundary

The model can run agents in the background, then amend, stop, list
and continue them. Reports flow back to the main agent automatically.
Print mode never leaves an agent running after it exits. Code-writing
agents can work in git worktrees that are merged back. Covers
CTL-01..07 and ISO-01..02.

</domain>

<decisions>
## Implementation Decisions

### Launch
- **D-01:** Add `background: bool` to the `agent` call (default
  false). A background dispatch returns at once with
  `{id, state: queued|running, archive path}`.

### Control tools (main agent only; never given to agents)
- **D-02:** `send_message {id, message}`:
  - Running agent: the message is delivered through the agent's steer
    channel at its next turn boundary, never during a tool call, and
    is appended to `brief.md` as an amendment.
  - Finished agent: this is a continue (CTL-06). See D-05.
- **D-03:** `stop_agent {id}` cancels the agent's token. The agent ends
  as `stopped` and writes a partial report. Use `id: "all"` to stop
  every agent.
- **D-04:** `list_agents` returns id, description, state, elapsed time,
  turns, tokens and report path for every agent in the current run.

### Continue
- **D-05:** To continue a finished agent, rebuild it from its
  `transcript.jsonl`, append the new message, run it again under the
  same id, and append to its brief and report. This also works for
  agents from an earlier nanopi process in the same run. Agents from
  other runs (`interrupted` or older) cannot be continued in this
  milestone.

### Report injection
- **D-06:** When a background agent finishes, the main agent receives
  `[agent a3 finished: done] <capped report>`. If the main agent is
  idle, the message starts a new turn, using the queued follow-up path.
  If it is streaming, the message is delivered as
  `SteerMessage::FollowUp`. Several reports that finish together are
  batched into one message.

### Print mode
- **D-07:** `-p` waits for every background agent before printing the
  final result and exiting. It adds their reports, then runs one more
  main turn if any reports arrived. Ctrl-C stops all of them.

### Worktrees (ISO-01, ISO-02)
- **D-08:** Add `isolation: "worktree"` to the dispatch (opt-in). In
  orchestrator mode, the orchestrator sets it for code-writing agents
  that run in parallel. It is ignored with a warning when the
  directory is not a git repo.
- **D-09:** Worktrees are created at
  `.nanopi/worktrees/<run>-<id>` on branch `nanopi/<run>/<id>`. When
  the agent finishes, it commits its changes in the worktree.
- **D-10:** A worktree with no changes is removed, together with its
  branch.
- **D-11 (owner decision): automatic merge.** When an agent finishes
  with changes, the main agent merges its branch into the main working
  tree:
  - No conflicts: the merge happens automatically, and the worktree
    and branch are removed afterwards.
  - Conflicts: the merge is aborted, the worktree and branch are kept,
    and the user is asked how to proceed. Nothing is resolved silently.
  - The merge result (merged / conflict + branch) is added to the
    report given to the main agent.
- **D-12:** Agents that run in worktrees get their own cwd. Every file
  tool resolves paths against that cwd, so an agent cannot write to
  the main tree by accident.

### Claude's Discretion
- How to name and serialise events; whether the merge runs as the
  `git` CLI or a tool call (the `git` CLI is preferred: no new crates).

</decisions>

<specifics>
## Specific Ideas

Reference: Claude Code's `SendMessageTool`, `TaskStopTool` and
`<task-notification>` messages (`claude-code-haha-main/src/tools/`,
`src/coordinator/coordinatorMode.ts`).

</specifics>

<canonical_refs>
## Canonical References

- `.planning/REQUIREMENTS.md` — CTL-01..07, ISO-01..02
- `.planning/phases/01-in-process-runtime/01-CONTEXT.md`,
  `02-archive-lifecycle/02-CONTEXT.md`
- `.planning/research/ARCHITECTURE.md` — injection path and print-mode
  exit
- `src/event.rs` — `SteerMessage::FollowUp`; the follow-up queue in
  `src/mode/tui.rs`

</canonical_refs>

<code_context>
## Existing Code Insights

The follow-up queue already starts the next turn automatically when the
main agent goes idle (v0.11). Reuse it; do not add a second injection
mechanism.

</code_context>


## Revision 2026-10-03 (supersedes conflicting decisions above)

Phase 1 changed to a child-process runtime (see `01-child-process-runtime/01-CONTEXT.md`). Agents are `nanopi -p` children controlled only by the orchestrator; the user never controls them directly.
- Amend = append to brief.md (no in-memory steer channel). Stop = kill the child process group. Continue = start a new `nanopi -p --session` on the same transcript; also used for amendments that arrive after the child finished.
- No user Esc/stop-all for agents.

## Addendum (2026-10-04, planning)

Resolutions of the research open questions, applied by the phase plans:

1. **D-11 merge conflicts** are surfaced as text in the main agent's reply (the conflict and the kept branch are written into the report injected back to the main agent). No new blocking approval gate is added in TUI or print mode.
2. **Print-mode Ctrl-C during the drain loop** calls `AgentRegistry::stop_all()` per D-07. Verified while planning: `src/mode/print.rs` has no SIGINT/`ctrl_c` handling today, so the drain loop installs its own `tokio::signal::ctrl_c()` listener (new wiring, not an extension).
3. **Worktree "unchanged"** (D-10) = empty `git status --porcelain` in the worktree AND zero commits ahead of base (`git rev-list <base>..<branch> --count` == 0).

# Phase 1: Child-process runtime - Context

**Gathered:** 2026-10-03 (revised — replaces the in-process design)
**Status:** Ready for planning

<domain>
## Phase Boundary

Subagents run as isolated `nanopi -p` child processes. The orchestrator
(main agent) is the only thing that controls them; the user only talks
to the orchestrator. Each child works from a brief file that the
orchestrator can amend mid-run, and the child self-checks the brief
before finishing. Covers RT-01..09 and ISO-03. Archive lifecycle
(index, interrupted marking, cleanup), dynamic dispatch, background
launch, the strip and orchestrator mode belong to later phases.

</domain>

<decisions>
## Implementation Decisions

### Why child processes (owner decision 2026-10-03)
- The in-process design was executed and rolled back (`2bd0343`).
  Reason: `panic = "abort"` in the release profile means any panic in
  an in-process subagent kills nanopi; stack overflow / OOM cannot be
  isolated in-process at all. Child processes give real isolation and
  keep subagents decoupled: a subagent is just a normal `nanopi -p`
  run that can be reproduced and debugged on its own.
- Keep `panic = "abort"`.

### Runtime shape
- **D-01:** Restore the child-process runtime (`run_single` /
  `spawn_and_collect` as of `28fe37d`) as the base and extend it.
  Child command: `nanopi -p --output json` plus new flags (D-05, D-07,
  D-09). Same provider/model/api-key/base-url as the parent unless the
  dispatch says otherwise (fix the old inheritance gap).
- **D-02:** A `SubagentRegistry` in the parent tracks every child: id
  (`a1`, `a2`, … per run), pid, state, start time, agent dir. Run dir
  uses a uuid v7.
- **D-03:** Agent dir: `.nanopi/agents/<run>/<id>/` containing
  `brief.md`, `transcript.jsonl` (child's own session) and `report.md`.
  Phase 2 adds the index, interrupted marking and cleanup.

### Control (orchestrator only)
- **D-04:** No user-facing subagent controls: no Esc/Ctrl+X handling
  for subagents, no permission prompts, no TUI actions. Stopping is
  done by the orchestrator (tool, Phase 4) or implicitly: when the
  parent turn is cancelled or nanopi exits, its children are killed.
  Use `kill_on_drop` plus killing the process group so a child's own
  bash subprocesses die too. No orphans.

### Permissions (orchestrator decides)
- **D-05:** The dispatch carries the allowed tool list; the parent
  passes it to the child (e.g. `--tools read,grep,edit`). The child
  registers exactly those tools. It never prompts; any other call is
  denied in-band. The subagent and control tools are always removed
  (depth 1), regardless of the list. Hook `PreToolUse`/`PostToolUse`
  still run in the child; the child gets `NANOPI_AGENT_ID` in its env
  and hooks receive it as `agent_id`.

### Limits (configurable under `[subagent]`)
- **D-06:** `max_live = 8` live children (dispatch beyond it fails with
  a clear in-band error); `max_concurrency = 4` running at once,
  excess queue.
- **D-07:** `max_turns = 50`, `token_budget = 300_000`, passed to the
  child as flags. Hitting either stops the child with a partial report
  and `status: limit_reached` naming the limit.

### Failure isolation
- **D-08:** Non-zero exit, signal, timeout or unparseable JSON → agent
  `failed` with stderr tail / error text. The parent never panics on
  child output.

### Brief file and amendments (owner idea)
- **D-09:** Parent writes `brief.md` (task, role, tools, model) before
  spawning and starts the child with `--brief <path>`. The child uses
  it as its task.
- **D-10:** While running, the orchestrator may only append
  `## Amendment N` sections (whole section in one write). The child
  only reads it. Between turns the child checks the brief (size/mtime);
  new amendments are injected as steering messages for the next turn.
  Never mid tool call.
- **D-11:** Before finishing, the child re-reads `brief.md` and checks
  every requirement and amendment. If something is undone it keeps
  working, at most 2 extra turns, then writes `report.md` regardless
  with a per-item checklist (done / not done + note).
- **D-12:** If an amendment arrives after the child has finished
  (`report.md` exists), the parent notices; handling it by continuing
  the child on its session is Phase 4 (CTL-06). This phase adds the
  child-side `--session <path>` resume flag needed for it.

### Shared working tree
- **D-13 (ISO-03):** Cross-process stale-write guard: on read, the
  child records the file's mtime + content hash (std `DefaultHasher`,
  no new crate); on edit/write it re-checks the on-disk file and
  refuses with "file changed since you read it — re-read first" if it
  differs. Writes are atomic (temp + rename). A file never read may be
  written, so the main agent behaves as today.

### WASM extensions
- **D-14:** Children load WASM extensions like any nanopi process, but
  only those in the allowed tool list.

### Claude's Discretion
- Exact flag names, JSON output schema extensions, test layout.

</decisions>

<canonical_refs>
## Canonical References

- `.planning/REQUIREMENTS.md` — RT-01..09, ISO-03
- `src/tool/subagent.rs` — the child-process runtime being extended
- `src/mode/print.rs`, `src/main.rs` — `-p` mode and CLI flags
- `src/agent/loop_.rs` — `run_turn` and its steer receiver (used for
  injecting amendments)
- `src/event.rs:126` — `SteerMessage`
- `src/agent/hook.rs` — hook payload
- `superseded-inprocess/` — rolled-back plans; reusable test ideas
  (stale-write tests, limit tests)

</canonical_refs>

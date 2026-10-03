# Phase 1: In-process runtime - Context

**Gathered:** 2026-10-03
**Status:** Ready for planning

<domain>
## Phase Boundary

Replace the child-process `subagent` runtime with in-process tasks. Every
subagent can be stopped, has limits on concurrency, turns and tokens,
cannot crash nanopi, and keeps its transcript separate from the parent
session. Covers RT-01..08 and ISO-03. The archive, dynamic dispatch,
background launch, the agents strip and orchestrator mode belong to
later phases.

</domain>

<decisions>
## Implementation Decisions

### Runtime shape
- **D-01:** Add a `SubagentRegistry`, shared through `ToolContext`
  (`src/tool/mod.rs:309`), that owns every live agent: id, state,
  `CancellationToken`, steer sender, start time and usage. The TUI
  reads a snapshot of it later, in Phase 5.
- **D-02:** Build each subagent with `Agent::build_fresh`
  (`src/agent/build.rs:321`) and run it with `tokio::spawn`, with its
  own provider instance. Remove `run_single` / `spawn_and_collect` and
  the child-process path completely; do not keep a fallback.
- **D-03:** Give each agent an id that is short and readable in the
  strip, such as `a1`, `a2`, …, unique per run. Generate a uuid v7 for
  the run directory.
- **D-04:** Unless the dispatch says otherwise, a subagent uses the
  same provider and model as the parent. This fixes the old
  `--api-key` / `--base-url` inheritance gap.

### Cancellation
- **D-05:** A foreground subagent's token is a `child_token()` of the
  main turn's token, so Esc stops it. A background subagent's token is
  a child of a registry root token instead, so Esc does not touch it.
- **D-06:** "Stop all subagents" cancels the registry root token. Its
  default key is **Ctrl+X**. Confirm Ctrl+X is free; otherwise choose
  the nearest free key and record it. It is configurable through the
  existing keybindings system.
- **D-07:** Cancellation is cooperative. The agent loop checks it at
  turn and tool boundaries, as it already does. A tool that is running
  finishes or aborts through its own existing cancel handling; never
  abort a file write halfway.

### Limits (configurable, under `[subagent]` in config.toml)
- **D-08:** Defaults:
  - `max_concurrency = 4`, enforced with a global semaphore. Excess
    dispatches queue.
  - `max_live = 8`, counting queued, running and waiting agents. A
    dispatch beyond the cap fails with a clear in-band error.
  - `max_turns = 50`.
  - `token_budget = 300_000`, input plus output.
- **D-09:** When an agent hits the turn limit or the token budget, it
  stops and returns a partial report with `status: limit_reached` and
  says which limit it hit.
- **D-10:** Subagents never receive the subagent or control tools
  (dispatch, send_message, stop, list). Enforce this with a deny-list
  in tool construction, not in the prompt. The maximum depth is 1.

### Failure isolation
- **D-11:** Keep `panic = "abort"`. Instead, audit the subagent path
  for `unwrap()`, `expect()` and indexing, and convert them to errors.
  Provider and tool errors end the agent as `failed`, carrying the
  error text. nanopi keeps running.

### Transcripts
- **D-12:** Each subagent writes its own session JSONL to
  `.nanopi/agents/<run>/<id>/transcript.jsonl`. Phase 2 adds the brief
  and report next to it. The parent session records only the tool call
  and the tool result.

### Permissions (interim until Phase 5)
- **D-13:** A subagent permission request is put on a queue and the
  subagent waits for the answer. Until Phase 5 the TUI answers through
  a simple inline confirm prompt labelled `[a3] wants to run: …`. It
  must not get mixed up with the main agent's own prompt; show one at a
  time, first in, first out.
- **D-14:** In print mode (`-p`) there is no way to ask, so queued
  requests follow the existing non-interactive rule. Today that is
  deny; check it.

### Shared working tree
- **D-15 (ISO-03):** Each agent records the mtime and content hash of a
  file when it reads it. An edit or write is refused with an in-band
  error ("file changed since you read it — re-read first") when the
  current hash differs. Extend the existing `mutation_key` locking so
  it serializes writes to the same path across agents.

### Hooks and extensions
- **D-16:** `PreToolUse` and `PostToolUse` hooks run for subagent tool
  calls too, so security hooks still apply. Session and turn lifecycle
  hooks do not run for subagents in this milestone. The hook payload
  gets an `agent_id` field.
- **D-17:** WASM extensions: research whether one loaded instance can
  serve concurrent agents. If it can't, subagents get no WASM tools in
  this milestone; do not instantiate them once per agent.

### Claude's Discretion
- Internal registry data structures, event enum shape, and test layout.

</decisions>

<specifics>
## Specific Ideas

- Model this on Claude Code's AgentTool, where workers run in-process
  and are controlled through tools.
- Rate limits: if parallel agents start hitting 429s, share a backoff
  in the provider layer. Do that only if research shows it is needed;
  otherwise it stays deferred.

</specifics>

<canonical_refs>
## Canonical References

- `.planning/REQUIREMENTS.md` — RT-01..08, ISO-03
- `.planning/research/SUMMARY.md`, `ARCHITECTURE.md`, `PITFALLS.md`,
  `STACK.md`
- `src/tool/subagent.rs` — the current runtime, which is being replaced
- `src/agent/loop_.rs:76,1080-1130` — `Agent`, and `run_turn`'s steer
  and cancel handling
- `src/agent/build.rs:321` — `build_fresh`
- `src/event.rs:126` — `SteerMessage`
- `src/mode/tui.rs:1828,3429-3440` — `run_app`'s select loop, and where
  turns are spawned
- `src/agent/hook.rs` — hook events
- `/root/workspace/claude-code-haha-main/src/tools/AgentTool/`,
  `src/constants/tools.ts` — reference deny-list

</canonical_refs>

<code_context>
## Existing Code Insights

- `run_turn` already takes a `CancellationToken` and a steer receiver,
  so the loop needs no changes for stop or amend.
- `kill_on_drop` was the old way stop worked. It goes away, so every
  spawn must be registered.
- Bash now runs sequentially by default, because concurrent bash calls
  were silently losing updates (see STATE.md). The same concern applies
  across agents, which is why D-15 exists.

</code_context>

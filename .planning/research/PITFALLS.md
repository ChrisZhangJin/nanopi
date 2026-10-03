# Domain Pitfalls — v0.13.0 Orchestrator & Dynamic Subagents

**Domain:** Adding in-process subagents, orchestrator mode, channels, .md archives and a TUI agents strip to an existing Rust coding agent (nanopi)
**Researched:** 2026-10-03
**Confidence:** MEDIUM-HIGH. Based on nanopi's own code and history (`src/tool/subagent.rs`, `.planning/STATE.md`) and the reference implementation `/root/workspace/claude-code-haha-main` (`tools/AgentTool/runAgent.ts`, `agentToolUtils.ts`, `constants/tools.ts`, `tasks/`). Little external web verification was done.

Suggested phase names used below: **P1** in-process runner (replaces the child process), **P2** dynamic subagents (ad-hoc role and toolset), **P3** channels and control (amend, stop, report) plus .md archives, **P4** orchestrator mode, **P5** TUI agents strip, **P6** persistence, resume and hardening.

## Critical Pitfalls

### 1. Cancellation stops being free when the child process goes away
**What goes wrong:** Today, cancel safety comes from `kill_on_drop(true)`: dropping the future sends SIGKILL to the child, and the OS cleans up everything. In-process, dropping a future doesn't stop a `tokio::spawn`ed task, a bash child it started, or a half-done write. Orphan tasks keep using tokens and editing files after Esc.
**Why:** Cancellation-by-process-death is assumed throughout the current design. nanopi has already shipped cancel bugs in parallel batches and corrupted session files on cancel (STATE.md hardening pass, `2e386ef`).
**Prevention:** Every subagent gets a `CancellationToken` that is a child of the parent turn's token. Background or orchestrated agents get their own token tree that is not linked to the parent turn, as the reference does: "async agents get a new unlinked controller". Hold the `JoinHandle` in a registry. On drop, or when stop is called, cancel the token and then await with a timeout. Each subagent's own tools (bash) keep `kill_on_drop`. Pick one stop rule and document it: either Esc in the main agent stops every subagent, or Esc stops only the main turn while orchestrated agents keep running.
**Detection:** A test that cancels mid-tool and then asserts no task is alive, no child pid exists, and no further provider requests go out. Re-run the T4.4-style "parallel batch of 2+ on Anthropic transport" row with subagents.
**Phase:** P1. This blocks everything else.

### 2. Tool results lost when a parallel batch is cancelled or fails partway
**What goes wrong:** This is the `2e386ef` failure mode again: subagents finish (side effects done, cards drawn), then the parent turn dies or drops the results. The model never sees the reports and re-dispatches the same work.
**Prevention:** Write every subagent result to its report .md *before* returning it through the channel. The tool_result must always be emitted, even for cancelled or partial runs (use "cancelled after N turns; partial report at <path>"). Never persist a `tool_use` without a matching `tool_result`.
**Phase:** P1 and P3.

### 3. Parallel subagents edit the same file
**What goes wrong:** With child processes, each subagent had its own read-before-edit state. In-process, a shared file-state cache, or none at all, lets agent A's `edit` overwrite agent B's change without either noticing.
**Prevention:** Give each subagent a *cloned* read-file state (the reference uses `cloneFileStateCache`). Make `edit` and `write` check mtime or content hash against the last read and fail with "file changed since you read it". Have the orchestrator prompt assign disjoint file sets per subagent. Optionally add a per-path advisory lock. Use git worktree isolation only as a later option; it costs more than nanopi's size budget should spend now.
**Detection:** A test where two agents edit the same file concurrently must end in an explicit error, not a silent overwrite.
**Phase:** P1 for state isolation; P4 for disjoint file assignment.

### 4. Unbounded recursion and fan-out
**What goes wrong:** A dynamic subagent with an ad-hoc toolset includes `subagent` itself. That leads to exponential spawning, token blowup, and rate-limit storms.
**Prevention:** Keep a global deny-list of tools that are never handed to subagents (the reference has `ALL_AGENT_DISALLOWED_TOOLS`). By default, remove `subagent` and the orchestrator control tools from children. If nesting is ever allowed, enforce a hard `depth` counter (max 1) plus a global live-agent cap. Put the existing parallel cap in one global semaphore, not a per-call one. Validate a model-supplied toolset against an allow-list; never trust it as given.
**Phase:** P2.

### 5. Token and cost blowup
**What goes wrong:** Each subagent re-sends a full system prompt and tool specs. The orchestrator copies every full report into its own context. Amends restart work. A long-running subagent with no `max_turns` loops forever.
**Prevention:** Per-subagent `max_turns` and token budget, with a default. Reports are capped: the orchestrator receives a summary plus the .md path and reads the full file only on demand. Aggregate usage per subagent and show the total in the strip and the session stats. Print-mode runs need a hard budget.
**Detection:** Session cost compared with the same task run without the orchestrator. Alert when the ratio is above about 3x.
**Phase:** P2 for limits; P4 for report summarization.

### 6. The orchestrator does the work itself anyway
**What goes wrong:** If the prompt alone says "don't implement", the model still calls `edit` and `bash` because it is faster. The feature then quietly turns into the normal mode.
**Prevention:** Enforce this through the toolset, not the prompt. In orchestrator mode the main agent's registry exposes only read-only tools (read, grep, ls) plus spawn, amend, stop, report and list. Write, edit and bash are not registered at all. Also cover the opposite failure: the orchestrator spawns one subagent per trivial line. Give guidance on task size in the prompt.
**Detection:** A test asserts that the tool specs sent in orchestrator mode contain no mutating tools.
**Phase:** P4.

### 7. Amend and stop race with the agent's own turn
**What goes wrong:** An amend arrives while a tool is running, or right after the agent produced its final answer. The amend is lost, or applied to a finished agent, or injected mid-`tool_use`, which breaks the tool_use/tool_result pairing. A stop arrives between "result written" and "status set", and the status ends up wrong.
**Prevention:** Model the subagent as a state machine (Queued → Running → Finishing → Done/Failed/Stopped). Amends go into an mpsc queue that is drained only at turn boundaries, reusing the existing v0.11 steer/follow-up injection path. Don't build a second one. Amend on Done/Stopped returns an explicit error to the orchestrator. Each amend carries a sequence number, and the agent acknowledges it. Stop is idempotent. Final state transitions are single-writer.
**Phase:** P3.

### 8. Background agents hit permission or trust prompts
**What goes wrong:** A subagent hits a confirm prompt (bash approval, cwd-guard, trust) while the TUI belongs to the main agent. Either it deadlocks waiting for input, or prompts from several agents interleave and the user approves the wrong one.
**Prevention:** Background subagents never prompt. The reference sets `shouldAvoidPermissionPrompts: true` and auto-denies. Auto-deny with a clear reason that the subagent can report, or route the request to one queued approval surface labelled with the agent id. Subagents inherit the parent's trust and cwd guard and can never widen them. Escape tests for the `write`/`edit` cwd guard must also run on subagent paths.
**Phase:** P1 for inheriting the guard; P3 or P5 for the approval queue.

## Moderate Pitfalls

### 9. Report loss and .md archive problems
Reports kept only in memory are lost on crash or cancel. Paths built from the agent name or task text break with slashes, collide, or escape the directory. Prevention: unique ids (ULID-like), sanitized paths under a dedicated session-scoped directory, atomic writes (tmp file then rename), the brief written before the agent starts, and the report written incrementally or on every exit path, including panic. Decide whether the archive dir is gitignored, so subagents don't pick up their own reports through grep or `git add`. **P3.**

### 10. TUI redraw contention
N agents sending progress events cause flicker, a slow redraw loop, or blocking on a full channel. Prevention: agents publish state into a shared snapshot (`watch` or `Arc<Mutex>`). The UI redraws on its own tick at about 10 Hz and never per event. Use bounded channels and drop or coalesce progress events; never block an agent on the UI. The strip must stay within 1–3 lines on narrow terminals: truncate by display width (remember the `de38681` 20-column overflow), with the elapsed time computed at render. Subagent output must not write to stdout or stderr directly. **P5.**

### 11. Provider rate limits and connection sharing
Child processes used to have separate clients. In-process, all agents share one HTTP client and one rate limit, so a 429 storm hits everyone and retries pile up. Prevention: one global concurrency semaphore per provider, shared backoff state when a 429 or Retry-After arrives, and jitter. **P1/P2.**

### 12. Session persistence and resume
Subagent messages leak into the parent session file (they would replay as tool_use the parent never requested; see invariant 15 / `f70e5cc`). On resume, agents show as "running" but no task exists. Prevention: each subagent gets its own session file, and the parent stores only the tool_use/tool_result plus the archive path. On resume, any non-terminal agent is marked "interrupted". Don't auto-restart; the orchestrator can re-dispatch from the brief .md. **P6, with the separate-file rule set in P1.**

### 13. Losing behaviour the child-process model gave for free
Things to keep: the `{previous}` chain substitution, the soft-error-when-agent-is-unknown output, the scope (user/project) for agent files, and the parallel cap. A panic in a child process used to be isolated; in-process it can kill the whole TUI, so wrap each agent in `catch_unwind`, or rely on a JoinHandle error, and turn it into a Failed report. Global state such as cwd or env vars set by one agent now affects all of them, so never call `set_current_dir`; pass cwd explicitly. **P1.**

## Minor Pitfalls

### 14. Hooks and extensions fire for subagent tools
They fire without agent context, or they fire N times. Pass the agent id into the hook payload and decide which events subagents emit. **P2.**

### 15. Ad-hoc role prompts leak the parent context, or omit it entirely
Define exactly what a dynamic subagent receives: the brief, cwd, and the AGENTS/CLAUDE.md-style project instructions. Don't pass the parent's history. **P2.**

### 16. Experimental toggle leaks into the default flow
The new tool specs change the prompt cache and model behaviour even when the toggle is off. With the toggle off, the tool specs and prompts must be byte-identical to v0.12. Don't repeat the bug where `thinking_level` was written but never read: check that the toggle is actually read back. **P4.**

### 17. Binary size
New deps such as tokio-util or ULID crates, or a UI widget lib, eat into the ~4 MB budget. Prefer what's already in the tree. **All phases.**

## Phase-Specific Warnings

| Phase | Likely pitfall | Mitigation |
|-------|---------------|------------|
| P1 In-process runner | Orphan tasks after Esc; panic kills TUI; shared file state | Token tree + JoinHandle registry; catch panics; cloned read state; test "no live tasks after cancel" |
| P2 Dynamic subagents | Recursion; toolset escalation; cost | Deny-list, depth cap, global semaphore, max_turns/budget |
| P3 Channels + .md | Amend races; report loss; path traversal | State machine, turn-boundary drain, write-before-return, atomic sanitized files |
| P4 Orchestrator | Does the work itself; context bloat from reports | Remove mutating tools from the registry; summary+path reports |
| P5 TUI strip | Redraw storms, width overflow, prompt interleaving | Snapshot + tick render, width truncation, single approval queue |
| P6 Resume/hardening | Phantom "running" agents; session pollution | Separate session files; mark interrupted on load; manual test-plan rows per pitfall |

Process note from STATE.md: four of the six v0.12 defects sat *in front of* working code (unreachable keybinding, an unread setting). Each new control path (amend key, expand shortcut, toggle) needs a manual end-to-end row, not just unit tests.

## Sources

- `/root/workspace/nanopi/src/tool/subagent.rs` (kill_on_drop cancellation model, parallel cap, chain)
- `/root/workspace/nanopi/.planning/STATE.md` (`2e386ef` parallel batch failure, cancel/session corruption fixes, invariant 15)
- `/root/workspace/claude-code-haha-main/src/tools/AgentTool/runAgent.ts` (unlinked AbortController for async agents, `shouldAvoidPermissionPrompts`, cloned readFileState, maxTurns)
- `/root/workspace/claude-code-haha-main/src/constants/tools.ts`, `agentToolUtils.ts` (`ALL_AGENT_DISALLOWED_TOOLS`, `ASYNC_AGENT_ALLOWED_TOOLS`)
- `/root/workspace/claude-code-haha-main/src/tools/AgentTool/AgentTool.tsx` (recursive fork guard, worktree isolation)

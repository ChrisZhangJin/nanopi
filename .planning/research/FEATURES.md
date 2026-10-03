# Feature Landscape — v0.13.0 Orchestrator & Dynamic Subagents

**Domain:** Coding-agent CLI, multi-agent delegation
**Researched:** 2026-10-03
**Confidence:** HIGH for Claude Code patterns (read from source in `/root/workspace/claude-code-haha-main`); MEDIUM for Roo/Cline/opencode/Codex (from training knowledge, not re-checked on the web)

## Reference behaviour (claude-code-haha-main)

| Concept | File | What it does |
|---------|------|--------------|
| Agent tool schema | `src/tools/AgentTool/AgentTool.tsx:83-100` | `description` (3-5 words), `prompt`, optional `subagent_type`, `model`, `run_in_background`, `name`, `isolation: worktree`, `cwd` |
| Dynamic default | `AgentTool.tsx:319-322` | If `subagent_type` is left out, the general-purpose agent is used (or a "fork" of the parent context when that flag is on). You don't need an agent file. |
| General-purpose agent | `src/tools/AgentTool/built-in/generalPurposeAgent.ts:29` | `tools: ['*']`, which means it inherits every parent tool except the blocked ones |
| Built-in roles | `built-in/{explore,plan,verification}Agent.ts` | Read-only explore/plan roles and a verifier role |
| Recursion block | `src/constants/tools.ts:36-93` | `ALL_AGENT_DISALLOWED_TOOLS`: subagents can't call Agent/TaskOutput |
| Background + output file | `AgentTool.tsx:148-152, 426` | An async launch returns `agentId` and `outputFile` straight away. Completion arrives later as a notification. |
| Continue / amend | `src/tools/SendMessageTool/SendMessageTool.ts:809-851` | If the target is running, the message is queued (`queuePendingMessage`) and injected at its next turn. If the target has stopped, it is auto-resumed with that message (`resumeAgent.ts`). |
| Stop | `src/tools/TaskStopTool/TaskStopTool.ts` | `task_id` → aborts the task and returns its status |
| Coordinator mode | `src/coordinator/coordinatorMode.ts:36-40, 84-93, 131-300` | Turned on by an env flag. Replaces the system prompt. The coordinator only gets Agent, SendMessage and TaskStop. Workers get the full toolset minus the team-internal tools. The prompt teaches research → synthesise → implement → verify, and a "continue vs spawn fresh" decision table. |
| Result delivery | `coordinatorMode.ts:144-162` | Arrives as a user-role `<task-notification>` with task-id, status (completed/failed/killed), summary, result, and usage (tokens, tool_uses, duration_ms) |
| Status UI | `src/components/CoordinatorAgentStatus.tsx`, `AgentTool/agentColorManager.ts` | One line per agent with its colour, state and elapsed time |

Other ecosystems (MEDIUM): **Roo Code "Orchestrator/Boomerang"** has a mode that can't edit files. It calls `new_task(mode, message)`, the child returns a summary through `attempt_completion`, and only that summary goes back to the parent. **opencode** has a `task` tool that uses predefined agents, plus a general agent. **Codex CLI** has no stable orchestrator (LOW). **Aider** has no subagents. Across all of these, the pattern is the same: the delegator gets restricted tools, the child gets fresh context, and only a summary comes back.

## Table Stakes

| Feature | Why Expected | Complexity | Depends on (existing) |
|---------|--------------|------------|------------------------|
| Ad-hoc dispatch: `task` + `description` with no agent name → built-in general-purpose role | This is how CC works by default. The model shouldn't have to write files first. | Low | `resolve_agent` in `src/tool/subagent.rs:402`; make `agent` optional |
| Optional inline `role`/system prompt and `tools` allowlist per call | The milestone asks for it. It also lets the model create a read-only explorer on the fly. | Med | Tool registry filtering; reuse the agent-file frontmatter shape |
| Predefined agent files keep working (single/parallel/chain) | Avoids regressions | Low | Current `Mode` enum, `subagent.rs:60` |
| Subagents can't spawn subagents | Prevents runaway recursion and cost (`constants/tools.ts:92`) | Low | Filter the tool out of the child registry |
| Only the final summary goes back to the parent, with a size cap | Keeps the parent's context clean. Every reference does this. | Low | `final_assistant_text`, `subagent.rs:257` |
| Stable agent id + short label | Needed for amend/stop/panel addressing | Low | New |
| Stop a running subagent (cooperative cancel at the next tool or turn boundary, and abort in-flight streams) | Needed for CC TaskStop and for the milestone | Med | In-process runtime: CancellationToken per agent |
| Amend a running subagent (message is queued and injected before its next LLM turn) | Matches CC SendMessage to a running agent | Med | Reuse the v0.11 steer/follow-up injection queue |
| Background/non-blocking launch in orchestrator mode, with results delivered as a synthetic notification message | The orchestrator must keep running while workers work (CC `<task-notification>`) | High | Agent loop has to accept external messages between turns |
| Status per agent: running / completed / failed / stopped, plus elapsed time, tokens and tool count | Shown in the CC notification usage block and the status UI | Low | Usage accounting that already exists |
| Orchestrator toggle that is off by default (a TUI command plus a flag/env var) | Experimental and opt-in, per the PROJECT decision | Low | TUI slash commands |
| Orchestrator gets a restricted toolset (dispatch/amend/stop plus read-only tools at most) and a dedicated system prompt | This is how "does no implementation itself" is enforced (CC `coordinatorMode.ts:84-93`) | Med | Tool registry filtering |
| Concurrency cap on parallel agents | Old, low-resource boxes; there's already a parallel cap | Low | Existing parallel cap |
| Bottom agents strip (1-3 lines, collapses when idle) with an expand shortcut | Milestone item; CC has CoordinatorAgentStatus | Med | TUI layout |

## Differentiators

| Feature | Value Proposition | Complexity | Notes |
|---------|-------------------|------------|-------|
| `.md` archive per agent (`brief.md`, `report.md`, maybe a progress log) under the session dir | Human-readable trail plus recovery. CC only has an opaque output file. | Med | Write the brief at spawn and the report on finish. Include status and usage in front-matter. |
| Continue a *finished* agent with its context kept (CC auto-resume) | Reuses loaded context instead of making a fresh worker re-read everything | High | Requires keeping the child's message history. Could defer. |
| Recovery after a crash/restart from the archive (re-dispatch unfinished briefs) | Uses the `.md` files | Med | Can stay manual at first (the user asks to resume) |
| Expanded panel: per-agent last tool call, a live tail, and inline stop/amend keys | The human can step in directly | Med | |
| Orchestrator prompt with a synthesise step ("never delegate understanding") and a continue-vs-spawn table | Delegation quality. Borrow this from CC `coordinatorMode.ts:280-300`. | Low | Prompt work only |
| Per-agent model override (a cheap model for exploration) | Saves cost | Low | CC `model` param |
| Worktree isolation for parallel writers | Prevents edit conflicts | High | Defer |

## Anti-Features

| Anti-Feature | Why Avoid | Instead |
|--------------|-----------|---------|
| Nested agents and agent swarms or teams (CC TeamCreate, shutdown_request protocol) | Complexity and cost blow-up | One level: orchestrator → workers |
| Peer-to-peer messaging between workers | Hard to reason about | All communication goes through the orchestrator |
| Streaming a worker's full transcript into the parent's context | Floods the context | Summary in context; full detail in `.md` and the panel |
| Orchestrator also editing files "just this once" | Breaks the mode's contract | Remove the write tools from the orchestrator's toolset entirely |
| Making orchestrator mode the default, or changing the default flow | The PROJECT decision says opt-in | Keep it behind a toggle |
| Hard-killing tasks mid-write | Corrupts files | Cooperative cancel at tool boundaries; abort only during LLM streaming |
| Requiring the model to write an agent file before it can dispatch | That's exactly the problem this milestone fixes | Inline role/tools |
| Unbounded parallel workers | Small machines, rate limits | Configurable cap, then a queue |
| Fork-of-parent-context as the default | Expensive and leaks context | Fresh context plus a self-contained brief |

## Feature Dependencies

```
In-process subagent runtime (replaces child process) ──┬→ Stop (cancel token)
                                                       ├→ Amend (inbox channel → steer queue)
                                                       ├→ Status events → TUI agents strip
                                                       └→ .md archive (brief/report)
Dynamic dispatch (optional agent, inline role/tools) → Orchestrator mode
Background launch + notification injection into main loop → Orchestrator mode
Restricted tool registry filtering → both dynamic toolsets and orchestrator
Continue-finished-agent → retained child history (defer)
```

## MVP Recommendation

1. Dynamic dispatch with a general-purpose default, an inline role/tools option, and the recursion block (in either runtime).
2. In-process runtime with an id, status events, a cancel token, an inbox for amendments, and the `.md` brief/report.
3. TUI agents strip with expand.
4. Orchestrator toggle: restricted tools, coordinator prompt, background launch, `<task-notification>`-style result messages, a final summary.

Defer: continuing finished agents, crash recovery automation, worktree isolation, per-agent model override.

## Sources

- `/root/workspace/claude-code-haha-main/src/tools/AgentTool/AgentTool.tsx`, `built-in/generalPurposeAgent.ts`, `resumeAgent.ts`
- `/root/workspace/claude-code-haha-main/src/tools/SendMessageTool/SendMessageTool.ts`
- `/root/workspace/claude-code-haha-main/src/tools/TaskStopTool/TaskStopTool.ts`
- `/root/workspace/claude-code-haha-main/src/coordinator/coordinatorMode.ts`
- `/root/workspace/claude-code-haha-main/src/constants/tools.ts`
- `/root/workspace/claude-code-haha-main/src/components/CoordinatorAgentStatus.tsx`
- `/root/workspace/nanopi/src/tool/subagent.rs`
- Roo Code Orchestrator/Boomerang, opencode `task` tool: training knowledge (MEDIUM/LOW)

# Architecture Patterns — v0.13.0 Orchestrator & Dynamic Subagents

**Domain:** in-process multi-agent orchestration in a Rust/tokio coding-agent CLI
**Researched:** 2026-10-03
**Confidence:** HIGH for integration points (read from source); MEDIUM for the proposed design

## Key Answers

1. **Can `Agent` run several times in one process? Yes.** `Agent` (src/agent/loop_.rs:76) is an owned struct (`Box<dyn Provider>`, `ToolRegistry` of `Arc<dyn Tool>`, `Context`, `PathBuf`s) and is already `Send + 'static`: the TUI moves it into `tokio::spawn` for every turn (src/mode/tui.rs:3429-3440, `run_turn(&msg, &tx, Some(ct), Some(steer_rx))`). No globals tie a turn to the process. N agents = N `Agent` values, each built with `Agent::build_fresh` (src/agent/build.rs:321). Things to watch:
   - Each needs its **own provider** instance (cheap; rebuild from `base_url/api_key/model`).
   - Each needs its **own `session_path`**: every turn persists to JSONL (loop_.rs ~1581, 2019). Print mode's `--no-session` uses a temp file (main.rs:436-443). For subagents use `.nanopi/agents/<run-id>/<agent-id>/transcript.jsonl` — it doubles as the recovery trail.
   - **Hooks & WASM extensions:** `build_fresh` takes `extensions`; pass none (or a shared read-only set) for subagents in v0.13 — WASM store sharing across concurrent agents is unverified (LOW). Fire no `session_start` hooks for subagents, or a dedicated `subagent_start` event.
   - **Permission gate:** `PermissionGate` is per-agent; subagents cannot pop TUI prompts. Inherit the parent's decisions, deny anything that would `ask`.
   - Recursion: subagent registries must exclude `subagent`/orchestration tools (replaces today's fork-bomb cap, subagent.rs:50).

2. **Existing control primitives already fit amend/stop:** `run_turn` takes a `CancellationToken` (stop) and an `mpsc::Receiver<SteerMessage>` (src/event.rs:126 — `Steering`/`FollowUp`) drained at the top of each loop iteration (loop_.rs:1080-1130). **Amend = send `SteerMessage::Steering`; stop = `ct.cancel()`.** No loop changes needed for the core control path.

3. **Where the TUI takes status updates:** `run_app` (tui.rs:1795) is a single `tokio::select!` (tui.rs:1828) over `tick` (120 ms), key `EventStream`, and `recv_optional(ag_rx)` (tui.rs:3652) for the main turn's `AgentEvent`s. Add **one more arm** on a `broadcast`/`mpsc` receiver of `SubagentEvent` from the registry, or simplest: read a shared `Arc<SubagentRegistry>` snapshot on the existing tick arm (120 ms is already the spinner cadence). Rendering goes in `draw_dock` (tui.rs:5041) — its `Layout` has `Min(0) | status(1) | input | cwd(1) | stats(1)`; insert `Constraint::Length(n_agent_rows)` (0 when no agents, 1-3 collapsed, more expanded) between status and input, drawn by a new `draw_agents_strip`. Also bump `DOCK_HEIGHT` handling.

## Recommended Architecture

```
            TUI run_app select! (tui.rs:1828)
     keys ─┐   tick ─┐   ag_rx (main turn) ─┐   sub_rx (NEW) ─┐
           v         v                       v                  v
        App state ── agents_view (NEW) ◄── SubagentRegistry (NEW, Arc)
                                              │ spawn / amend / stop / list
   Main Agent ──tool calls──► AgentTool / SendMessage / StopAgent (NEW tools)
                                              │ tokio::spawn per subagent
                                              v
                              SubagentHandle{ id, ct, steer_tx, JoinHandle,
                                              state, task, started_at }
                                              │ runs Agent::run_turn
                                              v
                              .nanopi/agents/<run>/<id>/{brief.md,report.md,transcript.jsonl}
```

### Component Boundaries

| Component | New/Modified | Responsibility | Talks to |
|-----------|--------------|----------------|----------|
| `src/agent/subagents.rs` — `SubagentRegistry` | NEW | Owns `HashMap<AgentId, SubagentHandle>`; spawn, amend (steer_tx), stop (ct), status snapshot, emits `SubagentEvent` | tools, TUI, archive |
| `src/agent/subagent_runner.rs` | NEW | Build child `Agent` (role prompt via `compose_system_prompt`, tool allowlist via `ToolRegistry::standard_with_allowlist` mod.rs:559), run turns, consume own `AgentEvent`s into progress (last tool, tokens), extract final assistant text as report | `build.rs`, `loop_.rs` |
| `src/agent/archive.rs` | NEW | Write `brief.md` at spawn, append amendments, `report.md` on finish; index file for recovery | fs |
| `src/tool/subagent.rs` | MODIFIED (rewrite) | Keep single/parallel/chain schema + predefined agents (`resolve_agent` :402); add dynamic `description`, `prompt`, `tools`, `run_in_background`; replace `run_single`/`spawn_and_collect` (:498-630) with registry calls | registry |
| `src/tool/agent_ctl.rs` (`send_message`, `stop_agent`, `list_agents`) | NEW | Orchestrator controls (mirrors haha `SendMessageTool`, `TaskStopTool`) | registry |
| `ToolContext` (tool/mod.rs:309) | MODIFIED | Add `Option<Arc<SubagentRegistry>>` (+ parent event tx) so tools reach the registry without globals | all tools |
| `src/event.rs` | MODIFIED | Add `SubagentEvent { id, state, task, elapsed, last_activity }` and a `AgentEvent::SubagentReport` / notification for background completions | TUI |
| `Agent` (loop_.rs:76) | MODIFIED (small) | Add `orchestrator: bool`; when true, registry filtered to orchestration tools + read-only and system prompt gets coordinator section; background reports injected via `pending_follow_ups` (loop_.rs:117) | build.rs |
| `src/mode/tui.rs` | MODIFIED | `/orchestrator` toggle (slash + settings), new select arm or tick poll, `draw_agents_strip`, shortcut to expand (avoid Ctrl+O, already "expand tool output" tui.rs:1252; suggest Ctrl+G/F2 — check keys.rs), Esc semantics: stop main turn only, not subagents | registry |
| `src/render/panel.rs` | MODIFIED | Subagent summary line (already special-cased, panel.rs:182) shows id + state | — |
| `src/mode/print.rs` | MODIFIED | Print mode must await foreground subagents and drain/kill background ones before exit | registry |

### Data Flow

1. Main agent calls `subagent{task, prompt?, tools?, background?}` → tool asks registry to spawn → runner writes `brief.md`, builds `Agent`, `tokio::spawn`s `run_turn` with its own `ct` + `steer_rx`.
2. Foreground: tool awaits `JoinHandle` and returns report (today's semantics; parent Esc cancels child via ct linked as child token `ct.child_token()`).
   Background (orchestrator default): tool returns `{agent_id, brief_path}` immediately; on completion registry writes `report.md` and pushes a `<subagent-report id=..>` message to the main agent: if main is idle → TUI starts a turn (same path as follow-ups, tui.rs:1805 `follow_up_slot`); if streaming → `SteerMessage::FollowUp`.
3. `send_message{id, text}` → `steer_tx.send(Steering)`; if child finished, start a new `run_turn` on the retained `Agent` (resume, haha `resumeAgent.ts` pattern) — retain idle agents until session end.
4. `stop_agent{id}` → `ct.cancel()`; loop already writes an abort marker (loop_.rs:1263); report = partial text + "stopped".
5. Runner forwards a throttled `SubagentEvent` to registry → TUI strip.

## Patterns to Follow

- **Reuse steer channel + CancellationToken** rather than a new control protocol. Child tokens give "Esc on main cascades to foreground children" for free.
- **Registry as `Arc<std::sync::Mutex<..>>` with short critical sections**; never hold it across `.await`. Agents themselves are owned by their task, not the registry (avoids the `agent_slot` take/put dance of tui.rs:3436).
- **Coordinator = tool filtering + prompt** (haha `coordinatorMode.ts`: `INTERNAL_WORKER_TOOLS`, worker tool set). Toggle sets `orchestrator` and rebuilds registry; takes effect on next turn, never mid-turn.
- **Files are the archive, channels are the control.** Write brief before spawn, report after; recovery reads the index, never reconstructs live state.

## Anti-Patterns to Avoid

- **Sharing one `Agent`/provider across tasks behind a Mutex** — serializes everything; build one per subagent.
- **Letting subagents use the parent session JSONL** — corrupts resume/fork.
- **Global static registry** — breaks tests (many `#[tokio::test]` in subagent.rs) and print mode; pass via `ToolContext`.
- **Subagents writing to the terminal** — only the TUI thread draws; subagent events must go through the channel.
- **Unbounded concurrency** — cap concurrent subagents (reuse MAX_PARALLEL constant) and concurrent writes to the same file (`mutation_key`, tool/mod.rs:227, is per-agent today; cross-agent edit races are a known gap).

## Suggested Build Order

1. **In-process runner + registry (foreground only)** — swap child process for `Agent` in `run_single`; keep tool schema; tests pass unchanged. Proves multi-instantiation, per-agent session, permissions, cancellation. *(Foundation for everything.)*
2. **Archive (`brief.md`/`report.md`/index)** — small, depends on 1.
3. **Dynamic subagents** — `prompt`/`tools`/`description` args; predefined agents still resolve. Depends on 1.
4. **Background mode + `send_message`/`stop_agent`/`list_agents` + report injection** — depends on 1-2; touches follow-up path in TUI and print-mode drain.
5. **TUI agents strip + expand shortcut** — depends on registry events (1, 4); can start in parallel with 3.
6. **Orchestrator toggle** — tool filtering + coordinator prompt; last since it only composes 3-5.

## Scalability Considerations

| Concern | 1-3 agents | 5-10 agents | Many |
|---------|-----------|-------------|------|
| Memory | trivial (vs ~process per agent today — in-process is a win on low-RAM boxes) | contexts dominate; fine | cap concurrency |
| Rate limits | n/a | provider 429s; serialize via semaphore | queue |
| TUI strip | 1 row each | collapse to counts + expand | scroll in expanded view |

## Open Questions / Flags

- WASM extension (`extensions`) thread-safety when shared by concurrent agents — needs a phase-1 spike (LOW).
- Hook events for subagents (fire or not) — decide in requirements.
- Exact shortcut key — verify free bindings in `src/keys.rs`.

## Sources

- nanopi source: src/agent/loop_.rs, src/agent/build.rs, src/mode/tui.rs, src/tool/subagent.rs, src/tool/mod.rs, src/event.rs, src/main.rs (HIGH)
- Reference: /root/workspace/claude-code-haha-main/src/{tools/AgentTool,tools/SendMessageTool,tools/TaskStopTool,tasks/LocalAgentTask,coordinator/coordinatorMode.ts} (structure inspected, MEDIUM)

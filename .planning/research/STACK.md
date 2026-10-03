# Technology Stack — v0.13.0 additions

**Project:** nanopi (Orchestrator & Dynamic Subagents)
**Researched:** 2026-10-03
**Verdict:** ZERO new crates. Every primitive needed is already in Cargo.toml / Cargo.lock.

## Resolved versions in use (Cargo.lock)

| Crate | Locked | Features enabled | Relevant to |
|-------|--------|------------------|-------------|
| tokio | 1.53.1 | rt-multi-thread, macros, io-util, process, time, sync, signal | in-process tasks, channels, timers |
| tokio-util | 0.7.19 | (none) | `sync::CancellationToken` — already used in `agent/loop_.rs` and `render/spinner.rs` |
| ratatui | 0.29.0 | crossterm | agents status strip / expanded panel |
| crossterm | 0.29.0 | events, event-stream, bracketed-paste | expand/collapse shortcut |
| uuid | 1.x | v7, std | subagent run IDs (time-ordered = sortable archive filenames) |
| chrono | 0.4 | clock | elapsed time, report timestamps |
| serde / serde_json | 1.0 | derive | tool schema for dynamic dispatch, report front-matter |
| async-trait | 0.1 | — | existing `Tool` trait, no change |

## Needed capability -> existing primitive

| Capability | Primitive | Why this one |
|------------|-----------|--------------|
| Run subagent in-process | `tokio::spawn` running the existing agent loop (`agent/loop_.rs`) with its own context/session; keep `JoinHandle` | Loop already accepts `cancel: Option<CancellationToken>`; no process spawn, no JSON envelope parsing |
| Stop subagent | `tokio_util::sync::CancellationToken` — `child_token()` from the orchestrator's turn token | Already the loop's cancellation contract; child tokens make Esc on the parent cascade to all subagents for free. Use `JoinHandle::abort()` only as a last-resort fallback (it skips cleanup/report writing) |
| Amend running subagent | `tokio::sync::mpsc::Sender<SteerMessage>` per subagent | Reuse the existing steer/follow-up injection path (`event::SteerMessage`, `plugin_send.rs` `steer_tx`) — an amend is just a steer into the child loop. Bounded (32) like existing code |
| Report / progress events up | one shared `mpsc::Sender<SubagentEvent>` (Started/Tool/Progress/Done{report}/Failed) cloned into each child | Single receiver in TUI `select!`; mirrors existing `AgentEvent` channel |
| Status snapshot for TUI strip | `tokio::sync::watch` per subagent (state, task, started_at, last tool) OR a `Arc<std::sync::Mutex<Registry>>` read at frame time | watch is in `sync` feature already; Mutex registry is simpler for the renderer — recommend registry + mpsc event to trigger redraw |
| Await final report from orchestrator tool | `tokio::sync::oneshot` per run (or `JoinHandle` output) | Lets `subagent` tool block (sync mode) or return immediately with an id (async/orchestrator mode) |
| Concurrency cap | existing `tokio::sync::Semaphore` in subagent.rs | Unchanged; now caps tasks, not processes |
| Elapsed time ticks | `tokio::time::interval` (1 s) only while any agent is running | Avoid idle redraws |
| Archive briefs/reports as .md | `tokio::fs` / `std::fs` + `chrono` + `uuid::now_v7` | Plain markdown; hand-written front-matter, no YAML crate |
| Bottom strip 1-3 lines / expanded | ratatui `Layout` with `Constraint::Length(n)` row, `Paragraph`/`List`, `Gauge` not needed | ratatui 0.29 already compiled in; a strip is just one more layout chunk in `mode/tui.rs` |
| Toggle orchestrator mode | settings flag + slash command / key binding via crossterm `KeyEvent` | No dependency |

Note: `tokio::task::JoinSet` (in tokio `rt`, already enabled) is a good fit to own all running subagent handles and reap them; `AbortHandle` comes with it.

## What NOT to add

| Don't add | Why |
|-----------|-----|
| tokio-util `rt` feature / `TaskTracker` | Only a convenience; JoinSet + CancellationToken suffice. Avoid feature growth |
| `flume`, `crossbeam-channel`, `async-channel` | tokio mpsc/watch/oneshot cover every case; duplicates code size |
| `tokio-stream` | `futures-util` already present for stream combinators |
| Actor frameworks (`actix`, `ractor`, `kameo`) | Hundreds of KB for what is ~3 channels per subagent |
| `serde_yaml` / front-matter crates | Write `key: value` header by hand; agent-file parser already exists in `agent/agents.rs` |
| `tui-tree-widget`, `tui-scrollview`, other ratatui widget crates | Built-in `List`/`Paragraph` + manual scroll offset are enough |
| `dashmap` / `parking_lot` | Registry is tiny; `std::sync::Mutex` is fine |
| Bumping ratatui to 0.30 this milestone | 0.30 splits into ratatui-core/widgets crates and has API churn; out of scope, size risk |
| `tracing` | Not needed for the feature; separate decision |

## Integration points

- `src/tool/subagent.rs`: replace `spawn_and_collect(Command)` with an in-process runner; keep the child-process path behind a fallback only if isolation is still wanted (recommend removing it later to drop the `process` usage here — `process` stays for bash tool anyway).
- `src/agent/loop_.rs`: loop already takes `CancellationToken`; needs a steer receiver parameter for subagents (likely already present for the TUI path) and a tool-filter for ad-hoc toolsets.
- `src/event.rs`: add `SubagentEvent` variants alongside `SteerMessage`.
- `src/mode/tui.rs`: add strip chunk + expand shortcut; consume `SubagentEvent` in the existing `select!`.
- Panic note: `panic = "abort"` in release means a panicking subagent task kills the whole process (no JoinError isolation as in-process). This is the main regression vs child processes — guard with careful error handling; do not rely on `catch_unwind`.

## Binary-size impact

Expected ~0 KB from dependencies (all generic code already monomorphized for similar types); small growth (tens of KB) from new code only. Verify with `cargo build --release && ls -l target/release/nanopi` before/after.

## Installation

```bash
# nothing to install
```

## Sources

- /root/workspace/nanopi/Cargo.toml, Cargo.lock (HIGH)
- Existing usage: src/agent/loop_.rs (CancellationToken), src/plugin_send.rs (steer mpsc), src/tool/subagent.rs (Semaphore, process spawn) (HIGH)
- tokio-util CancellationToken lives in `sync` module with no feature flag required — confirmed by existing compilation with default-features=false (HIGH)

# Phase 1: Child-process runtime - Research

**Researched:** 2026-10-03
**Domain:** Rust/tokio process supervision, nanopi `-p` mode extension, cross-process file guards
**Confidence:** HIGH (codebase read directly; no new crates needed)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- Child processes replace the in-process design (rolled back in `2bd0343`) because `panic = "abort"` lets any in-process panic kill nanopi. Keep `panic = "abort"`.
- **D-01:** Restore the child-process runtime (`run_single` / `spawn_and_collect` as of `28fe37d`) as the base and extend it. Child command: `nanopi -p --output json` plus new flags (D-05, D-07, D-09). Same provider/model/api-key/base-url as the parent unless the dispatch says otherwise (fix the old inheritance gap).
- **D-02:** A `SubagentRegistry` in the parent tracks every child: id (`a1`, `a2`, … per run), pid, state, start time, agent dir. Run dir uses a uuid v7.
- **D-03:** Agent dir: `.nanopi/agents/<run>/<id>/` containing `brief.md`, `transcript.jsonl` (child's own session) and `report.md`. Phase 2 adds the index, interrupted marking and cleanup.
- **D-04:** No user-facing subagent controls (no Esc/Ctrl+X, no permission prompts, no TUI actions). Stopping is by the orchestrator (tool, Phase 4) or implicitly when the parent turn is cancelled or nanopi exits. Use `kill_on_drop` plus killing the process group so a child's own bash subprocesses die too. No orphans.
- **D-05:** Dispatch carries the allowed tool list; parent passes it as e.g. `--tools read,grep,edit`. Child registers exactly those tools, never prompts, anything else denied in-band. Subagent and control tools always removed (depth 1). `PreToolUse`/`PostToolUse` hooks still run in the child; child gets `NANOPI_AGENT_ID` env, hooks receive it as `agent_id`.
- **D-06:** `max_live = 8` (beyond → clear in-band error); `max_concurrency = 4` running, excess queue.
- **D-07:** `max_turns = 50`, `token_budget = 300_000`, passed as flags. Hitting either stops the child with a partial report and `status: limit_reached` naming the limit.
- **D-08:** Non-zero exit, signal, timeout or unparseable JSON → agent `failed` with stderr tail / error text. Parent never panics on child output.
- **D-09:** Parent writes `brief.md` (task, role, tools, model) before spawning; child started with `--brief <path>` and uses it as its task.
- **D-10:** Orchestrator may only append `## Amendment N` sections (one write each). Child only reads. Between turns the child checks size/mtime; new amendments injected as steering messages for the next turn. Never mid tool call.
- **D-11:** Before finishing, child re-reads `brief.md`, checks every requirement/amendment; if undone keeps working at most 2 extra turns, then writes `report.md` regardless with per-item checklist (done / not done + note).
- **D-12:** Amendment after finish (`report.md` exists) → parent notices; continuing is Phase 4 (CTL-06). This phase adds child-side `--session <path>` resume flag.
- **D-13 (ISO-03):** On read, record mtime + content hash (std `DefaultHasher`, no new crate); on edit/write re-check on-disk file, refuse with "file changed since you read it — re-read first" if different. Writes atomic (temp + rename). A never-read file may be written.
- **D-14:** Children load WASM extensions only if in the allowed tool list.

### Claude's Discretion
- Exact flag names, JSON output schema extensions, test layout.

### Deferred Ideas (OUT OF SCOPE)
- Archive index, interrupted marking, cleanup (Phase 2); dynamic dispatch, background launch (later); strip UI and orchestrator mode (later); stop/amend/continue control tools (Phase 4).
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| RT-01 | Isolated `nanopi -p` child; crash never affects parent | Existing `spawn_and_collect` (src/tool/subagent.rs:554); harden: no `?` on child data → in-band failure (Pattern 4) |
| RT-02 | Tracked (id, pid, state), killed on stop/cancel/exit, no orphans | `process_group(0)` + `ChildGuard` Drop → `killpg`; child sets `PR_SET_PDEATHSIG` (Pattern 2) |
| RT-03 | Only orchestrator controls | Nothing user-facing added; child stdin null, own process group (no terminal SIGINT) |
| RT-04 | Own transcript, no leak | New `--session-file <path>` writes child session to agent dir; replaces `--no-session` |
| RT-05 | No recursion; global live cap | Child always strips `subagent` from registry when `NANOPI_AGENT_ID` set / `--brief` given; registry `max_live` + `Semaphore(max_concurrency)` |
| RT-06 | Turn limit + token budget flags | `MAX_ITERATIONS` const (loop_.rs:1059) → field; `usage_total` (loop_.rs:1391) checked per iteration |
| RT-07 | Exact tool list, never prompt | `ToolRegistry::standard_with_allowlist` exists; `-p` already non-interactive; pass `--approve`-equivalent trust decided by parent |
| RT-08 | Failures reported, nanopi keeps running | Map spawn error / non-zero / signal / timeout / bad JSON all to `ToolOutput{is_error:true}` |
| RT-09 | Brief file + amendments + self-check | Print mode currently passes `steer_rx = None` (print.rs:~217); add brief watcher task feeding `SteerMessage::Steering` |
| ISO-03 | Cross-process stale-write guard | `ToolContext` is only `{cwd}` (tool/mod.rs:309); add per-process `FileStateTracker`; atomic write in write.rs/edit.rs |
</phase_requirements>

## Summary

The revert restored the code to `28fe37d`: `src/tool/subagent.rs` (869 lines) already has a working child-process runtime — `run_single` builds `nanopi -p --output json --no-session [--model] [--tools] [--append-system-prompt file] "Task: …"`, `spawn_and_collect` drains stdout/stderr concurrently, waits, and parses `JsonEnvelope` (print.rs:27). It has `kill_on_drop(true)` but no process group, no registry, no limits, no brief, no agent dir, and it does **not** pass provider/api-key/base-url (the "inheritance gap"). Unparseable JSON currently returns `Err(ToolError::Execution)` rather than a structured failed-agent result. Nothing from the in-process run (registry, FileStateTracker, `[subagent]` config, hook `agent_id`) survives — all must be re-added; the superseded plans in `superseded-inprocess/` are useful references for the tracker and config shapes and their tests.

Work splits cleanly into (a) **child-side** changes to `-p` mode (new flags `--brief`, `--session-file`, `--max-turns`, `--token-budget`; brief watcher → steer channel; self-check + `report.md`; recursion strip; limit stop reason in envelope), (b) **parent-side** supervisor (registry, caps, process-group kill, failure mapping, provider inheritance, agent dir + brief writing), and (c) **ISO-03** in read/write/edit tools. No new crates: tokio 1.53.1 already has `process` (with `Command::process_group`, stable since tokio 1.24), `libc` is a unix dependency, `uuid` v7 is enabled.

**Primary recommendation:** Extend the existing `subagent.rs` runtime; put children in their own process group and kill the group from a Drop guard; make the child self-terminating via `PR_SET_PDEATHSIG`; implement amendments as a polling task in print mode that feeds the already-existing `steer_rx` of `run_turn`.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Spawn/track/kill children, caps | Parent process (`tool/subagent.rs` + new `agent/subagent_registry.rs`) | — | Only the orchestrator owns lifecycle |
| Brief writing / amendment appends | Parent | Filesystem (agent dir) | Parent is sole writer (D-10) |
| Brief polling, steering injection, self-check, report.md | Child (`mode/print.rs`) | `agent/loop_.rs` steer channel | Child is sole reader |
| Turn limit / token budget | Child (`agent/loop_.rs`) | — | Must stop the loop from inside |
| Tool allowlist, recursion strip | Child (`tool/mod.rs`, `main.rs`) | Parent builds the list | Child registers exactly what it was given |
| Stale-write guard | Child/any process (`tool/read.rs`,`edit.rs`,`write.rs`) | Filesystem | Per-process tracker; on-disk state is the shared truth |
| Transcript | Child (`session.rs`) | Agent dir | RT-04 |

## Standard Stack

### Core (all already in Cargo.toml — no installs)
| Library | Version | Purpose |
|---------|---------|---------|
| tokio | 1.53.1 (Cargo.lock) [VERIFIED: Cargo.lock] | `process::Command` (`process_group`, `kill_on_drop`), `sync::Semaphore`, `time::timeout`, `fs::metadata` polling |
| libc (unix) | 0.2 [VERIFIED: Cargo.toml] | `killpg`, `prctl(PR_SET_PDEATHSIG)`, `getppid` |
| uuid | 1.10, feature v7 [VERIFIED: Cargo.toml] | run dir id |
| serde_json | existing | envelope extension |
| std `DefaultHasher` | std | content hash (D-13) |

**Installation:** none.

## Package Legitimacy Audit

No external packages are installed in this phase. slopcheck not run — not applicable.

## Architecture Patterns

### System Architecture Diagram
```
orchestrator model ──tool call "subagent"──▶ SubagentTool::execute
   │                                            │
   │                       registry.reserve() ──┤ live>=max_live → in-band error
   │                       semaphore.acquire()  │ (queue when >max_concurrency)
   │                                            ▼
   │              write .nanopi/agents/<run-uuidv7>/<aN>/brief.md
   │                                            ▼
   │   spawn `nanopi -p --output json --brief B --session-file T --tools L
   │          --max-turns N --token-budget K --model/--api-key/--base-url…`
   │          process_group(0), stdin null, env NANOPI_AGENT_ID=aN
   │                                            │ ChildGuard (Drop → killpg SIGKILL)
   │                                            ▼
   │   CHILD: build registry(L minus subagent) → run_turn(brief text, steer_rx)
   │          ├─ brief watcher (poll size/mtime) ─new "## Amendment N"─▶ steer_tx
   │          ├─ per-iteration: turns>=N or tokens>=K → StopReason::Limit
   │          ├─ finish → self-check re-read brief (≤2 extra turns) → report.md
   │          └─ stdout: JsonEnvelope{… status, limit, report_path}
   │                                            ▼
   │   parent: exit status / signal / timeout / JSON parse
   │          ok → completed|limit_reached ; else → failed(stderr tail)
   ◀──────────────── ToolOutput (never Err for child faults) ◀─┘
```

### Recommended structure
```
src/agent/subagent_registry.rs   # new: registry, ids, states, caps, ChildGuard
src/agent/brief.rs               # new: brief render, amendment parse/diff, report checklist
src/tool/file_state.rs           # new: FileStateTracker (ISO-03)
src/tool/subagent.rs             # extend run_single/spawn_and_collect
src/mode/print.rs, src/main.rs   # new flags, watcher, self-check, envelope fields
src/agent/loop_.rs               # configurable max turns + token budget + stop reason
src/agent/hook.rs                # agent_id in HookInput from NANOPI_AGENT_ID
```

### Pattern 1: Process-group spawn + guard (RT-02)
`kill_on_drop` SIGKILLs only the direct child pid; the child's bash tool subprocesses survive. Put the child in its own group and kill the group.
```rust
// tokio::process::Command::process_group is unix-only (tokio >= 1.24)
command.process_group(0).kill_on_drop(true);
let child = command.spawn()?;
let pgid = child.id(); // == pgid because process_group(0)
struct ChildGuard { pgid: Option<u32> }
impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(unix)] if let Some(p) = self.pgid.take() {
            unsafe { libc::killpg(p as libc::pid_t, libc::SIGKILL); }
        }
    }
}
// disarm (pgid = None) only after wait() returned AND you still want to
// reap stragglers? -> No: always killpg on drop; ESRCH is harmless.
```
Note: once `wait()` reaped the leader, its pid could theoretically be reused; the group id stays valid while any member lives. Accept (killpg ESRCH is harmless; reuse window is negligible) [ASSUMED].

### Pattern 2: Parent-death signal in the child (RT-02, nanopi exit)
With `panic = "abort"` or SIGKILL of nanopi, no Drop runs. Make the child die by itself: at child startup (when `--brief` or `NANOPI_AGENT_ID` present) call `libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL)` then check `getppid()` against the expected parent pid (pass `NANOPI_PARENT_PID`) to close the race where the parent died before prctl. Linux-only; on macOS fall back to a watchdog task polling `getppid() == 1`. Doing it in the child's `main` avoids `unsafe pre_exec`. Residual: grandchildren (bash) of a SIGKILLed child — child's bash tool should also spawn in a group the child kills on its own SIGTERM... with SIGKILL nothing runs; document as residual risk or have bash children also set PDEATHSIG is impossible without pre_exec. Recommend: parent normal-exit path (TUI quit, Ctrl+C handler) drains registry and `killpg`s every group; abort path covered only for direct children.

### Pattern 3: Amendments via existing steer channel (RT-09)
`run_turn(msg, &tx, cancel, steer_rx)` (loop_.rs:781) already drains `steer_rx` at iteration boundaries (loop_.rs:1100) — exactly "between turns, never mid tool call". Print mode passes `None`. Add: `(steer_tx, steer_rx) = mpsc::channel(8)`; spawn a watcher that every ~500 ms stats `brief.md`; on size/mtime change re-read, parse `## Amendment (\d+)` headings, send `SteerMessage::Steering{text}` for numbers > last seen. Only emit a section once the next heading or EOF is reached AND size is stable across two polls (guards against torn reads even though parent writes whole section in one `write_all`; append with `O_APPEND` in the parent).

### Pattern 4: Failure mapping (RT-08)
Every child fault → `Ok(ToolOutput{is_error:true, metadata:{status:"failed", error, stderr_tail}})`. Change the current `serde_json::from_str(...).map_err(ToolError::Execution)` and spawn `?` to in-band outputs. Use `ExitStatusExt::signal()` to name signals. Wrap in `tokio::time::timeout` (configurable `[subagent].timeout_secs`). Cap stdout/stderr buffers (e.g. last 64 KiB of stderr) — a runaway child must not OOM the parent.

### Pattern 5: Limits inside the loop (RT-06)
Replace `const MAX_ITERATIONS: u32 = 50` with `self.max_turns` (default 50 for main agent to keep behaviour). After usage accumulation (loop_.rs:1391), if `input+output >= token_budget` stop with a `StopReason::LimitReached{limit:"token_budget"}`. Print mode then runs the report step (no extra turns when limited) and emits `status:"limit_reached","limit":...` in the envelope.

### Pattern 6: Stale-write guard (ISO-03)
Per-process `FileStateTracker { map: Mutex<HashMap<PathBuf,(SystemTime,u64,u64 len)>> }` held in `ToolContext` (widen struct; update all constructors). Read records canonical path → (mtime, len, hash of full bytes — hash the whole file, not the truncated page returned). Edit/write: if tracked, re-stat + re-hash; mismatch → error "file changed since you read it — re-read first". After own successful write, update the entry. Atomic write: temp file in same dir (`.<name>.nanopi-<pid>-<rand>.tmp`), write, fsync, preserve original permissions, `rename`. Keep existing symlink/hard-link refusals: the `nlink>1` check and `O_NOFOLLOW` semantics must be re-expressed (check target with `symlink_metadata` before rename; rename replaces a symlink rather than following it — behaviour change, keep refusing symlinks explicitly so the existing write.rs tests stay green).

### Anti-Patterns to Avoid
- Repeating in-process execution (rolled back). Any shared state with children must be files/argv/env.
- Using existing `--session SESSION_ID` (it is an id lookup under `~/.nanopi/sessions`, conflicts with `--continue`/`--fork`) for the agent-dir path. Add a distinct `--session-file <path>` (create if missing, resume if exists — this satisfies D-12's resume flag).
- Keeping `--no-session` for children (violates RT-04) or letting children call `set_active_session` (would steal the cwd's pointer — print.rs:126).
- Passing the api key on argv (visible in `ps`). Pass via env (e.g. `NANOPI_API_KEY`) or inherit config; check how main.rs resolves `--api-key` env fallback.
- Letting the parent's terminal SIGINT reach children: `process_group(0)` already prevents it — intended (RT-03).

## Don't Hand-Roll

| Problem | Don't Build | Use Instead |
|---------|-------------|-------------|
| Concurrency queue | custom wait list | `tokio::sync::Semaphore` (owned permits held by the child future) |
| Group kill | walking /proc for descendants | `process_group(0)` + `libc::killpg` |
| Timeouts | manual timers | `tokio::time::timeout` |
| Tool allowlist | new filter | `ToolRegistry::standard_with_allowlist` (tool/mod.rs, already rejects unknown names) |
| Steering | new IPC | existing `SteerMessage` + `run_turn` steer_rx |
| Unique ids | random strings | `uuid::Uuid::now_v7()` |

## Common Pitfalls

1. **Orphaned grandchildren** — `kill_on_drop` kills only the leader. Use groups (Pattern 1). Test: child runs `bash -c 'sleep 300 & echo $! > pidfile; sleep 300'`, cancel parent future, assert pid gone.
2. **Abort skips Drop** — `panic="abort"` means guards don't run on a parent panic; PDEATHSIG covers direct children (Pattern 2).
3. **Pipe deadlock / OOM** — must drain stdout and stderr concurrently (already done) and bound buffers.
4. **Recursion via tool list** — orchestrator could include `subagent` in `--tools`; child must strip it unconditionally when running as an agent, not rely on the parent.
5. **WASM tools in allowlist** — `standard_with_allowlist` validates builtin names only; plugin tool names must be allowed through and other plugins skipped (D-14). Check how `plugin_tools.rs` registers and that unknown-name errors don't reject plugin names.
6. **Torn amendment reads** — require stable size across polls; parent appends with a single `write_all` on an `O_APPEND` handle.
7. **mtime granularity** — coarse mtime (1s on some FS) can miss a same-second change; that's why D-13 also hashes. Compare hash always when tracked; mtime is just a fast path.
8. **Hash of truncated read** — read.rs caps output (apply_default_cap); hash the full on-disk bytes, else every later edit is falsely "changed".
9. **Atomic rename vs existing safety tests** — write.rs has symlink/hard-link tests (lines 240-371); keep them green.
10. **Hook `agent_id`** — HookInput must include it only when env present, keep payload byte-identical for the main agent (hook.rs comments stress byte-identity with PI).
11. **Self-check loop** — cap at 2 extra turns hard; write report.md even on limit/failure paths inside the child (best effort).
12. **Provider inheritance** — parent's resolved provider/base_url/api-kind must be passed explicitly; old runtime omitted them (`aa0b846` fixed a similar empty-api_key bug in the in-process version).

## Code Examples
See Patterns 1-6. Existing anchors: `run_single` subagent.rs:498, `spawn_and_collect` :554, `JsonEnvelope` print.rs:27, `run_turn` call print.rs (~line 217, passes `None, None`), `MAX_ITERATIONS` loop_.rs:1059, `ToolContext` tool/mod.rs:309, `write_no_follow` write.rs:91, `--tools` main.rs:143, `--no-session` main.rs:153.

## State of the Art
| Old | Current | Impact |
|-----|---------|--------|
| In-process subagents (01-01..01-06, reverted) | Child processes | Reuse tests/config shapes only |
| Ephemeral `--no-session` children | `--session-file` in agent dir | RT-04, enables CTL-06 |

## Assumptions Log
| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | pid/pgid reuse window after reaping is negligible | Pattern 1 | Wrong process group killed (very low) |
| A2 | Grandchildren of a SIGKILLed child on parent abort are acceptable residual risk | Pattern 2 | Orphans in crash case — needs owner OK |
| A3 | Polling at ~500 ms is adequate amendment latency | Pattern 3 | Slower steering only |
| A4 | API key should travel via env not argv | Anti-patterns | Key exposure in `ps` |

## Open Questions (RESOLVED)
1. Timeout default for a child — RESOLVED: `[subagent].timeout_secs = 1800` (implemented in 01-03, enforced in 01-05).
2. Child envelope contents — RESOLVED: envelope carries `status` + `report_path` (+ `limit`, `agent_id`); parent reads report.md (01-04, consumed in 01-05).
3. Trust in child — RESOLVED: parent passes its resolved trust explicitly as `--approve` / `--distrust`; child never prompts (01-04 contract, 01-05 argv).
4. API key transport — RESOLVED: via `OPENAI_API_KEY` env, never argv; test asserts absence from argv (01-05).
5. Orphaned grandchildren after a hard nanopi crash — RESOLVED as accepted known gap: PR_SET_PDEATHSIG on the child (01-04) + killpg on all normal paths (01-03/01-05/01-07); documented in docs/subagents.md (01-07).

## Environment Availability
| Dependency | Available | Version |
|------------|-----------|---------|
| rustc / cargo | yes | 1.98.1 |
| Linux prctl/killpg | yes | kernel 7.0 |
Nothing missing.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | cargo test (built-in) + tokio::test; e2e via fake OpenAI endpoint in `tests/print_mode_e2e.rs` |
| Quick run | `cargo test --lib subagent file_state brief` |
| Full suite | `cargo test` |

### Phase Requirements → Test Map
| Req | Behavior | Type | Command | Exists? |
|-----|----------|------|---------|---------|
| RT-01/08 | child exits non-zero / killed by signal / garbage stdout / timeout → failed ToolOutput, test process alive | unit (spawn `sh -c` stand-ins via injectable command) | `cargo test --lib subagent::tests::failure_` | ❌ Wave 0 |
| RT-02 | cancel future → group (incl. background sleep) dead; PDEATHSIG child dies when parent killed | integration | `cargo test --test subagent_runtime kill_` | ❌ |
| RT-04 | transcript.jsonl in agent dir, parent session unchanged, active pointer untouched | e2e fake endpoint | `cargo test --test print_mode_e2e session_file` | ❌ |
| RT-05 | child registry lacks `subagent` even if listed; 9th live dispatch errors; 5th waits | unit | `cargo test --lib subagent_registry` | ❌ |
| RT-06 | `--max-turns 2` / `--token-budget 10` → `status:limit_reached`, report.md written | e2e fake endpoint | `cargo test --test print_mode_e2e limit_` | ❌ |
| RT-07 | out-of-list tool call denied in-band, no prompt (stdin null doesn't hang) | e2e | `cargo test --test print_mode_e2e tools_allowlist` | partial |
| RT-09 | amendment appended mid-run appears as steering user message before next LLM call; self-check ≤2 extra turns; checklist in report.md | e2e (fake endpoint scripted to delay) + unit amendment parser | `cargo test brief` | ❌ |
| ISO-03 | read→external modify→edit refused; never-read write allowed; two processes; atomic write leaves no temp; symlink tests still pass | unit + integration | `cargo test --lib file_state write edit` | ❌ (port from superseded 01-02) |
| RT-03 | no new keybindings/prompts | review/grep | `grep -n subagent src/keys.rs` empty | n/a |

### Sampling Rate
Per task: quick run; per wave: `cargo test`; phase gate: `cargo test && cargo clippy -- -D warnings` plus `cargo build --release` (panic=abort path).

### Wave 0 Gaps
- `tests/subagent_runtime.rs` (process-group / pdeathsig tests)
- fake-endpoint helper extension in `print_mode_e2e.rs` for scripted multi-turn + delay
- injectable child command in `subagent.rs` for failure-mode unit tests

## Security Domain
| ASVS | Applies | Control |
|------|---------|---------|
| V4 Access control | yes | tool allowlist enforced in child; recursion strip |
| V5 Input validation | yes | brief/amendment parsing; never panic on child output; bounded buffers |
| V6 Crypto | no | DefaultHasher is change-detection, not security |
| V8 Data protection | yes | api key via env not argv; brief/prompt files 0o600 |

Threats: secret leak via `ps` (env passing); TOCTOU symlink swap on atomic write (keep refusals); resource exhaustion (caps, timeouts, buffer limits); prompt-injected repo agents escalating tools (existing scope check in `resolve_agent`, subagent.rs:425).

## Project Constraints (from CLAUDE.md)
No project CLAUDE.md. Global: container in China, proxy for network — irrelevant (no installs).

## Sources
- Codebase: src/tool/subagent.rs, src/mode/print.rs, src/main.rs, src/agent/loop_.rs, src/event.rs, src/tool/mod.rs, src/tool/write.rs, Cargo.toml/Cargo.lock [VERIFIED]
- git log 28fe37d..2bd0343, superseded-inprocess/ plans [VERIFIED]
- tokio `Command::process_group` (unix, tokio ≥1.24) [ASSUMED from training; tokio 1.53 present]
- Linux `prctl(2)` PR_SET_PDEATHSIG, `killpg(2)` [ASSUMED — standard POSIX/Linux]

## Metadata
Stack HIGH; architecture HIGH; pitfalls MEDIUM-HIGH. Valid until 2026-11-03.

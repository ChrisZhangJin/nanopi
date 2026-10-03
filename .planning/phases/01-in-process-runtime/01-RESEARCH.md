# Phase 1: In-process runtime - Research

**Researched:** 2026-10-03
**Domain:** Rust async runtime — replacing child-process subagents with
in-process `tokio::spawn` tasks inside an existing agent loop (nanopi)
**Confidence:** HIGH

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

#### Runtime shape
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

#### Cancellation
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

#### Limits (configurable, under `[subagent]` in config.toml)
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

#### Failure isolation
- **D-11:** Keep `panic = "abort"`. Instead, audit the subagent path
  for `unwrap()`, `expect()` and indexing, and convert them to errors.
  Provider and tool errors end the agent as `failed`, carrying the
  error text. nanopi keeps running.

#### Transcripts
- **D-12:** Each subagent writes its own session JSONL to
  `.nanopi/agents/<run>/<id>/transcript.jsonl`. Phase 2 adds the brief
  and report next to it. The parent session records only the tool call
  and the tool result.

#### Permissions (interim until Phase 5)
- **D-13:** A subagent permission request is put on a queue and the
  subagent waits for the answer. Until Phase 5 the TUI answers through
  a simple inline confirm prompt labelled `[a3] wants to run: …`. It
  must not get mixed up with the main agent's own prompt; show one at a
  time, first in, first out.
- **D-14:** In print mode (`-p`) there is no way to ask, so queued
  requests follow the existing non-interactive rule. Today that is
  deny; check it.

#### Shared working tree
- **D-15 (ISO-03):** Each agent records the mtime and content hash of a
  file when it reads it. An edit or write is refused with an in-band
  error ("file changed since you read it — re-read first") when the
  current hash differs. Extend the existing `mutation_key` locking so
  it serializes writes to the same path across agents.

#### Hooks and extensions
- **D-16:** `PreToolUse` and `PostToolUse` hooks run for subagent tool
  calls too, so security hooks still apply. Session and turn lifecycle
  hooks do not run for subagents in this milestone. The hook payload
  gets an `agent_id` field.
- **D-17:** WASM extensions: research whether one loaded instance can
  serve concurrent agents. If it can't, subagents get no WASM tools in
  this milestone; do not instantiate them once per agent.

### Claude's Discretion
- Internal registry data structures, event enum shape, and test layout.

### Deferred Ideas (OUT OF SCOPE)
- The archive (`brief.md`/`report.md`), dynamic dispatch, background
  launch, the agents strip and orchestrator mode belong to later
  phases (2-6).
- Shared 429 / backoff coordination across concurrent agents — deferred
  unless this phase's research finds it necessary (it did not; see
  Pitfall table — not flagged as required for Phase 1).
- Automatic recovery or re-run of interrupted agents from the archive.
- Stopping or messaging a subagent directly from an expanded panel,
  nested teams/swarms, orchestrator with write/edit/bash, orchestrator
  mode as default, forking the parent's full context into subagents by
  default, the child-process fallback, ratatui 0.30 upgrade — all out
  of scope per `.planning/REQUIREMENTS.md`'s Out of Scope table.
</user_constraints>


## Summary

nanopi already has nearly every primitive this phase needs: `Agent::build_fresh`
builds a fully independent agent (own provider, own context, own tool
registry); `run_turn` already accepts an `Option<CancellationToken>` and an
`Option<mpsc::Receiver<SteerMessage>>`, so cancellation and mid-turn
messaging need no changes to the turn loop itself. `tokio-util`'s
`CancellationToken` (parent/child tree via `child_token()`) and `tokio::sync::Semaphore`
are both already dependencies, used elsewhere in the codebase
(`src/mode/tui.rs`, `src/render/spinner.rs`, `src/tool/subagent.rs`). No new
crates are required, which satisfies QA-02's binary-size constraint
(deferred to Phase 6 but worth protecting now).

The current `src/tool/subagent.rs` spawns a whole second `nanopi` process
per subagent (`Command::new(nanopi_invocation())...kill_on_drop(true)`) and
parses its `-p --output json` envelope back. This phase deletes that
mechanism (`run_single`, `spawn_and_collect`, the tempfile prompt staging,
the child-process module) and replaces it with: a `SubagentRegistry` shared
through `ToolContext`, each subagent built via `Agent::build_fresh` and run
with `tokio::spawn`, a cancel-token tree (foreground = child of the main
turn's token; background = child of a registry root token), a global
concurrency semaphore, per-agent turn/token limits, a deny-list that blocks
subagents from registering the `subagent` tool (depth cap of 1), per-agent
session transcripts, and a cross-agent stale-write guard (ISO-03) that
extends the existing `mutation_key` concept from intra-batch to
cross-agent.

**Primary recommendation:** Do not reinvent cancellation, steering, or the
turn loop — thread the existing `CancellationToken`/`SteerMessage` plumbing
into a new `SubagentRegistry`, and make the subagent tool a thin dispatcher
that builds fresh `Agent`s and spawns them, mirroring Claude Code's
in-process `AgentTool` shape referenced in CONTEXT.md.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Subagent lifecycle (spawn/cancel/track) | Backend (agent runtime) | — | `SubagentRegistry` lives in-process, shared via `ToolContext`; no network/UI tier involved |
| Cancellation (Esc / stop-all) | Backend (agent runtime) | TUI (key dispatch) | Token tree lives in registry; TUI only triggers cancel on keypress |
| Turn/token limits | Backend (agent runtime) | Config (`config.toml`) | Enforced inside the spawned agent's own turn loop (`run_turn` loop bound + usage check) |
| Tool deny-list / depth cap | Backend (tool registry construction) | — | Enforced when building the subagent's `ToolRegistry`, not via prompt instructions |
| Transcript isolation | Backend (session writer) | Filesystem (`.nanopi/agents/<run>/<id>/`) | Each subagent gets its own `session_path`; parent session untouched except tool_call/tool_result |
| Shared file-state guard (ISO-03) | Backend (tool execution: `write`/`edit`) | — | Extends `mutation_key` infra already in `src/tool/mod.rs` |
| Permission queueing (interim) | Backend (permission gate) | TUI (inline confirm) | Queue lives in registry/context; TUI renders one prompt at a time |
| WASM extension availability | Backend (`src/wasm/loader.rs`) | — | `ComponentBridge` is `Mutex`-serialized around one `Store`; concurrent agents cannot safely share it (see Pitfall 6) |

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| RT-01 | Subagents run as in-process tasks; child-process runtime removed | §Standard Stack, §Architecture Patterns — `Agent::build_fresh` + `tokio::spawn` replaces `Command::new(nanopi_invocation())` |
| RT-02 | Each subagent has its own cancel token; foreground stops on Esc | §Code Examples (cancel tree), D-05 — `child_token()` of main turn's token |
| RT-03 | User can stop all running subagents with one shortcut | §Code Examples (registry root token), D-06 — Ctrl+X confirmed free in `src/keys.rs` |
| RT-04 | Each subagent has its own transcript; nothing leaks into parent | §Architecture Patterns — separate `session_path` per agent, D-12 |
| RT-05 | Subagents cannot spawn subagents; global cap on live agents | §Don't Hand-Roll, §Common Pitfalls #3 — deny-list + semaphore + `max_live` |
| RT-06 | Turn limit + token budget, configurable, partial report on limit | §Code Examples (limit check), D-08/D-09 |
| RT-07 | Background permission request queued, not interrupting main convo | §Common Pitfalls #7, D-13 — interim inline confirm queue |
| RT-08 | Subagent failure/provider error never crashes nanopi | §Common Pitfalls #1/#13, D-11 — panic audit + `JoinHandle` error mapping |
| ISO-03 | Edit refused if file changed since this agent read it | §Code Examples (stale-read guard), D-15 — extends `mutation_key` |
</phase_requirements>

## Standard Stack

### Core
| Library | Version (locked) | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `tokio` | 1.40 (Cargo.lock: 1.53 per research summary) | `tokio::spawn`, `Semaphore`, `mpsc` | Already the async runtime; subagents are just more tasks on it |
| `tokio-util` | 0.7 | `CancellationToken` (parent/child tree via `child_token()`) | Already used for turn cancellation (`src/mode/tui.rs:53`, `src/render/spinner.rs:19`) |
| `uuid` | 1.10 (`v7`, `std` features) | Run-id generation for `.nanopi/agents/<run>/` dirs | Already a dependency (`crate::util::uuid::v7()`), time-ordered |
| `async-trait` | 0.1 | `Tool`/`Provider` trait objects | Already used throughout `src/tool/`, `src/agent/loop_.rs` |
| `serde`/`serde_json` | 1.0 | Config (`[subagent]` section), tool args | Already standard throughout |

**Version verification:** [VERIFIED: codebase] — confirmed by reading `Cargo.toml` directly; all four core crates are already declared dependencies at `Cargo.toml:39-95`. No `npm view`/`cargo search` needed — this is an existing Rust workspace, not a new install.

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `tokio::sync::Semaphore` (part of `tokio`) | 1.40 | Global `max_concurrency` gate | Already used identically in current `src/tool/subagent.rs:451` (`Semaphore::new(MAX_CONCURRENCY)`) — same pattern, just promoted to a registry-owned, cross-call semaphore instead of one scoped to a single `parallel` call |
| `std::sync::Mutex` or `tokio::sync::Mutex` | stdlib/tokio | `SubagentRegistry` internal map | `std::sync::Mutex` is fine if no `.await` is held across the lock (per SUMMARY.md: "a `std::sync::Mutex` registry snapshot"); use `tokio::sync::Mutex` only where an async operation must hold the lock |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Hand-rolled registry + semaphore | `flume`, `crossbeam`, `dashmap`, an actor framework | Explicitly rejected in `.planning/research/SUMMARY.md` — binary size budget (QA-02) and "no new crates" are explicit project constraints; std/tokio primitives are sufficient at this scale (≤8 live agents) |
| `tokio::spawn` per-agent tasks | A dedicated worker-pool crate | Unnecessary — `tokio`'s multi-thread runtime already handles scheduling; a pool adds indirection with no benefit at this concurrency |

**Installation:**
```bash
# No new dependencies. All required crates are already in Cargo.toml.
```

## Package Legitimacy Audit

**Not applicable.** This phase adds zero new external packages — every
primitive it needs (`CancellationToken`, `Semaphore`, `uuid::v7`,
`async_trait`) is an existing, already-used dependency in `Cargo.toml`.
slopcheck was not run because there is nothing to check; this is a decision
the planner can treat as closed, not deferred.

| Package | Registry | Age | Downloads | Source Repo | slopcheck | Disposition |
|---------|----------|-----|-----------|--------------|-----------|-------------|
| (none — no new packages) | — | — | — | — | — | N/A |

**Packages removed due to slopcheck [SLOP] verdict:** none
**Packages flagged as suspicious [SUS]:** none

## Architecture Patterns

### System Architecture Diagram

```
 Parent turn (TUI or -p)
   │
   │  model emits tool_call: subagent({agent, task}, mode=single/parallel/chain)
   ▼
 SubagentTool::execute(args, ctx: &ToolContext)
   │
   │  ctx.registry: Arc<SubagentRegistry>  (D-01, shared via ToolContext)
   ▼
 SubagentRegistry::dispatch(spec)
   │
   ├─ acquire global semaphore permit (max_concurrency, D-08)        ──► if max_live exceeded: in-band error, no spawn
   │
   ├─ allocate id "a{N}" (D-03), run-dir uuid v7
   │
   ├─ build cancel token:
   │     foreground → main_turn_token.child_token()   (Esc propagates, D-05)
   │     background → registry_root_token.child_token() (Esc does NOT propagate)
   │
   ├─ Agent::build_fresh(AgentBuildInputs { ..., registry: deny-listed ToolRegistry, provider: parent's provider/model (D-04) })
   │
   ├─ tokio::spawn(async move {
   │       session write → .nanopi/agents/<run>/<id>/transcript.jsonl (D-12, RT-04)
   │       a.run_turn(task, tx, Some(cancel_token), Some(steer_rx))
   │         │  existing turn loop: already checks cancel at iteration
   │         │  boundaries (loop_.rs:1080-1130); NEW: turn-count / token
   │         │  budget check added here → status: limit_reached (D-09)
   │         │  tool calls go through the SAME execute_tool path, so
   │         │  PreToolUse/PostToolUse hooks fire with agent_id (D-16)
   │         └─ on panic-free error: AgentError → status: failed, error text (D-11, RT-08)
   │       registry.mark_done(id, result)
   │   })
   │
   └─ registry stores JoinHandle + CancellationToken + state + start_time + usage
         (TUI reads a snapshot later, Phase 5 — not built here)

 Stop-all (Ctrl+X, D-06) → registry_root_token.cancel()
 Esc (existing binding)   → main_turn_token.cancel() (cascades only to foreground children)

 Shared working tree (ISO-03):
   write/edit tool execute() ──► check (mtime, hash) recorded at last read
                              ──► mismatch → ToolError "file changed since read — re-read first"
                              ──► extend mutation_key (src/tool/mod.rs:227) to a
                                  cross-agent lock keyed on canonical path,
                                  not just intra-batch grouping
```

### Recommended Project Structure
```
src/
├── agent/
│   ├── build.rs          # unchanged entry point: Agent::build_fresh (already supports this)
│   └── subagent/         # NEW module (or keep flat as agent/subagent_registry.rs)
│       ├── registry.rs   # SubagentRegistry, AgentHandle, AgentState enum
│       └── limits.rs     # turn/token budget checks, config defaults
├── tool/
│   ├── mod.rs             # ToolContext gains `registry: Arc<SubagentRegistry>` (D-01)
│   └── subagent.rs        # REWRITTEN: dispatch-only, no Command::new, no JSON envelope parsing
├── config.rs              # NEW [subagent] section: max_concurrency, max_live, max_turns, token_budget
└── keys.rs                # NEW ActionId::StopAllSubagents, default Ctrl+X (D-06)
```

### Pattern 1: Cancel-token tree (parent/child via `child_token()`)
**What:** `tokio_util::sync::CancellationToken` supports `child_token()` — cancelling a parent cancels all children, but cancelling a child does not affect the parent or siblings.
**When to use:** Foreground subagents get `main_turn_token.child_token()`; background subagents get `registry_root_token.child_token()`. This directly implements D-05/D-06 with zero new types.
**Example:**
```rust
// Source: tokio-util docs (CancellationToken::child_token), already used
// in nanopi at src/mode/tui.rs:3431 for the main turn token.
let main_turn_token = CancellationToken::new();
let registry_root_token = CancellationToken::new(); // owned by SubagentRegistry, lives for process lifetime

// Foreground dispatch: Esc (which cancels main_turn_token) cascades here.
let fg_cancel = main_turn_token.child_token();

// Background dispatch: Esc does NOT cascade; only Ctrl+X does.
let bg_cancel = registry_root_token.child_token();

// Stop-all shortcut:
registry_root_token.cancel(); // cancels every background/foreground-under-registry child
```

### Pattern 2: In-process agent spawn (replaces `Command::new`)
**What:** Build a fully independent `Agent` with `Agent::build_fresh`, give it its own `Provider` instance (D-04: same provider/model as parent unless overridden), then `tokio::spawn` a task that calls `run_turn`.
**When to use:** Every subagent dispatch, single/parallel/chain alike — chain and parallel just become multiple spawns coordinated by `join_all`/`JoinSet` instead of multiple child processes.
**Example:**
```rust
// Source: src/agent/build.rs:321 (Agent::build_fresh), already used for
// the main agent at process startup. This phase reuses it verbatim for
// subagents rather than inventing a second builder.
let (agent, _diags) = Agent::build_fresh(AgentBuildInputs {
    cwd: run_cwd,
    registry: build_subagent_tool_registry(&parent_registry), // deny-list applied (D-10)
    provider: parent.clone_provider_for_subagent(), // same model/base_url/api_key (D-04)
    session_path: agents_dir.join(&run_id).join(&agent_id).join("transcript.jsonl"), // D-12
    session_id: format!("{run_id}-{agent_id}"),
    permission: parent.permission.clone(), // inherits trust; cannot widen (Pitfall 8)
    hooks: parent.hooks.clone(), // PreToolUse/PostToolUse fire; session/turn lifecycle do not (D-16)
    model: parent.model.clone(),
    base_url: parent.base_url.clone(),
    api_key: parent.api_key.clone(),
    skill_load: SkillLoad::None, // or inherited — Claude's discretion per CONTEXT.md
    no_context_files: true, // brief is the interface, not the parent's context (per REQUIREMENTS.md "Out of Scope")
    prompt_overrides: ad_hoc_role_or_agent_file_prompt,
    initial_follow_up: None,
    tool_exec_mode: parent.tool_exec_mode,
    tool_exec_overrides: parent.tool_exec_overrides.clone(),
    extensions: ExtensionsConfig::none(), // D-17: no WASM tools for subagents this milestone
});

let handle = tokio::spawn(async move {
    let (tx, _rx) = mpsc::channel(64); // subagent events; registry can forward a subset
    let result = agent.run_turn(&task, &tx, Some(fg_or_bg_cancel), Some(steer_rx)).await;
    result // Result<String, AgentError> — AgentError never panics the process (D-11)
});
```

### Pattern 3: Stale-write guard across agents (ISO-03)
**What:** Extend the existing `mutation_key` (`src/tool/mod.rs:227`) from "serialize writes within one tool batch" to "detect a write/edit against a file another agent changed since this agent last read it."
**When to use:** Every `read` on a file records `(path, mtime, content_hash)` in a structure reachable from `ToolContext` (per-agent read-cache, but checked against a *shared* registry of last-known-good state so cross-agent changes are visible). Every `write`/`edit` recomputes the current `(mtime, hash)` and compares against what was recorded at read time.
**Example:**
```rust
// Source: existing mutation_key pattern at src/tool/mod.rs:227, extended.
// NEW: a process-wide `Arc<Mutex<HashMap<PathBuf, FileReadState>>>` shared
// through ToolContext (same tier as mutation_key's grouping, not a new
// subsystem). FileReadState { mtime: SystemTime, hash: u64 }.

fn check_stale(path: &Path, recorded: &FileReadState) -> Result<(), ToolError> {
    let current = read_file_state(path)?; // mtime + cheap hash (e.g. content hash of bytes about to be replaced)
    if current.mtime != recorded.mtime || current.hash != recorded.hash {
        return Err(ToolError::Execution(
            "file changed since you read it — re-read first".into(),
        ));
    }
    Ok(())
}
```
This is a NEW piece of state (not literally `mutation_key`, which only groups same-batch calls for serialization) but it reuses `mutation_key`'s canonicalization logic (`resolve_in_cwd` + `canonicalize`) so the same path always maps to the same key regardless of spelling or symlink alias.

### Anti-Patterns to Avoid
- **Keeping the child-process path as a fallback:** CONTEXT.md D-02 and REQUIREMENTS.md's Out-of-Scope table explicitly forbid this ("Child-process subagent fallback — Owner chose to remove the old runtime"). Delete `run_single`/`spawn_and_collect` entirely; do not feature-gate them.
- **`std::env::set_current_dir` for per-agent cwd:** PITFALLS.md #13 — global process state (cwd) shared across concurrently-running agents corrupts every other agent's relative paths. Pass `cwd` explicitly into `Agent::build_fresh` / `ToolContext`, as the codebase already does.
- **Catching panics with `catch_unwind`:** Dead end — `panic = "abort"` (kept per D-11) makes `catch_unwind` a no-op; the real fix is auditing `unwrap()`/`expect()`/indexing on the subagent code path and converting to `Result`.
- **One WASM `Store` instance per concurrent agent, instantiated on demand:** Rejected by D-17 pending research showing it isn't safe — see Common Pitfalls #6 below.
- **Building a second steer/cancel plumbing path for subagents:** `run_turn` already takes `Option<CancellationToken>` and `Option<mpsc::Receiver<SteerMessage>>`; reuse exactly that signature for subagents instead of inventing parallel control types (Code Context note: "the loop needs no changes for stop or amend").

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|--------------|-----|
| Cancellation propagation tree | A custom `Arc<AtomicBool>` + polling scheme | `tokio_util::sync::CancellationToken::child_token()` | Already the codebase's own pattern (`src/mode/tui.rs`, `src/render/spinner.rs`); battle-tested, zero new code for tree semantics |
| Concurrency limiting | A hand-rolled counter + condvar | `tokio::sync::Semaphore` | Already used in the exact same role in current `src/tool/subagent.rs:451` |
| Unique, sortable run/agent ids | A custom counter + timestamp string | `uuid::v7()` (already a dependency, `crate::util::uuid`) | Time-ordered, no coordination needed across concurrent spawns |
| Stale-file detection | A full diff/merge algorithm | mtime + content hash compare (cheap, already partially present via `mutation_key`'s canonicalization) | ISO-03 only needs "did it change", not "what changed" |
| Turn/token budget enforcement | A separate token-counting service | Reuse `Usage` accumulation already computed per turn (`agent.usage_total`, `Usage` struct in `src/event.rs`) and compare against `token_budget` each iteration | The data already exists; this is a threshold check, not new instrumentation |

**Key insight:** Every "don't hand-roll" item in this phase is actually "don't hand-roll a SECOND TIME" — nanopi already solved cancellation trees, concurrency limiting, id generation, and usage tracking for the main agent loop. The work here is almost entirely wiring, not invention.

## Common Pitfalls

### Pitfall 1: Orphaned tasks after Esc (no more `kill_on_drop`)
**What goes wrong:** The current design's cancel-safety comes entirely from `kill_on_drop(true)` on the child `Command` — dropping the future SIGKILLs the OS process, which cleans up everything. A `tokio::spawn`ed task is NOT cancelled by dropping its `JoinHandle`; it keeps running, keeps burning provider tokens, and can keep writing files after Esc.
**Why it happens:** The mental model "cancel = drop the future" was true for child processes and silently stops being true in-process.
**How to avoid:** Never rely on dropping a `JoinHandle`. Every spawned subagent task must be registered in `SubagentRegistry` with its `CancellationToken`; stopping means (1) `token.cancel()`, (2) the agent loop observes it at the next turn/tool boundary (cooperative, per D-07 — never abort a tool call mid-write), (3) the registry awaits the `JoinHandle` (with a bounded timeout) before considering the agent fully stopped.
**Warning signs:** A test that cancels mid-turn and then asserts zero live tasks, zero outstanding provider requests, and no further file writes.

### Pitfall 2: `panic = "abort"` + any `unwrap()`/`expect()`/indexing on the subagent path takes down the whole process
**What goes wrong:** `catch_unwind` is useless under `panic = "abort"` (confirmed project setting, kept per D-11). A single subagent hitting an `unwrap()` on, say, a malformed provider response kills nanopi for the user and every other agent.
**Why it happens:** Code written assuming "this can't happen" in a single-agent process becomes "this can't happen, except now 4 agents increase the odds and the blast radius is the whole app."
**How to avoid:** Audit the entire subagent execution path (agent loop, provider adapters, tool execution reachable from a subagent) for `unwrap()`, `expect()`, and direct indexing (`v[i]`), converting each to a `Result`/`Option` with a `failed` status surfaced through the registry. This is explicit, required work (D-11), not optional hardening.
**Warning signs:** `grep -rn "\.unwrap()\|\.expect(\|\[0\]\|\[idx\]"` on the reachable subagent code paths (agent loop, provider, deny-listed tool set) — every hit is a candidate crash point now reachable by an adversarial or malformed model response running unattended in the background.

### Pitfall 3: Unbounded recursion / toolset escalation
**What goes wrong:** If a subagent somehow gets the `subagent` tool (or `send_message`/`stop`/`list`), it can dispatch its own subagents — exponential fan-out, token blowup, provider rate-limit storms, and eventually a fork-bomb-shaped crash.
**Why it happens:** Tool registries are built generically; forgetting to deny-list control tools for a nested registry is an easy omission, especially once dynamic/ad-hoc toolsets (Phase 3) make tool lists data rather than hardcoded.
**How to avoid:** Enforce the deny-list (D-10) in tool construction — i.e., `build_subagent_tool_registry()` never inserts `subagent`/`send_message`/`stop`/`list`, full stop, regardless of what's requested — not via a system-prompt instruction, which a model can ignore or be jailbroken past. Enforce `max_live` (D-08) as a hard cap at dispatch time, returning an in-band error rather than queuing indefinitely. Depth is implicitly 1 because subagents simply never have the tool to go deeper.
**Warning signs:** A test asserting the subagent's `ToolSpec` list never contains `subagent`/control-tool names, run against every construction path (single/parallel/chain/dynamic).

### Pitfall 4: Permission prompts from background/queued subagents interleave with or block the main TUI
**What goes wrong:** A subagent needs a permission decision (e.g., approving a tool) while the user is mid-conversation with the main agent. Two interleaved prompts from different agents, or a prompt that silently blocks the whole TUI event loop, are both bad outcomes.
**Why it happens:** nanopi's existing permission gate (`src/agent/permission.rs`) was designed for one agent, one terminal, one prompt at a time. Multiple concurrent agents break that single-prompt assumption.
**How to avoid:** D-13's queue: a subagent's permission request is pushed onto an ordered (FIFO) queue; the subagent task `.await`s the answer; the TUI renders exactly one prompt at a time, labelled `[a3] wants to run: …`, never overlapping the main agent's own prompt. D-14: in print mode (`-p`), there's no human to ask, so the existing non-interactive default (deny — verify this is actually the current behavior, see Open Questions) applies to queued requests too.
**Warning signs:** Two prompts rendered simultaneously; a subagent silently hanging forever because nothing ever answers its queued request in `-p` mode (must resolve to deny promptly, not hang).

### Pitfall 5: WASM extensions cannot safely serve concurrent subagents
**What goes wrong:** `src/wasm/loader.rs`'s `ComponentBridge` wraps a single `wasmtime::Store<PluginState>` behind `Mutex<BridgeInner>` (confirmed at `loader.rs:1536`, and STATE.md documents the existing "observe-only with `try_lock`-and-drop" discipline used for event delivery). A long-running subagent tool call that needs the lock while another agent also holds/wants it either serializes invisibly (defeating the point of parallel subagents) or, if a naive per-agent instantiation is attempted, multiplies memory/compile cost per concurrent agent.
**Why it happens:** The WASM host state model was built for "one component, one agent, occasional event dispatch" — not N concurrently-running long-lived tool callers.
**How to avoid:** Per D-17 (and SUMMARY.md's open question #3): **do not instantiate a `Store` per agent.** Ship this milestone with subagents receiving NO WASM-plugin tools at all (deny-listed alongside `subagent`/control tools). Revisit shared-instance safety in a later milestone if plugin tools for subagents become a real requirement.
**Warning signs:** None needed this phase if the deny-list is enforced — this pitfall is closed by scoping it out, not by solving the concurrency problem.

### Pitfall 6: Cross-agent file edits silently overwrite each other
**What goes wrong:** Two subagents (or a subagent and the main agent) both read `foo.rs`, both edit it; the second write wins, discarding the first agent's work with no error — exactly the class of bug already found and fixed for concurrent `bash` calls (STATE.md: "bash now runs sequentially by default... concurrent bash calls were silently losing updates").
**Why it happens:** `write`/`edit` validate against the file's CURRENT disk content for anchor-matching (edit's find/replace), but nothing today checks "has this changed since I, specifically, last read it" across agent boundaries — read-then-edit races are invisible to a single-agent design.
**How to avoid:** ISO-03/D-15: record `(mtime, content hash)` at read time; refuse the edit/write with a clear in-band error if the current state doesn't match. Extend `mutation_key`'s existing canonicalization (`resolve_in_cwd` + `canonicalize`) so the same file is recognized as the same key regardless of path spelling, and serialize writes to the same canonical path across agents (not just within one batch, which is all `mutation_key`'s current grouping does).
**Warning signs:** A test where agent A reads `foo.rs`, agent B writes `foo.rs`, then agent A attempts a write — must fail with the stale-read error, not silently succeed.

## Code Examples

Verified patterns from the existing codebase (these are not external library docs — they are nanopi's own established idioms that this phase must reuse rather than reinvent):

### Cancellation (existing, reused verbatim)
```rust
// Source: src/mode/tui.rs:3429-3440 — already how the main turn's
// cancel token is created and wired into run_turn. Subagents follow
// the identical call shape; see Pattern 2 above.
let ct = CancellationToken::new();
let ct_task = ct.clone();
let task = tokio::spawn(async move {
    let result = a.run_turn(&msg, &tx, Some(ct_task), Some(steer_rx)).await;
    result
});
```

### Turn-boundary cancel check (existing, in `run_turn`, unmodified)
```rust
// Source: src/agent/loop_.rs:1080-1130 — already checks cancellation
// at the top of every iteration. No change needed here for RT-02/RT-06;
// a turn-limit/token-budget check is a NEW condition added alongside
// the existing `if let Some(ct) = cancel.as_ref() { if ct.is_cancelled() ... }`.
for iteration_idx in 0..MAX_ITERATIONS {
    if let Some(ct) = cancel.as_ref() {
        if ct.is_cancelled() {
            self.drain_steer_to_follow_ups(&mut steer_rx, &mut follow_up_queue);
            return Ok(final_text);
        }
    }
    // NEW for subagents: if self.turn_count >= max_turns || self.usage_total.total() >= token_budget {
    //     return Ok(partial_report_with_status_limit_reached());
    // }
    ...
}
```

### Semaphore-bounded fan-out (existing pattern, promoted to registry scope)
```rust
// Source: src/tool/subagent.rs:446-472 (run_parallel) — the exact
// Semaphore-guard-per-future pattern already exists; this phase moves
// the Semaphore from a per-call Arc to a registry-owned, cross-call Arc
// so max_concurrency is a GLOBAL cap (RT-05), not one cap per `parallel`
// invocation.
let _permit = sem.acquire().await.expect("subagent semaphore is never closed");
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|---------------|--------|
| Spawn a child `nanopi -p --output json` process per subagent, parse its JSON envelope | Build an in-process `Agent` via `Agent::build_fresh`, run with `tokio::spawn` | This phase (Phase 1, v0.13.0) | Removes process-spawn latency and `--api-key`/`--base-url` inheritance gaps (D-04); removes `kill_on_drop` as the cancellation mechanism, requiring an explicit token tree instead |
| Cancellation via OS process death (`kill_on_drop`) | Cooperative `CancellationToken` tree, checked at turn/tool boundaries | This phase | Stop is no longer "free"; must be designed and tested explicitly (Pitfall 1) |
| One subagent concurrency cap per `parallel` tool call (local `Semaphore`) | One global `max_concurrency` across all live agents regardless of mode/call | This phase | Prevents a model issuing multiple `subagent` calls in sequence from exceeding the intended resource ceiling |

**Deprecated/outdated:**
- Child-process subagent runtime (`run_single`, `spawn_and_collect`, tempfile prompt staging via `write_prompt_tempfile`): removed entirely this phase, no fallback, per explicit owner decision (D-02, REQUIREMENTS.md Out-of-Scope table).

## Assumptions Log

> Per the provenance rule, anything not directly confirmed by reading
> this repository's own source/config is logged here, even though the
> underlying tokio/tokio-util APIs (`CancellationToken::child_token`,
> `Semaphore`) are standard, long-stable library behavior.

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | `tokio-util`'s `CancellationToken::child_token()` semantics (parent cancel cascades to children; child cancel does not propagate up or to siblings) match the exact behavior needed for D-05/D-06 | Architecture Patterns, Pattern 1 | If the actual propagation semantics differ (e.g., a child cancelling also marks itself as a "cancelled" node visible to `is_cancelled()` queries on the parent, which it does not in reality), the stop-all/Esc distinction could be subtly wrong. This is a well-documented, stable tokio-util API; risk is LOW but unverified against the current crate docs in this session (no Context7/web lookup was performed — training-data knowledge only). |
| A2 | "Non-interactive default is deny" (D-14's premise) accurately describes nanopi's CURRENT `-p` permission behavior | Common Pitfalls #4 | If `-p` mode currently defaults to allow (or errors) rather than deny for unanswerable permission prompts, D-14's "check it" instruction surfaces a real discrepancy the planner must resolve — this needs a direct read of the current `-p`/non-interactive permission code path, which was not located precisely in this research pass. |
| A3 | A cheap content hash (not a full diff) is sufficient for the ISO-03 stale-check without false negatives in practice | Pattern 3, Pitfall 6 | Low risk — any collision-resistant hash (even a fast non-cryptographic one) over file bytes is adequate for "did this change", and mtime alone is a reasonable first-pass signal already used elsewhere in the ecosystem. |

## Open Questions

1. **Exact current `-p` (print mode) behavior for an unanswerable permission prompt**
   - What we know: STATE.md and D-14 both reference "the existing non-interactive rule. Today that is deny; check it."
   - What's unclear: The precise code path (`src/agent/permission.rs` was read for its `TrustLevel`/hook-enable logic, not its print-mode tool-approval behavior) that currently governs this for the MAIN agent, which the subagent path must mirror.
   - Recommendation: Planner's first task in this area should be a direct `grep`/read of the print-mode permission-decision code before writing the queued-request design, to confirm D-14's premise holds.

2. **Exact shape of `ToolContext` after adding the registry**
   - What we know: D-01 specifies `ToolContext` gains the registry; today `ToolContext` is `{ cwd: PathBuf }` only (`src/tool/mod.rs:306-309`).
   - What's unclear: Whether `registry` should be `Option<Arc<SubagentRegistry>>` (so non-agent tool contexts, e.g. in tests, don't need one) or always-present with a no-op default.
   - Recommendation: Favor `Arc<SubagentRegistry>` always-present with a trivially-constructible default (`SubagentRegistry::new_standalone()`), to avoid an `Option` unwrap creeping into every tool that touches it — consistent with the project's own stated aversion to `unwrap()` risk (Pitfall 2).

3. **Content-hash algorithm for ISO-03**
   - What we know: Any fast hash suffices functionally.
   - What's unclear: Whether the codebase has a preferred hash already in the dependency tree (e.g., something pulled in transitively) versus needing `std::hash::Hasher`/a simple CRC, to avoid adding a new crate under the QA-02-style "no unjustified new crates" spirit (QA-02 itself is Phase 6-scoped, but the principle likely applies here too).
   - Recommendation: Use `std::collections::hash_map::DefaultHasher` (SipHash, already in std, zero new dependency) rather than reaching for `sha2`/`blake3` — collision resistance against malicious input is not the threat model here (it's "did another agent change this file", not "verify integrity against tampering").

## Environment Availability

Skipped — this phase is a pure in-process Rust code change with zero new
external tool/service/runtime dependencies. All required libraries are
already vendored in `Cargo.lock`; `cargo build`/`cargo test` are the only
tools involved, and their presence is a given for a Rust project already
building successfully (confirmed by `STATE.md`'s test-count reporting).

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness (`cargo test`), `#[tokio::test]` for async cases — already used throughout `src/tool/subagent.rs`'s existing test module |
| Config file | none — standard `cargo test`; project convention is `-- --test-threads=1` for the full suite per STATE.md (shared `TempNanopiHome`/`test_lock()` guard against env races) |
| Quick run command | `cargo test --lib subagent::` (or the new module's path, e.g. `cargo test --lib agent::subagent_registry::`) |
| Full suite command | `cargo test --features wasm -- --test-threads=1` (844 lib tests as of STATE.md) and `cargo test -- --test-threads=1` (724 default) |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|---------------------|--------------|
| RT-01 | No child `nanopi` process spawned; old functions gone | unit + compile-time | `cargo build` (absence of `Command::new` in subagent path) + `grep -c "spawn_and_collect\|run_single" src/tool/subagent.rs` returns 0 | ❌ Wave 0 — write a test asserting no child pid via `/proc` count delta, or rely on code-removal + existing process-count assertions pattern |
| RT-02 | Esc stops a foreground subagent; background unaffected | integration | `cargo test --lib subagent_registry::foreground_cancel_on_esc_stops_task` | ❌ Wave 0 |
| RT-03 | One shortcut stops all running subagents | integration | `cargo test --lib subagent_registry::stop_all_cancels_every_background_agent` + a `keys.rs` binding test confirming Ctrl+X is free/mapped | ❌ Wave 0 |
| RT-04 | Separate transcript per subagent; parent session untouched | integration | `cargo test --lib subagent_registry::subagent_session_isolated_from_parent` | ❌ Wave 0 |
| RT-05 | Deny-list enforced; global cap enforced | unit | `cargo test --lib subagent::tool_registry_denies_control_tools` + `cargo test --lib subagent_registry::dispatch_beyond_max_live_errors` | ❌ Wave 0 |
| RT-06 | Turn limit / token budget → partial report, `status: limit_reached` | integration (fake provider) | `cargo test --lib subagent_registry::turn_limit_yields_partial_report` | ❌ Wave 0 |
| RT-07 | Background permission request queued, FIFO, one at a time | integration | `cargo test --lib permission::queued_subagent_permission_requests_are_fifo` | ❌ Wave 0 |
| RT-08 | Subagent error/panic-free failure never crashes nanopi | unit + fuzz-ish | `cargo test --lib subagent_registry::provider_error_marks_failed_not_crash` + the `unwrap()`/`expect()` audit itself (no automated test substitutes for the audit; track as a checklist item) | ❌ Wave 0 |
| ISO-03 | Stale write refused with in-band error | integration | `cargo test --lib tool::stale_write_refused_after_concurrent_change` | ❌ Wave 0 |

### Sampling Rate
- **Per task commit:** targeted module test (`cargo test --lib <module>::`)
- **Per wave merge:** `cargo test -- --test-threads=1` (default features); run `--features wasm` variant too since D-17 touches WASM-adjacent deny-list behavior
- **Phase gate:** Full suite green (both feature configurations) before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] `src/agent/subagent_registry.rs` (or wherever placed) — new module, needs its own test file/inline `#[cfg(test)] mod tests`
- [ ] A fake/mock `Provider` impl usable for turn-limit and error-path tests without real network calls (check whether `src/agent/loop_.rs`'s existing test module already has one — `loop_.rs:3502` etc. suggest it does; reuse it)
- [ ] A test helper for "assert no live tokio task remains after cancel" — likely needs a small registry-introspection method (`SubagentRegistry::live_count()`) exposed for tests
- [ ] Framework install: none — `cargo test` already configured

## Security Domain

> `security_enforcement` config key was not found in `.planning/config.json`
> (file not inspected directly in this pass — treat as enabled per default
> rule and include this section).

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-------------------|
| V2 Authentication | no | No new auth surface — subagents inherit the parent's already-authenticated provider credentials |
| V3 Session Management | yes | Each subagent's transcript is a distinct session file (`.nanopi/agents/<run>/<id>/transcript.jsonl`); must never be writable/readable cross-agent, and must never leak into the parent's `session.rs`-managed file (RT-04) |
| V4 Access Control | yes | Deny-list enforcement (D-10) is the access-control boundary: subagents must not reach `subagent`/control tools regardless of what a dynamic toolset request (Phase 3) asks for — enforce server-side (tool construction), never trust client-asserted (model-asserted) toolsets |
| V5 Input Validation | yes | Dynamic/ad-hoc agent specs (deferred to Phase 3, but the deny-list groundwork is this phase) must validate requested tool names against an allowlist before construction, as Phase 3 will build on top of this phase's `build_subagent_tool_registry()` |
| V6 Cryptography | no | No new cryptographic material; content hashing for ISO-03 is integrity-of-state detection, not a security control against a trust boundary (not a candidate for "never hand-roll crypto" — this is not crypto) |

### Known Threat Patterns for this stack

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|------------------------|
| Recursive subagent spawn (fork-bomb via tool escalation) | Denial of Service | Hard deny-list at tool-registry construction (D-10), not prompt-level; global `max_live` semaphore cap (D-08) |
| A subagent's panic/unwrap taking down the whole process | Denial of Service | `unwrap()`/`expect()`/indexing audit on the reachable subagent path (D-11); `AgentError` propagation instead of panics |
| Cross-agent file write race silently discarding another agent's work | Tampering (of a sort — data loss via race, not malice) | ISO-03 stale-read/write guard (D-15) |
| A model-controlled dynamic toolset request smuggling in the `subagent`/control tools (future Phase 3 surface, but the enforcement point is built here) | Elevation of Privilege | Deny-list checked server-side against the FINAL constructed registry, never against the raw request string |
| Background subagent permission request bypassing user review by auto-approving under load | Elevation of Privilege | D-13/D-14: queue + explicit FIFO single-prompt-at-a-time; `-p` mode defaults to deny (pending confirmation per Open Question #1), never silently allow |

## Project Constraints (from CLAUDE.md)

No project-local `./CLAUDE.md` exists in this repository (`/root/workspace/nanopi/CLAUDE.md` not found). The user's GLOBAL `~/.claude/CLAUDE.md` (runtime/environment/proxy instructions for the sandbox this agent runs in) contains no directives applicable to nanopi's own coding conventions, so there are no additional constraints to layer on top of CONTEXT.md's locked decisions.

## Sources

### Primary (HIGH confidence — direct repository inspection)
- `/root/workspace/nanopi/src/tool/subagent.rs` — current child-process implementation, read in full (869 lines)
- `/root/workspace/nanopi/src/tool/mod.rs` — `ToolContext`, `Tool` trait, `mutation_key` (lines 180-340, 952-1140)
- `/root/workspace/nanopi/src/agent/loop_.rs` — `Agent` struct, `run_turn` cancel/steer handling (lines 1-140, 1060-1140)
- `/root/workspace/nanopi/src/agent/build.rs` — `Agent::build_fresh` (lines 280-380)
- `/root/workspace/nanopi/src/agent/permission.rs` — `PermissionGate`, `TrustLevel` (lines 1-60)
- `/root/workspace/nanopi/src/mode/tui.rs` — `run_app` select loop, turn-spawn pattern (lines 1795-1850, 3400-3460)
- `/root/workspace/nanopi/src/keys.rs` — `ActionId`, `KeyBindings::default()` — confirms Ctrl+X is unbound (lines 1-200)
- `/root/workspace/nanopi/src/wasm/loader.rs` — `ComponentBridge`, `Mutex<BridgeInner>` around single `Store` (grep of Mutex/Store/instantiate usages)
- `/root/workspace/nanopi/Cargo.toml` — confirms `tokio-util`, `tokio::Semaphore`, `uuid` v7, `async-trait` already present
- `/root/workspace/nanopi/.planning/phases/01-in-process-runtime/01-CONTEXT.md` — locked decisions D-01..D-17
- `/root/workspace/nanopi/.planning/REQUIREMENTS.md` — RT-01..08, ISO-03 definitions, Out-of-Scope table
- `/root/workspace/nanopi/.planning/STATE.md` — prior concurrency bugs (`2e386ef`, sequential-bash fix), test-suite counts and conventions
- `/root/workspace/nanopi/.planning/research/SUMMARY.md`, `PITFALLS.md`, `ARCHITECTURE.md`, `STACK.md` — prior research synthesis for this milestone, directly reused and cross-checked against source

### Secondary (MEDIUM confidence)
- None used this pass — no WebSearch/WebFetch/Context7 lookups were performed. All findings derive from direct repository inspection (HIGH confidence per the source hierarchy, since this is a codebase-specific implementation phase, not a "what does library X do" question).

### Tertiary (LOW confidence)
- `tokio-util::CancellationToken::child_token()` exact propagation semantics (Assumption A1) — stated from training-data knowledge of the stable, long-unchanged tokio-util API, not re-verified against current docs.rs in this session.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — every library is already a locked dependency, confirmed by reading `Cargo.toml` directly; zero new-package risk.
- Architecture: HIGH — all integration points (`Agent::build_fresh`, `run_turn`'s cancel/steer params, `ToolContext`) were read directly from source, not inferred.
- Pitfalls: HIGH — largely inherited from `.planning/research/PITFALLS.md`, which was itself built from this codebase's own incident history (`2e386ef`, sequential-bash fix, session corruption fixes) plus a reference implementation; cross-checked against current source in this pass.

**Research date:** 2026-10-03
**Valid until:** 30 days (stable, internal-codebase-only research; the only decay risk is if CONTEXT.md decisions change before planning, not external library drift)

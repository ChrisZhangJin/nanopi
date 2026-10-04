# Phase 4: Background launch & control - Research

**Researched:** 2026-10-04
**Domain:** background process control, message injection, print-mode drain, git worktrees (Rust/tokio CLI)
**Confidence:** MEDIUM-HIGH for integration points (read from current source); MEDIUM for new design (no existing background path to copy)

## Summary

Phase 4 turns the **synchronous** child-dispatch path built in Phases 1-3
(`run_single` in `src/tool/agent.rs` spawns a `nanopi -p` child and
**awaits** its full exit before the tool call returns) into an
**optionally-asynchronous** one: `background: true` must return
`{id, state, archive_path}` immediately while the child keeps running
unsupervised by the calling tool call, with its completion (or a
`send_message`/`stop_agent` amendment) surfacing later through the
existing follow-up/steer machinery that already exists for human-typed
messages in the TUI (`src/mode/tui.rs`) and does not yet exist at all in
print mode (`src/mode/print.rs`, which currently has no "wait for more
work" loop — it runs exactly one `agent.run_turn` and exits).

The CONTEXT.md Revision (2026-10-03) is authoritative and overrides the
original `ARCHITECTURE.md` research memo: agents are **child processes**
(`nanopi -p`), not in-process `Agent` instances. `ARCHITECTURE.md`
describes an in-process design that was tried and rolled back
(`2bd0343`) — it is cited below only where its control-plane reasoning
(steer channel, follow-up queue, registry-as-source-of-truth) still
applies to the child-process shape, and is otherwise **not a valid
reference for this phase**.

**Primary recommendation:** do not block the dispatching tool call on
the child's exit when `background: true`. Spawn via `tokio::spawn`,
register a `JoinHandle` + `CancellationToken` + brief path in
`AgentRegistry` (already exists, needs these three fields added), and
have the spawned task — not the tool call — perform the
`ensure_report`/`cap_report`/state-transition work `run_single` does
today. When that task finishes, it formats `[agent {id} finished: {state}] {capped report}`
and delivers it exactly the way `src/mode/tui.rs`'s `pick_follow_up` /
`steer_or_queue` already deliver a demoted steer message: via
`Agent::pending_follow_ups` if the main agent is mid-turn (so it is
drained as a `SteerMessage::FollowUp` at the next iteration boundary),
or via the TUI's `follow_up_slot`/new-turn path if idle. Print mode
needs the **same `pending_follow_ups` → new turn loop**, which it does
not have today and must gain as new code (not reuse), because
`run_print_mode` currently has no multi-turn loop at all.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Background spawn & id allocation | AgentRegistry (`src/agent_registry.rs`) | Tool layer (`src/tool/agent.rs`) | Registry already owns ids/state/caps; must gain non-blocking spawn |
| Amend (running) | Brief file + registry lookup | Child process (self-polls brief) | RT-09 pattern already does this for sync agents; D-02 generalizes it |
| Amend (finished) | Continue path (CTL-06) | — | Same as D-05: rebuild from transcript, new `-p --session` |
| Stop | AgentRegistry (`ChildGuard`/pid) | — | `kill_group`/`ChildGuard` already exist; just needs manual trigger outside exit path |
| List | AgentRegistry snapshot | archive (`index.md`) for extra fields | `snapshot()` exists; needs description/tokens/report path added to `AgentEntry` or read from brief/report front matter |
| Report injection | Main `Agent` (`pending_follow_ups`) | TUI `follow_up_slot` / print-mode new loop | Reuses the exact mechanism the follow-up queue already uses; do not build a second one |
| Print-mode drain (CTL-07) | `src/mode/print.rs` (new loop) | AgentRegistry (enumerate live children) | Print mode currently exits after one turn; must poll registry before final exit |
| Worktrees (ISO-01/02) | Tool/dispatch layer (`src/tool/agent.rs`) | `git` CLI via `std::process::Command` | D-11's Claude's-Discretion note: prefer `git` CLI, no new crate |

## Standard Stack

### Core
No new crates are required. `QA-02` caps binary growth at ~150 KB and
wants no new crates unless justified — this phase can be built entirely
on what's already in `Cargo.toml` (`tokio` process/sync/time features,
already-present `CancellationToken` via `tokio-util`, `std::process::Command`
for git).

| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `tokio` | 1.40 (pinned, in tree) | `tokio::spawn` for detached background tasks, `mpsc`/`oneshot` for completion signalling | already a dependency; `rt-multi-thread`, `sync`, `process`, `time` features already enabled |
| `tokio-util` | 0.7 (in tree) | `CancellationToken` for `stop_agent` | already used for the main turn's Ctrl-C cancellation |
| `libc` | (in tree, used by `agent_registry.rs`) | `killpg`/`kill` for stop/stop-all | already used by `kill_group` |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `git` CLI (external binary, not a crate) | system | worktree add/remove/merge/branch -D | D-08..D-12; owner's stated preference in CONTEXT.md discretion note |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| `git` CLI via `std::process::Command` | `git2` crate (libgit2 bindings) | `git2` adds a C dependency and meaningfully increases binary size (violates QA-02); CLI shelling is what the owner explicitly asked for |
| `tokio::spawn` detached task | A dedicated worker-pool actor | Over-engineering for this phase's scale (max_live is already small, bounded by `AgentConfig`) |

**Installation:** none — no new packages needed. `cargo add` not required.

## Package Legitimacy Audit

No external packages are being added in this phase. `slopcheck` was not
run because there is nothing to check.

**Packages removed due to slopcheck [SLOP] verdict:** none (N/A — no new packages)
**Packages flagged as suspicious [SUS]:** none (N/A — no new packages)

## Architecture Patterns

### System Architecture Diagram

```
                     Main agent turn (TUI or -p)
                             │
                 tool call: agent{..., background: true}
                             │
                    AgentRegistry.reserve()  ──► brief.md written
                             │
                    tokio::spawn(run_background_single)
                             │  (tool call returns HERE: {id, state: "queued"/"running", archive path})
                             │
              ┌──────────────┴───────────────────────────┐
              │ background task (owns the child process)  │
              │  - acquire_run() permit                    │
              │  - spawn `nanopi -p --session ...` child    │
              │  - watch brief.md for amendments (existing  │
              │    RT-09 machinery, unchanged)               │
              │  - on stop_agent: cancel token → kill_group  │
              │  - on child exit: ensure_report, cap_report, │
              │    set_state                                 │
              └──────────────┬───────────────────────────┘
                             │  finished: [agent a3 finished: done] <capped report>
                             ▼
      ┌──────────────────────────────────────────────────────────┐
      │ injection path (SAME mechanism as human follow-ups)        │
      │                                                             │
      │  Main agent streaming? → Agent::pending_follow_ups.push_back│
      │     (drained as SteerMessage::FollowUp at next iteration)   │
      │                                                             │
      │  Main agent idle (TUI)? → tui.rs follow_up_slot /            │
      │     pick_follow_up() starts a new turn (same path humans use)│
      │                                                             │
      │  Main agent idle (-p)? → NEW: print-mode loop must poll      │
      │     the registry's pending-report queue and start another    │
      │     agent.run_turn before the final exit (does not exist     │
      │     today — print.rs runs exactly one turn and returns)      │
      └──────────────────────────────────────────────────────────┘
                             │
                    -p exit path only:
                    CTL-07: before printing final JSON/text and
                    returning, drain/await (or stop_agent on Ctrl-C)
                    every entry in AgentRegistry.snapshot() that is
                    not yet terminal.
```

### Recommended Project Structure
```
src/
├── agent_registry.rs   # MODIFIED: add JoinHandle + CancellationToken +
│                        #   description/task fields to AgentEntry;
│                        #   add stop(id), stop_all(), pending_reports queue
├── tool/
│   ├── agent.rs         # MODIFIED: background: bool arg; split run_single
│   │                     #   into reserve+spawn (sync path keeps awaiting
│   │                     #   the JoinHandle; background path does not)
│   └── agent_ctl.rs     # NEW: send_message / stop_agent / list_agents
│                         #   tool implementations (main-agent-only, never
│                         #   exposed to a dispatched child's ToolRegistry)
├── worktree.rs           # NEW: git CLI wrapper — add/remove worktree,
│                         #   branch create/delete, merge-or-report-conflict
├── mode/
│   ├── tui.rs            # MODIFIED (small): report-injection reuses
│   │                     #   pending_follow_ups/follow_up_slot, no new
│   │                     #   select! arm needed if registry pushes through
│   │                     #   the same channel already feeding ag_rx/follow-ups
│   └── print.rs          # MODIFIED: add a "drain background agents" loop
│                          #   before the final JSON/text print (CTL-07);
│                          #   this is new code, not a reuse of an existing
│                          #   print-mode loop (none exists)
```

### Pattern 1: Non-blocking dispatch via owned background task
**What:** When `background: true`, the tool call does the synchronous
prefix of `run_single` (gitignore-once, `reserve`, write `brief.md`,
`regenerate_index`) then hands the rest (`acquire_run().await`, spawn,
`spawn_and_collect_with`, `ensure_report`, `set_state`) to a
`tokio::spawn`'d task and returns `{id, state, archive_path}`
immediately — it does NOT await the `JoinHandle`.
**When to use:** Every `background: true` dispatch (CTL-01).
**Example:**
```rust
// src/tool/agent.rs (sketch — mirrors existing run_single structure)
let (id, dir) = reg.reserve(&agents_root)?;
write_private(&dir.join("brief.md"), &brief)?;
let handle = tokio::spawn({
    let reg = Arc::clone(reg);
    let dir = dir.clone();
    let id = id.clone();
    async move {
        let _permit = reg.acquire_run().await;
        reg.set_state(&id, AgentState::Running);
        let out = spawn_and_collect_with(command, timeout, |pid| reg.set_pid(&id, pid)).await;
        // ensure_report / cap_report / set_state as in run_single today
        reg.record_background_finished(&id, out); // NEW: pushes to pending-reports queue
    }
});
reg.track_background(&id, handle, cancel_token); // NEW
return Ok(ToolOutput { content: json!({"id": id, "state": "queued", "archive_path": dir}).to_string(), ..});
```
This is new code — Phase 1-3 never built a non-blocking path, so there
is no existing function to copy; it is a structural split of
`run_single`'s existing body, not a net-new algorithm. Confidence:
MEDIUM (straightforward `tokio::spawn` pattern, but the split itself is
unverified against the actual 2476-line `agent.rs`; the planner should
budget a task to do this refactor carefully so the synchronous
(`background: false`) path's existing tests keep passing byte-for-byte).

### Pattern 2: Report injection reuses `pending_follow_ups`, not a new channel
**What:** `src/event.rs`'s `SteerMessage::FollowUp` and
`Agent::pending_follow_ups` (a `VecDeque<String>` field on `Agent`,
drained by `tui.rs`'s `pick_follow_up`, see `tui.rs:2154`) is the
existing mechanism for "queue text to run as a follow-on turn without
interrupting the current one." CONTEXT.md D-06 asks for exactly this
behavior for background-agent reports. Do not add a parallel
notification channel.
**When to use:** Whenever a background agent (or `stop_agent`'s partial
report) needs to reach the main agent.
**Example (conceptual, from existing code at `tui.rs:2140-2165`):**
```rust
// Existing priority order tui.rs already implements for humans:
// 1. Agent::pending_follow_ups (a FollowUp handled inside the turn,
//    or a demoted steer) — highest priority, drained first.
// 2. follow_up_slot (a queued line that missed its turn window).
// Background-agent completions should be pushed into priority 1 if the
// Agent is streaming (reachable via the agent handle held by the turn
// task) or priority 2 if idle (same follow_up_slot the TUI drains).
let from_agent = g.as_mut().and_then(|a| a.pending_follow_ups.pop_front());
let follow_up = pick_follow_up(from_agent, &mut follow_up_slot);
if let Some(text) = follow_up { /* start next turn with `text` */ }
```
**Batching (D-06):** "Several reports that finish together are batched
into one message" — implement by having the registry coalesce: when
pushing to the follow-up queue, if nothing has been drained yet and
another report arrives within the same tick, concatenate both into one
`[agent a3 finished: done] ... \n\n[agent a4 finished: failed] ...`
string before it is ever observed by `pick_follow_up`. A simple
approach: the registry holds its own small outbox and flushes it as ONE
string the next time anything asks "is there a pending report," rather
than pushing N separate follow-ups that would start N separate turns.

### Pattern 3: Print-mode drain loop (CTL-07) — new code
**What:** `run_print_mode` (`src/mode/print.rs:70`) currently runs
`agent.run_turn` exactly once (`print.rs:373`/`391` for the optional
checklist self-check) then shuts down. For `-p` with background agents
in flight, it must, after the main turn finishes and before the final
JSON/text print:
1. Check `AgentRegistry::global()` (already exists, `agent_registry.rs:238`)
   for any non-terminal entries.
2. If none: proceed to exit as today.
3. If some: either wait for them (poll `JoinHandle`s / a shared
   notify) or, on Ctrl-C, call the registry's new `stop_all()`.
4. For every report that arrives while waiting, feed it to the main
   agent as one more `agent.run_turn` call (D-07: "runs one more main
   turn if any reports arrived") — i.e., a SMALL loop around the
   existing single-turn call, bounded to run at most once more (per
   D-07's wording: "one more main turn", not an unbounded loop).
**When to use:** Always in `-p` mode, gated on whether any background
agent was ever dispatched during the run (skip the wait entirely if the
registry has 0 entries — no behavior change for today's `-p` users).
**Confidence:** MEDIUM — this is genuinely new code, not a refactor of
existing code; the exact shape of "wait for N JoinHandles while still
accepting stop_agent via Ctrl-C" needs a plan task of its own and a
design decision, flagged as an Open Question below.

### Pattern 4: Worktree isolation via `git` CLI
**What:** `isolation: "worktree"` runs
`git -C <main repo> worktree add -b nanopi/<run>/<id> .nanopi/worktrees/<run>-<id> <base-branch-or-HEAD>`
before the child starts, with the child's cwd set to the new worktree
path (D-12: every file tool resolves against the agent's own cwd — this
already happens today via each child's independently-passed `cwd`, so
no new path-confinement mechanism is needed, just pointing the existing
`cwd` field at the worktree).
**When to use:** ISO-01, opt-in per dispatch; orchestrator mode (Phase 6)
will set it by default for parallel writers — Phase 4 only needs the
primitive and manual opt-in.
**Example:**
```rust
// src/worktree.rs (new)
pub fn create(repo_root: &Path, run: &str, id: &str) -> Result<Worktree, String> {
    let branch = format!("nanopi/{run}/{id}");
    let wt_path = repo_root.join(".nanopi/worktrees").join(format!("{run}-{id}"));
    let status = std::process::Command::new("git")
        .args(["-C", &repo_root.display().to_string(), "worktree", "add", "-b", &branch])
        .arg(&wt_path)
        .status()
        .map_err(|e| format!("git worktree add: {e}"))?;
    if !status.success() { return Err("git worktree add failed".into()); }
    Ok(Worktree { path: wt_path, branch })
}
```
Not-a-git-repo detection: `git -C <cwd> rev-parse --is-inside-work-tree`
exit code != 0 → log a warning and ignore `isolation: "worktree"` for
that dispatch (CONTEXT.md D-08: "ignored with a warning when the
directory is not a git repo").
**Merge (D-11):** `git -C <main repo> merge --no-edit <branch>`; on
non-zero exit, `git merge --abort`, keep the worktree/branch, surface
"conflict, branch kept at `<branch>`, run `git worktree` manually" in
the report, and signal "ask the user" (D-11: "nothing is resolved
silently" — this likely means the main agent's reply to the user should
surface the conflict rather than auto-resolving; it does NOT mean a new
approval-gate UI, which is out of scope per REQUIREMENTS.md's
"Stopping or messaging an agent directly from the expanded panel" OOS
row — that OOS row is about Phase 5's panel, not about surfacing text,
so is not directly blocking here, but no new interactive prompt
mechanism exists in print mode to "ask" synchronously; treat this as
"report the conflict in text," not "block for input").
**Cleanup (D-10):** `git -C <repo> worktree remove <path>` (no changes)
then `git branch -D <branch>`; "no changes" = `git -C <wt_path> status --porcelain` empty AND no commits ahead of base (`git rev-list <base>..<branch> --count` == 0).

### Anti-Patterns to Avoid
- **A second notification/event channel for background reports** — D-06
  explicitly reuses the follow-up queue; building a parallel
  `SubagentEvent` broadcast (as `ARCHITECTURE.md`'s now-superseded
  design proposed) duplicates state and risks reports racing with
  human-typed follow-ups for queue position.
- **Blocking the dispatching tool call's thread while "detaching"** —
  `tokio::spawn` must truly return without `.await`ing the child; any
  accidental `.await` on the handle before returning defeats CTL-01's
  "keeps working" requirement.
- **Giving background-control tools to dispatched agents** — CONTEXT.md
  is explicit: "main agent only; never given to agents." `agent_ctl.rs`
  tools must only be registered on the ToolRegistry built for the
  TUI/print-mode main agent, never on `build_child_args`'s tool
  allowlist path.
- **Auto-resolving merge conflicts** — D-11 is explicit: conflicts are
  never resolved silently.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Process group kill | A new kill mechanism | `agent_registry.rs::kill_group`/`ChildGuard` (already exists, tested) | Already handles pgid-reuse window (research A1) and SIGKILL semantics |
| Mid-turn message delivery | A new steer/event bus | `SteerMessage` + `Agent::pending_follow_ups` (`src/event.rs`) | Exactly the mechanism D-06 asks to reuse; already drained correctly at iteration boundaries |
| Stale-file protection across worktrees | A new mtime/hash check | `ISO-03`'s existing `FileStateTracker` (process-global, from Phase 1) | Worktrees give each agent its own cwd so collisions are less likely, but the guard should stay as the backstop, not be replaced |
| Git merge logic | Hand-rolled 3-way merge or diff/patch | `git merge` via CLI | git's merge/conflict detection is the correct, battle-tested tool; CONTEXT.md discretion note prefers the CLI explicitly |

**Key insight:** Nearly everything phase 4 needs already exists in
skeletal form from phases 1-3 (registry, kill, steer queue, brief/report
archive). The work is almost entirely **wiring and a control-flow
split** (blocking → spawn-and-track), not new subsystems. The one
genuinely new subsystem is the print-mode drain loop, because print
mode was built (phase 1-3) assuming exactly one turn and no background
work outlives it.

## Common Pitfalls

### Pitfall 1: Awaiting the JoinHandle defeats "keeps working" (CTL-01)
**What goes wrong:** A naive `tokio::spawn(...).await` inside the tool's
`execute()` still blocks the calling turn until the child finishes —
indistinguishable from the current synchronous path.
**Why it happens:** `execute()` is an `async fn` returning
`Result<ToolOutput, ToolError>`; it's easy to reflexively await
everything in scope.
**How to avoid:** Store the `JoinHandle` in the registry and return
immediately; a separate task (or the registry's own bookkeeping,
triggered from wherever its completion is observed) handles the
eventual report.
**Warning signs:** A test asserting `list_agents` shows "running" for a
background agent fails because the tool call itself didn't return until
the child was already "done."

### Pitfall 2: Report injection racing with a currently-executing tool call
**What goes wrong:** CONTEXT.md D-02 requires amendments to be delivered
"never during a tool call" — the same constraint implicitly applies to
background report injection: a report must not jump in mid-tool-call
either, since `pending_follow_ups` is only drained at iteration
boundaries. If the injection accidentally pushes straight into the
active turn's conversation while a tool is executing, the transcript
order breaks and the model sees a user message interleaved with its own
tool call/result pair.
**Why it happens:** The registry doesn't know the main agent's turn
phase; a naive injection might call directly into `Agent::context.messages`.
**How to avoid:** Injection must ALWAYS go through `pending_follow_ups`
or the idle-start path, never direct message mutation. This is already
guaranteed if Pattern 2 above is followed correctly.
**Warning signs:** A transcript/session JSONL replay test shows a
follow-up user message appearing between a `tool_call` and its
`tool_result` entry.

### Pitfall 3: Print-mode exit races a background agent that finishes AFTER the drain check
**What goes wrong:** `-p` checks the registry once, sees "all terminal,"
exits — but a child that was about to transition states (e.g., writing
its final report.md) hasn't yet called `set_state`, so the parent exits
successfully leaving a truncated report or (worse) an orphaned process
whose `kill_on_drop(true)` only fires if the `Child` handle itself is
still owned by a live task, which it won't be if the background task
already detached from any `Drop` guard.
**Why it happens:** TOCTOU between "registry says terminal" and
"process has actually exited and flushed its report file."
**How to avoid:** The drain loop must `.await` the tracked `JoinHandle`s
themselves (which only resolve after `ensure_report`+`set_state` have
run inside the spawned task), not just poll `AgentState`. Polling state
is fine for `list_agents` (a point-in-time snapshot is expected there);
the print-mode exit gate needs the stronger guarantee a `JoinHandle`
await gives.
**Warning signs:** An E2E test that races `-p` exit against a slow
background agent flakes with a missing or truncated `report.md`.

### Pitfall 4: `stop_agent {id: "all"}` racing new dispatches
**What goes wrong:** Between enumerating "all live agents" and killing
them, a new `background: true` dispatch from the same turn (the model
can call tools in parallel) could register — leaving an unstoppable
orphan from the user's perspective ("I said stop all and one kept
running").
**Why it happens:** `AgentRegistry::reserve` and a hypothetical
`stop_all` both need the same lock but the window between enumeration
and kill isn't naturally atomic if `stop_all` just snapshots then kills.
**How to avoid:** `stop_all` should hold a short critical section that
(a) marks a "stopping" flag or (b) at minimum re-snapshots right before
killing, and the kill set should include anything added during the same
model turn (same tool-call batch) since both calls are issued together,
not truly concurrent in the common case. Exact-simultaneity races across
different turns are lower priority; document this as a known, accepted
gap per the project's existing tolerance for the pid-reuse window
(research A1 in Phase 1).
**Warning signs:** Flaky test where a dispatched-then-immediately-stopped
agent is still found Running after `stop_agent{id:"all"}` returns.

## Code Examples

### Existing follow-up priority logic to extend (verified in source)
```rust
// src/mode/tui.rs (verified at lines ~2140-2165)
// 1. `Agent::pending_follow_ups` — a `SteerMessage::FollowUp` handled
//    inside the turn, or a steer `drain_steer_to_follow_ups` demoted.
// 2. `follow_up_slot` — a human's line that missed its turn, noticed
//    ... see `pick_follow_up`.
let from_agent = g.as_mut().and_then(|a| a.pending_follow_ups.pop_front());
let follow_up = pick_follow_up(from_agent, &mut follow_up_slot);
```

### Existing registry primitives to extend (verified in source)
```rust
// src/agent_registry.rs (verified, lines 139-168)
pub fn set_state(&self, id: &str, state: AgentState) { /* ... persists to brief+index */ }
pub fn snapshot(&self) -> Vec<AgentEntry> { self.lock().clone() }
pub fn kill_all(&self) { /* SIGKILLs every Running child's pgid */ }
```
These need, respectively: no change to `set_state`; `snapshot` extended
with description/report path fields (or a joined read from brief.md
front matter at list-time, cheaper than widening `AgentEntry`); and a
new `kill_one(id)` alongside the existing `kill_all` (today only used at
process exit) for `stop_agent{id}` / `stop_agent{id:"all"}`.

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|---------------|--------|
| In-process `Agent` instances spawned via `tokio::spawn` for subagents (`ARCHITECTURE.md`, 2026-10-03 research memo) | Child-process `nanopi -p` agents, tracked by pid/process-group | 2026-10-03 (same day), commit `2bd0343` rolled back the in-process attempt | Every mention of `SubagentRegistry`/`SubagentHandle`/in-process spawn in `ARCHITECTURE.md` must be read as "child-process equivalent" — the control-plane ideas (steer reuse, follow-up injection, registry-as-truth) still apply; the implementation primitives (spawn an `Agent` vs spawn a process) do not |

**Deprecated/outdated:**
- `ARCHITECTURE.md`'s `SubagentRegistry`/`SubagentHandle` Rust sketches:
  superseded by `src/agent_registry.rs::AgentRegistry`/`AgentEntry`,
  which already exists and is process-oriented (pid, `ChildGuard`), not
  in-process-agent-oriented.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | D-07's "one more main turn if any reports arrived" means at most one extra `run_turn` call in print mode, not an unbounded drain loop | Pattern 3 | If wrong, a chatty background agent could cause print mode to loop turns indefinitely; planner should bound it explicitly either way |
| A2 | D-11's "the user is asked how to proceed" on merge conflict means the conflict is surfaced as text in the main agent's reply, not a new blocking interactive prompt | Pattern 4 | If the owner actually wants a blocking approval gate, this needs new UI plumbing in both TUI and print mode, which is a much bigger task |
| A3 | `stop_agent{id:"all"}` only needs to cover agents already registered at call time, not ones racing in from parallel tool calls in later turns | Pitfall 4 | If wrong, an edge-case orphan could survive "stop all"; likely acceptable per project's existing pid-reuse tolerance precedent |
| A4 | "No changes" for worktree cleanup (D-10) means empty `git status --porcelain` AND zero commits ahead of the base branch (an agent that committed then reverted back to identical content still counts as "changed" by commit count, not just tree diff) | Pattern 4 | If wrong (owner means tree-diff only), a worktree with empty net diff but real commits would be wrongly kept; cheap to flip once decided |
| A5 | Background agents still go through the existing `max_live`/`max_concurrency` caps in `AgentRegistry`/`AgentConfig` unchanged — no separate background-specific cap | Architectural Responsibility Map | If owner wants a distinct background cap, needs a new config key |

**If this table is empty:** N/A — see rows above; all are flagged for
planner/discuss-phase confirmation.

## Open Questions

1. **How does the main agent's turn-task actually reach a background
   report into `pending_follow_ups` when the main agent is mid-stream?**
   - What we know: `Agent::pending_follow_ups` is a field on the live
     `Agent` struct that the current turn owns (behind a guard/mutex
     per `tui.rs`'s `g.as_mut()` pattern at line 2154).
   - What's unclear: the registry (a separate, long-lived
     `Arc<AgentRegistry>`) has no existing handle back into "whichever
     `Agent` instance is currently running the main turn" — that
     linkage doesn't exist yet in either `tui.rs` or `print.rs`.
   - **Recommended default:** thread an `Arc<Mutex<Option<...>>>`-style
     weak reference (or a dedicated `mpsc::Sender<String>` created once
     per process and handed to both the `Agent` builder and the
     registry) so the registry can push a finished report string
     directly into whichever consumer is listening, mirroring how
     `steer_tx`/`ag_rx` are already threaded through `tui.rs`'s
     `run_app`. This needs a plan task dedicated to "wire the
     report-injection channel," and should be designed before the
     spawn-and-track work lands, since it changes `Agent`/`Launcher`
     construction signatures.

2. **Exact shape of the print-mode drain loop's wait (CTL-07): does
   Ctrl-C during the wait stop all background agents, or just exit
   print mode and leave them orphaned?**
   - What we know: D-07 says "Ctrl-C stops all of them" for the general
     background-agent lifecycle; print mode's existing Ctrl-C handling
     (not reviewed in depth this session — flagged LOW) likely maps to
     cancelling the main turn only today.
   - What's unclear: whether print mode's SIGINT handler (if any exists
     today — needs verification) already has a hook point to call
     `stop_all`, or whether this is new wiring.
   - **Recommended default:** install a `tokio::signal::ctrl_c()` listener
     in the drain loop that calls the new `AgentRegistry::stop_all()`
     before exiting, matching D-07 exactly. Flag for a dedicated
     verification task since print mode's current SIGINT behavior
     wasn't read in this session (budget-constrained) — the planner
     should have a task re-verify current `-p` Ctrl-C handling before
     building on top of it.

3. **Does `list_agents` read live state from `AgentRegistry` in-memory,
   or from `index.md`/`brief.md` on disk?**
   - What we know: both exist and should agree (`set_state` persists to
     both); `AgentEntry` today lacks `description`/`turns`/`tokens`/
     `report_path` fields that D-04 requires in `list_agents` output.
   - What's unclear: whether it's cheaper to widen `AgentEntry` (kept in
     sync at every state transition) or to read `brief.md`/`report.md`
     front matter live at `list_agents` call time (simpler, always
     fresh, one extra disk read per entry per call).
   - **Recommended default:** read from disk (brief/report front
     matter) at `list_agents` call time. The archive is already the
     durable source of truth per the project's own stated philosophy
     ("Files are the archive, channels are the control" —
     `ARCHITECTURE.md`'s one line of design guidance that does survive
     the runtime-model change), and it avoids widening the hot-path
     `AgentEntry` struct with fields only needed for a rarely-called
     listing tool.

4. **Does the "continue" path (CTL-06/D-05) register the new `-p
   --session` child under the SAME agent id, or a new id with a
   `continued_from` pointer?**
   - What we know: D-05 says "run it again under the same id."
   - What's unclear: whether `AgentRegistry::reserve` (which allocates
     sequential ids `a1`, `a2`, ...) needs a new "re-activate existing
     id" path, since `reserve` today always allocates a fresh id and a
     fresh directory.
   - **Recommended default:** add `AgentRegistry::reactivate(id)` that
     transitions an existing terminal entry back to `Queued`/`Running`
     in place (same dir, same id, appends to the existing brief/report
     rather than creating new ones), distinct from `reserve`. This
     keeps "same id" literally true per D-05's wording.

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| `git` CLI | Worktree isolation (ISO-01/02, D-08..D-12) | checked below | — | Dispatch proceeds without `isolation: "worktree"`, with a warning (D-08 already specifies this fallback for "not a git repo"; the same fallback should apply if `git` itself is simply missing from `$PATH`) |

**Missing dependencies with no fallback:** none — `git` absence already
has a specified fallback behavior per D-08.

**Missing dependencies with fallback:**
- `git` CLI — if `command -v git` fails, treat exactly like "not a git
  repo": warn and ignore `isolation: "worktree"` for that dispatch.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | `cargo test` (built-in Rust test harness), matching all prior phases |
| Config file | none — standard `#[test]`/`#[tokio::test]` in-source, plus `tests/*.rs` integration tests (`tests/agent_runtime.rs`, `tests/agent_spawn.rs`, `tests/agent_archive.rs`, `tests/print_mode_e2e.rs` already exist and are the natural extension points) |
| Quick run command | `cargo test --lib -- --test-threads=1 agent_registry::` (or the relevant module path) |
| Full suite command | `cargo test -- --test-threads=1` (project convention established in STATE.md: tests must run single-threaded due to shared env-var/global state across the suite) |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|--------------------|-------------|
| CTL-01 | `background: true` returns `{id, state, archive_path}` without waiting for child exit | unit | `cargo test --lib tool::agent::tests::background_dispatch_returns_immediately -- --test-threads=1` | ❌ Wave 0 |
| CTL-02 | Amending a running background agent appends to brief, delivered at next turn boundary; amending a finished one continues it (CTL-06) | integration | `cargo test --test agent_spawn amend_running_background_agent -- --test-threads=1` | ❌ Wave 0 (extend `tests/agent_spawn.rs`) |
| CTL-03 | `stop_agent{id}` kills the child, partial report written, state = stopped | integration | `cargo test --test agent_runtime stop_agent_writes_partial_report -- --test-threads=1` | ❌ Wave 0 (extend `tests/agent_runtime.rs`) |
| CTL-04 | `list_agents` returns id/description/state/elapsed/turns/tokens/report path for every agent in the run | unit | `cargo test --lib tool::agent_ctl::tests::list_agents_reports_all_fields -- --test-threads=1` | ❌ Wave 0 (new file `src/tool/agent_ctl.rs`) |
| CTL-05 | Finished background report starts a new turn when idle, queues as follow-up when streaming | integration | `cargo test --test print_mode_e2e background_report_starts_new_turn_when_idle -- --test-threads=1` plus a TUI-side unit test for the streaming case using the existing `pick_follow_up` harness (`tui.rs` tests near line 5787) | ❌ Wave 0 |
| CTL-06 | Continuing a finished agent starts a new `nanopi -p --session` on the same id/transcript | integration | `cargo test --test agent_spawn continue_finished_agent_same_id -- --test-threads=1` | ❌ Wave 0 |
| CTL-07 | `-p` waits for (or stops on Ctrl-C) background agents before exit; no orphan processes | integration (e2e-ish) | `cargo test --test print_mode_e2e print_mode_waits_for_background_agent -- --test-threads=1` (process-check style, like `agent_registry.rs`'s `wait_gone` helper) | ❌ Wave 0 |
| ISO-01 | Writer dispatched with `isolation: "worktree"` gets its own worktree/branch; report includes path+branch | integration | `cargo test --test agent_spawn worktree_isolation_reports_path_and_branch -- --test-threads=1` | ❌ Wave 0 (requires a real git repo fixture — see gap below) |
| ISO-02 | Unchanged worktrees removed; changed ones kept and listed | integration | `cargo test --test agent_spawn unchanged_worktree_is_removed` / `changed_worktree_is_kept_and_listed -- --test-threads=1` | ❌ Wave 0 |

### Sampling Rate
- **Per task commit:** targeted `cargo test --lib <module>::` or
  `cargo test --test <file> <test_name>` for the area touched.
- **Per wave merge:** `cargo test -- --test-threads=1` (full suite,
  matching the project's single-threaded convention from STATE.md's
  flakiness history — do not deviate from `--test-threads=1`).
- **Phase gate:** full suite green (both default and, if touched,
  `--features wasm`) before `/gsd:verify-work`.

### Wave 0 Gaps
- [ ] `tests/agent_spawn.rs` additions — background dispatch, amend,
      continue, worktree isolation tests (file exists, needs new test
      functions).
- [ ] `tests/agent_runtime.rs` additions — stop_agent kill + partial
      report (file exists, needs new test functions).
- [ ] `tests/print_mode_e2e.rs` additions — CTL-05/CTL-07 (file exists,
      needs new test functions).
- [ ] `src/tool/agent_ctl.rs` — new source file, needs its own
      `#[cfg(test)] mod tests` from scratch (CTL-03/CTL-04 unit-level
      coverage).
- [ ] `src/worktree.rs` — new source file; needs a test fixture helper
      that creates a throwaway git repo under `tempfile::tempdir()`
      (pattern already used across the test suite, e.g.
      `agent_registry.rs`'s `tempfile::tempdir()` usage) since git
      worktree tests cannot run against the real project repo.
- [ ] Framework install: none — `cargo test` is already fully wired;
      no new test framework or config needed.

## Security Domain

> `security_enforcement` config key was not found in
> `.planning/config.json` in this session (file was not read per task
> instructions — excluded from the commit scope). Treating as enabled
> per the default-is-enabled instruction.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-------------------|
| V2 Authentication | no | No new auth surface — control tools are gated by "main-agent-only" tool registration, not credentials |
| V3 Session Management | yes | Continue (CTL-06) reuses the existing session-file mechanism (`--session-file`, already validated in Phase 1); no new session semantics |
| V4 Access Control | yes | "Main agent only; never given to agents" (CONTEXT.md) — enforced by never registering `agent_ctl.rs` tools on a dispatched child's `ToolRegistry`, same pattern as RT-05's "no agent/control tools for children" |
| V5 Input Validation | yes | `send_message`/amend text goes through the existing `fm_value` front-matter sanitizer (Phase 2, T-02-01) before being appended to `brief.md` — must reuse, not reimplement |
| V6 Cryptography | no | Not applicable to this phase |

### Known Threat Patterns for this stack

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|----------------------|
| A dispatched child invoking background-control tools on itself (privilege escalation to control siblings) | Elevation of Privilege | Never register `agent_ctl.rs`'s tools in `build_child_args`'s allowlist path; covered by existing RT-05 test pattern (child tool registry excludes agent/control tools) |
| Front-matter injection via amendment text forging a `state:` key | Tampering | Reuse `fm_value` sanitizer (Phase 2) for every value written into `brief.md`, including `send_message` amendment text |
| Worktree path escaping `.nanopi/worktrees/` via a crafted run/id string | Tampering / path traversal | `run`/`id` are both generated by the registry (`new_run_id`, `a{n}` counter), never user-controlled strings, so no sanitization gap exists if worktree paths are built only from those — do not let a model-supplied string influence the worktree directory name |
| `stop_agent{id:"all"}` killing processes outside the agent registry's own pgid tracking | Denial of Service | `kill_group` already scopes to `pgid > 1` and only pids the registry itself set via `set_pid`; do not widen this to a broader process-matching heuristic |

## Sources

### Primary (HIGH confidence — read directly from source this session)
- `src/agent_registry.rs` (460 lines, read in full) — `AgentRegistry`, `AgentState`, `ChildGuard`, `kill_group`, test patterns
- `src/event.rs` (244 lines, read in full) — `AgentEvent`, `SteerMessage::{Steering,FollowUp}`
- `src/tool/agent.rs:1034-1293` (`run_single`, `ChildProgram`, `spawn_and_collect_with`) — current synchronous dispatch path
- `src/mode/print.rs:1-90, 395-474` — `run_print_mode` structure, single-turn loop, report.md-on-every-exit-path pattern
- `src/mode/tui.rs` (grep-verified line ranges 1873-2165, 3700-3780, 5787-5900) — `follow_up_slot`, `pick_follow_up`, `pending_follow_ups` priority order
- `.planning/phases/04-background-control/04-CONTEXT.md` — locked decisions D-01..D-12, including the 2026-10-03 revision that supersedes the in-process design
- `.planning/ROADMAP.md`, `.planning/REQUIREMENTS.md`, `.planning/STATE.md` — phase goal, requirement text, prior-phase completion state and test-suite conventions (`--test-threads=1`, current test counts)

### Secondary (MEDIUM confidence)
- `.planning/research/ARCHITECTURE.md` — control-plane reasoning (steer reuse, follow-up injection, "files are the archive, channels are the control") still valid; its concrete Rust sketches for an in-process `SubagentRegistry` are NOT valid (superseded by the child-process pivot, confirmed via CONTEXT.md's revision note and commit `2bd0343` mentioned there)

### Tertiary (LOW confidence)
- Print mode's existing Ctrl-C/SIGINT handling was not read in depth this session (flagged as Open Question 2) — needs verification before building the drain-loop's cancellation path
- `Cargo.toml` dependency list was read but the exact current `tokio-util` version pin / `CancellationToken` usage site was not individually re-verified in this session beyond confirming the crate is present

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — no new crates, confirmed against `Cargo.toml` directly
- Architecture: MEDIUM-HIGH for integration points (read from live source), MEDIUM for new designs (print-mode drain loop, report-injection channel wiring) since no prior phase built an analogous non-blocking path to copy
- Pitfalls: MEDIUM — derived from direct code reading plus the project's own documented history of similar races (STATE.md's TOCTOU/pid-reuse precedents), not from external sources

**Research date:** 2026-10-04
**Valid until:** 14 days (fast-moving — phase 3 just landed 2026-10-04 and this phase's design depends on reading the exact current shape of `agent.rs`/`print.rs`, which may shift before planning starts)

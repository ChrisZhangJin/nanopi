---
phase: 04-background-control
reviewed: 2026-10-04T11:00:24Z
depth: standard
files_reviewed: 14
files_reviewed_list:
  - src/agent/brief.rs
  - src/agent_registry.rs
  - src/archive.rs
  - src/lib.rs
  - src/main.rs
  - src/mode/print.rs
  - src/mode/tui.rs
  - src/tool/agent.rs
  - src/tool/agent_ctl.rs
  - src/tool/mod.rs
  - src/worktree.rs
  - tests/agent_archive.rs
  - tests/agent_spawn.rs
  - tests/print_mode_e2e.rs
findings:
  critical: 1
  warning: 3
  info: 2
  total: 6
status: issues_found
---

# Phase 04: Code Review Report

**Reviewed:** 2026-10-04T11:00:24Z
**Depth:** standard
**Files Reviewed:** 14
**Status:** issues_found

## Summary

Reviewed the background-control subsystem added in this phase: `AgentRegistry` (stop/stop_all/adopt_from_disk/background tracking), `tool/agent.rs` dispatch and background-spawn machinery, `tool/agent_ctl.rs` (list/stop/send_message), `worktree.rs` isolation, and the SIGINT/SIGTERM/ctrl-c wiring in `main.rs`/`mode/print.rs`/`mode/tui.rs`. The core concurrency design (CancellationToken + JoinHandle tracking, `StateGuard` drop-safety, id-validation before any path join in `adopt_from_disk`/`worktree::create`) is solid and well tested. One real gap was found in the top-level signal handling: the code comment claims SIGINT is handled at the process level to clean up background agent children, but only SIGTERM is actually wired up, leaving orphaned background agent processes possible on a plain Ctrl-C outside the narrow drain window. A few lower-severity robustness/clarity issues are also noted below.

## Critical Issues

### CR-01: Top-level process only traps SIGTERM, not SIGINT, despite the comment claiming both — orphaned background agents possible on Ctrl-C

**File:** `src/main.rs:589-599` (see also `wait_for_term_signal` at `src/main.rs:198-218`)
**Issue:** The comment directly above the `tokio::select!` says:
```rust
// SIGINT/SIGTERM in print mode: kill every agent child, then exit
// 130/143. Dropping the print future also drops each child's guard.
tokio::select! {
    r = print_fut => r,
    code = wait_for_term_signal() => {
        if let Some(reg) = nanopi::agent_registry::global() {
            reg.kill_all();
        }
        Ok(code)
    }
}
```
but `wait_for_term_signal` only registers `SignalKind::terminate()` (SIGTERM); it does not listen for SIGINT at all:
```rust
async fn wait_for_term_signal() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => { term.recv().await; 143 }
            Err(_) => std::future::pending().await,
        }
    }
    ...
}
```
The only place `tokio::signal::ctrl_c()` is awaited is deep inside `mode::print::run_print_mode`'s background-drain window (`src/mode/print.rs:424`), which only exists after the main turn (and any bounded self-check turns) have already finished and only while `reg.has_background()` is true. If a user presses Ctrl-C at any other point — e.g. while the main turn itself is still running with a `background: true` agent dispatched and alive — Rust installs no SIGINT handler for that signal at all, so the process terminates via the OS's default SIGINT disposition. That bypasses Rust's normal unwind/Drop machinery (including every `ChildGuard`'s `Drop::drop` and `AgentRegistry::kill_all`), so the background agent's child process (in its own process group per `RT-03`) is never sent SIGKILL and is orphaned, continuing to run and consume resources/API quota after the parent nanopi process has exited.
**Fix:** Register a `tokio::signal::unix::SignalKind::interrupt()` listener alongside the terminate listener in `wait_for_term_signal` (or race `tokio::signal::ctrl_c()` in the same `select!`), e.g.:
```rust
async fn wait_for_term_signal() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut int = signal(SignalKind::interrupt()).ok();
        tokio::select! {
            _ = async { if let Some(s) = term.as_mut() { s.recv().await; } else { std::future::pending().await } } => 143,
            _ = async { if let Some(s) = int.as_mut() { s.recv().await; } else { std::future::pending().await } } => 130,
        }
    }
    ...
}
```
and ensure this still composes with the existing in-drain `ctrl_c()` listener (e.g. by having both consumers race the same cancellation token, or by only installing the top-level SIGINT handler and removing the narrower one) so a Ctrl-C at any point during the run reliably reaches `kill_all()`.

## Warnings

### WR-01: `StateGuard::drop` runs blocking `git` subprocess calls synchronously on the async executor thread when a background dispatch is cancelled

**File:** `src/tool/agent.rs:1062-1072` (`StateGuard::drop`), `src/tool/agent.rs:1079-1105` (`finish_worktree_and_record`), used from the cancellation branch of `spawn_background` at `src/tool/agent.rs:1479-1500`
**Issue:** When `AgentRegistry::stop`/`stop_all` cancels a background dispatch's token, `tokio::select!`'s losing branch (`run_body(reg_ref, prepared)`) is dropped mid-flight, which synchronously runs `StateGuard::drop`. For an isolated (`isolation: "worktree"`) agent, that drop path calls `finish_worktree_and_record` directly — not via `spawn_blocking` — which shells out to several `git` subprocesses (`git status`, `git add`, `git commit`, `git merge`, `git worktree remove`, `git branch -D`) and blocks on each `Command::output()` call. This runs on whatever tokio worker thread is polling the cancelled task, blocking that worker (and, on a single-worker/current-thread runtime, potentially every other task) for as long as those git commands take. The code comment acknowledges this ("the `StateGuard::drop` path (sync by construction) calls it directly") but doesn't mitigate it.
**Fix:** Either (a) document and enforce a multi-threaded tokio runtime so one blocked worker cannot starve the whole process, or (b) restructure so cancellation finishes the worktree via `spawn_blocking` too — e.g. have `stop`/`stop_all` request cancellation but let the task's own async body (not `Drop`) perform the worktree finish-up via `tokio::task::spawn_blocking`, with `Drop` only as a last-resort synchronous fallback for truly aborted futures (e.g. panics), not the normal cancel-token path.

### WR-02: Second `wait_background()` call after the bounded "extra" turn is not covered by the Ctrl-C drain race

**File:** `src/mode/print.rs:435-452`
**Issue:** The `tokio::select!` that races `reg.wait_background()` against `tokio::signal::ctrl_c()` only guards the *first* drain (lines 422-434). If no Ctrl-C arrived there, control falls into the `if !interrupted` branch, which runs one more bounded turn (`agent.run_turn(text, ...)`, line 442) and then calls `reg.wait_background().await` again unconditionally at line 449, with no Ctrl-C race around it. If any background agents were (re-)spawned as a side effect of that extra turn (e.g. the model invokes `agent` with `background: true` again) and the user presses Ctrl-C while that second `wait_background()` is pending, the process will block until all of those background tasks finish naturally — there is no way to abort it at that point.
**Fix:** Wrap the second `wait_background()` call (and ideally the extra turn itself) in the same `tokio::select!` pattern used for the first drain, calling `reg.stop_all()` on Ctrl-C and setting `interrupted = true` there too.

### WR-03: `ensure_gitignore_entry`'s line-matching allows a prefix/typo collision to silently suppress re-adding the real entry

**File:** `src/tool/agent.rs:1138-1164`
**Issue:** The membership check:
```rust
if existing.lines().any(|l| matches!(l.trim(), e if e == entry || e == bare || e == format!("/{entry}") || e == format!("/{bare}")))
```
only treats a line as "already present" if it is *exactly* one of those four normalized forms. This is actually fine for correctness (it does not have false positives), but note it also does not detect a broader/less specific existing ignore (e.g. `.nanopi/` already covers `.nanopi/worktrees/`) and will happily append the more specific `.nanopi/worktrees/` on top, growing the file with redundant entries over repeated runs in different cwds. Low-impact (append-only, never removes anything, and `.gitignore` tolerates duplicates/redundant entries) but worth a one-line comment or fix so the file doesn't accumulate avoidable cruft over the lifetime of a project.
**Fix:** Optionally also skip appending when an existing line is a prefix directory of `entry` (e.g. `.nanopi/` already covers `.nanopi/worktrees/`), or leave as-is with a comment noting the accepted redundancy.

## Info

### IN-01: `AgentRegistry::stop` does not update the in-memory entry's state to `Stopped`

**File:** `src/agent_registry.rs:191-209`
**Issue:** `stop()` cancels the background token and kills the process group but never sets `entry.state = AgentState::Stopped` itself (unlike `kill_all`, which does at `src/agent_registry.rs:476-482`). It relies entirely on the cancelled background task's own `StateGuard::drop` (in `tool/agent.rs`) to eventually call `reg.set_state(&id, AgentState::Stopped)`. This is consistent with how the code is actually used today (every call site for a *tracked background* agent goes through `spawn_background`/`spawn_continue_background`, both of which install a `StateGuard`), but `stop()` is a public registry method with no doc comment stating that precondition; a future caller that calls `stop()` for an id without a corresponding `StateGuard`-protected task would leave the entry permanently stuck in its pre-stop state (e.g. `Running`) even though the OS process was killed.
**Fix:** Either set `e.state = AgentState::Stopped` directly inside `stop()` (and make the `StateGuard` a no-op/idempotent `set_state` instead of the sole writer), or add a doc comment on `stop()` spelling out that state transition is the caller's/background task's responsibility.

### IN-02: `finish_worktree_and_record`'s report-append and brief-rewrite are not atomic with each other

**File:** `src/tool/agent.rs:1079-1105`
**Issue:** The function first appends the worktree outcome line to `report.md` (via `OpenOptions::append`), then separately reads/rewrites `brief.md`'s front matter via `write_private` (a plain, non-atomic `std::fs::write`). If the process is killed between these two steps (e.g. a hard `SIGKILL` from a *second* `stop()`/`kill_all()` racing the drop-path cleanup described in WR-01), `report.md` can end up with the outcome line while `brief.md`'s `worktree_outcome` field stays unset, or vice versa if the order were reversed. Low likelihood (this already runs inside a cleanup path) but worth noting since the rest of the codebase is otherwise careful about atomic writes (`crate::tool::file_state::atomic_write` is used elsewhere, e.g. `ensure_report`).
**Fix:** No action required unless this is evidenced in practice; if tightened, use `atomic_write` consistently for both files, or write to the brief first since it has a single idempotent field update.

---

_Reviewed: 2026-10-04T11:00:24Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_

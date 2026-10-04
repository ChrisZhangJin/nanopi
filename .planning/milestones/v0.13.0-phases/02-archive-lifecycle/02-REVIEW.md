---
phase: 02-archive-lifecycle
reviewed: 2026-10-04T07:57:01Z
depth: standard
files_reviewed: 14
files_reviewed_list:
  - src/agent/brief.rs
  - src/mode/print.rs
  - src/paths.rs
  - src/tool/grep.rs
  - src/tool/find.rs
  - src/archive.rs
  - src/lib.rs
  - src/config.rs
  - src/agent_registry.rs
  - src/tool/agent.rs
  - src/main.rs
  - src/mode/tui.rs
  - src/command.rs
  - tests/agent_archive.rs
findings:
  critical: 1
  warning: 3
  info: 2
  total: 6
status: issues_found
---

# Phase 02: Code Review Report

**Reviewed:** 2026-10-04T07:57:01Z
**Depth:** standard
**Files Reviewed:** 14
**Status:** issues_found

## Summary

Reviewed the diff for phase 02 (archive lifecycle) against base commit `00312cd`. The archive module (`src/archive.rs`) itself is careful and well-tested: run-id shaping, symlink-safe size/deletion, live-process detection, and `.gitignore` idempotency all look correct, and the front-matter injection-resistance in `agent/brief.rs` is deliberately tested. The `grep`/`find` archive-exclusion logic (D-08) is duplicated between the two tools but is consistent and covers both the ripgrep and built-in code paths, including the `all=true` case.

The one blocking defect is a regression against an existing, explicitly-tested project invariant: several new code paths in `src/agent_registry.rs` and `src/tool/agent.rs` write directly to `eprintln!` for debug diagnostics, even though these same functions (`AgentRegistry::reserve`, `AgentRegistry::set_state`, and the `AgentTool`/`run_single` dispatch path) run while the TUI is active in raw mode. The codebase has a dedicated `note!` macro (`src/render/raw_tty.rs`) specifically to prevent exactly this: a stray `eprintln!` while raw mode is enabled corrupts the next redraw (see the regression test at `src/render/raw_tty.rs:186-189`). This phase's new archive-I/O error paths reintroduce that exact defect.

## Critical Issues

### CR-01: New archive-error diagnostics use raw `eprintln!` inside TUI-reachable code paths, corrupting the terminal

**File:** `src/agent_registry.rs:112`, `src/agent_registry.rs:149`, `src/tool/agent.rs:670`, `src/tool/agent.rs:706`, `src/tool/agent.rs:710`, `src/tool/agent.rs:770`

**Issue:** `AgentRegistry::reserve` (archive.rs `write_run_pid` failure), `AgentRegistry::set_state` (`set_agent_state` failure), and `tool/agent.rs`'s `ensure_gitignore_once`/`ensure_report`/the post-brief `regenerate_index` call all log errors via plain `eprintln!`. These functions are on the hot path of `AgentTool::execute` → `run_single` (`src/tool/agent.rs:485`,`:725`), which is dispatched as an ordinary tool call from the agent loop in **both** print mode and TUI mode. The TUI enables raw/alternate-screen mode for the duration of the session (`src/mode/tui.rs:721`, `enable_raw_mode()`), and the project has a dedicated `note!` macro (`src/render/raw_tty.rs:174-180`) whose entire purpose, per its own regression test comment, is to prevent `eprintln!` from reaching stderr while raw mode is up ("the defect: while the TUI is up, a `note!` went to stderr ... the next redraw" — `src/render/raw_tty.rs:186-189`).

Any archive I/O failure during a live TUI session — disk full, permission denied on `.nanopi/agents`, a read-only filesystem, a concurrent process holding a lock — now reintroduces that exact defect: the raw-mode screen gets corrupted mid-session. This is a regression against an invariant the codebase already built tooling and tests to protect.

Contrast with the pre-existing `grep.rs` code in the same diff, which correctly uses `crate::note!` for its own fallback diagnostic (`src/tool/grep.rs:164`), and with `src/main.rs:485`/`:490` (`mark_interrupted`/`auto_prune` failures), which is safe only because it runs at startup before the TUI enters raw mode — it is not a model for the other call sites.

**Fix:**
```rust
// src/agent_registry.rs — both call sites
if let Err(e) = archive::write_run_pid(&run_dir) {
    crate::note!("nanopi: debug: write_run_pid({}): {e}", run_dir.display());
}
...
if let Err(e) = archive::set_agent_state(&dir, state.as_str()) {
    crate::note!("nanopi: debug: set_agent_state({}): {e}", dir.display());
}

// src/tool/agent.rs — all four call sites (ensure_gitignore_once,
// ensure_report's two branches, and the regenerate_index call in run_single)
crate::note!("nanopi: debug: ensure_gitignore({}): {e}", cwd.display());
crate::note!("nanopi: debug: precreate report({}): {e}", report.display());
crate::note!("nanopi: debug: ensure_report({}): {e}", report.display());
crate::note!("nanopi: debug: regenerate_index({}): {e}", run_dir.display());
```
`src/mode/print.rs`'s `write_report_durable`/`write_report_with` `eprintln!`s (lines 487, and inside `write_report_with`) are lower risk since print mode never enables raw mode, but for consistency and because `report.md` writes can also be reached from TUI-driven dispatch via `run_single`'s own report path (which already correctly calls `ensure_report`, not `write_report_durable` — these are two different writers for print-mode vs child-dispatch reports), they should be audited too; if any of this print.rs code is reachable from a raw-mode context it needs the same fix.

## Warnings

### WR-01: `/agents clean` runs synchronous, unbounded filesystem deletion on the TUI event-loop task

**File:** `src/mode/tui.rs:2766-2793` (`KeyAction::CleanAgents` arm)

**Issue:** `handle_action`'s `CleanAgents` branch calls `crate::archive::clean_runs` synchronously inline. `clean_runs` walks every run directory, computes `dir_size` recursively (full directory walk) for every removed run, and calls `std::fs::remove_dir_all` — all synchronous blocking I/O — directly inside what appears to be the async TUI key-handling path. On an archive with many/large runs (exactly the scenario this command exists to clean up), this blocks the task driving the TUI for the full duration of the scan+delete, freezing input/redraws until it completes. This is a robustness/UX regression distinct from "performance" in the algorithmic sense (out of scope) — it's blocking I/O on an async executor task that otherwise expects to stay responsive.

**Fix:** Wrap the call in `tokio::task::spawn_blocking` (or move it off the event-loop task) and post the resulting report back via the existing message-passing mechanism, the same way other potentially slow operations in the TUI are expected to be dispatched off the UI thread.

### WR-02: `ensure_gitignore_once` permanently gives up after one failed attempt per cwd

**File:** `src/tool/agent.rs:659-671`

**Issue:** `gitignored_cwds()` inserts the cwd into the "seen" set *before* checking whether `ensure_gitignore` succeeded. If the first attempt fails (e.g. the repo's `.gitignore` is briefly locked, or a transient permission error), the archive directory will never be registered in `.gitignore` for the remainder of the process's lifetime, even though later attempts might succeed. This silently reintroduces the risk `ensure_gitignore` exists to avoid (accidentally committing `.nanopi/agents/`).

**Fix:** Only insert into the `seen` set on success, or track failures separately with a bounded retry/backoff instead of a permanent one-shot latch:
```rust
fn ensure_gitignore_once(cwd: &Path) {
    let mut seen = gitignored_cwds().lock().unwrap_or_else(|e| e.into_inner());
    if seen.contains(cwd) {
        return;
    }
    match archive::ensure_gitignore(cwd) {
        Ok(_) => { seen.insert(cwd.to_path_buf()); }
        Err(e) => crate::note!("nanopi: debug: ensure_gitignore({}): {e}", cwd.display()),
    }
}
```

### WR-03: `find`/`grep` archive-exclusion logic is duplicated verbatim across two modules

**File:** `src/tool/find.rs:111-123` and `src/tool/grep.rs:386-398` (`is_within_agents_root`)

**Issue:** The exact same `is_within_agents_root` function (lexical + canonicalized prefix check) is copy-pasted between `find.rs` and `grep.rs`, including the doc comment. D-08 is a security-relevant invariant (agents must never search other agents' archived data); having two independent copies means a future fix to one (e.g. a canonicalization edge case, or a new bypass found later) can easily be applied to only one call site, silently reopening the hole in the other tool.

**Fix:** Extract `is_within_agents_root` into a shared location (e.g. `src/paths.rs`, alongside `project_agents_dir` which it is conceptually paired with) and have both `find.rs` and `grep.rs` import it.

## Info

### IN-01: Dead no-op statement in `regenerate_index`

**File:** `src/archive.rs:113`

**Issue:** `let _ = &name;` inside the loop body does nothing (it's a no-op reference-and-discard of a variable that is used two lines later anyway). Looks like leftover debugging/refactor residue.

**Fix:** Remove the line.

### IN-02: Inconsistent debug-logging prefix / mechanism across new archive call sites

**File:** `src/main.rs:485`, `:490`; `src/agent_registry.rs:112`, `:149`; `src/tool/agent.rs:670`, `:706`, `:710`, `:770`; `src/mode/print.rs:487` and inside `write_report_with`

**Issue:** Beyond the raw-mode safety issue in CR-01, these new sites all hand-roll `"nanopi: debug: ..."` / `"nanopi: cannot ..."` prefixed `eprintln!` strings rather than using a single shared helper, so the format is inconsistent (`debug:` vs `cannot write ... :` vs no prefix) and any future change to the diagnostic format (e.g., adding a log level or timestamp) requires touching every call site individually.

**Fix:** Route all of these through `note!` (per CR-01) and, if a consistent prefix is desired, bake it into the macro or a thin wrapper rather than repeating it ad hoc at each call site.

---

_Reviewed: 2026-10-04T07:57:01Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_

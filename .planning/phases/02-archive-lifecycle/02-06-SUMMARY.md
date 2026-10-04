---
phase: 02-archive-lifecycle
plan: 06
subsystem: agent-runtime
tags: [archive, agent-registry, brief, report, gitignore, startup]

requires:
  - phase: 02-archive-lifecycle
    provides: new_run_id, set_agent_state, regenerate_index, write_run_pid, mark_interrupted, ensure_gitignore, auto_prune (02-03/02-05)
  - phase: 02-archive-lifecycle
    provides: BriefMeta, render_brief_with_meta, ReportMeta, render_report (02-02)
provides:
  - "AgentRegistry run_id in D-01 format, AgentState::Interrupted, every state transition persisted to brief.md/index.md"
  - "every dispatch leaves brief.md (front-matter), index.md, and report.md regardless of how the child ended"
  - "startup scan marks stale agents interrupted and prunes old runs before any dispatch"
affects: [archive-lifecycle, agent-dispatch, startup]

tech-stack:
  added: []
  patterns:
    - "set_state releases the in-memory mutex before calling the archive persistence function, avoiding lock-across-IO"
    - "best-effort archive writes: log to stderr debug, never propagate or abort the caller"
    - "once-per-cwd gitignore registration via a static Mutex<HashSet<PathBuf>>"

key-files:
  created: []
  modified: [src/agent_registry.rs, src/tool/agent.rs, src/main.rs]

key-decisions:
  - "AgentState::as_str() keeps Rust variant names (Completed, LimitReached, ...) and only changes the on-disk string for Completed -> \"done\", avoiding churn across the existing enum call sites"
  - "run_dir_is_new is tested via run_dir.exists() before create_private_dir, so write_run_pid fires exactly once per run directory rather than once per agent"
  - "ensure_report is called from both the normal run_single return path and StateGuard::drop (the cancelled/dropped-future path), so no exit path can return without a report.md on disk"
  - "ensure_gitignore is gated by a static per-process Mutex<HashSet<PathBuf>> keyed on cwd rather than re-invoked on every dispatch, since ensure_gitignore itself is idempotent but still does a .gitignore read on every call"

requirements-completed: [ARC-01, ARC-02, ARC-03, ARC-04, ARC-05]

duration: 35min
completed: 2026-10-04
---

# Phase 02 Plan 06: Wire the Archive into the Live Runtime Summary

**Registry run-ids and every state transition now persist through `archive::set_agent_state`, every dispatch renders a front-matter brief and guarantees a report.md via a parent-side fallback, and startup marks stale agents interrupted and prunes old runs before the first tool call — turning the archive module from a library into observed runtime behavior.**

## Performance

- **Duration:** 35 min
- **Tasks:** 3
- **Files modified:** 3

## Accomplishments

- `AgentRegistry::new` uses `archive::new_run_id()` (D-01 `YYYYMMDD-HHMMSS-<8hex>` format) instead of a raw UUIDv7; the old `run_id_is_uuid_v7_and_ids_sequential` test was rewritten to assert the new shape rather than keeping a UUID assertion.
- `AgentState::Interrupted` variant added (terminal) plus `AgentState::as_str()` mapping every variant to its D-05 on-disk name (`Completed` -> `"done"`, `LimitReached` -> `"limit_reached"`, etc.), with the Rust variant names left untouched to avoid call-site churn.
- `AgentRegistry::reserve` writes `run.pid` via `archive::write_run_pid` the first time a run directory is created (not once per agent).
- `AgentRegistry::set_state` now persists every transition to the on-disk brief/index via `archive::set_agent_state`, releasing the entries mutex before doing IO; failures are logged (stderr debug) and never propagated, per the plan's best-effort requirement.
- `src/tool/agent.rs::run_single` now: resolves the archive root via `paths::project_agents_dir(cwd)` (the single D-01 root definition) instead of a local `.nanopi/agents` join; registers `.nanopi/agents/` in the project's `.gitignore` once per cwd per process via `ensure_gitignore_once` (D-07); renders `brief.md` with `render_brief_with_meta` carrying `BriefMeta { id, state: "queued", started, parent: run_id }` (D-02), then regenerates `index.md`.
- New `ensure_report(dir, id, state, error_text)` guarantees `report.md` exists before it is ever read back: if the child already wrote one it is left untouched; otherwise a report is rendered with `render_report`, pre-created at mode 0600, and written via `atomic_write` (D-04). It's invoked both at the end of `run_single` (covers killed/crashed/timed-out children that exited without a report) and from `StateGuard::drop` (covers the future being dropped/cancelled before any terminal state was recorded — e.g. parent cancellation).
- `src/main.rs`: right after installing the global `AgentRegistry`, startup now calls `archive::mark_interrupted(agents_root, run_id)` (D-06) and then `archive::auto_prune(agents_root, run_id, cfg.agent.archive_keep_days)` (D-09), both before any mode (`-p`/TUI) dispatch; both best-effort and silently return early if the archive root doesn't exist yet.

## Task Commits

1. **Task 1: registry run id, Interrupted state, persisted transitions** - `7c76496` (feat)
2. **Task 2: dispatch writes front-matter brief, fallback report, gitignore** - `6aedf2f` (feat)
3. **Task 3: startup interrupted scan and auto-prune** - `b33b674` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/agent_registry.rs` - `archive` import; `run_id` via `archive::new_run_id()`; `AgentState::Interrupted` + `as_str()`; `reserve` writes `run.pid` on first creation of a run dir; `set_state` persists via `archive::set_agent_state`; rewrote the UUID test to `run_id_matches_archive_format_and_ids_sequential`; added `reserve_writes_run_pid_once` and `set_state_persists_to_brief_and_index` tests.
- `src/tool/agent.rs` - `project_agents_dir`/`archive`/`render_brief_with_meta`/`render_report`/`BriefMeta`/`ReportMeta` imports; `ensure_gitignore_once` (static `Mutex<HashSet<PathBuf>>` guard); `ensure_report`; `StateGuard` now carries `dir` and calls `ensure_report` on drop; `run_single` rewritten to resolve the archive root via `paths::project_agents_dir`, call `ensure_gitignore_once`, render the brief with front-matter, regenerate the index, and call `ensure_report` before reading `report.md` back; added `fallback_report_written_when_child_leaves_none`, `fallback_report_not_overwritten_when_child_writes_one`, `brief_front_matter_and_index_after_dispatch`, `dispatch_registers_gitignore_entry` tests.
- `src/main.rs` - captured the `Arc<AgentRegistry>` from `AgentRegistry::new` before installing it globally (to read `run_id()` without a second `global()` call); added the `mark_interrupted`/`auto_prune` startup block immediately after, before `set_launch_spec` and any mode dispatch.

## Decisions Made

- `AgentState::as_str()` deliberately keeps existing Rust variant names and only changes the serialized string, so none of the ~15 existing call sites comparing `AgentState::Completed` etc. needed touching.
- `write_run_pid` fires once per run directory (checked via `run_dir.exists()` before `create_private_dir`), not once per agent reservation, matching the plan's "first creation of the run dir" wording.
- `ensure_report`'s pre-create-then-atomic-write sequence mirrors the existing `regenerate_index`/`report_state` pattern elsewhere in `archive.rs`, so a rename always preserves the 0600 mode.
- `ensure_gitignore_once` is a new helper distinct from `archive::ensure_gitignore` (which is itself idempotent on disk) — the in-process cache avoids a redundant `.gitignore` file read on every single dispatch in a hot parallel/chain loop.

## Deviations from Plan

None - plan executed exactly as written. All three tasks' acceptance-criteria greps pass exactly as specified (`Uuid::parse_str` absent, `archive::set_agent_state` appears once in agent_registry.rs; `ensure_gitignore`/`render_brief_with_meta`/`fn ensure_report` all present in tool/agent.rs; `archive::mark_interrupted`/`archive::auto_prune` both present in main.rs before the first mode dispatch).

## Issues Encountered

- The codebase has no `tracing` crate dependency; the plan's "logging but not propagating errors" guidance was satisfied with `eprintln!("nanopi: debug: ...")`, matching the existing debug-logging style already used elsewhere in the codebase (e.g. `nanopi: warning: ...` lines in `main.rs`).

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

- ARC-01 through ARC-05 are now observable in a real dispatch: run-id format, persisted state transitions, front-matter briefs, guaranteed reports, gitignore registration, and startup interrupted-marking + pruning are all wired end to end.
- `cargo test --lib`, `cargo test --test agent_runtime`, and `cargo test --test print_mode_e2e` all pass (912 lib tests + 1 ignored, 9 agent_runtime, 28 print_mode_e2e).
- No blockers for plan 02-07.

---
*Phase: 02-archive-lifecycle*
*Completed: 2026-10-04*

## Self-Check: PASSED

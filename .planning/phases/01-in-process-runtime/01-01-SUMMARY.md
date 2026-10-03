---
phase: 01-in-process-runtime
plan: 01
subsystem: agent-runtime
tags: [tokio, cancellation-token, semaphore, uuid-v7, subagent]

requires: []
provides:
  - "SubagentConfig under [subagent] (max_concurrency/max_live/max_turns/token_budget, D-08 defaults, zero-clamping)"
  - "SubagentRegistry: ids, run id, root cancel-token tree (stop_all), global semaphore, max_live reservation, SpawnTemplate, snapshot/usage tracking"
  - "PermissionBroker: FIFO queue, Deny-mode default, Interactive front/answer_front, cancel resolves false"
  - "FileStateTracker: per-agent read fingerprints + stale-write refusal (ISO-03)"
  - "Process-wide path_lock() extending mutation_key serialization across agents"
  - "Widened ToolContext { cwd, registry, agent_id, turn_cancel, file_state } + ToolContext::new(cwd) constructor"
affects: [01-02, 01-03, 01-04, 01-05, 01-06]

tech-stack:
  added: []
  patterns:
    - "Registry-owned Arc<Semaphore> + CancellationToken tree (child_token/stop_all swap) for cross-call concurrency limits"
    - "LiveSlot RAII guard removes its own map entry on Drop so failures cannot leak reserved capacity"
    - "std::sync::Mutex for registry/tracker internal maps, never held across .await"
    - "Process-wide OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> for cross-agent path locking"

key-files:
  created:
    - src/agent/subagent_registry.rs
    - src/tool/file_state.rs
  modified:
    - src/config.rs
    - src/agent/mod.rs
    - src/tool/mod.rs
    - src/tool/bash.rs
    - src/tool/edit.rs
    - src/tool/find.rs
    - src/tool/grep.rs
    - src/tool/ls.rs
    - src/tool/read.rs
    - src/tool/write.rs
    - src/tool/subagent.rs
    - src/agent/loop_.rs
    - src/wasm/host.rs

key-decisions:
  - "SubagentConfig merge rule follows tool_exec_mode's existing precedent: project wins wholesale if it differs from default, not per-field merge"
  - "FileStateTracker.check() treats a deleted/unreadable recorded path as a refusal (content plainly changed), not as 'nothing to compare'"
  - "hash is authoritative over mtime for staleness: an mtime mismatch with an equal hash is NOT a refusal"
  - "ToolContext::new always builds Arc<SubagentRegistry::standalone()> and Arc<FileStateTracker::default()> rather than Option — resolves research open question 2"

requirements-completed: [RT-03, RT-05, RT-06, RT-07, ISO-03]

duration: 55min
completed: 2026-10-03
---

# Phase 1 Plan 01: Shared Runtime Foundations Summary

**SubagentRegistry (ids/cancel-tree/semaphore/max_live/PermissionBroker) + config.toml's `[subagent]` section + FileStateTracker, with every one of the ~56 literal `ToolContext { cwd }` construction sites migrated to a widened `ToolContext::new(cwd)`**

## Performance

- **Duration:** 55 min
- **Started:** 2026-10-03T05:37:00Z
- **Completed:** 2026-10-03T06:32:00Z
- **Tasks:** 2
- **Files modified:** 15 (2 created, 13 modified)

## Accomplishments
- `[subagent]` config section with D-08 defaults (4/8/50/300000), zero-field clamping with a stderr warning
- `SubagentRegistry` — uuid v7 run id, `"a{N}"` sequential ids, root `CancellationToken` with `stop_all()` cancel-and-swap semantics (D-05/D-06), `Arc<Semaphore>` for `max_concurrency`, `reserve()`/`LiveSlot` RAII guard enforcing `max_live` with an in-band error (D-08, T-01-01), `SpawnTemplate` carrying parent-inheritance fields (D-04), `agents_dir()` (D-12)
- `PermissionBroker` — FIFO queue, `Deny` mode default (D-14, mirrors nanopi's existing non-interactive `-p` behavior), `Interactive` mode exposing only the front request at a time, cancellation resolves to `false` (D-13, T-01-02)
- `FileStateTracker` — per-agent read fingerprint (mtime + `DefaultHasher` content hash), `check()` refuses a write/edit when a recorded path's hash has changed or the file is gone, unread paths always pass (ISO-03, T-01-03)
- Process-wide `path_lock()` extending `mutation_key`'s intra-batch canonicalization to cross-agent serialization
- `ToolContext` widened to carry `registry: Arc<SubagentRegistry>`, `agent_id: Option<String>`, `turn_cancel: Option<CancellationToken>`, `file_state: Arc<FileStateTracker>`; new `ToolContext::new(cwd)` constructor; every literal `ToolContext { cwd: X }` site across `bash.rs`, `edit.rs`, `find.rs`, `grep.rs`, `ls.rs`, `read.rs`, `write.rs`, `subagent.rs`, `loop_.rs`, `wasm/host.rs` migrated

## Task Commits

1. **Task 1: [subagent] config + SubagentRegistry + PermissionBroker** - `ace0cb9` (feat)
2. **Task 2: FileStateTracker + widened ToolContext** - `f31807d` (feat)

_No separate RED/GREEN/REFACTOR commits — `tdd="true"` tests were written inline with the implementation in each task's single commit, consistent with this plan's "interface-first" nature (new modules, not behavior retrofits onto existing code)._

## Files Created/Modified
- `src/agent/subagent_registry.rs` - `SubagentRegistry`, `AgentLimits`, `AgentState`, `AgentSnapshot`, `SpawnTemplate`, `LiveSlot`, `PermissionBroker`, `PermissionRequest`, `BrokerMode`; 9 inline unit tests
- `src/tool/file_state.rs` - `FileFingerprint`, `FileStateTracker`, `canonical_key()`, `path_lock()`; 6 inline unit tests
- `src/config.rs` - `SubagentConfig` struct + `Default` + `clamp_zeros()`, wired into `Config`, `builtin_defaults()`, `merge()`, `load_config()`; 2 new tests, 2 existing test literals updated for the new field
- `src/agent/mod.rs` - registered `pub mod subagent_registry;`
- `src/tool/mod.rs` - widened `ToolContext`, added `ToolContext::new()`, migrated its own test site
- `src/tool/{bash,edit,find,grep,ls,read,write,subagent}.rs`, `src/agent/loop_.rs`, `src/wasm/host.rs` - migrated every `ToolContext { cwd: X }` test-construction site to `ToolContext::new(X)`

## Decisions Made
- `SubagentConfig` merge follows the `tool_exec_mode` precedent (whole-struct override when non-default) rather than per-field merging, since mixing e.g. a global `max_live` with a project `max_turns` has no obvious "right" semantics and the existing codebase convention already answers this the same way for a similar scalar-ish settings block.
- `FileStateTracker::check()` treats a deleted or unreadable recorded path as a refusal, matching "the content plainly changed" rather than silently passing because there's nothing to compare.
- Content hash (not mtime) is authoritative for staleness, per the plan's explicit instruction and research open question 3 resolution (`DefaultHasher`, no new crate).

## Deviations from Plan

None - plan executed exactly as written. Both tasks' `<acceptance_criteria>` checks all passed as specified (zero `ToolContext { cwd` literals remaining, zero `unwrap()`/`expect()` in non-test registry code, both new structs/modules present with the required public surface).

## Issues Encountered
None.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- `SubagentRegistry`, `PermissionBroker`, `FileStateTracker` and the widened `ToolContext` are the fixed interfaces Wave 2/3 plans (01-02 through 01-05) build against.
- `cargo test --lib -- --test-threads=1` is green at 812 passed / 0 failed / 1 ignored (no regressions; `mutation_key` tests unaffected).
- `cargo build --features wasm` is green.
- No blockers for 01-02 (stale-write wiring into `write`/`edit` tool execute()) or 01-03 (deny-list + hook `agent_id` wiring).

---
*Phase: 01-in-process-runtime*
*Completed: 2026-10-03*

## Self-Check: PASSED
All created files and both task commits verified present.

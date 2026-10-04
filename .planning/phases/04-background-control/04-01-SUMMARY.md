---
phase: 04-background-control
plan: 01
subsystem: agent
tags: [tokio, cancellation-token, background-tasks, agent-registry]

requires:
  - phase: 03-dynamic-subagents
    provides: AgentRegistry (reserve/set_state/snapshot/kill_all), run_single, brief/report pipeline
provides:
  - AgentRegistry::track_background/stop/stop_all/reactivate/push_report/take_reports/wait_background/install_report_sink
  - tool::agent::spawn_background and a prepare_run/run_body split of run_single
  - `background: true` arg on the `agent` tool (single mode only)
affects: [04-02, 04-03, 04-04, 04-05]

tech-stack:
  added: []
  patterns:
    - "prepare (sync) + run_body (async) split so the same async body is shared by the foreground await and a tokio::spawn'd background task"
    - "installed notify-sink wakes a consumer without carrying payload, matching plugin_send.rs's installed-sink pattern"

key-files:
  created: []
  modified:
    - src/agent_registry.rs
    - src/tool/agent.rs
    - tests/agent_spawn.rs
    - tests/agent_archive.rs

key-decisions:
  - "stop()/stop_all() kill the process group directly from the registry's own pid bookkeeping; spawn_background's cancelled branch relies on StateGuard's existing drop handler (ensure_report + set_state Stopped) rather than duplicating that logic"
  - "push_report's capped text is computed from the report.md on disk after the select! resolves (either branch), not threaded through run_body's return value, so both the normal-finish and stopped paths share one report-reading code path"

requirements-completed: [CTL-01, CTL-05]

duration: 45min
completed: 2026-10-04
---

# Phase 04 Plan 01: Background dispatch + registry control primitives Summary

**`agent` calls can now set `background: true` to get `{id, state, archive_path}` back immediately while the child keeps running under a tracked `tokio::spawn` task; the registry gained stop/stop_all/reactivate and a batched, notify-sink-backed report outbox that plans 02-05 build the rest of CTL-01..CTL-06 on.**

## Performance

- **Duration:** ~45 min
- **Tasks:** 2 completed
- **Files modified:** 4 (2 source, 2 test — 2 of the 4 were pre-existing compile breaks unrelated to this plan, fixed in-flight)

## Accomplishments

- `AgentRegistry` gained `track_background`, `stop`, `stop_all`, `reactivate`, `push_report`/`take_reports` (batched into one string per D-06), `has_pending_reports`, `install_report_sink`, and `wait_background` (awaits tracked `JoinHandle`s including ones registered mid-wait).
- `src/tool/agent.rs`'s `run_single` was split into `prepare_run` (sync: gitignore-once, reserve, brief.md, regenerate_index, build the child `Command`) and `run_body` (async: acquire_run, spawn_and_collect_with, ensure_report, cap_report, set_state) — `run_single` is now `prepare_run` + awaited `run_body`, byte-for-byte unchanged behavior.
- `spawn_background` races `run_body` against a `CancellationToken` inside `tokio::spawn`, registers the handle via `track_background`, and returns without awaiting it. Whichever branch finishes, the capped `report.md` is read back and pushed to the outbox as `[agent aN finished: <state>] <capped report>`.
- `agent` tool schema/execute gained `background: bool` (single mode only; parallel/chain reject it in-band with a clear error).

## Task Commits

1. **Task 1: Registry background tracking, stop, reactivate and report outbox** - `ca861f8` (feat)
2. **Task 2: `background: true` dispatch via spawn_background** - `34023fb` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/agent_registry.rs` - background task tracking, stop/stop_all/reactivate, report outbox with batching + notify sink, wait_background
- `src/tool/agent.rs` - prepare_run/run_body split, spawn_background, `background` arg on the tool schema, run_item_background
- `tests/agent_spawn.rs` - fixed a pre-existing compile break (missing `label` arg to `run_single`) and added `background_dispatch_does_not_block_caller`
- `tests/agent_archive.rs` - fixed a pre-existing compile break (missing `BriefMeta.label` field)

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] `tests/agent_spawn.rs` and `tests/agent_archive.rs` were already failing to compile**
- **Found during:** Task 2, running `cargo test --test agent_spawn` per the plan's verify step
- **Issue:** An earlier phase-03 change added a `label: Option<&str>` parameter to `run_single` and a `label` field to `BriefMeta`, but these two test files were never updated — `cargo test` (and `cargo build --tests`) failed before any of this plan's changes.
- **Fix:** Added the missing `None`/`label: None` argument at both call sites.
- **Files modified:** `tests/agent_spawn.rs`, `tests/agent_archive.rs`
- **Commit:** `34023fb`

No other deviations — the plan's interfaces (`track_background`, `stop`, `stop_all`, `reactivate`, `push_report`/`take_reports`, `spawn_background`, `run_body`) were implemented as specified.

## Known Stubs

None.

## Threat Flags

None — all three dispositions in the plan's threat register (T-04-01 DoS via spawn_background, T-04-02 DoS via report outbox, T-04-03 EoP via stop/stop_all) are mitigated as specified: background runs still go through `reserve`/`acquire_run` (max_live/max_concurrency unchanged), every outbox entry is built from an already `cap_report`-capped string, and `stop`/`stop_all` only ever kill a pgid recorded via `set_pid`, through the existing `kill_group` pgid > 1 guard.

## Issues Encountered

None beyond the two pre-existing compile breaks documented above.

## Next Steps

Plan 02 consumes `stop`/`stop_all`/`reactivate` and the report outbox to wire user-facing control (`/agents stop`, `/agents resume`) and injection of `take_reports()` into the next turn.

## Self-Check: PASSED

All claimed files and commit hashes verified present.

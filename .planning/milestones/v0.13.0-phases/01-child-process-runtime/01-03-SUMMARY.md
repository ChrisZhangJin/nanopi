---
phase: 01-child-process-runtime
plan: 03
subsystem: subagent-runtime
tags: [registry, process-group, config, concurrency]
requires: []
provides: [SubagentRegistry, AgentState, AgentEntry, ChildGuard, SubagentConfig, subagent_registry::global]
affects: [01-05 supervisor, TUI/print exit paths]
tech-stack:
  added: []
  patterns: [OnceLock global registry, tokio OwnedSemaphorePermit, killpg SIGKILL on Drop]
key-files:
  created: [src/subagent_registry.rs]
  modified: [src/config.rs, src/lib.rs]
decisions:
  - "[subagent] merge: project section wins wholesale if it differs from defaults, else global"
  - "Terminal states (Completed/LimitReached/Failed/Stopped) free max_live slots; Queued and Running count"
  - "kill_all marks Running entries Stopped after killpg"
metrics:
  duration: ~10min
  completed: 2026-10-03
---

# Phase 1 Plan 03: Subagent Registry Summary

Parent-side bookkeeping for `nanopi -p` children: uuid-v7 run dir with sequential `a1..` ids (0700 dirs), max_live hard cap (default 8), max_concurrency semaphore (default 4), and a ChildGuard plus `kill_all()` that SIGKILL whole process groups via `libc::killpg`. The `[subagent]` config section uses defaults 8/4/50/300_000/1800s.

## Tasks

| Task | Name | Commit |
| ---- | ---- | ------ |
| 1 | [subagent] config section | d8ef6f7 |
| 2 | SubagentRegistry and ChildGuard | 4b85e86 |

## Verification

- `cargo test --lib config::`: 23 passed, including 2 new subagent tests
- `cargo test --lib subagent_registry`: 7 passed
- No new clippy warnings in touched files

## Deviations from Plan

- [Rule 3 - Blocking] Added `subagent` to every `Config` struct literal and to `merge()` in config.rs so the crate compiles.
- Test helper: the process-group kill tests count a zombie (`/proc/<pid>/stat` state Z) as dead. A container whose PID 1 never reaps would otherwise leave the orphaned sleep looking alive.
- Added `ChildGuard::disarm()` so a child that was already reaped normally is not group-killed.

## Notes

- The plan TDD-tagged both tasks, but the tests and implementation went in the same commit for each task. There are no separate RED commits.
- RT-02 and RT-05 are not marked complete. The supervisor (01-05) still has to wire these pieces in.

## Self-Check: PASSED

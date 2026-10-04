---
phase: 01-child-process-runtime
plan: 05
subsystem: subagent-runtime
tags: [subagent, supervisor, process-group, registry, brief, failure-isolation]
requires: [01-02 brief model, 01-03 SubagentRegistry/ChildGuard, 01-04 child CLI + envelope, 01-06 report.md]
provides: [tool::subagent::spawn_and_collect, build_child_args, build_child_env, ChildLaunchSpec, ChildProgram, run_single, set_launch_spec]
affects: [src/main.rs startup/exit, tool registry construction]
tech-stack:
  added: []
  patterns: [process-wide OnceLock launch spec, injectable ChildProgram seam, in-band failure ToolOutput]
key-files:
  created: [tests/subagent_spawn.rs]
  modified: [src/tool/subagent.rs, src/tool/mod.rs, src/main.rs]
decisions:
  - "Parent settings reach the tool through a process-wide ChildLaunchSpec (set_launch_spec in main), so ToolRegistry::standard() keeps its signature"
  - "Agent system prompt goes into brief.md as the role; the --append-system-prompt temp file is gone"
  - "Unset trust is passed as --distrust so a child never prompts"
  - "max_concurrency now comes from the registry semaphore; the per-call MAX_CONCURRENCY constant is removed (MAX_TASKS=8 stays)"
metrics:
  duration: ~25min
  completed: 2026-10-03
  tasks: 2
  files: 4
---

# Phase 1 Plan 05: Parent-side subagent supervisor Summary

The `subagent` tool is now a supervised launcher. Every single, parallel or chain dispatch reserves a registry slot and creates the agent dir `.nanopi/agents/<run>/<id>/` (0700). It writes `brief.md` (0600), waits for a concurrency permit, then spawns `nanopi -p --output json --brief ... --session-file ...` in its own process group, with the parent's model, base url, api kind, trust and limits. The API key goes through `OPENAI_API_KEY` only. Any child fault comes back as an in-band `status: failed` result with the stderr tail.

## Tasks

| # | Task | Commit |
|---|------|--------|
| 1 | In-band failure mapping, process group, timeout, bounded buffers, ChildProgram seam (8 tests) | 8f66e29 |
| 2 | Registry, agent dir, brief, inherited provider, argv/env builders, report.md, real-binary integration test (7 unit + 1 integration) | f1c0324 |

## What changed

- `spawn_and_collect(Command, Duration) -> ToolOutput` never returns `Err`. Spawn error, non-zero exit (`exit code N`), signal (`killed by signal 9 (SIGKILL)`), timeout (`timed out`), oversized or garbage stdout (`unparseable output`) all map to `failed_output`. The envelope's `completed`, `limit_reached` and `failed` statuses go into metadata.
- Children get `stdin(null)`, `kill_on_drop` and `process_group(0)`. A `ChildGuard` SIGKILLs the whole group on cancel or timeout, and also after a normal exit to sweep up stray grandchildren.
- stdout is capped at 8 MiB and stderr keeps only its last 64 KiB. `report.md` is capped at 64 KiB when it is returned to the model.
- `run_single` sets registry state to Queued, then Running with the pid, then Completed, LimitReached or Failed. If the future is dropped, a StateGuard sets the state to Stopped. A dispatch beyond `max_live` gets back the in-band error "subagent limit reached". A dispatch beyond `max_concurrency` waits in a queue.
- When `report.md` exists, its text becomes the result and `report_path` is added to the metadata.
- `--no-session` is gone, and the transcript now lives in the agent dir.
- `main.rs` installs the global registry from `[subagent]` and the `ChildLaunchSpec`, and calls `kill_all()` at the single exit point.

## Deviations from Plan

1. **[Rule 3] `src/main.rs` changed.** It is not listed in `files_modified`, but the action requires capturing the parent's resolved provider settings, and those only exist in `main`. `set_launch_spec` and `set_global` are now called there. `main` already had a single exit point, so `kill_all` went there as the plan allows.
2. **[Rule 3] `src/tool/mod.rs`**: `SubagentTool` changed from a unit struct to a struct, so its registration now calls `SubagentTool::new()`.
3. **Test-seam placement.** The failure-mode tests call `spawn_and_collect` directly with `sh -c`. The registry, brief and concurrency tests use `SubagentTool::with_parts(..., ChildProgram{sh -c})` and call `run_single` with an in-memory `AgentConfig`, which avoids depending on HOME for agent discovery. `run_single` and `launcher()` are now `pub` so the integration test can use them.
4. **Acceptance grep note.** `grep '"--no-session"' src/tool/subagent.rs` still matches one line. That line is the test assertion that the argv does *not* contain the flag. No code passes it.

## Verification

- `cargo test --lib tool::subagent`: 27 passed (20 existing + Task 1, then 7 more for Task 2)
- `cargo test --test subagent_spawn spawn_real_child`: passes. It spawns the real binary against the fake endpoint and checks that `brief.md`, `transcript.jsonl` and `report.md` are under `.nanopi/agents/<run>/a1/`, that `report_path` matches, and that the registry ends Completed with a pid.
- Full `cargo test --lib`: 848 passed. `print_mode_e2e`: 26 passed.
- Clippy reports nothing new in the touched files.

## Threat mitigations

- T-01-11: stdout cap, stderr tail, per-child timeout. T-01-12: key goes in env only, checked by test `no_key_in_argv`.
- T-01-13: the `resolve_agent` trust gate is unchanged, and children never get `subagent` (01-04). T-01-14: a serde parse error maps to failed.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: src/tool/subagent.rs, tests/subagent_spawn.rs
- FOUND: commits 8f66e29, f1c0324

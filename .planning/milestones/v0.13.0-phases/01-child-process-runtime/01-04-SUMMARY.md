---
phase: 01-child-process-runtime
plan: 04
subsystem: cli
tags: [child-process, print-mode, session-file, limits, pdeathsig, rt-02, rt-04, rt-05, rt-06, rt-07]
requires: [01-01, 01-02]
provides: [--session-file, --max-turns, --token-budget, --brief, print::ChildOptions, JsonEnvelope.status/limit/report_path/agent_id/error, Session::open_or_create_at, tool::split_tool_allowlist, ToolRegistry::remove, ToolRegistry::set_plugin_allowlist, ToolRegistry::plugin_tool_names]
affects: [src/main.rs, src/mode/print.rs, src/session.rs, src/tool/mod.rs, src/tool/subagent.rs, tests/print_mode_e2e.rs]
tech-stack:
  added: []
  patterns: [plugin allowlist enforced inside register_external, child options bundled in one struct]
key-files:
  created: []
  modified: [src/main.rs, src/mode/print.rs, src/session.rs, src/tool/mod.rs, src/tool/subagent.rs, tests/print_mode_e2e.rs]
decisions:
  - "Agent mode means NANOPI_AGENT_ID is set or --brief is given. In agent mode, subagent is removed from the registry before the agent is built, so it never appears in the prompt or the tools array"
  - "--tools plugin names are enforced by a plugin allowlist stored on ToolRegistry. register_external silently skips unlisted plugin tools. Names that are neither built-in nor loaded plugin tools are a hard error, checked after extensions load"
  - "In JSON mode a run_turn error becomes status failed (with an error field) and exit code 1 instead of a bare stderr error, so the parent can always parse the envelope. Text mode is unchanged"
  - "--session-file opens or creates the transcript at that exact path and resumes it if the file is non-empty. It never calls set_active_session"
metrics:
  duration: ~25min
  completed: 2026-10-03
  tasks: 2
  files: 6
requirements: [RT-04, RT-05, RT-06, RT-07, RT-02]
---

# Phase 01 Plan 04: Child-side -p CLI surface Summary

`nanopi -p --output json --brief B --session-file T --tools L --max-turns N --token-budget K` now works as a subagent child:
- It writes its own transcript at T and never touches the active-session pointer.
- It strips `subagent` in agent mode.
- It accepts WASM plugin tool names in `--tools`.
- It ties its lifetime to the parent with PR_SET_PDEATHSIG plus a NANOPI_PARENT_PID race check.
- It reports `status`, `limit`, `report_path` and `agent_id` in the JSON envelope.

## Tasks

| Task | Name | Commit |
| ---- | ---- | ------ |
| 1 | Flags, agent mode, recursion strip, PDEATHSIG, allowlist split, envelope and session-file plumbing (+4 unit tests) | 7698d17 |
| 2 | e2e tests: session_file, session_file_resume, limit_max_turns, limit_token_budget, tools_allowlist_agent | 55564d3 |

## Verification

- `cargo test --bin nanopi args`: 2 passed.
- `cargo test --lib tool::`: 102 passed.
- `cargo test --test print_mode_e2e`: 20 passed (15 existing + 5 new).
- Full `cargo test`: all suites green (828 lib tests).
- Manual check: `NANOPI_AGENT_ID=x NANOPI_PARENT_PID=999999 nanopi -p hi` printed "parent process is gone" and exited 1.
- No new clippy warnings in the touched files. The `too_many_arguments` warning on `run_print_mode` was already there, and the new args are bundled into `ChildOptions` to avoid making it worse.

## Deviations from Plan

**1. [Rule 3] Code was split across the two task commits differently than planned.** `print.rs` needs `Session::open_or_create_at`, so the session.rs and print.rs changes for Task 2 went into the Task 1 commit to keep it compiling. The Task 2 commit holds the e2e tests.

**2. [Rule 3] The child options are a single `print::ChildOptions` struct** rather than more positional parameters on `run_print_mode`, which already had 20.

**3. [Rule 3] `JsonEnvelope` now derives `Default`.** The two test literals in `src/tool/subagent.rs` use `..Default::default()`.

**4. [Rule 2] Added an `error` field to the envelope, and JSON mode reports `status: "failed"`.** Without this, a failed child would print nothing on stdout for the parent to parse.

**5. TDD note.** Both tasks' tests passed on their first run, because the implementation was written alongside them (the plumbing in Task 1 already covered the behavior that Task 2's tests check). There is no separate RED commit.

## Known gaps / assumptions

- If nanopi itself is SIGKILLed, bash grandchildren of a child may survive. This is accepted in the plan (T-01-10).
- The non-Linux getppid()==1 poller compiles only on non-Linux unix targets and was not exercised here.
- The plugin-name path of `--tools` is covered by unit tests (split helper and register_external allowlist), not by a WASM e2e test.

## Threat mitigations

- T-01-08: in agent mode `subagent` is removed unconditionally (e2e test: the request's tools array is exactly `["read"]`).
- T-01-09: no change. The key still comes from OPENAI_API_KEY or `--api-key`. Using the env var is 01-05's job.
- T-01-10: PR_SET_PDEATHSIG(SIGKILL), plus an immediate exit if getppid differs from NANOPI_PARENT_PID.

## Self-Check: PASSED

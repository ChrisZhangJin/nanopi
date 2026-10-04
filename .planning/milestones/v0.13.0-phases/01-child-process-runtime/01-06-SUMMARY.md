---
phase: 01-child-process-runtime
plan: 06
subsystem: cli
tags: [child-process, print-mode, brief, amendments, self-check, report, rt-09]
requires: [01-02, 01-04]
provides: [mode::brief_watch::spawn_brief_watcher, print-mode brief flow, report.md]
affects: [src/mode/brief_watch.rs, src/mode/mod.rs, src/mode/print.rs, src/main.rs, tests/print_mode_e2e.rs]
tech-stack:
  added: []
  patterns: [polling file watcher feeding existing steer channel, per-turn watcher lifetime tied to receiver drop]
key-files:
  created: [src/mode/brief_watch.rs]
  modified: [src/mode/mod.rs, src/mode/print.rs, src/main.rs, tests/print_mode_e2e.rs]
decisions:
  - "A new watcher starts for each run_turn (task turn and each self-check turn). It exits when that turn drops its steer receiver, and its initial_last is re-read from the brief each time"
  - "Torn-read guard: the size must be stable across two polls AND parse_amendments_with(stable=false) drops an unterminated last section"
  - "Envelope report_path now points to <brief dir>/report.md instead of the brief itself"
  - "An unreadable brief is not fatal. The child warns on stderr, falls back to the positional message, and still writes report.md"
metrics:
  duration: ~20min
  completed: 2026-10-03
  tasks: 2
  files: 5
requirements: [RT-09]
---

# Phase 01 Plan 06: Brief-driven child Summary

`nanopi -p --brief B` now does the following:
- It takes B as its task.
- It picks up `## Amendment N` sections appended while it runs. Each is sent as a steering message at the next iteration boundary, exactly once.
- It runs at most 2 self-check turns that re-read the brief.
- It always writes a 0o600 `report.md` next to the brief, with a `- [x]` / `- [ ]` checklist.

## Tasks

| Task | Name | Commit |
| ---- | ---- | ------ |
| 1 | Brief watcher (`spawn_brief_watcher`, 5 unit tests) | cf68aea |
| 2 | Brief task, self-check, report.md in print mode (6 e2e tests) | f0fb7c7 |

## Verification

- `cargo test --lib mode::brief_watch`: 5 passed.
- `cargo test --test print_mode_e2e`: 26 passed (20 existing + 6 new brief_ tests).
- Full `cargo test`: green (833 lib tests).
- Clippy: no new warnings in touched files. The `too_many_arguments` warning on `run_print_mode` was already there.

## Deviations from Plan

**1. [Rule 3] Edited `src/main.rs`, which is not in the plan's file list.** When `--brief` is given without a message, the message becomes empty instead of being read from stdin. Before this change a child with stdin set to null exited with "no message".

**2. [Rule 1] Updated the existing `tools_allowlist_agent...` e2e assertion.** It now expects `report_path` to be `report.md` rather than the brief path, because the plan changes that field to point to the report.

**3. Added a sixth e2e test, `brief_self_check_stops_early_when_all_done`.** It checks the early stop when the reply has no `- [ ]` lines.

**4. The amendment e2e test uses a 2.5 s first-response delay instead of 1.5 s.** With the 500 ms poll and its two-poll stability rule, an amendment can take up to about 1 s to be seen, and the extra delay leaves room for that.

**5. TDD note.** Tests and implementation were written together, so there are no separate RED commits.

## Known gaps

- An amendment that arrives after the last turn has finished becomes a follow-up in `pending_follow_ups`. Print mode does not run it. It is still covered by the next self-check's re-read of the brief, if a self-check turn is still left to run.
- The error-path report (status failed) is implemented but not covered by a dedicated e2e test. The limit-path test covers the same "report without self-check" code.

## Threat mitigations

- T-01-15: the file is parsed only after its size is stable across two polls, and an unterminated final section is dropped (unit test `half_written_append_waits_until_complete`).
- T-01-16: `SELF_CHECK_TURNS = 2` is a hard cap, and no self-check runs after a limit or an error (e2e tests `brief_self_check_is_bounded_to_two_extra_turns` and `brief_report_on_limit_has_no_self_check`).

## Self-Check: PASSED

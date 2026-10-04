---
phase: 01-child-process-runtime
plan: 07
subsystem: subagent-runtime
tags: [shutdown, kill_all, sigterm, e2e, release, docs]
requires: [01-01, 01-04, 01-05, 01-06]
provides: [print-mode SIGINT/SIGTERM handler, TUI-quit kill_all, tests/subagent_runtime.rs, docs/subagents.md]
affects: [src/main.rs, src/mode/tui.rs]
tech-stack:
  added: []
  patterns: [tokio::select between print future and termination signal, one scripted fake endpoint serving parent and child]
key-files:
  created: [tests/subagent_runtime.rs, docs/subagents.md]
  modified: [src/main.rs, src/mode/tui.rs]
decisions:
  - "Print mode races run_print_mode against SIGINT/SIGTERM; on a signal it kills all children and exits 130/143. Dropping the print future also drops each ChildGuard"
  - "A child SIGKILLing itself stands in for a release-build panic (panic=abort): both builds see the child die on a signal"
metrics:
  duration: ~20min
  completed: 2026-10-03
  tasks: 2
  files: 4
requirements: [RT-01, RT-02, RT-03, RT-04, RT-05, RT-06, RT-07, RT-08, RT-09, ISO-03]
---

# Phase 01 Plan 07: Shutdown kill paths and end-to-end proof Summary

nanopi now kills every subagent child on all exit paths: TUI quit, print exit, and SIGINT/SIGTERM in print mode (exit codes 130/143). A new suite, `tests/subagent_runtime.rs`, runs real parent and child `nanopi -p` processes against one scripted fake endpoint. It proves all six Phase 1 success criteria in both debug and release builds.

## Tasks

| # | Task | Commit |
|---|------|--------|
| 1 | kill_all on TUI quit and on print-mode signals; docs/subagents.md | 6a335b2 |
| 2 | tests/subagent_runtime.rs (8 tests) | 58bb1ae |

## Tests

| Test | What it proves |
|------|----------------|
| sc1_single_parallel_chain | Each dispatch mode returns child results, and every child has an agent dir with report.md |
| sc1_child_panic_isolated | A child that SIGKILLs itself comes back as "Subagent failed ... signal 9", and the parent finishes and exits 0 |
| sc2_cancel_kills_group | SIGTERM to the parent gives exit 143. The child, its bash grandchild and the marker processes are all gone within 2s |
| sc3_transcript_isolated | transcript.jsonl is in `.nanopi/agents/<run>/a1/`, and the parent session contains no child-internal output |
| sc4_tools_limits_cap | A tool outside the allowlist gets "unknown tool" in-band, `max_turns=1` stops with "max_turns reached", and `max_live=1` refuses exactly one of two parallel tasks |
| sc5_amendment_and_checklist | An amendment appended mid-turn reaches the next child request and appears as a line in the report.md checklist |
| sc6_cross_process_stale_write | When process A edits after B has changed the file, A is refused with "file changed since you read it" |
| sc_no_subagent_ui_controls | keys.rs has no subagent bindings |

## Verification

- `cargo test --test subagent_runtime`: 8 passed. `cargo test --release --test subagent_runtime`: 8 passed.
- Full `cargo test`: all green (848 lib tests).
- No new clippy warnings in the touched files. `cargo clippy -- -D warnings` still fails on errors in other files, as noted in the plan's instructions.

## Deviations from Plan

1. **Main exit point.** 01-05 already calls `kill_all` at main's single exit point. This plan added the print-mode signal handler and a TUI call that runs before terminal teardown.
2. **Panic test.** This test uses a child that SIGKILLs itself instead of an `NANOPI_TEST_PANIC` hook, so no test-only code goes into the binary. Under panic=abort the parent sees the same thing: the child dies on a signal.
3. **Extra test.** `sc_no_subagent_ui_controls` was added to cover roadmap criterion 6.

## Known gaps

- If nanopi itself is SIGKILLed, the children's bash grandchildren can be orphaned. This is documented and accepted (T-01-18).
- PDEATHSIG is Linux-only. Other platforms poll getppid instead.

## Known Stubs

None.

## Self-Check: PASSED

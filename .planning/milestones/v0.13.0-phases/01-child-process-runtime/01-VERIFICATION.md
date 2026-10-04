---
phase: 01-child-process-runtime
verified: 2026-10-03T00:00:00Z
status: human_needed
score: 6/6 must-haves verified
overrides_applied: 0
human_verification:
  - test: "In the interactive TUI, start a long-running subagent, then quit nanopi (normal quit and Ctrl+C) and run `pgrep -af 'nanopi -p'`"
    expected: "No child nanopi processes remain"
    why_human: "TUI quit path (src/mode/tui.rs:616 kill_all) is not covered by the e2e suite, which drives print mode"
  - test: "Run a real-provider parallel dispatch and append an amendment to one child's brief.md mid-run"
    expected: "Amendment is applied at the next turn; report.md checklist lists base items and the amendment as done/not done"
    why_human: "e2e tests use a mock HTTP provider; real model behaviour with the self-check prompt is not machine-checkable"
---

# Phase 1: Child-process runtime Verification Report

**Phase Goal:** Subagents run as isolated `nanopi -p` child processes, controlled only by the orchestrator, driven by a brief file that can be amended mid-run, and can never crash nanopi or leak into the parent session.
**Status:** human_needed — all automated checks pass.
**Re-verification:** No

## Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | single/parallel/chain run as `nanopi -p` children; panic/kill reported as failed, parent keeps running (release build) | VERIFIED | `sc1_single_parallel_chain`, `sc1_child_panic_isolated` pass under `cargo test --release --test subagent_runtime`; child argv built in src/tool/subagent.rs:~590 |
| 2 | stop / cancel / exit kill children, no orphans | VERIFIED | `process_group(0)` (subagent.rs:890, registry:291), `killpg` SIGKILL (subagent_registry.rs:160), `kill_all` on exit paths (main.rs:562,600; tui.rs:618), PR_SET_PDEATHSIG (main.rs:219); `sc2_cancel_kills_group` passes. Known accepted gap: grandchild bash may orphan if nanopi is SIGKILLed |
| 3 | own transcript per child; parent session has only tool call + result | VERIFIED | `--session-file` passed to child; `sc3_transcript_isolated` passes |
| 4 | exact tools, no subagent/control tools, never prompts, turn/token limits with partial report, global cap | VERIFIED | `--tools`, `--max-turns`, `--token-budget`, `--distrust`/`--approve` args; recursion strip in main.rs; `sc4_tools_limits_cap` passes |
| 5 | amendment applied at next turn boundary; self-check; report checklist | VERIFIED | src/mode/brief_watch.rs, src/agent/brief.rs; `sc5_amendment_and_checklist` asserts amendment absent from turn 1, present later, and `- [` checklist line in report.md |
| 6 | cross-process stale write refused | VERIFIED | FileStateTracker + atomic_write (01-01); `sc6_cross_process_stale_write` passes |

Plus RT-03 (no user-facing subagent controls): `sc_no_subagent_ui_controls` passes.

## Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Full suite | `cargo test` | 848 lib + all integration suites pass, 0 failed | PASS |
| SC e2e, release | `cargo test --release --test subagent_runtime` | 8 passed, 0 failed | PASS |

## Requirements Coverage

| Req | Status | Evidence |
|-----|--------|----------|
| RT-01 | SATISFIED | SC1 tests |
| RT-02 | SATISFIED | registry + killpg + exit-path kill_all, SC2 test |
| RT-03 | SATISFIED | sc_no_subagent_ui_controls |
| RT-04 | SATISFIED | SC3 test |
| RT-05 | SATISFIED | recursion strip + live cap, SC4 test |
| RT-06 | SATISFIED | CLI limits, SC4 test |
| RT-07 | SATISFIED | `--tools` allowlist, child non-interactive, SC4 test |
| RT-08 | SATISFIED | failure mapping (8f66e29), SC1 panic test |
| RT-09 | SATISFIED | SC5 test |
| ISO-03 | SATISFIED | SC6 test |

No orphaned requirements.

## Anti-Patterns

No TBD/FIXME/XXX markers in phase files (subagent_registry.rs, brief_watch.rs, brief.rs, tool/subagent.rs). Note: when a dispatch carries an empty tool list, `--tools` is omitted and the child gets its default toolset (subagent/control tools still stripped) — consistent with agent definitions that declare no tools. Info only.

## Gaps Summary

None. Two human checks remain (TUI quit path, real-provider amendment flow).

_Verifier: Claude (gsd-verifier)_

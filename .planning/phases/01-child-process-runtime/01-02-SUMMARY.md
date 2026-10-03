---
phase: 01-child-process-runtime
plan: 02
subsystem: agent
tags: [loop-limits, hooks, brief, rt-06, rt-09]
requires: []
provides: [TurnLimits, Agent::set_max_turns, Agent::set_token_budget, Agent::last_limit_hit, HookInput.agent_id, agent::brief]
affects: [src/agent/loop_.rs, src/agent/hook.rs, src/agent/brief.rs, src/agent/mod.rs, src/agent/build.rs, src/mode/tui.rs]
tech-stack:
  added: []
  patterns: [limit check at bottom of a tool-call iteration, marker-delimited amendment parsing]
key-files:
  created: [src/agent/brief.rs]
  modified: [src/agent/loop_.rs, src/agent/hook.rs, src/agent/mod.rs, src/agent/build.rs, src/mode/tui.rs]
decisions:
  - "Limits live in one Agent field `limits: TurnLimits` (default 50 turns, no budget). Hitting a limit returns Ok and is reported by last_limit_hit(), which is reset at the start of each run_turn"
  - "Amendments are parsed only after the `<!-- nanopi:amendments -->` marker that render_brief emits. Body lines that look like structure are escaped with a backslash"
  - "parse_amendments(content) treats the content as complete. parse_amendments_with(content, false) drops a trailing section that has no newline at the end (torn-read guard)"
metrics:
  duration: ~15min
  completed: 2026-10-03
  tasks: 2
  files: 6
requirements: [RT-06, RT-09]
---

# Phase 01 Plan 02: Loop limits, hook agent_id, brief model Summary

The agent loop now has a turn cap that can be configured (`max_turns`, default 50 as before) and an optional per-turn input+output token budget. `last_limit_hit()` reports which limit stopped the turn. Hook payloads carry `agent_id` only when `NANOPI_AGENT_ID` is set. The new `agent::brief` module renders briefs and checklist reports, and appends and parses numbered amendments.

## Tasks

| Task | Name | Commit |
| ---- | ---- | ------ |
| 1 | Turn limit and token budget in the agent loop (+3 tests) | e636956 |
| 2 | Hook agent_id + brief/amendment/report model (+7 tests) | 353b218 |

## Verification

- `cargo test --lib agent::` passed: 214 passed. The existing stuck-loop test passes unchanged.
- `cargo build` succeeded. No new clippy warnings in the touched files.

## Deviations from Plan

**1. [Rule 3] Limits are one `limits: TurnLimits` field, not two loose fields.** Every `Agent { .. }` struct literal needed a new field: about 27 in loop_.rs tests, plus build.rs and tui.rs. One field with a Default keeps that change to a single line per literal.

**2. Added `parse_amendments_with(content, stable)`.** The plan's `parse_amendments(content)` signature has no `stable` flag, so the torn-read guard is in this sibling function.

**3. Amendment marker.** To guarantee that headings in the task text are never treated as amendments, `render_brief` ends with `<!-- nanopi:amendments -->`. Only text after that marker is parsed.

## Threat mitigations

- T-01-04: max_turns and token_budget are enforced inside the loop.
- T-01-05: append_amendment opens the file with mode 0o600, and a test checks this.

## Self-Check: PASSED

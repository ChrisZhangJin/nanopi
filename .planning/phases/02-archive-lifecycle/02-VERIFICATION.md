---
phase: 02-archive-lifecycle
verified: 2026-10-04T00:00:00Z
status: passed
score: 5/5 must-haves verified
overrides_applied: 0
---

# Phase 2: Archive & lifecycle Verification Report

**Phase Goal:** Every agent leaves an inspectable, loss-proof `.md` trail with a clear lifecycle state.
**Verified:** 2026-10-04
**Status:** passed
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths (ROADMAP Success Criteria / ARC-01..05)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Starting an agent creates `.nanopi/agents/<run>/<id>/brief.md` with task, role, tools, model; amendments appended | ✓ VERIFIED | `src/agent/brief.rs` (`render_brief_with_meta`, `append_amendment`, `BriefMeta`/`BriefSpec`), wired in `src/tool/agent.rs:748` at dispatch. Unit tests cover front-matter round-trip and amendment heading parsing (both new and legacy `## Amendment N` forms). Threat T-02-01 (field forging) covered by tests. |
| 2 | `report.md` exists on disk before the parent sees the result | ✓ VERIFIED | `src/mode/print.rs` writes report.md via `file_state::atomic_write` (temp file + fsync + rename) before emitting the JSON envelope on done/failed/limit_reached paths; falls back to plain write on atomic-write failure. `src/tool/agent.rs` has a parent-side fallback (`fallback_report_written_when_child_leaves_none` test) that writes report.md if the child left none, before returning the tool result. Both test paths pass. |
| 3 | `.nanopi/agents/` appears in `.gitignore` and never shows up in agents' grep/glob results | ✓ VERIFIED | `archive::ensure_gitignore` appends the entry once (idempotent, tested in `gitignore_registered_once`), called from `src/tool/agent.rs::ensure_gitignore_once` on first archive creation. `src/tool/grep.rs` and `src/tool/find.rs` unconditionally exclude `paths::project_agents_dir(&ctx.cwd)` from both ripgrep args and fallback walk, verified path-based (not name-based) — `.nanopi/skills` stays searchable per plan 02-02 tests. |
| 4 | After killing nanopi mid-run, the next start marks those agents `interrupted` without re-running them | ✓ VERIFIED | `archive::mark_interrupted` rewrites non-terminal brief states in non-current runs to `interrupted` and regenerates index.md; wired at `src/main.rs:484` before dispatch loop starts. Integration test `stale_running_agent_marked_interrupted_not_rerun` in `tests/agent_archive.rs` proves no report.md/transcript.jsonl is ever created by this path (never re-run), and a second pass is a no-op. |
| 5 | `/agents clean` keeps the most recent N runs or removes all | ✓ VERIFIED | `archive::clean_runs` with `CleanMode::{AllButCurrent, OlderThanDays, KeepRecent}` implemented and reports bytes/count removed. Wired to `/agents clean[--older N\|--keep N]` via `SlashCmd::Agents` → `parse_agents_args` → `KeyAction::CleanAgents` → `archive::clean_runs` in `src/mode/tui.rs`. Bad-argument parsing tested (`parse_agents_args("").is_err()`, etc.) and never deletes on error. Auto-prune (`agent.archive_keep_days`, default 2, 0 disables) wired at startup in `src/main.rs`. |

**Score:** 5/5 truths verified

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `src/agent/brief.rs` | BriefMeta, ReportMeta, render_brief_with_meta, parse_front_matter, render_report, brief_write_lock | ✓ VERIFIED | 688 lines, all symbols present, used by archive.rs and tool/agent.rs |
| `src/paths.rs` | `project_agents_dir(cwd)` | ✓ VERIFIED | 246 lines, single definition used consistently across grep/find/archive |
| `src/tool/grep.rs` / `src/tool/find.rs` | unconditional agents-root exclusion | ✓ VERIFIED | `project_agents_dir` referenced in both rg-args and fallback-walk code paths plus tests |
| `src/archive.rs` | new_run_id, TERMINAL_STATES, is_terminal_state, set_agent_state, regenerate_index, mark_interrupted, ensure_gitignore, write_run_pid, run_is_live, CleanMode, CleanReport, clean_runs, auto_prune, format_bytes | ✓ VERIFIED | 955 lines, all symbols present; 27 unit tests pass |
| `src/mode/print.rs` | write_report_durable / atomic_write usage, report state mapping | ✓ VERIFIED | 961 lines, uses `file_state::atomic_write`, tests pass |
| `src/config.rs` | `AgentConfig.archive_keep_days` | ✓ VERIFIED | field present, default 2, 0 disables (tested in archive.rs) |
| `src/agent_registry.rs` | run-id via archive::new_run_id, AgentState::Interrupted, set_state persistence | ✓ VERIFIED | 459 lines, `Interrupted` variant present and wired |
| `src/tool/agent.rs` | render_brief_with_meta usage, fallback report, ensure_gitignore call | ✓ VERIFIED | 1907 lines, all three wiring points confirmed at listed line numbers |
| `src/main.rs` | startup mark_interrupted + auto_prune | ✓ VERIFIED | both calls present before dispatch, lines 484-490 |
| `src/mode/tui.rs` | SlashCmd::Agents, KeyAction::CleanAgents, parse_agents_args | ✓ VERIFIED | all present and wired to archive::clean_runs |
| `tests/agent_archive.rs` | integration tests, min 80 lines | ✓ VERIFIED | 153 lines, 4 tests, all pass (interrupted-marking, prune, clean, gitignore) |

### Key Link Verification

| From | To | Via | Status | Details |
|------|-----|-----|--------|---------|
| `brief.rs::append_amendment` | `brief_write_lock` | process-wide mutex | ✓ WIRED | confirmed in source, tests pass |
| `tool/grep.rs`, `tool/find.rs` | `paths::project_agents_dir` | exclusion check | ✓ WIRED | grep confirms calls at multiple sites |
| `archive.rs::set_agent_state` | `file_state::atomic_write` / `brief_write_lock` | atomic rewrite | ✓ WIRED | present in archive.rs |
| `mode/print.rs` | `file_state::atomic_write` | report.md write | ✓ WIRED | confirmed, with plain-write fallback on failure |
| `archive.rs::auto_prune` | `clean_runs` | `CleanMode::OlderThanDays` | ✓ WIRED | confirmed and tested |
| `agent_registry.rs::set_state` | `archive::set_agent_state` | persist state transitions | ✓ WIRED | confirmed |
| `main.rs` | `archive::mark_interrupted` / `auto_prune` | startup, before dispatch | ✓ WIRED | confirmed lines 484-490 |
| `mode/tui.rs` | `archive::clean_runs` | `KeyAction::CleanAgents` handler | ✓ WIRED | confirmed line 2775 |

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|------------|-------------|--------|----------|
| ARC-01 | 02-01, 02-06 | brief.md written at start with task/role/tools/model, amendments appended | ✓ SATISFIED | brief.rs + tool/agent.rs dispatch wiring |
| ARC-02 | 02-04, 02-06 | report.md written before result returned to parent | ✓ SATISFIED | print.rs atomic write + tool/agent.rs fallback |
| ARC-03 | 02-02, 02-03, 02-06 | `.nanopi/agents/` in `.gitignore`, excluded from grep/glob | ✓ SATISFIED | ensure_gitignore + grep.rs/find.rs exclusion |
| ARC-04 | 02-03, 02-06, 02-07 | agents running at exit marked `interrupted`, never re-run | ✓ SATISFIED | mark_interrupted wired at startup; integration test proves no re-run |
| ARC-05 | 02-05, 02-07 | `/agents clean` keeps N most recent or removes all | ✓ SATISFIED | clean_runs + TUI slash command wiring |

No orphaned requirements found — all five ARC-0x IDs are claimed by plans in this phase and covered above.

### Anti-Patterns Found

None. Grep for `TBD|FIXME|XXX|TODO|HACK|PLACEHOLDER|not yet implemented` across all phase-touched files (`src/archive.rs`, `src/agent/brief.rs`, `src/mode/print.rs`, `src/paths.rs`, `src/tool/grep.rs`, `src/tool/find.rs`, `src/agent_registry.rs`, `src/tool/agent.rs`, `src/config.rs`, `tests/agent_archive.rs`) returned no matches.

### Test Execution

Ran `cargo build --quiet` (clean) and `cargo test --quiet` (full suite): 917 unit tests passed, 1 ignored, 0 failed, plus integration suites including `tests/agent_archive.rs` (4/4 passed). Targeted `archive::` unit tests: 27/27 passed.

### Human Verification Required

None. All five success criteria are mechanically verifiable via unit/integration tests and source inspection, and all pass.

### Gaps Summary

No gaps. All observable truths, artifacts, and key links for Phase 2 are present, substantive, and wired, with passing automated test coverage (unit + integration) and no outstanding debt markers.

---

_Verified: 2026-10-04_
_Verifier: Claude (gsd-verifier)_

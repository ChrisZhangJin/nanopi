---
phase: 02-archive-lifecycle
plan: 07
subsystem: agent-runtime
tags: [archive, slash-command, tui, integration-tests]

requires:
  - phase: 02-archive-lifecycle
    provides: "archive::CleanMode, clean_runs, mark_interrupted, auto_prune, ensure_gitignore, format_bytes (02-01..02-06)"
provides:
  - "/agents clean slash command (D-10, ARC-05) exposed in the TUI palette"
  - "tests/agent_archive.rs: end-to-end proof of D-06/D-07/D-09/D-10 against the public lib API"
affects: [tui-palette, archive-lifecycle]

tech-stack:
  added: []
  patterns:
    - "parse_agents_args follows the strict-parse-or-usage shape already used nowhere else in tui.rs but mirrors /name's bare-vs-arg split; any unrecognized shape returns the same usage string rather than guessing"
    - "KeyAction::CleanAgents/AgentsUsage follow the existing dispatch_slash -> KeyAction -> handle_action three-step pattern used by /name"

key-files:
  created: [tests/agent_archive.rs]
  modified: [src/mode/tui.rs, src/command.rs]

key-decisions:
  - "Added \"agents\" to command::RESERVED_COMMAND_NAMES (not in the plan's files_modified) because the existing reserved_command_names_match_the_builtin_palette test asserts this list stays in sync with slash_items() — Rule 3 blocking fix, not a deviation in behavior"
  - "CleanAgents' current-run id falls back to an empty string when agent_registry::global() is None; clean_runs' own is_run_id_shaped/run_is_protected checks mean an empty string never matches a real run and never unprotects a live one, so this is safe rather than a workaround"

requirements-completed: [ARC-04, ARC-05]

duration: 25min
completed: 2026-10-04
---

# Phase 02 Plan 07: /agents clean + Phase-Level Integration Tests Summary

**`/agents clean [--older <days> | --keep <n>]` is now a real TUI slash command backed by `archive::clean_runs`, and `tests/agent_archive.rs` proves D-06/D-07/D-09/D-10 end to end against the public lib API.**

## Performance

- **Duration:** 25 min
- **Tasks:** 2
- **Files modified:** 3 (1 created)

## Accomplishments

- `SlashCmd::Agents` + a `/agents` palette entry (`"Agent archive: clean [--older <days> | --keep <n>]"`), with `"agents"` added to `command::RESERVED_COMMAND_NAMES` so a plugin can never claim the name.
- `parse_agents_args(arg: &str) -> Result<archive::CleanMode, String>`: `"clean"` → `AllButCurrent`, `"clean --older <u64>"` → `OlderThanDays`, `"clean --keep <usize>"` → `KeepRecent`; anything else (bare, `"list"`, non-numeric, trailing garbage) → `Err(AGENTS_USAGE)`, deleting nothing (T-02-19).
- `KeyAction::CleanAgents(CleanMode)` / `KeyAction::AgentsUsage(String)` with `dispatch_slash` and `handle_action` arms following the existing `/name` three-step pattern; the handler resolves `root = paths::project_agents_dir(&app.cwd)` and `current = agent_registry::global().map(run_id)` (empty string if no live registry), calls `archive::clean_runs`, and prints `"Removed N run(s), <size> freed"` plus a `"Skipped live: …"` line when any runs were protected.
- `tests/agent_archive.rs` (4 tests, public API + tempfile only): `stale_running_agent_marked_interrupted_not_rerun` (D-06 — proves a stale `running` brief is rewritten `interrupted`, no `report.md`/`transcript.jsonl` ever appears, and a second pass is a no-op), `prune_respects_keep_days_and_current` (D-09 — only runs past the cutoff are removed, current is always kept, `keep_days == 0` disables pruning), `clean_modes_report_count_and_size` (D-10/ARC-05 — `OlderThanDays`, `KeepRecent`, `AllButCurrent` each report accurate `removed_runs`/`removed_bytes`), `gitignore_registered_once` (D-07 — a second `ensure_gitignore` call appends no duplicate entry).

## Task Commits

1. **Task 1: /agents clean slash command** - `ef06d6b` (feat)
2. **Task 2: archive integration tests** - `b274cc7` (test)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/mode/tui.rs` - `SlashCmd::Agents` variant + palette entry; `AGENTS_USAGE` const + `parse_agents_args`; `KeyAction::CleanAgents`/`AgentsUsage` + their `dispatch_slash`/`handle_action` arms; 5 new tests (`agents_clean_parses_bare_and_flags`, `agents_clean_rejects_bad_args`, `dispatch_agents_clean_yields_clean_agents_action`, `dispatch_agents_bad_arg_yields_usage`, `agents_in_palette`); `"/agents"` added to the `typing_a_command_name_preselects_it` list.
- `src/command.rs` - `"agents"` added to `RESERVED_COMMAND_NAMES` (Rule 3 blocking fix: the existing sync test fails without it).
- `tests/agent_archive.rs` (new) - 4 integration tests covering D-06, D-07, D-09, D-10/ARC-04/ARC-05 against `nanopi::archive` and `nanopi::agent::brief`'s public API.

## Decisions Made

- Fully-qualified `crate::archive::...` / `crate::paths::...` / `crate::agent_registry::...` paths were used in `tui.rs` instead of new `use` statements, to avoid any name collision with the file's existing large import block.
- Test run ids for aged runs are built with `chrono::Local::now() - Duration::days(n)` formatted into the exact `YYYYMMDD-HHMMSS-<8hex>` shape `is_run_id_shaped`/`run_started` expect, rather than touching the clock or injecting a fake "now".

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Added "agents" to `command::RESERVED_COMMAND_NAMES`**
- **Found during:** Task 1, running `cargo test --lib mode::tui::`
- **Issue:** `reserved_command_names_match_the_builtin_palette` (a pre-existing guard test) failed once `/agents` was added to `slash_items()`, because `RESERVED_COMMAND_NAMES` in `src/command.rs` is a separate hand-maintained list the test asserts stays in sync.
- **Fix:** Added `"agents"` to `RESERVED_COMMAND_NAMES` in `src/command.rs`.
- **Files modified:** `src/command.rs`
- **Commit:** `ef06d6b`

## Issues Encountered

None beyond the Rule 3 fix above.

## User Setup Required

None.

## Next Phase Readiness

- ARC-01 through ARC-05 are now fully observable and testable: every archive-lifecycle behavior from D-01 through D-10 has either a unit test (`src/archive.rs`) or a phase-level integration test (`tests/agent_archive.rs`), and the user-facing cleanup command exists in the TUI.
- Full gate passes: `cargo test --lib` (917 passed, 1 ignored), `cargo test --test agent_runtime` (9 passed), `cargo test --test print_mode_e2e` (28 passed), `cargo test --test agent_archive` (4 passed).
- This is the last plan of Phase 02 (7 of 7). No blockers for the next phase.

---
*Phase: 02-archive-lifecycle*
*Completed: 2026-10-04*

## Self-Check: PASSED

---
phase: 02-archive-lifecycle
plan: 05
subsystem: infra
tags: [archive, retention, config, toml, chrono]

requires:
  - phase: 02-archive-lifecycle
    provides: run_started, run_is_live, is_terminal_state, front_matter_get (02-03)
provides:
  - "AgentConfig.archive_keep_days config knob (default 2, 0 disables)"
  - "archive::CleanMode, CleanReport, clean_runs, auto_prune, format_bytes"
affects: [archive-lifecycle, cli-clean-command]

tech-stack:
  added: []
  patterns:
    - "run-id-shaped-name guard scopes destructive filesystem ops to the archive root only"
    - "symlink_metadata (never metadata) for both sizing and liveness-adjacent filesystem walks, so symlinks can't be used to read/delete outside the archive"

key-files:
  created: []
  modified: [src/archive.rs, src/config.rs]

key-decisions:
  - "archive_keep_days deliberately excluded from the zero-cap rejection list in validate_agent — 0 is a valid, meaningful value (disables auto-prune) per D-09"
  - "CleanMode::KeepRecent added beyond D-10's two modes to cover ARC-05/roadmap SC-5 (keep N most recent runs); does not contradict D-10"
  - "Age fallback for malformed run-id timestamps uses directory mtime via symlink_metadata, never metadata, to avoid following a symlinked run dir"

requirements-completed: [ARC-05]

duration: 25min
completed: 2026-10-04
---

# Phase 02 Plan 05: Archive Deletion (auto-prune + clean_runs) Summary

**Added `archive_keep_days` config knob plus `clean_runs`/`auto_prune`/`format_bytes` in `src/archive.rs`, giving nanopi both automatic startup pruning and an on-demand clean command with all-but-current, older-than-N-days, and keep-N-most-recent selection modes.**

## Performance

- **Duration:** 25 min
- **Tasks:** 2
- **Files modified:** 2

## Accomplishments
- `AgentConfig.archive_keep_days: u64` defaults to 2; `0` is valid and disables auto-prune, and is explicitly kept out of the existing zero-cap validation rejection list.
- `clean_runs(agents_root, current_run, mode)` with `CleanMode::{AllButCurrent, OlderThanDays(u64), KeepRecent(usize)}`, returning a `CleanReport { removed_runs, removed_bytes, skipped_live }`.
- Safety invariants: only direct children of `agents_root` whose name matches the `YYYYMMDD-HHMMSS-<8hex>` run-id shape are candidates; symlinked entries are skipped outright; a run with a live `run.pid` or any non-terminal agent brief is protected and reported in `skipped_live`, never deleted.
- `dir_size` sums file bytes via `symlink_metadata`, never following symlinks, so sizing (and by extension deletion scope) cannot be steered outside the run directory.
- `auto_prune(agents_root, current_run, keep_days)` is a pure no-op (empty report, no directory read) when `keep_days == 0`; otherwise delegates to `clean_runs(.., OlderThanDays(keep_days))`.
- `format_bytes` renders B / KiB / MiB / GiB with one decimal place above bytes.

## Task Commits

1. **Task 1: archive_keep_days config** - `b9c3286` (feat)
2. **Task 2: clean_runs and auto_prune** - `52c22fb` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified
- `src/config.rs` - `AgentConfig.archive_keep_days` field, Default impl, two new tests (defaults include the field; `archive_keep_days = 0` parses and loads without error).
- `src/archive.rs` - `is_run_id_shaped`, `run_effective_start`, `run_is_protected`, `dir_size`, `CleanMode`, `CleanReport`, `clean_runs`, `auto_prune`, `format_bytes`, plus 12 new tests covering every behavior bullet in the plan (OlderThanDays, AllButCurrent, KeepRecent, live-pid skip, non-terminal-brief skip, byte reporting, non-run-id-shaped entries left alone, missing-root empty report, auto_prune(0) no-op, auto_prune delegation, format_bytes unit table).

## Decisions Made
- `archive_keep_days` is not added to `validate_agent`'s zero-cap rejection list, since 0 is the documented "disable pruning" sentinel (D-09), unlike `max_live`/`timeout_secs` where 0 breaks the feature entirely.
- `CleanMode::KeepRecent` was added beyond the two modes D-10 named, to satisfy ARC-05/roadmap SC-5's "keep most recent N runs" requirement — the plan's own `<action>` called this out as an intentional, non-contradicting extension.
- Candidate age falls back to directory mtime (via `symlink_metadata`, not `metadata`) only when the run-id timestamp prefix fails to parse, keeping the symlink-safety invariant consistent across both the deletion-candidate and sizing code paths.
- Switched `sort_by` to `sort_by_key(Reverse(..))` in `KeepRecent` per `cargo clippy`'s `unnecessary_sort_by` lint — no behavior change.

## Deviations from Plan

None - plan executed exactly as written (one clippy-driven micro-refactor, not a deviation rule trigger: `sort_by(|a,b| b.cmp(a))` → `sort_by_key(Reverse)`, behaviorally identical, verified by re-running `cargo test --lib archive::` afterward).

## Issues Encountered
None.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- `clean_runs`/`auto_prune`/`format_bytes` are ready to be wired into a CLI `clean` subcommand and into startup (calling `auto_prune` with `config.agent.archive_keep_days`) in a later plan — this plan only adds the library functions and config knob, per its stated scope (`files_modified: [src/archive.rs, src/config.rs]`).
- No blockers.

---
*Phase: 02-archive-lifecycle*
*Completed: 2026-10-04*

## Self-Check: PASSED

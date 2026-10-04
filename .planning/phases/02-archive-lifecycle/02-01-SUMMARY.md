---
phase: 02-archive-lifecycle
plan: 01
subsystem: agent
tags: [brief-format, report-format, front-matter, tdd]

# Dependency graph
requires:
  - phase: 01
    provides: BriefSpec, render_brief, append_amendment, parse_amendments_with, ChecklistItem, render_report (old signature)
provides:
  - "pub struct BriefMeta and render_brief_with_meta emitting the D-02 hand-written front-matter block"
  - "pub fn parse_front_matter / front_matter_get / set_front_matter_field, the single parser for both brief.md and report.md"
  - "pub fn brief_write_lock, a process-wide mutex append_amendment now holds for the duration of its write"
  - "timestamped `## Amendment N (<rfc3339>)` headings, with parse_amendments_with still accepting the legacy bare `## Amendment N` form"
  - "pub struct ReportMeta and a new render_report(meta, summary, files_changed, open_issues, items) signature per D-03, dropping the old `## Status` section"
affects: [03-archive-lifecycle-state-updates, 04-archive-lifecycle-report-writer]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Front-matter values always pass through fm_value (whitespace-collapse, 120-char cap, empty -> `(none)`) before interpolation, which is the T-02-01 injection mitigation: no value can contain a newline so none can forge a key line or the closing `---`"
    - "parse_front_matter/set_front_matter_field only ever look at the leading `---`/`---` block, so a `state: done` string anywhere in the body (task text, role, amendments) is inert"

key-files:
  created: []
  modified:
    - src/agent/brief.rs
    - src/mode/print.rs

key-decisions:
  - "render_brief stays the body renderer; render_brief_with_meta wraps it rather than duplicating the section logic (keeps one place that defines the brief body)"
  - "set_front_matter_field rewrites only the matching key line and returns None both when there is no front-matter block and when the block has no closing `---` (treated as malformed, same as absent)"
  - "print.rs call sites pass empty files_changed/open_issues slices and the existing `status` string as-is; wiring real file/issue lists and proper state mapping is explicitly deferred to plan 04 per the plan's own scope note"

patterns-established:
  - "fm_value: the single sanitizer for any front-matter value, used by both render_brief_with_meta and render_report"

requirements-completed: [ARC-01, ARC-02]

# Metrics
duration: 12min
completed: 2026-10-04
---

# Phase 02 Plan 01: Brief/Report Front-Matter Summary

**brief.md and report.md now carry a hand-written (no-YAML) front-matter block parsed by one function, amendments carry an RFC3339 timestamp while still accepting the legacy heading, and front-matter values are injection-proof by construction.**

## Performance

- **Duration:** 12 min
- **Started:** 2026-10-04T07:08:00Z
- **Completed:** 2026-10-04T07:20:00Z
- **Tasks:** 2 (both TDD: RED + GREEN commits each)
- **Files modified:** 2

## Accomplishments
- `BriefMeta` / `render_brief_with_meta` add the D-02 front-matter block (id, role, model, tools, state, started, parent) ahead of the unchanged brief body
- One parser (`parse_front_matter`, `front_matter_get`, `set_front_matter_field`) reads/rewrites the leading block only, immune to a `state: done` string anywhere in the body
- `append_amendment` now timestamps headings (`## Amendment N (<rfc3339>)`) under a new process-wide `brief_write_lock`, and `parse_amendments_with` accepts both the timestamped and legacy bare heading forms
- `ReportMeta` / `render_report` add the D-03 front-matter block (id, state, ended, turns, tokens, optional worktree/branch) and reorder the body to Summary -> Files changed -> Open issues -> Checklist, dropping the old `## Status` section
- `src/mode/print.rs`'s two call sites updated to the new `render_report` signature; crate builds and all tests pass

## Task Commits

Each task was committed as a RED/GREEN pair:

1. **Task 1: brief.md front-matter and timestamped amendments**
   - `025af80` test(02-01): add failing tests for brief front-matter and timestamped amendments
   - `73030ba` feat(02-01): add brief.md front-matter and timestamped amendments
2. **Task 2: report.md front-matter and section order**
   - `27b62c5` test(02-01): add failing tests for report.md front-matter and section order
   - `0e6bc68` feat(02-01): add report.md front-matter and D-03 section order

## Files Created/Modified
- `src/agent/brief.rs` - BriefMeta, ReportMeta, render_brief_with_meta, parse_front_matter, front_matter_get, set_front_matter_field, brief_write_lock, fm_value; append_amendment timestamping; new render_report signature and section order
- `src/mode/print.rs` - both render_report call sites updated to the new signature (ReportMeta, empty files/issues slices for now)

## Decisions Made
- `render_brief` remains the single body renderer; `render_brief_with_meta` composes it rather than re-implementing the body.
- `set_front_matter_field` returns `None` uniformly for "no block" and "block never closes" — a malformed block is treated as absent rather than partially rewritten.
- Wiring real `files_changed`/`open_issues` content and proper state-string mapping into print.rs is explicitly deferred to plan 04, matching the plan's own instruction ("here use `status` as-is").

## Deviations from Plan

None - plan executed exactly as written. The TDD RED tests failed to compile (missing functions/types) as expected before each GREEN implementation; one test's own expected amendment order was corrected (document order, not numeric order) before the GREEN commit — this was a test-authoring correction, not a deviation from the plan's behavior spec.

## Issues Encountered

None.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

- `src/agent/brief.rs` now exposes the full front-matter/parse/rewrite contract (`BriefMeta`, `ReportMeta`, `render_brief_with_meta`, `parse_front_matter`, `set_front_matter_field`, `brief_write_lock`) that plan 03 (state updates) and plan 04 (report writer) are expected to consume.
- `cargo test --lib agent::brief::` (22 tests) and `cargo test --lib mode::print::` (4 tests) pass; `cargo build` succeeds; `grep -c "yaml" Cargo.toml` is 0.
- No blockers for plan 02/03/04.

---
*Phase: 02-archive-lifecycle*
*Completed: 2026-10-04*

## Self-Check: PASSED

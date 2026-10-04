---
phase: 02-archive-lifecycle
plan: 02
subsystem: tool
tags: [grep, find, ripgrep, path-exclusion, security]

# Dependency graph
requires:
  - phase: 02-archive-lifecycle
    provides: plan 01's archive write path and fm_value sanitizer under .nanopi/agents
provides:
  - "paths::project_agents_dir(cwd) — single definition of the archive root (D-01)"
  - "grep/find unconditional, path-based exclusion of .nanopi/agents/ (D-08)"
affects: [02-archive-lifecycle, later plans that read/list the archive deliberately via explicit tools]

# Tech tracking
tech-stack:
  added: []
  patterns: ["path-based exclusion checked outside the `all` flag branch so it cannot be disabled by a tool argument"]

key-files:
  created: []
  modified: [src/paths.rs, src/tool/find.rs, src/tool/grep.rs]

key-decisions:
  - "Exclusion compares both lexical (joined-from-cwd) and canonicalized paths so a symlinked cwd still hides the archive"
  - "A base/search path that is itself inside .nanopi/agents returns an empty result instead of an error, matching 'archive invisible' rather than surfacing its existence via an error message"
  - "ripgrep_args takes an optional agents_glob computed as the archive root's path relative to the rg search root, only when the archive root is actually inside that root"

requirements-completed: [ARC-03]

# Metrics
duration: 25min
completed: 2026-10-04
---

# Phase 02 Plan 02: Archive Exclusion from grep/find Summary

**Added `paths::project_agents_dir` and made both the built-in `grep`/`find` tools and ripgrep's arg list skip `.nanopi/agents/` unconditionally, independent of the `all` flag.**

## Performance

- **Duration:** ~25 min
- **Tasks:** 2
- **Files modified:** 3

## Accomplishments
- `paths::project_agents_dir(cwd)` is now the single source of truth for the archive root (D-01), mirroring `project_skills_dir`.
- `find`'s walk skips any entry equal to or nested under the archive root, checked outside the `if !all` block, so `all=true` cannot surface it; a base path inside the archive returns an empty result.
- `grep`'s ripgrep path always appends a `--glob=!/<rel-path-to-agents-root>` exclusion (computed relative to the rg search root, only when the archive root is actually inside it) regardless of `all`; the fallback builtin walk applies the same path-based skip.
- `.nanopi/skills` remains fully searchable under `all=true` in both tools, since the exclusion is path-based (matches only `.nanopi/agents`), not name-based.

## Task Commits

Each task was committed atomically (TDD: test then feat):

1. **Task 1: project_agents_dir and find exclusion**
   - `e7f4c39` test(02-02): add failing test for archive exclusion in find
   - `09cde03` feat(02-02): add project_agents_dir and exclude archive from find (D-08)
2. **Task 2: grep exclusion in rg args and fallback walk**
   - `e2674ae` feat(02-02): exclude agents archive from grep rg args and fallback walk (D-08) (includes the new `agents_archive_hidden_even_with_all_true` test, added in the same commit after verifying it failed pre-implementation)

## Files Created/Modified
- `src/paths.rs` - Added `project_agents_dir(cwd)` next to `project_skills_dir`.
- `src/tool/find.rs` - `walk` now takes `agents_root`, skips it unconditionally; `execute` short-circuits to an empty result when `base` is inside the archive; new tests `agents_archive_hidden_even_with_all_true`, `agents_archive_base_returns_empty`.
- `src/tool/grep.rs` - `ripgrep_args` gained an `agents_glob: Option<&str>` parameter always appended as `--glob=!/<glob>`; `search_ripgrep` computes that glob relative to the rg search root; `search_builtin`/`walk` thread an `agents_root` and skip it unconditionally via new `is_within_agents_root`; `execute` short-circuits to an empty result when the explicit `path` argument is inside the archive; existing test call sites (`run`, `compat_flags_are_present`) updated for the new signatures; new test `agents_archive_hidden_even_with_all_true` covering the fallback walk, the explicit-path-inside-archive case, and the rg glob.

## Decisions Made
- Both path checks compare lexical paths first (fast, no syscall) and fall back to canonicalized comparison only when both sides canonicalize successfully, so a symlinked cwd is still covered without making every walk iteration pay for a `canonicalize` call that will usually succeed trivially.
- Chose to make an explicit search `path` inside the archive return an empty result rather than an error — consistent with "archive invisible to built-in search," and avoids an error message that would itself disclose the archive's existence/location.

## Deviations from Plan

None - plan executed exactly as written (TDD RED/GREEN per task, both tasks' acceptance criteria met verbatim).

## Issues Encountered
- Initial test draft used pattern `.*` against grep, which `regex::RegexBuilder::unicode(false)` rejects ("pattern can match invalid UTF-8"). Switched the test to a concrete literal pattern (`secret`) — not a deviation in the task's behavior, just a corrected test pattern.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness
- `paths::project_agents_dir` is available for later archive-lifecycle plans (e.g. retention/cleanup) needing the canonical root.
- `grep`/`find` are confirmed to never leak archive contents under any flag; `cargo test --lib tool::` (123 passed, 1 ignored) and `cargo test --lib paths::` (7 passed) both green; no new clippy warnings in the touched files.

---
*Phase: 02-archive-lifecycle*
*Completed: 2026-10-04*

## Self-Check: PASSED
All referenced files and commit hashes verified present.

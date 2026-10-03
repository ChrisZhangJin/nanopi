---
phase: 01-in-process-runtime
plan: 02
subsystem: tool-runtime
tags: [tokio, path-lock, file-state, iso-03]

requires:
  - "FileStateTracker, canonical_key, path_lock from 01-01 (src/tool/file_state.rs)"
provides:
  - "read.rs records a per-agent fingerprint (mtime + content hash) on every read, text and image"
  - "write.rs/edit.rs acquire the process-wide path_lock, check staleness under it, and update the tracker after a successful write — cross-agent writes to the same path are serialized and a stale read is refused in-band"
affects: [01-03, 01-04, 01-05, 01-06]

tech-stack:
  added: []
  patterns:
    - "OwnedMutexGuard bound to a named local (_path_guard) so it stays held across the create_dir_all/write/update_after_write sequence, released only at end of execute()"

key-files:
  modified:
    - src/tool/read.rs
    - src/tool/write.rs
    - src/tool/edit.rs

key-decisions:
  - "canonical_key is recomputed independently in write.rs/edit.rs rather than threaded through from the earlier resolve_in_cwd/abs resolution, since it needs the raw path_str (not the already-resolved abs) and file_state owns its own canonicalization fallback for not-yet-existing paths"
  - "Stale-read and ambiguous/not-found errors share the same Err(ToolError::Execution(..)) in-band convention edit.rs and write.rs already used — no new error shape was introduced"

requirements-completed: [ISO-03]

duration: 35min
completed: 2026-10-03
---

# Phase 1 Plan 02: ISO-03 Stale-Write Guard Wiring Summary

**read/write/edit now record and check per-agent file fingerprints under a process-wide path lock, refusing a write/edit when another agent changed the file since this agent's last read**

## Performance

- **Duration:** 35 min
- **Tasks:** 2
- **Files modified:** 3

## Accomplishments
- `read.rs` records the fingerprint of the full file bytes on every read (both the text and image branches), using `file_state::canonical_key`; recording never fails the read itself.
- `write.rs` and `edit.rs` resolve a canonical path key at the start of `execute`, acquire `file_state::path_lock(&key).lock_owned().await` for the duration of the call, call `ctx.file_state.check(&key)` under the lock (refusing with the existing in-band `ToolError::Execution` convention on staleness, no disk write), and call `ctx.file_state.update_after_write` after a successful write.
- A dedicated regression test (`stale_write_refused_after_concurrent_change` in `edit.rs`) exercises the full cross-agent scenario: agent A reads, a second `ToolContext`/`FileStateTracker` (simulating agent B) writes, A's edit is refused with "file changed since you read it — re-read first" and the file is untouched; after A re-reads, its edit succeeds.
- `successive_own_edits_are_not_refused` and `edit_after_own_read_succeeds` cover the two non-refusal behaviours (own successive edits, and read-then-edit-same-ctx).
- `write.rs::concurrent_writers_one_succeeds_one_refused` races two `ToolContext`s' writes via `tokio::join!` after both read the same file; the path lock serializes them so exactly one succeeds and the other is refused, final content matches the winner. Stable across 5 repeated runs.
- Writing a brand-new file with no prior read is unaffected (`creates_new_file` et al. still pass unchanged — `canonical_key` resolves even for a not-yet-existing path via its deepest-existing-ancestor fallback, and `check()` on an unrecorded path always passes).

## Task Commits

1. **Task 1: Record on read, check + lock + update on write/edit** - `6747c8a` (feat)
2. **Task 2: Concurrent-writer test across trackers** - `5b5df8c` (test)

## Files Created/Modified
- `src/tool/read.rs` - records `ctx.file_state.record(&key, &raw)` right after the raw bytes are read, before image/text branching
- `src/tool/write.rs` - acquires `path_lock`, checks staleness, writes, then `update_after_write`; added `concurrent_writers_one_succeeds_one_refused`
- `src/tool/edit.rs` - same guard sequence around the read-modify-write; added `stale_write_refused_after_concurrent_change`, `successive_own_edits_are_not_refused`, `edit_after_own_read_succeeds`

## Decisions Made
- `canonical_key` is recomputed from the raw `path_str` in `write.rs`/`edit.rs` rather than derived from the already-resolved `abs`, since `file_state::canonical_key` owns its own escape/canonicalization logic (including the not-yet-existing-path fallback) independently of `resolve_in_cwd`'s guard — duplicating the resolution is cheap and keeps the two concerns (cwd-escape guard vs. staleness key) decoupled.
- The stale-read refusal reuses the existing `Err(ToolError::Execution(String))` in-band error convention already used for "oldText not found" / "path escapes cwd", rather than introducing a new error variant.

## Deviations from Plan

### Auto-fixed Issues
None — plan executed as written for the guard wiring itself.

**1. [Minor/no-op] Acceptance-criteria filter string `cargo test --lib tool::write::concurrent` does not match**
- **Found during:** Task 2
- **Issue:** The plan's verify/acceptance command assumes the test name appears directly after the module path, but every test module in this codebase is wrapped in `mod tests { ... }`, so the actual test path is `tool::write::tests::concurrent_writers_one_succeeds_one_refused`. Cargo's substring filter requires the literal `tool::write::concurrent` to appear contiguously, which it does not (there's a `tests::` in between) — this is pre-existing codebase structure, not something introduced by this plan.
- **Fix:** Verified the test instead with `cargo test --lib -q tool::write::tests::concurrent` (and the full `cargo test --lib -- --test-threads=1`), both green, including 5 repeated runs for flake-checking per the acceptance criteria's intent.
- **Files modified:** none (verification-only)
- **Commit:** n/a

## Issues Encountered
None.

## User Setup Required
None.

## Next Phase Readiness
- ISO-03 holds for all agents, including the main agent: `cargo test --lib -- --test-threads=1` green at 816 passed / 0 failed / 1 ignored (no regressions from 812 baseline in 01-01, +4 new tests: 3 in edit.rs, 1 in write.rs).
- No blockers for subsequent 01-xx plans; `FileStateTracker`/`path_lock` usage pattern established here (resolve key → lock_owned → check → mutate → update_after_write) is reusable wherever a future tool needs the same guard.

---
*Phase: 01-in-process-runtime*
*Completed: 2026-10-03*

## Self-Check: PASSED
Both task commits (`6747c8a`, `5b5df8c`) verified present in git log; `src/tool/read.rs`, `src/tool/write.rs`, `src/tool/edit.rs` all exist with the expected content.

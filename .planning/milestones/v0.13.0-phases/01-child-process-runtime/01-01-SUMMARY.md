---
phase: 01-child-process-runtime
plan: 01
subsystem: tool
tags: [file-safety, iso-03, atomic-write]
requires: []
provides: [file_state::global, file_state::atomic_write, file_state::guarded_write]
affects: [src/tool/read.rs, src/tool/edit.rs, src/tool/write.rs]
tech-stack:
  added: []
  patterns: [process-global OnceLock tracker, temp+rename atomic write]
key-files:
  created: [src/tool/file_state.rs]
  modified: [src/tool/mod.rs, src/tool/read.rs, src/tool/edit.rs, src/tool/write.rs]
decisions:
  - "Stale check decides on len + full-content DefaultHasher hash; mtime is recorded but never trusted alone"
  - "read stamps the bytes it already read (record_bytes) to avoid a second read racing the returned content"
  - "edit and write share file_state::guarded_write (stale check, symlink + nlink>1 refusal, atomic rename, stamp refresh); write_no_follow removed"
metrics:
  duration: ~10min
  completed: 2026-10-03
  tasks: 2
  files: 5
requirements: [ISO-03]
---

# Phase 01 Plan 01: Cross-process stale-write guard Summary

Each nanopi process now keeps a process-global FileStateTracker that records the length and DefaultHasher hash of every file it reads. edit and write refuse with "file changed since you read it — re-read first" if the file changed on disk after that read, and all writes go through an atomic temp-file-plus-rename.

## Tasks

| Task | Name | Commit |
| ---- | ---- | ------ |
| 1 | FileStateTracker module + atomic_write (7 unit tests) | c2b2cc8 |
| 2 | Guard wired into read/edit/write (+5 tests) | acee9af |

## Verification

- `cargo test --lib tool::` passed: 100 passed, 1 ignored.
- The existing symlink, dangling-symlink, swapped-symlink and hard-link refusal tests still pass.

## Deviations from Plan

**1. [Rule 2 - Missing safety] edit had no symlink or hard-link refusal.** The plan said to "keep the existing" refusals, but only write.rs had them. Both tools now share `guarded_write`, so edit gets the same refusals. A new `edit_refuses_hard_link` test covers this.

**2. [Rule 1 - Race] Added `record_bytes`.** read now stamps the bytes it actually returned instead of reading the file a second time.

**3. write_no_follow removed.** It was replaced by `guarded_write`. The rename replaces a symlink rather than following it, and the explicit `symlink_metadata` check refuses one before that point.

## Deferred Issues

- `cargo clippy -- -D warnings` fails with 42 errors that were already in the codebase, in files outside this plan. None are in the files this plan touched. They are out of scope.

## Self-Check: PASSED

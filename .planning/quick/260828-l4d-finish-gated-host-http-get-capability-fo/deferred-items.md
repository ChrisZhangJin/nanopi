# Deferred items — discovered during 260828-l4d

## Pre-existing test flakiness under parallel execution

**Discovered:** while establishing the baseline for this task, *before* any
code was written. Not caused by this change.

Several lib unit tests mutate process-global environment (`HOME`,
`NANOPI_HOME`, cwd) and race with each other when `cargo test` runs them on
the default thread pool. Observed failures across repeated runs:

- `session::tests::roundtrip_all_entry_types` (failed on the very first
  baseline run at commit `324a7d8`, before any edit)
- `agent::prompt_override::tests::*` (10 of them, as a cluster)
- `paths::tests::nanopi_home_honors_env`
- `provider::openai::tests::resumed_session_outgoing_request_has_tools`
- `agent::hook::tests::run_hook_exit_2_means_block`

Reproduction: `for i in $(seq 6); do cargo test --lib; done` — fails on
roughly 1 run in 3, with a different subset each time. It reproduces in the
**default** suite, which contains none of this task's code, which is what
identifies it as pre-existing rather than a regression.

Both suites are deterministically green with `-- --test-threads=1`.

**Why deferred:** out of scope for this task (unrelated files, pre-existing).
The fix is either a shared env mutex/serial_test guard or refactoring the
affected tests to take an injected path/config rather than reading globals —
its own commit.

## Non-canonicalized path guard in the built-in write/edit tools

`src/tool/write.rs:48` and `src/tool/edit.rs` guard against cwd escape with
`starts_with` on a **non-canonicalized** path, which accepts
`<cwd>/../../etc/passwd`. Real bug, explicitly deferred to its own commit by
the task brief. `resolve_readable` in `src/wasm/loader.rs` is the reference
fix.

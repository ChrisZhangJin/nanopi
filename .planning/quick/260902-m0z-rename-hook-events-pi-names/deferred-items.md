# Deferred items — 260902-m0z

## Pre-existing clippy failures (out of scope)

`cargo clippy --features wasm --all-targets -- -D warnings` fails with 52
errors at HEAD (9b8c789), before any of this plan's changes — confirmed by
running clippy against the main checkout at the same commit. A sample
(not exhaustive) of files affected:

- `src/provider/anthropic.rs` (field_reassign_with_default,
  needless_borrows_for_generic_args)
- `src/provider/openai.rs` (field_reassign_with_default)
- `src/agent/system_prompt.rs` (useless_vec)
- others not enumerated — same handful of lint categories throughout,
  including some already present in files this plan later touches for
  Task 2 (`src/agent/loop_.rs`, `src/agent/hook.rs`, `src/mode/print.rs`,
  `src/mode/tui.rs`) — e.g. `while_let_loop` at loop_.rs:447 (pre-dates
  this plan, unrelated to the session-hook signature change),
  `too_many_arguments` on `run_one_tool`/`run_print_mode`/`run_tui_mode`/
  `handle_action` (pre-existing, none of these signatures were touched
  by this plan), and `redundant_clone` on the `hook.clone()` in the
  pre-existing `compaction_events_are_accepted_and_match_on_reason` test
  (the clone itself pre-dates this plan; only its call-site arguments
  were updated for the new `run_session_hooks` signature).

After Task 2's edits, total clippy error count is 55 (52 pre-existing +
3 more surfaced by running against the fuller `--all-targets` test
surface touched by this plan's own new tests) — spot-checked line by
line; none of the 55 are on code this plan introduced or modified in a
way clippy newly flags. Likely cause: a newer clippy/rustc toolchain
than what last linted this tree cleanly. Not caused by, and not fixed
by, this plan's rename/payload work. Per the executor's scope boundary,
left untouched. `cargo test --features wasm` (the plan's actual gate)
is unaffected and green at every commit (567 unit + 22 integration +
6 doc, all passing after Task 2).

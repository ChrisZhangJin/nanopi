# Quick task 260903-l1s: inline `<think>` tags render as thinking — Summary

One-liner: added a streaming `InlineThinkSplitter` state machine and wired
it into the OpenAI-compat adapter behind a per-vendor gate (xiaomi,
minimax), so `<think>…</think>` inlined in `delta.content` renders as
`ThinkingDelta` instead of leaking into the reply, the context, the
session transcript, and `-p --output json`.

## What changed

**Task 1 — `src/provider/think_tags.rs`** (commit `3cc93ee`)
- `Segment::{Text, Think}` + `InlineThinkSplitter { push, finish }`.
- Char-boundary-only slicing throughout; carry buffer bounded by
  `"</think>".len()` so it can never grow with stream length.
- 12 unit tests: whole-tag-in-one-chunk, split-at-every-char-boundary-offset
  (the loop the plan called out as the one that matters), byte-at-a-time,
  a CJK variant of the every-offset loop, repeated blocks, nested opener
  (pinned: inner `<think>` while already inside is literal content, first
  `</think>` closes), unclosed tag, partial-prefix-at-EOF, bare closer,
  mere-mention (documented as splitting, with the no-loss invariant still
  checked), empty-chunk/empty-segment guards, and a round-trip helper.

**Task 2 — wiring** (commit `4acd24d`)
- `Vendor::inlines_think_tags() -> bool`, default `false`; overridden
  `true` for `XiaomiVendor` and `MinimaxVendor`.
- `Config::inline_think_tags: Option<bool>` escape hatch (`None` defers to
  vendor, `Some` is final).
- `OpenAiProvider::split_inline_think: bool`, set from the vendor in
  `with_vendor` and overridable via `with_inline_think`.
- SSE loop: `delta.content` routed through the splitter when the flag is
  set, emitting `TextDelta`/`ThinkingDelta` per segment; the
  `reasoning_content` arm is untouched. Drained via `finish()` at both
  stream exits (before `Done`, and after the `while` loop for a stream
  that ends without an explicit `finish_reason`) — draining twice is
  harmless since `finish()` resets state.
- Four new e2e tests in `tests/print_mode_e2e.rs` (e2e count 6 → 10):
  split-across-two-deltas (the exact bug repro), JSON-envelope exclusion,
  a negative test proving a non-gated vendor still sees the literal tag
  (the gate is real), and unclosed-tag delivery.
- One new unit test in `src/vendor/mod.rs` pinning `inlines_think_tags`
  true only for xiaomi/minimax.
- `Config::inline_think_tags` had to be threaded through
  `crate::provider::build()` and both `run_print_mode` /
  `run_tui_mode` signatures (and the TUI `App` struct, alongside the
  existing `cfg_provider` field) since none of them previously carried
  the full `Config` — this fan-out across `main.rs`, `mode/print.rs`,
  `mode/tui.rs`, and every `provider::build()` call site inside `tui.rs`
  was not explicit in the plan's file list but was required to make the
  config escape hatch reachable at all (Rule 3 — blocking issue, the
  build function's new required parameter had to compile everywhere).

**Task 3 — docs** (commit `b2d5759`)
- `.release-notes-v0.12.0.md`: new Highlights bullet.
- `README.md` / `README_zh.md`: extended the existing multi-provider
  bullet (parallel wording; no networking commentary added to the
  English file per project convention).

## Deviations from Plan

**1. [Rule 3 - blocking issue] `README.zh-CN.md` does not exist; the repo's
Chinese README is `README_zh.md`.** Edited `README_zh.md` instead — same
content, same location in the file, same style. Files_modified in the
plan frontmatter names the wrong filename; verification (Task 3's grep)
was run against the real filename and passed.

**2. [Rule 3 - blocking issue] `provider::build()` needed a new required
parameter (`inline_think_tags: Option<bool>`) to receive the config
escape hatch, which meant updating every call site:
`src/main.rs` (both `run_print_mode`/`run_tui_mode` calls), the function
signatures and internal `provider::build()` calls in `src/mode/print.rs`
and `src/mode/tui.rs` (7 call sites total in `tui.rs`), and a new
`App.inline_think_tags` field mirroring the existing `App.cfg_provider`
pattern. None of these files were in the plan's `files_modified` list
(which only named `src/provider/openai.rs`, `src/vendor/*`,
`src/config.rs`, `src/provider/mod.rs`, `tests/print_mode_e2e.rs`). This
was necessary to make `Config::inline_think_tags` (mandated by the plan's
Task 2 action item 2) actually reach `OpenAiProvider` rather than being a
dead config field.

**3. [Rule 1 - bug caught by own test] Initial `think_tags.rs` test
`byte_at_a_time_matches_whole_chunk` asserted exact segment-vector
equality between a whole-chunk feed and a one-char-per-`push` feed.**
That assertion was wrong by design, not a splitter bug: feeding one
character at a time naturally yields more (but never empty, never
mis-tagged) segments than a single bulk push, since the splitter emits
whatever it can classify on every `push` call. Fixed the test to assert
on the concatenated string (the actual no-loss invariant) plus a
non-empty-segment check, matching what the plan's `<behavior>` section
actually specified ("same result", not "same segments"). Caught during
Task 1's own `cargo test`, before committing.

No stubs. No auth gates. No architectural deviations (no Rule 4).

## Verification

- `cargo test --features wasm`: 629 unit (616 baseline + 13 new: 12 in
  `think_tags`, 1 in `vendor::mod`), 0 doc-only unit failures, 10 e2e
  (6 baseline + 4 new), 6 doc-tests, 30 integration — all green at every
  commit.
- `cargo build --release` (no wasm feature): compiles clean at every
  commit.
- `git log --oneline d98cefd..HEAD`: exactly three commits, matching
  Tasks 1–3 in order; no commit at or before `d98cefd` altered.
- `VERSION` unmodified; no tag created.
- `git status --short`: clean (untracked screenshots at repo root
  untouched, not part of this change).

## Self-Check

- `src/provider/think_tags.rs` — FOUND
- `tests/print_mode_e2e.rs` (updated with 4 new tests) — FOUND
- Commit `3cc93ee` — FOUND
- Commit `4acd24d` — FOUND
- Commit `b2d5759` — FOUND

## Self-Check: PASSED

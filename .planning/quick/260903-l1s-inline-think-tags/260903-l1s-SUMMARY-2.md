# Inline `<think>` tags: from vendor gate to position rule — Summary 2

## What changed

The vendor gate shipped an hour earlier (`Vendor::inlines_think_tags()`,
true only for xiaomi/minimax) is replaced with a **position rule**:
only a LEADING `<think>` block — nothing but whitespace emitted before
the opener — is reclassified as reasoning. Any `<think>` appearing
after other text (a second block after the first closed, a model
discussing or quoting the tag mid-answer, one inside a code fence) is
left as literal text.

The splitter is now on by default for every vendor on the OpenAI wire
(including `fallback` and the no-vendor case), not a two-vendor
allowlist — `<think>` is a model-level convention (R1 and its
distills, QwQ, GLM, etc.), not an OpenAI API feature, so it shows up
through any OpenAI-compatible endpoint (ollama, vLLM, DeepSeek direct,
gateways), and a vendor allowlist needed a code change per new vendor.

`Config::inline_think_tags` stays as the escape hatch in both
directions: `false` disables the splitter entirely (even a leading
block renders literal); `true` is a no-op (already the default).

## Implementation

- `src/provider/think_tags.rs` — `InlineThinkSplitter` gains a
  `leading: bool` field (init `true`). It flips to `false` the moment
  either (a) non-whitespace text is emitted while not inside a block,
  or (b) a block completes (its `</think>` is found). Once `leading`
  is `false`, the drain loop stops scanning for `OPEN` entirely and
  flushes the carry verbatim as `Text` — `<think>` is then
  indistinguishable from any other substring. While `leading` is
  `true`, the existing OPEN/CLOSE state machine is unchanged (same
  carry-buffering, same char-boundary-safe `longest_prefix_suffix`
  logic for partial delimiters at chunk boundaries).
- `src/vendor/mod.rs` — deleted `Vendor::inlines_think_tags()` and its
  doc block; deleted the test asserting it's true only for
  xiaomi/minimax.
- `src/vendor/xiaomi.rs`, `src/vendor/minimax.rs` — deleted the
  `inlines_think_tags() -> true` overrides (no trait method left to
  override).
- `src/provider/openai.rs` — `OpenAiProvider::new()` now defaults
  `split_inline_think: true` (was `false`, and was set per-vendor in
  `with_vendor`). `with_vendor` no longer touches `split_inline_think`
  at all — it's unconditional now, only the `Config::inline_think_tags`
  escape hatch (via `with_inline_think`, wired in `provider::build`)
  can turn it off.
- Doc comments in `src/config.rs`, `src/provider/mod.rs`,
  `src/mode/print.rs`, `src/mode/tui.rs` updated to describe the
  position rule instead of "defers to the vendor."
- `.release-notes-v0.12.0.md`, `README.md`, `README_zh.md` — rewrote
  the Highlights entry / feature bullets to describe the position rule
  and state the residual failure mode (a reasoning block arriving
  after other text degrades to the old plain-text rendering).

## Tests

`src/provider/think_tags.rs` unit tests (17 total, was 12):

- `leading_tag_in_one_chunk_splits` — baseline: nothing precedes the
  tag, it splits.
- `leading_whitespace_before_opener_still_counts_as_leading` —
  `"\n\n<think>…"` still splits; whitespace doesn't defeat leading.
- `cjk_whitespace_preamble_still_counts_as_leading` — same, with a
  Unicode ideographic space (U+3000), confirming the whitespace check
  is `char::is_whitespace`-based, not ASCII-only.
- `opener_after_non_whitespace_text_is_literal` — `"a<think>b</think>c"`
  (formerly split under the vendor gate) is now ONE literal `Text`
  segment. This is the main behavior-changing test versus the prior
  design.
- `code_fence_mid_answer_is_untouched` — the case the whole rule exists
  to protect: a model explaining the R1 output format inside a code
  fence, mid-answer, is untouched.
- `second_block_after_first_closes_stays_literal` — replaces
  `repeated_blocks_yield_two_think_one_text`. `"<think>a</think>x<think>b</think>"`
  now yields `[Think("a"), Text("x<think>b</think>")]` — the leading
  window closed after the first block, so the second is literal.
  Doc comment explains why.
- `split_at_every_offset_produces_identical_result`,
  `byte_at_a_time_matches_whole_chunk`,
  `cjk_reasoning_trace_split_at_every_offset` — kept, but their fixture
  changed from `"pre<think>mid</think>post"` to `"<think>mid</think>post"`
  (dropped the `"pre"` prefix) so they still exercise the OPEN/CLOSE
  state machine rather than being short-circuited by the position rule.
- `nested_opener_is_literal_inside_thinking` — unchanged, still passes
  (single leading block, nested opener inside it is literal content).
- `unclosed_leading_tag_flushes_as_think_at_finish` — replaces
  `unclosed_tag_flushes_as_think_at_finish`; fixture changed from
  `"x<think>partial"` (no longer leading, no longer splits) to
  `"<think>partial"` (genuinely leading).
- `unclosed_non_leading_tag_is_literal_text` — new: covers what the old
  fixture (`"x<think>partial"`) now does — stays fully literal.
- `mere_mid_text_mention_of_tag_no_longer_splits` — replaces
  `mere_mention_of_tag_splits_but_loses_no_other_bytes`; same input
  (`"the <think> tag is used by mimo"`), inverted assertion: it no
  longer splits.
- `round_trip_invariant_holds` — extended with both regimes: a leading
  fixture (delimiters stripped) and a non-leading fixture (delimiters
  survive verbatim) in the same test, still proving no byte is lost
  either way.
- `partial_prefix_at_end_of_stream_is_not_lost`,
  `bare_closer_with_no_opener_is_literal_text`,
  `empty_chunks_and_segments_never_emitted` — unchanged, still pass.

`tests/print_mode_e2e.rs`:

- `inline_think_untouched_for_other_vendors` (tested the now-deleted
  vendor gate) replaced with
  `inline_think_mid_answer_reaches_stdout_literally` — a `<think>`
  arriving after `"ANSWER first, then "` reaches stdout literally, no
  provider override needed (splitter is on by default regardless of
  vendor; position, not vendor, decides).
- New: `inline_think_tags_false_disables_splitting_even_for_leading_block`
  — the escape hatch, `inline_think_tags = false` in `config.toml`,
  leaves a genuinely leading block untouched even though the splitter
  is on by default.
- `inline_think_split_across_two_content_deltas`,
  `inline_think_excluded_from_json_envelope`,
  `inline_think_unclosed_still_delivers` — unchanged, still pass (all
  use leading fixtures, or in the unclosed case a fixture where the
  literal-text outcome is what's asserted either way).

## Verification

- `cargo build --features wasm` — clean.
- `cargo test --features wasm` — 633 unit passed (was 629; net +5 —
  17 tests in `think_tags.rs` replacing 12, minus 1 deleted vendor
  test), 11 e2e passed (was 10; +1 new escape-hatch test), 30 + 6
  integration passed, 0 doc tests (pre-existing, unrelated to this
  change).
- `cargo build --release` (no wasm feature) — clean.

## Commits

1. `e2eaa57` — `feat(provider): replace inline <think> vendor gate with
   a position rule` — the rule + all test changes.
2. `78958cd` — `docs: correct inline <think> docs to the position rule,
   not a vendor gate` — `.release-notes-v0.12.0.md` (gitignored, added
   with `-f`), `README.md`, `README_zh.md`.

Both on branch `v0.12.0`, directly on top of `450b817` (no rebase, no
amend of prior history).

## Deviations from plan

None beyond what the task brief itself specified — the brief already
called for updating `repeated_blocks_yield_two_think_one_text` and
extending the invariant tests; this summary documents exactly which
other pre-existing tests also needed fixture or assertion changes as a
direct, mechanical consequence of the position rule (see Tests
section above) since their old fixtures (`"pre<think>..."`,
`"x<think>partial"`, `"the <think> tag..."`) all had non-whitespace
text preceding the tag and therefore stopped exercising the
Think-splitting path once the rule changed.

## Known stubs / threat flags

None. No new network surface, auth path, or schema change was
introduced — this is a pure text-classification change to an existing
in-process transform of provider stream deltas.

## Worktree note

The spawned worktree's initial branch (`worktree-agent-…`) was created
off `main` instead of the session's checked-out branch, `v0.12.0` —
flagged before any code changes per the mandatory base check, confirmed
safe by the coordinator, and corrected with `git reset --hard 450b817`.
All commits above landed on `v0.12.0` as intended; no history was lost.

---
phase: 260907-r3k-plugin-send-user-message-stage-4
plan: 01
date: 2026-09-07
commits: 1fd55f4..e9931ce
---

# Stage 4: `host-send-user-message`

A plugin granted `allow_send_message = true` can start or steer a turn with
text of its own. It is the only capability nanopi has that spends the user's
money, and everything below is about making that bounded structurally rather
than by convention.

## What shipped

- **`src/plugin_send.rs`** (new, non-gated) — a process-wide installed sink
  plus the loop-guard state. Same seam as `plugin_tools.rs` and for the same
  reason: `steer_tx_slot` and `follow_up_slot` are LOCALS of the TUI turn
  loop and the plugin calls the import from a synchronous guest thread, so
  neither is reachable from a `PluginState` built at load time. Non-gated so
  `tui.rs` gains no `cfg(feature = "wasm")` (verified: 3 before, 3 after;
  `loop_.rs` 2 and 2).
- **The seventh import** on `world extension` only. Still 3 worlds; the 3
  committed fixtures still load (30 integration tests green).
- **`allow_send_message`** on `ExtensionConfig`, default off, with
  `deny_unknown_fields` from stage 2 making a typo a load error.
- **`send_gated`** in `build_linker`, so BOTH `PluginEngine::load` and
  `PluginRebuild::build` carry it.
- **Three drain/boundary sites in `tui.rs`**: echoes rendered on the ticker
  (`render_user_echo`, not `insert_line` — this text IS a user message as
  far as the model is concerned); `pick_follow_up` as a third source behind
  the human's queued line; `reset_turn` + per-turn `install` beside
  `notify::reset_turn()`.
- **Docs**: two spec amendments (below), invariants 13/14 amended, invariant
  16 added, §3's escalated-warning list, five rows in claims-and-races'
  steering table, staging table gains a Status column, both READMEs.

## Decisions worth remembering

**The inherited `Pending` / `take_echoes` design was KEPT.** The orchestrator
asked whether splitting "sent" and "echoed" into two moments reintroduces
`b90b27f`. It does not, and the reason is worth writing down:

- `routed == true` means the text is in the steer channel's buffer, and
  `run_turn` calls `drain_steer_to_follow_ups` on **every** early return
  (cancel, iteration cap, normal exit — `loop_.rs` 1074, 1263, 1554), which
  demotes a buffered `Steering` to `pending_follow_ups`. So a message that
  reached the channel runs, even if the turn dies immediately.
- The echo drain is on the TUI ticker, which outlives any single turn.
- The two are decided together under one lock: the echo is pushed only on
  the branch where `try_send` returned `Ok`, and the other branch queues to
  overflow where `StartTurn`'s own `render_user_echo` renders it. They
  cannot disagree.

Process death breaks it, but no design survives that — an atomic
echo-then-send cannot either, and the ordering the spec asked for is
strictly worse (it renders before knowing whether the send will succeed).

**But it was not kept as-is.** The two single-case tests were anecdotes, not
a pin. Added
`every_accepted_message_is_owed_exactly_one_rendering_and_no_other_is`: over
an interleaving of live and dead channels, `echoes + overflow == accepted
sends` exactly, AND the two sets are disjoint. That is the biconditional as
one countable invariant. It reds on tooth #6.

**Rule 1 clears at hand-off, not on return.** Clearing when `send` returns
makes the rule decorative. Clearing at hand-off has the opposite hazard — a
message that reached a turn and then died would wedge the plugin — which is
why `drain_steer_to_follow_ups`'s demotion path matters above.

**Rule 2 is per plugin.** A global flag is easier and silences every other
plugin for the whole turn, breaking the audit-plus-rules pair the spec's own
motivation rests on. Pinned by `another_plugin_may_still_send_during_that_turn`.

**A hazard found in `pick_follow_up` and documented rather than fixed.** It
CONSUMES from all three sources, and source 3 additionally clears rule 1 and
stages the turn origin. So whatever it returns must reach
`KeyAction::StartTurn`, which early-returns when `status == Streaming`. A
plugin message dropped in that early return would be one the guard already
counted as delivered: `b90b27f`'s shape with a stuck flag on top. Both call
sites establish the precondition (the ticker checks explicitly; the
turn-completion site sets `Idle` well above), so this is a caller obligation
written into the doc comment, not a live bug.

## Deviations from the plan

**Task 1's "install at both Agent build sites and refreshed per `run_turn`"
is not implementable, and doing it would have broken Q4.** Evidence: the
Agent never holds a steer *sender*. `run_turn` takes `steer_rx`;
`src/agent/build.rs` does not import `SteerMessage` at all. An Agent-side
install could therefore only publish `Sink { steer_tx: None }`, which is (a)
redundant in the TUI, which already installs that at `run_tui_mode`, and (b)
actively wrong in headless, where Q4's entire implementation is that no sink
exists so the call is refused. Installing one from `Agent::build_fresh`
would have made `-p` silently accept and drop — tooth #12's failure, shipped
on purpose.

The property teeth #11 attacks — a fresh sender every turn — is satisfied at
`KeyAction::StartTurn`, which is the one turn boundary the TUI already has.
This is the same shape as stage 3's `Arc<ToolRegistry>` finding: the plan
prescribed a site that does not hold the thing.

**The `/tools` grant token landed in the Task 2 commit**, not Task 3.
`grant_tokens` lives in `src/wasm/mod.rs`, which Task 2 was already editing;
splitting it would have meant a partial-file commit for no gain.

**Added beyond the plan:** a startup `[Extensions]` warning for
`allow_send_message` **alone**. The `allow_context`-alone precedent argues
against warning on single grants, but it does not apply here — that grant is
only sharp in combination, whereas this one causes billed turns by itself.
Same treatment `bash` in `allow_tools` gets.

## Teeth

All 12 applied, run, observed, reverted. Eleven red immediately; **#11 came
back GREEN in the form the plan actually names** and was strengthened.

| # | Reversion | Result |
|---|---|---|
| 1 | remove the `allow_send_message` gate | **RED** — `without_allow_send_message_the_call_is_refused_and_sends_nothing`, at the assert that nothing reached the turn |
| 2 | global loop guard instead of per-plugin | **RED** — `another_plugin_may_still_send_during_that_turn`: "the guard is PER PLUGIN — a global flag would silence every other plugin for the whole turn" |
| 3 | clear rule 1 on `send` returning | **RED** — `a_second_message_while_one_is_pending_is_refused` |
| 4 | drop rule 2 | **RED** — `a_plugin_cannot_send_during_a_turn_its_own_message_started` |
| 5 | remove the session cap | **RED** — "§2.4's two rules do not bound this loop; the session cap does" |
| 6 | echo before send (unconditional push) | **RED ×2** — `a_dead_receiver_queues_the_text_and_owes_no_echo`: "NO ECHO WITHOUT A SEND"; and the partition test: `left: 9, right: 6` (three messages owed two renderings each) |
| 7 | disclosure through quota-consuming `notify` | **RED** — with `MAX_NOTIFY_PER_TURN` exhausted first, the drain returned only `["… 1 more suppressed"]`: "flooding host-notify buys undisclosed spending" |
| 8 | drop the `PluginRebuild::build` half | **RED** — "the grant must be carried across a trap — otherwise the plugin goes mute after its first trap with nothing to diagnose" |
| 9 | overflow queue even when the channel is alive | **RED** — `a_message_with_a_live_turn_steers_it_and_is_echoed`: "it must reach the RUNNING turn: Empty" |
| 10 | plugin overflow first in the drain order | **RED** — `left: Some("from the plugin")` where the agent's follow-up was expected |
| 11 | **install once at startup, not per turn** | **GREEN, then strengthened.** Making `install` first-write-wins reds `installing_a_fresh_sink_replaces_the_previous_turns_sender` — but *deleting the per-turn call from `KeyAction::StartTurn`*, which is what the tooth names, passed all 781 tests. The call site needs a live `Term`, an agent slot and a spawned turn task to reach, so nothing behavioural covers it. Added `the_send_sink_is_republished_on_every_turn_not_once_at_startup`, which reads the source of the turn-start arm. Brittle by construction, and the doc comment says so and says why the trade is worth taking. Re-run of the same deletion: **RED** |
| 12 | let `-p` silently no-op | **RED** — `in_headless_mode_even_a_granted_plugin_is_refused_rather_than_no_opd`: "granted but unreachable is still a REFUSAL, not silence" |

## Verification

- `cargo test --features wasm -- --test-threads=1` — **782 lib** + 30
  `wasm_plugin_integration` + 11 `print_mode_e2e` + 6 `skills_integration`,
  0 failed, 1 ignored.
- `cargo test -- --test-threads=1` — **671 lib** + 11 + 6, 0 failed.
  (Base `74ea5a8`: 758 / 653. Delta is the 24 / 18 new tests.)
- `cargo test --features wasm --no-run` — 0 warnings.
- `grep -c '^world ' wit/nanopi-extension.wit` → **3**.
- `cfg(feature = "wasm")` count unchanged from base: `tui.rs` 3 → 3,
  `loop_.rs` 2 → 2.
- `src/mode/print.rs` contains no reference to `plugin_send`, which is Q4's
  implementation.

## Follow-ups

- **Wiki repo** (separate, unreachable from here): `allow_send_message`, the
  three refusal strings, the session cap, the headless refusal, and the
  `/tools` row token.
- **Stage 5** (`message_end` payload carrying assistant text) stays a
  separate decision — it changes shell-hook behaviour, per §5.
- The pre-existing `TEST_LOCK` poisoning cascade is untouched, as instructed.

## Self-Check: PASSED

`src/plugin_send.rs` present; commits `1fd55f4`, `55fd6fe`, `c3efd9e`,
`e9931ce` all in `git log`.

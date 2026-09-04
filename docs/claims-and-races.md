# Claims and races

This document specifies what nanopi is allowed to tell the user and the
model about its own actions, and what must happen at each interleaving
where a claim could outrun the action.

It exists because of a pattern, not a theory. Manual acceptance of
v0.12 turned up eight defects by hand; five of them were the same
mistake in five places — **nanopi describing its own action more
confidently than it performed it**:

| Defect | The claim | What actually happened |
|---|---|---|
| `87a81b4` | `[compacted: 2158 → 2158 chars]` | no boundary was found; nothing was compacted |
| `87a81b4` | a `session_before_compact` hook fired | no `session_compact` ever followed |
| `b90b27f` | `[steer] who mai` | the message stayed in the channel, unseen by the model |
| `af9f18b` | `To resume: --session <id>` | that was the session you started in, not the one you ended in |
| `c0ce35c` | `[error: Input hook blocked…]` | the user's own policy, working, framed as a malfunction |

None was caught by 682 passing tests. They are not logic bugs — every
one of them computed the right thing and then said something else
about it.

The vocabulary here is borrowed from PI's durability work
(`pi/packages/agent/docs/values.md`, `assistant-durability.md`,
`tool-durability.md`), which states the same rule as an invariant
rather than discovering it five times:

> Unsafe synthetic results explicitly state that captured output is
> incomplete and the external outcome is unknown.
> — `tool-durability.md`, invariant 9

> `entry_added` remains the only proof that the final assistant entry
> committed. […] There is no claim that a `message_update` was durable.
> — `assistant-durability.md`

nanopi's storage is a single append-only JSONL file, not PI's
transactional store with three backends and restart authority. The
storage design does not transfer. The **discipline about claims** does,
and costs nothing.

## Goals

1. For each thing nanopi reports, name the one signal that proves it.
2. Make everything else explicitly an observation, in the code and in
   the words shown to the user.
3. Require an unknown outcome to be reported as unknown, never as
   success and never as failure.
4. Enumerate the interleavings where a claim can outrun its action, and
   fix the required result for each.
5. Give a reviewer a checklist short enough to actually apply when
   adding a new status line or event.

## Non-goals

- Transactional storage, atomic multi-write commits, or a restart
  authority. nanopi appends to JSONL; a torn tail is a truncated line,
  handled where it is read.
- Exactly-once side effects. Tools are at-least-once and always were.
- A durable event log separate from the session file.
- Reworking the `AgentEvent` stream into a proof-carrying protocol.
  Events stay observations; that is the point.

## 1. Proof and observation

Two kinds of output, and the difference must be visible at the call
site:

**Proof.** A durable record whose presence means the thing happened.
For nanopi this is exactly one mechanism: a committed
`session::append_entry` line. Nothing else proves anything.

**Observation.** Everything a human or the model sees: `AgentEvent`
variants, TUI cards, `-p` markers, `crate::note!` lines, plugin event
deliveries. These may be emitted before, after, or instead of the
action. They are reports about intent or progress.

### The rule

An observation must not be phrased as an accomplished fact unless its
proof has committed.

This is a rule about **wording**, enforced by review, because the type
system cannot express it. `[compacted: 2158 → 2158 chars]` is a
well-typed string.

### Proof for each claim

| Claim | Proof | Note |
|---|---|---|
| the model said this | `SessionEntry::Message{role:"assistant"}` | text deltas are observation |
| this tool ran | `SessionEntry::ToolResult` | `ToolCall` proves only that it was *about* to run — see §3 |
| the context was compacted | `SessionEntry::Compaction` | `compact_now` returns `bool`; a caller may not infer from `estimate_chars()` |
| this prompt reached the model | `SessionEntry::Message{role:"user"}` | a `[steer]` echo is observation |
| this is the live session | `app.session_id` | NOT the `header` bound at startup |
| a plugin observed this event | nothing | delivery is best-effort by design — see §4 |

`SessionEntry::ToolCall` is deliberately not proof of execution. It is
persisted before the tool runs, and that order is correct: for `bash`,
the record of what was about to run outlives the process better than
the result, because the side effects may have landed either way.

### Unknown is a third outcome

When nanopi cannot determine what happened, it says so. It does not
pick the safer-sounding option.

The live example is `repair_orphaned_tool_calls` (`agent/loop_.rs`):
a session whose tail is a `tool_call` with no result gets a synthesized
result stating the outcome is **UNKNOWN**, and telling the model
explicitly not to assume failure. Reporting failure there would be a
lie in the case that matters most — the command may well have finished,
and a model that believes it failed will re-run a write.

## 2. Naming a refusal

A refusal by the user's own configuration is that configuration
working. It is not an error, and must not borrow error vocabulary.

Two audiences, two phrasings, both required:

**To the model** — it cannot see the config, so a bare "blocked" leaves
it to guess. It guessed a sandbox and spent a turn testing the theory
(`363916b`). Say what refused and that it is policy:

```
blocked by a user-configured `tool_execution_start` hook — this is a
policy refusal from the user's nanopi configuration, not a sandbox or
environment failure. Hook's reason: {reason}
```

**To the user** — they wrote the hook, but a red `error:` still reads
as a malfunction (`c0ce35c`):

```
your `input` hook refused this prompt (policy, not a failure) — {reason}
```

Corollaries:

- A message must not bracket itself when its renderer adds framing.
  Both renderers wrap `AgentEvent::Error` in `[error: …]`; a
  self-bracketing message produced `[error: [ … ]]`.
- A misconfigured hook must be loud. Exit 127/126 used to fail open in
  silence, so a typo'd path looked like a hook that never matched.
  Non-zero exits now print the code and a hint (`agent/hook.rs:470`).
  **Untested** — it is a `note!` to stderr on a path with no return
  value, so the hint could be deleted silently. Covered by
  `docs/v0.12-manual-test-plan.md` T2.4 instead.
- A rewritten tool call must be shown as rewritten. `-p` prints a
  second `↻` marker (it already emitted the first); the TUI corrects
  its stashed bar in place, since the card is only drawn on result.

## 3. Races

Required result for each interleaving.

| Mark | Meaning |
|---|---|
| ✅ | a regression test pins it |
| ⬜ | current behavior, nothing pinning it — could be deleted and the suite would stay green |
| type | not testable and not needing a test: the signature makes the wrong thing unwritable |

The ⬜ rows are the point of the table. Each one was found by asking
"what does the test actually assert?" rather than "is there a test?",
and several turned out to assert the in-memory half of a two-step
write. Do not upgrade a mark without reading the assertions.

### Steering

| Race | Required result | |
|---|---|---|
| steer arrives mid-iteration | pumped at the next iteration top and pushed into context | ✅ |
| — and persisted to the session | `append_entry` alongside the context push | ⬜ |
| steer arrives during a turn that ends without tool calls | demoted to a follow-up, which auto-starts the next turn — never dropped | ✅ `b90b27f` |
| steer arrives after the receiver is dropped | `steer_or_queue` echoes `[queued]` and queues it in the TUI | ✅ |
| steer arrives during cancellation | `drain_steer_to_follow_ups` keeps it as a follow-up | ✅ |
| steer echoed but never delivered | forbidden — the echo is emitted only after the send succeeds | ✅ |

The persistence half is unpinned: `steer_message_injected_as_user_turn`
asserts the message reaches `ctx.messages` and stops there. The
`append_entry` beside it could be deleted and every test would still
pass — the failure would surface only on `--continue`, as a resumed
session missing a turn the user typed.

The third and fourth rows cover a **dropped receiver**; the second
covers a **live receiver nobody returns to**. Conflating them is what
hid `b90b27f`: two of the three exits called
`drain_steer_to_follow_ups` and the ordinary one did not.

### Tool execution

| Race | Required result | |
|---|---|---|
| process loss between `ToolCall` and `ToolResult` | replay synthesizes an unknown-outcome result so the session stays resumable | ✅ `f70e5cc` |
| a hook rewrites arguments after the call was displayed | the rewrite is shown: `↻` line in `-p`, in-place correction in the TUI | ✅ |
| a hook blocks | the tool does not run; the model is told it was policy; subscribers still receive the event | ✅ |
| a WASM plugin traps | reported as a failed call; the plugin stays callable afterwards | ✅ |
| user cancels mid-tool | the turn aborts; a directive-only marker enters context, and does NOT embed partial text | ✅ |
| — and that marker is persisted | `append_entry` beside the context push | ⬜ |

The cancel row's rationale is worth keeping: embedding the half-written
response made the next turn's model continue it instead of answering
the new question. Its test asserts on `agent.context`, not on the
session file, so the persisted half is unpinned the same way the steer
persistence is.

### Compaction

| Race | Required result | |
|---|---|---|
| `/compact` with no boundary | no hooks, no events, no session entry, and a line that says nothing was compacted | ✅ `87a81b4` |
| auto-compact over threshold but no boundary | same — `maybe_compact` forwards the flag rather than hardcoding `true` | ✅ |
| `session_before_compact` fires, compaction then no-ops | forbidden — the boundary is checked before any hook fires, so the pair is always balanced | ✅ |

An unbalanced hook pair is indistinguishable, from a hook author's
side, from nanopi dying mid-compaction. That is why it is forbidden
rather than merely untidy.

### Session identity

| Race | Required result | |
|---|---|---|
| `/new`, `/resume`, `/fork`, `/import` then exit | the exit line names the session you ended in (`app.session_id`) | ⬜ T3.7 |
| in-session switch then a lifecycle hook | the hook payload carries the live session id | ✅ |
| a compaction hook fires | `session_id` is the real id; the reason lives in `arguments.reason` | ✅ |

The first row has no automated coverage on purpose: the two lines print
at the tail of a several-hundred-line async fn with no seam to inject a
fake `App`, and extracting a helper would pin the formatting rather
than which variable the call site passes — which is the entire defect.
`docs/v0.12-manual-test-plan.md` T3.7 covers it instead.

### Plugin events

| Race | Required result | |
|---|---|---|
| event arrives while the plugin is inside another guest call | dropped without waiting (`try_lock`, never a blocking lock) | ✅ |
| a shell hook blocks the event's action | subscribers still receive the event | ✅ |
| a plugin's handler panics or traps | delivery to the next subscriber continues | ✅ |
| a plugin returns a value from `handle-event` | ignored | type |

Delivery is explicitly **not** guaranteed, and that asymmetry is
deliberate: a busy plugin must never be able to extend a turn. An
audit plugin therefore cannot treat its own log as complete — which is
exactly why a *blocked* action must still be delivered, since the
refused requests are the ones such a plugin cares about most.

### Terminal output

| Race | Required result | |
|---|---|---|
| stderr written while the TUI owns the screen | `\r\n`, via `crate::note!` — a bare `\n` staircases in raw mode | ✅ `53bd497` |
| stderr written before `setup_terminal()` | bare `\n` is correct; `note!` handles both via the raw-mode flag | ✅ |
| a thinking delta lands after `Start`'s sticky color | each delta self-contained; `TextDelta` re-arms the color once per interruption | ✅ `d48f1aa` |
| a `note!` line lands in ratatui's managed region | it is wiped on the next redraw — known, unfixed, T4.7 | ⬜ |

## 4. Review checklist

When adding a status line, an `AgentEvent`, or a `note!`:

1. What proves this? Name the session entry. If none, it is an
   observation — phrase it as intent or progress, not as done.
2. Can the action fail or no-op after this is emitted? If so, either
   emit after, or make the wording survive both outcomes.
3. Is the audience the model? Then it cannot see the config, the hooks,
   or the filesystem. Say which mechanism acted and whether it was
   policy.
4. Is there an unknown case? Report it as unknown. Not success, not
   failure.
5. Does the renderer add framing? Then do not add your own brackets.
6. Which interleavings can this participate in? Add the row to §3, with
   ✅ or ⬜ stated honestly.

## Invariants

1. A committed `session::append_entry` line is the only proof that an
   action happened.
2. No observation is phrased as an accomplished fact before its proof
   commits.
3. `SessionEntry::ToolCall` proves intent, never execution.
4. An indeterminate outcome is reported as unknown, and never as
   success or failure.
5. A refusal by user configuration is named as policy, to both
   audiences, and never borrows error vocabulary.
6. A message does not bracket itself when its renderer adds framing.
7. A hook pair (`*_before_*` / `*_*`) either both fire or neither does.
8. A steer that is echoed to the user is delivered, or demoted to a
   follow-up that runs. It is never silently dropped.
9. Every replayed `tool_use` id has a matching result after load.
10. Plugin event delivery is best-effort and never extends a turn; a
    blocked action still delivers its event.
11. Session-identity output reads the live session, never the one bound
    at startup.
12. Anything written to stderr while raw mode is active terminates
    lines with `\r\n`.

## Required tests

Grouped by the invariant they defend. All present unless noted.

### Claims

- `compact_now` returns false and touches nothing when there is no
  boundary; returns true and shrinks the context when there is;
- a no-op compaction fires neither compaction hook;
- an orphaned `tool_use` gets an unknown-outcome result, placed
  directly after its assistant message;
- a partially-answered batch fills only the gaps, in call order;
- a healthy session and a text-only session replay byte-identical —
  this runs on every resume, so a false positive would inject a
  fabricated tool result into a working conversation;
- a refused prompt does not bracket itself, names the `input` hook, and
  says "policy".

### Races

- a steer that misses its turn becomes a follow-up — sent from inside
  `stream_turn`, because enqueuing before `run_turn` exercises the pump
  instead and passes against the unfixed code;
- a cancelled turn keeps pending steers as follow-ups;
- a blocking hook still delivers its event to subscribers;
- a turn delivers every event it can reach (guards against a missing
  `deliver_with` at any of the eleven sites, which is otherwise
  completely silent);
- a panicking handler does not stop delivery to the next subscriber.

### Rendering

- every thinking delta renders with identical SGR state;
- the reply is green again after reasoning and after a tool result;
- the color is re-armed once per interruption, not once per delta;
- a plain turn gains no extra escapes;
- `note!` translates every newline to CRLF under raw mode and leaves
  non-raw output in bare LF.

### Not automated, by decision

- T3.7 — the exit line names the live session (no seam; see §3);
- T4.7 — `note!` output is legible while on screen (it is still wiped
  on redraw);
- T8.2b — `kill -9` mid-tool, then `--continue`.

## What is not adopted from PI

Recorded so the next reader does not re-litigate it:

- **Transactional multi-write commits.** nanopi appends single JSONL
  lines. Atomicity across an entry, a usage row, and a state value is a
  property of PI's store; nanopi has no state values.
- **Restart authority / `effect_pending` state machine.** nanopi has no
  resumable operation state. A lost process loses the turn, and §3 says
  what replay does about it.
- **Typed durable addresses (`value<T>` / `list<T>`).** These serve a
  keyed mutable store. nanopi's session is a transcript.
- **Frame-level partial persistence.** PI persists provider frames so a
  killed process can reduce the latest partial. nanopi streams to the
  terminal and keeps the transcript's visual record only — a deliberate
  trade, since the alternative reintroduces the "model continues its
  own aborted response" bug.

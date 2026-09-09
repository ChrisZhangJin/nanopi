---
task: 260907-i8f
title: Plugin tool calls — stage 3 of docs/plugin-capabilities.md
date: 2026-09-07
commits: 07464d0…5fa5acb
---

# Stage 3: `host-call-tool`, `allow_tools`, and the `/tools` grant row

A plugin can now run nanopi's own built-in tools. That is the sharpest of the
six imports, and the whole stage is about making it small enough to grant
knowingly: a per-tool `allow_tools` gate, one execution path with an origin
flag, a 30s bound, a per-call disclosure, and the `/tools` row that finally
answers "what was this plugin allowed to do" without the user having to read a
config file.

## What shipped

**`ToolCallOrigin`, one execution path** (`07464d0`). `run_one_tool` takes a
tenth parameter. `Model` is byte-identical to before — both existing call sites
pass it and no existing assertion changed. `Plugin { deadline }` gates exactly
three things, each as a single `if` around an existing statement:

1. no `SessionEntry` — not the `ToolCall`, not the `ToolResult`, not the
   blocked-path `ToolResult`. A `tool_call` entry the model never emitted is the
   exact shape that made sessions permanently unresumable until `f70e5cc`;
2. no `AgentEvent` — the rewrite notice, the block `TextDelta`, and the final
   `ToolResult` card are all suppressed;
3. the deadline.

Hooks fire on **both** origins. A plugin does not outrank the user's own
policy, and a hook block comes back as the short `error: blocked by hook:
<reason>` rather than the long paragraph written for the model's benefit
(`PLUGIN_BLOCKED_PREFIX`, owned by `loop_.rs`, matched in `plugin_tools`).

The deadline wraps the `tool.execute` await **only**, not `run_one_tool`.
Wrapping the function would drop the future after `tool_execution_start` had
already fired — manufacturing a second instance of the unbalanced-hook-pair
defect `87a81b4`, in order to bound a timeout. A `bash` child the timeout
abandons may outlive the deadline: the plugin is unblocked, the child is not
killed. Recorded as a known limit in both `plugin-capabilities.md` and
`claims-and-races.md` (marked ⬜ — it is not pinned).

**The seam** (`src/plugin_tools.rs`, non-gated). A process-wide installed
`Dispatch`, following `notify::install_sink` and `plugin_context`: installed at
both Agent build sites and refreshed once per `run_turn` beside stage 2's
`context.system` refresh, so the live `session_id` survives `/new` and
`/resume`. `call_blocking` is the transport only — thread + private
current-thread runtime + `block_on` + `std::sync::mpsc` reply, exactly
`fetch_url`'s shape; a dead channel is `error: tool call failed to run`, never
an `unwrap`.

**The gate** (`d00244e`, `call_tool_gated` in `loader.rs`, beside
`store_set_gated` / `set_context_gated` for the same test-seam reason). Order is
load-bearing and commented as such: grant → dispatch-installed → resolve →
built-ins-only → run → disclose. `allow_tools` is per **tool**, not per plugin,
because the tools are not equivalent; empty (the default) denies everything, and
a name that is not a built-in is a **load error** naming the valid ones rather
than a silent no-op. `bash` in the list gets the escalated `[Extensions]`
warning. Each call that actually ran is disclosed once on the HOST budget —
stage 2's separate counter, for stage 2's reason: a disclosure an adversary can
suppress by flooding `host-notify` is not a disclosure. A refused call discloses
nothing, because it did not happen. The disclosure never carries tool output.

**The grant row** (`5fa5acb`, `src/plugin_grants.rs`, non-gated). Tokens are
pre-baked at load, not derived in the TUI — the TUI can only see the config on
*disk*, which is not necessarily the config the running plugin was loaded
under. `load_all → PluginLoadSummary.grants → Agent.plugin_grants (both build
sites) → app.plugin_grants_cache → grants_section`. Empty slice → zero lines →
no heading, the rule `subscriptions_section` already follows, which is also what
keeps the non-`wasm` build honest.

## Decisions worth remembering

**The spec prescribed something unimplementable, and we corrected the spec
rather than quietly building something else.** §"Implementation path is not
new" called for "an `Arc<ToolRegistry>` carried in `PluginState`". That cannot
work: the registry is still being assembled while `load_all` runs,
`EventSubscribers` does not exist until the Agent is built, and the
`mpsc::Sender<AgentEvent>` is created per turn and has no existence at plugin
load time. None of the three can be captured into a `PluginState` built once at
load. The doc now describes the installed-dispatch seam that exists.

**Built-ins only is the deadlock fix, not permission tidiness.**
`ComponentBridge::execute_tool` takes a **blocking** `self.inner.lock()`, unlike
`handle_event`'s `try_lock`. Two plugins each granted the other's tool would
hang: A's guest call holds A's lock and invokes B's tool, B's guest invokes A's
tool, and A's `execute_tool` waits on the lock A's own in-flight call holds.
`try_lock` guards event delivery but not the tool path, which is *supposed* to
wait — the caller wants the result. One match arm on `ToolSource::Builtin`
removes the cycle by construction. No cycle detection was added and neither lock
was touched.

**A plugin with no grants still gets a `/tools` row, reading `no grants`.**
Omitting it would make "not installed" and "installed, powerless" look
identical, and "this plugin can do nothing" is the answer a user came to
`/tools` to get.

## Deviations from the plan

1. **Modules are declared in `src/lib.rs`, not `src/main.rs`.** The plan named
   `src/main.rs` for both `plugin_tools` and `plugin_grants`. That is simply
   where the file isn't — `plugin_context` and `subscriber` are declared in
   `lib.rs`, and the new modules went beside them, unconditionally, with the
   same comment about why they are not feature-gated.

2. **`call_tool_gated` has one step the plan's ordering did not list: an
   `is_installed()` short-circuit between the grant check and resolution.**
   Without it, "no dispatch is installed yet" collapsed into `error: unknown
   tool "read"` — the author would be told their tool does not exist when what
   actually happened is that tool calls are not available yet. Two different
   problems must not share one message. It sits after the grant check, so an
   ungranted tool is still refused for the right reason.

3. **`allow_network` renders as `allow_network(no allowlist, reaches nothing)`
   in a grant row when the allowlist is empty.** Q3's token list implied a bare
   `allow_network`. But `allow_network = true` with an empty allowlist reaches
   no host at all, and a bare token reads as the opposite to anyone scanning the
   row for exfiltration risk. Pinned by a test.

## Teeth — every reversion applied, run, observed, reverted

| # | Reversion | Observed |
|---|---|---|
| 1 | remove the `allow_tools` gate | RED — `bash` ran: `{"content":"tool error: invalid arguments: command must be a string","is_error":true}` |
| 2 | delete the `ToolSource::Plugin` arm | RED — `{"content":"unknown tool: query","is_error":true}` instead of the refusal naming the extension |
| 3 | let `Plugin` origin write the `SessionEntry::ToolCall` | RED — session-file-byte-unchanged test |
| 4 | let `Plugin` origin send the `AgentEvent::ToolResult` | RED — throwaway-receiver-is-empty test |
| 5 | skip the hooks under `Plugin` origin | RED — both the hooks-fire-on-both-origins test and the hook-block test |
| 6 | `handle_event`'s `try_lock` → blocking `lock()` | RED — `a_busy_plugin_drops_the_event_and_counts_it`: "the event delivered while the plugin was busy must be dropped and counted, not queued". Reverted immediately. **See the note below on why this one did not need the `recv_timeout` harness.** |
| 7 | drop the `PluginRebuild::build` half of the grant | RED — "the per-tool grant must be carried across a trap, not silently dropped" |
| 8 | move `disclose` before the gate | RED — refused-call-discloses-nothing test |
| 9 | route the disclosure through quota-consuming `notify` | **initially GREEN — test strengthened, then RED.** See below. |
| 10 | skip the load-time unknown-tool check | RED — `allow_tools = ["nope"]` loaded, and the failure message showed a compile error rather than the grant error |
| 11 | wrap all of `run_one_tool` in the timeout | RED — balanced-hook-pair-after-timeout test |
| 12 | return the model's long hook paragraph to the plugin | RED — `error: blocked by hook: …` exact-form test |
| 13 | render `grants_section`'s heading on an empty slice | RED — `assertion failed: grants_section(&[]).is_empty()` |

**Reversion 9 was green, which means the test was not testing the thing.**
`a_call_that_ran_is_disclosed_and_a_refused_one_is_not` asserted one disclosure
per call, which `notify()` also satisfies when the budget is untouched. The
separation the design rests on is that a plugin flooding `host-notify` must not
thereby buy itself silent tool calls — so the test now exhausts
`MAX_NOTIFY_PER_TURN` first, drains, and *then* makes the granted call. With
that, reversion 9 reds. Stage 2 set this standard by measuring a weak test at
4-of-8 runs and replacing it; this is the same failure caught the same way.

**Reversion 1 also redded for the wrong reason first.** The refusal message was
`error: tool calls are not available right now` — the gate's own short-circuit,
not the grant check. The test now installs a dispatch before asserting, with a
comment saying why, and the reversion reds showing `bash` actually executing.

**Reversion 6 needed no timeout harness, and that is worth stating rather than
glossing.** The plan pre-flagged it as the one at risk of hanging instead of
failing. It does not, because the existing pin
(`a_busy_plugin_drops_the_event_and_counts_it`) already runs the busy call on a
background thread and asserts on a *counter*: with a blocking lock the event is
delivered late rather than dropped, `dropped_events()` stays 0, and the
assertion fails cleanly in ~1s. The reversion is observable as written; no
`recv_timeout` scaffolding was added, and none is needed.

## Verification

- `cargo test` — 653 lib tests pass, 0 fail (`--test-threads=1`).
- `cargo test --features wasm` — 758 lib + 30 integration + 17 others pass, 0
  fail (`--test-threads=1`). The 30 committed fixture tests staying green **is**
  the demonstration that a sixth import broke no committed guest; the fixtures
  predate the import and cannot call it, which is stated where a reader would
  otherwise expect an end-to-end test.
- `cargo test --features wasm --no-run` — **0 warnings**.
- `grep -c "^world " wit/nanopi-extension.wit` — **3**. The sixth import went on
  `world extension` only; the linear-ladder constraint binds *exports*.
- Both configurations built and tested, which is the point of the non-gated
  module placement: `loop_.rs` and `tui.rs` carry no new
  `#[cfg(feature = "wasm")]`.

Baseline was taken before any change, per the known flakiness hazard. No
`TEST_LOCK` work was attempted — it is tracked separately.

## Follow-ups

- **Wiki.** Per project convention the user-facing plugin page lives in the
  separate wiki repo, unreachable from here. Three things need to land there:
  `allow_tools` (including the `bash` warning and the load-error-on-unknown-name
  rule), `host-call-tool`'s return contract, and the `/tools` `Plugin grants`
  row.
- `VERSION` was not bumped and no tag was created, as instructed.
- The two pre-existing `src/agent/loop_.rs` warnings are unchanged; they were
  not introduced here and were left alone.

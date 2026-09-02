---
phase: quick
plan: 260902-nms
type: execute
wave: 1
depends_on: [260902-m0z]
subsystem: wasm
tags: [wasm, hooks, events, extensions, security]
files_modified:
  - wit/nanopi-extension.wit
  - src/config.rs
  - src/agent/hook.rs
  - src/subscriber.rs
  - src/lib.rs
  - src/wasm/loader.rs
  - src/wasm/host.rs
  - src/wasm/mod.rs
  - src/agent/build.rs
  - src/agent/loop_.rs
  - src/mode/tui.rs
  - examples/wasm-plugin-events/Cargo.toml
  - examples/wasm-plugin-events/src/lib.rs
  - examples/wasm-plugin-events/README.md
  - examples/wasm-plugin-events/.gitignore
  - tests/fixtures/events-plugin.component.wasm
  - tests/wasm_plugin_integration.rs
  - Makefile
  - config.toml.example
  - README.md
  - README_zh.md
  - .release-notes-v0.12.0.md
autonomous: true
requirements: [EV-WIT, EV-GRANT, EV-BRIDGE, EV-DELIVER, EV-TOOLS, EV-DOCS]

must_haves:
  truths:
    - "A plugin that lists an event in `list-events` AND is granted it in `[[extensions]].events` receives `handle-event` with the PI event name and the same payload JSON a shell hook gets on stdin."
    - "An event the config did not grant is never delivered, even if the plugin lists it — and the plugin is told at load which of its requests were refused."
    - "A component that exports neither `list-events` nor `handle-event` loads normally, subscribes to nothing, and keeps all its tools and commands."
    - "A trap inside `handle-event` leaves the plugin's tools callable, and `handle-event`'s return value never affects the turn."
    - "A plugin busy inside a tool call has its event deliveries DROPPED (try_lock, never waits), and the drop is counted and debug-logged."
    - "An event with zero subscribers costs a table lookup — no lock, no JSON serialization."
    - "`events` + `allow_network = true` prints one startup warning; `/tools` lists each plugin's subscriptions."
    - "`cargo build --release` (no wasm feature) still compiles, and `[[extensions]]`/`events` there are still ignored-with-a-warning, not an error."
  artifacts:
    - path: "wit/nanopi-extension.wit"
      provides: "extension-events world with list-events + handle-event"
      contains: "world extension-events"
    - path: "src/subscriber.rs"
      provides: "non-gated EventSubscribers table + EventHandler trait + deliver_with"
      contains: "pub struct EventSubscribers"
    - path: "tests/fixtures/events-plugin.component.wasm"
      provides: "committed real component exporting list-events/handle-event"
    - path: "examples/wasm-plugin-events/src/lib.rs"
      provides: "smallest event-subscribing plugin, ladder-stubbed commands"
  key_links:
    - from: "src/agent/loop_.rs"
      to: "src/subscriber.rs"
      via: "self.event_subscribers.deliver_with(...) at every emit site"
      pattern: "event_subscribers\\.deliver_with"
    - from: "src/wasm/mod.rs"
      to: "src/subscriber.rs"
      via: "PluginLoadSummary.subscribers built from bridge.event_subscriptions()"
      pattern: "event_subscriptions"
    - from: "src/wasm/loader.rs"
      to: "wit/nanopi-extension.wit"
      via: "get_typed_func(\"handle-event\") + try_lock delivery"
      pattern: "handle-event"
---

<objective>
Stage B of `docs/v0.12-events.md`: WASM plugins become event subscribers for
all eleven lifecycle events, observe-only, with the config granting and the
plugin asking.

Purpose: close the split where an author who wants "add a tool AND watch every
prompt" must ship a `.wasm` *and* a shell script with no shared state (§1).
Output: a third WIT world, a precomputed subscriber table, delivery at all
eleven emit sites, a committed real fixture, and the docs that describe it.

**The spec is FROZEN.** `docs/v0.12-events.md` §3 (observe-only), §4.0 (two
phases), §4.1 (the WIT delta), §4.2 (config grants), §4.3 (host shape), §5 (the
four hazards), §6 (out of scope) are decisions, not suggestions. Do not
redesign them. If the code cannot be written as specified, stop and say so
rather than substituting a different design.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@docs/v0.12-events.md
@.planning/quick/260902-m0z-rename-hook-events-pi-names/260902-m0z-SUMMARY.md
@.planning/STATE.md
@CLAUDE.md

Read only what a task needs, when it needs it:
- `wit/nanopi-extension.wit` — the two-world rationale is at the top of the file.
- `src/wasm/loader.rs` — `PluginEngine::{new,with_epoch,load}`, `ComponentBridge`,
  `BridgeInner`, `PluginRebuild::build`, the `list-commands` optional-resolution
  block (loader.rs:679-731) is the exact pattern to copy for `list-events`.
- `src/wasm/host.rs` — `WasmExecuteBridge` (default-bodied methods), `WasmTool`,
  `WasmCommandHandler`, `NoopBridge`.
- `src/wasm/mod.rs` — `PluginHost::load_all`, `PluginLoadSummary`, the existing
  `url_allowlist = ["*"]` startup warning (mod.rs:87-94).
- `src/agent/hook.rs` — `HookEvent` (post-Stage-A, PI names), `HookInput`,
  `run_hooks`, `run_session_hooks`, `retired_hook_key_error`.
- `src/agent/loop_.rs` — `Agent` struct (loop_.rs:74), `HooksConfig`
  (loop_.rs:54), `fire_session_start`/`fire_session_shutdown`, `compact_now`,
  `run_turn`, `run_one_tool`.
- `src/agent/build.rs` — `load_extensions` (both cfg variants, build.rs:110/151).
- `src/mode/tui.rs` — `KeyAction::ShowTools` handler (tui.rs:2607),
  `commands_cache` refresh (tui.rs:3262), `App` field decls (tui.rs:749).
- `src/config.rs` — `ExtensionConfig` (config.rs:149) + its `Default`.
- `examples/wasm-plugin-minimal/src/lib.rs` — the `#![no_std]` guest ABI to
  copy verbatim: bump arena, `cabi_realloc`, `string_result`, `read_string`,
  and `#[export_name = "…"]` (NOT `#[no_mangle]`) for hyphenated WIT names.
- `tests/wasm_plugin_integration.rs` — `fixture()`, the loopback-server pattern,
  and the header comment documenting fixture regeneration.
- `Makefile` — the `plugin` target (embed → new, three steps) and
  `ensure-wasm-tools`.

<interfaces>
Contracts an executor needs and must not re-derive.

Post-Stage-A `HookEvent` (src/agent/hook.rs) — `serde(rename_all="snake_case")`,
so the serialized names ARE PI's names already:

```rust
pub enum HookEvent {
    ToolExecutionStart, ToolExecutionEnd, Input, SessionStart, SessionShutdown,
    BeforeAgentStart, TurnStart, TurnEnd, MessageEnd,
    SessionBeforeCompact, SessionCompact,
}
impl HookEvent { pub fn env_var(self) -> &'static str; }  // "ToolExecutionStart", CamelCase
```

```rust
pub struct HookInput {                     // this IS the wire payload
    pub event: HookEvent,
    pub tool_name: Option<String>,         // skip_serializing_if None
    pub tool_call_id: Option<String>,      // skip_serializing_if None
    pub arguments: serde_json::Value,
    pub cwd: Option<String>,               // skip_serializing_if None
    pub session_id: Option<String>,        // skip_serializing_if None
}
```

```rust
// src/wasm/host.rs — extend this, do not replace it. Note the existing
// default bodies: that is how optional guest exports stay optional.
pub trait WasmExecuteBridge: Send + Sync {
    fn execute_tool(&self, name: &str, args_json: &str) -> Result<ToolOutput, String>;
    fn command_specs(&self) -> Vec<crate::command::CommandSpec> { Vec::new() }
    fn execute_command(&self, name: &str, args: &str)
        -> Result<crate::command::CommandAction, String> { … }
}
```

```rust
// src/wasm/loader.rs — current shape
const EPOCH_TICK: Duration = Duration::from_secs(1);
const EPOCH_BUDGET_TICKS: u64 = 30;
pub struct PluginEngine { engine: Engine, budget_ticks: u64 }
impl PluginEngine {
    pub fn load(&self, wasm_path: &Path, url_allowlist: Vec<String>, cwd: PathBuf,
                allow_fs: bool, allow_network: bool)
        -> Result<(Arc<dyn WasmExecuteBridge>, Vec<ToolSpec>), String>;
}
struct BridgeInner { store: Store<PluginState>, execute: ExecuteFunc,
                     execute_command: Option<CommandFunc> }
```

```rust
// src/wasm/mod.rs
pub struct PluginLoadSummary {
    pub tools: Vec<Arc<dyn crate::tool::Tool>>,
    pub commands: Vec<crate::command::PluginCommand>,
    pub loaded: usize,
    pub errors: Vec<(PathBuf, String)>,
}
```

`src/command.rs` is the precedent for a **non-gated** vocabulary module that
lets `mode/tui.rs` and `agent/loop_.rs` stay free of `cfg(feature = "wasm")`.
`src/subscriber.rs` must follow it exactly.
</interfaces>
</context>

<decisions_from_the_spec>
Restated so no task re-litigates them. Cite the section, not this list, in code
comments.

1. **Observe-only** (§3). `handle-event`'s return value is discarded. This is
   what licenses `try_lock` + drop-on-busy, which is what bounds turn latency.
2. **Two exports, one new world, a LINEAR ladder** (§4.1):
   `extension ⊂ extension-commands ⊂ extension-events`. An events-but-no-commands
   plugin stubs `list-commands`/`execute-command`. Accepted cost.
3. **The config grants; the plugin asks** (§4.2). Delivery requires the event in
   BOTH lists. Absent/empty `events` grants nothing. Requested-but-not-granted is
   reported at load.
4. **One vocabulary** (§2.1 + Stage A). `HookEvent`'s serde snake_case names are
   already PI's names, so there is NO translation table and none is needed. The
   §2.1 diagram showing `pre_tool_use` on the shell side and
   `tool_execution_start` on the WASM side is superseded by Stage A — record
   that in `hook.rs` next to `pi_name()` so the next reader does not go looking
   for the missing table.
5. **All 11 events deliverable**, payload identical to the shell hook's stdin,
   subscriber table precomputed at load so a no-subscriber event costs a lookup.
6. **A separate 2s event epoch budget** (§5.3), advisory: log and continue,
   never fail the turn.
7. **Startup warning** for `events` + `allow_network = true` (§5.1).
8. **`/tools` shows subscriptions** (§5.1).
9. **Out of scope** (§6): UI imports, `sessionManager`, provider-payload
   mutation, dynamic tool registration, veto, and `host-session-append`. Do not
   add any host import in this plan. The import list stays at three.
</decisions_from_the_spec>

<design_decisions_this_plan_makes>
Choices the spec left to the implementation. Each has a reason; do not silently
substitute another.

**D1 — Delivery runs on the blocking pool and IS awaited.**
`spawn_blocking(move || bridge.handle_event(..)).await`, exactly like
`WasmTool::execute`. Two constraints collide: wasmtime's API is synchronous and
runs guest code to completion on the calling thread, and nanopi targets machines
whose tokio runtime has one or two workers — so calling into the guest inline
would stall the TUI ticker, the SSE stream, and key handling (see the comment on
`WasmTool::execute` and the test
`blocking_plugin_call_does_not_starve_the_runtime`). Awaiting the handle rather
than firing and forgetting keeps the spec's ordering (§4.3's pseudocode is
sequential) and keeps at most one delivery per plugin in flight. The cost is
bounded by the 2s event budget — ~12s worst case if the handler fetches, which
is exactly why §5.3 tells plugin authors not to. Write that bound in the WIT
doc and at the delivery site.

**D2 — Subscription state lives on the bridge, not in `load`'s return tuple.**
`load()` keeps returning `(bridge, specs)`; the bridge gains
`event_subscriptions()` (granted ∩ requested, used for delivery and `/tools`)
and `unsatisfied_event_requests()` (requested \ granted, used for the load-time
report). Same reasoning as `command_specs()` — name and dispatch stay together,
which is what makes the name check in `handle_event` trustworthy — and it avoids
churning 20 existing `.load(&…)` call sites' destructuring.

**D3 — The grant list is a parameter to `load`, not read from config inside it.**
`load()` gains one trailing param `events_granted: Vec<String>`. 20 existing
call sites gain `, Vec::new()`. Mechanical; use sed, then read the diff.

**D4 — `Agent` gains one field, `event_subscribers: EventSubscribers`.**
`Default`-able and cheap to clone (an `Arc` inside). ~30 `Agent { … }` struct
literals in `loop_.rs` tests plus `tui.rs:4827` gain
`event_subscribers: Default::default(),`. Mechanical; sed on the existing
`plugin_commands: Vec::new(),` line, which appears in every one of them. A
process-global registry was considered and rejected: the suite already suffers
from shared-global flakiness (STATE.md's `TEST_LOCK` note) and one more global
would make it worse.

**D5 — Grant validation happens at plugin load, not config load.**
`[[extensions]]` must stay ignored-with-a-warning in the stock binary, so an
unknown `events` entry cannot be a `Config::load` hard error. The vocabulary
check (`parse_event_grants`) lives in the non-gated `hook.rs` so it is
unit-testable in both builds, and is *called* from `src/wasm/mod.rs`, surfacing
as a per-entry stderr report — never fatal, matching how every other extension
problem is reported.

**D6 — Poisoned lock is not "busy".** `Mutex::try_lock` returns `Err` for both
`WouldBlock` and `Poisoned`. Match on `TryLockError`: `WouldBlock` drops the
delivery, `Poisoned(e)` recovers with `e.into_inner()` — the same recovery
`execute_tool` already does with `unwrap_or_else(|e| e.into_inner())`. Treating
a poisoned lock as busy would silently stop delivering to that plugin forever.
</design_decisions_this_plan_makes>

<tasks>

<task type="auto" tdd="true">
  <name>Task 1: Vocabulary, grants, the WIT world, and the non-gated subscriber table</name>
  <files>wit/nanopi-extension.wit, src/agent/hook.rs, src/config.rs, src/subscriber.rs, src/lib.rs</files>
  <behavior>
    `src/agent/hook.rs`:
    - `HookEvent::pi_name()` returns the snake_case name for all 11 variants, and
      for every variant equals `serde_json::to_string(&v)` minus the quotes.
    - `parse_event_grants(&["tool_execution_start".into()])` → all granted, no reports.
    - `parse_event_grants(&["pre_tool_use".into()])` → refused, message names
      `pre_tool_use` AND `tool_execution_start` AND `v0.12-events.md`.
    - `parse_event_grants(&["message_update".into()])` and `tool_execution_update`
      → refused with a message saying they fire per streaming delta and are
      permanently undeliverable to plugins.
    - `parse_event_grants(&["nonsense".into()])` → refused, message lists the
      valid names.
    - `parse_event_grants(&[])` → empty grants, no reports (absent grants nothing).
    - `event_payload_json(...)` output parses back into a `HookInput` with the
      same fields, and — pinned by the byte-identity test in Task 4 — is what
      shell hooks receive on stdin.

    `src/subscriber.rs`:
    - `EventSubscribers::default()` is empty; `deliver_with` on it does NOT call
      the payload builder (assert via an `AtomicBool`) — the §4.3 performance claim.
    - A table with a subscriber for `turn_start` only: `deliver_with(TurnStart, …)`
      calls the builder exactly once and the handler exactly once, with the
      payload string unmodified; `deliver_with(TurnEnd, …)` calls neither.
    - Two subscribers to the same event: builder called ONCE, both handlers
      called with the same string (payload built once per event, §4.3).
    - `subscriptions()` returns `[(plugin_name, sorted event names)]` for `/tools`.
    - A handler that panics does not abort delivery to the next subscriber.
  </behavior>
  <action>
Four pieces, no wasm-gated code in this task.

**(a) `wit/nanopi-extension.wit`** — add the third world VERBATIM from
`docs/v0.12-events.md` §4.1 (`world extension-events { include extension-commands;
export list-events: func() -> string; export handle-event: func(event: string,
payload-json: string) -> string; }`), with the doc comments the spec gives.
Extend the file-header note from "TWO WORLDS, on purpose" to the ladder
(`extension ⊂ extension-commands ⊂ extension-events`) and say why a lattice was
refused (§4.1: four worlds now, eight after the next feature). In `handle-event`'s
doc comment state, in this order: the return value is IGNORED (return `"{}"`);
`event` is PI's name; `payload-json` is byte-identical to the shell hook's stdin;
a 2s epoch budget applies to GUEST code only, so a handler that calls
`host-http-get` is bounded by that function's own 10s timeout (~12s total) —
therefore **do not fetch from an event handler**; and delivery is DROPPED without
waiting if this plugin is already inside another guest call.

**(b) `src/agent/hook.rs`** — add, next to `retired_hook_key_error`:
- `pub const EVENT_NAMES: [&str; 11]` — the eleven PI names from §7, in §7's order.
- `pub const PER_DELTA_EVENTS: [&str; 2] = ["message_update", "tool_execution_update"]`
  with the §5.2 comment: these fire per streaming delta, would mean hundreds of
  boundary crossings per turn, are permanently refused for plugins, and any
  future per-delta event inherits the rule. §9 asks for this note at the emit
  site; there is no emit site for either event yet, so this const IS the site —
  say so in the comment so a future author adding `message_update` finds it.
- `impl HookEvent { pub fn pi_name(self) -> &'static str }` — the snake_case
  name, one match arm per variant. Above it, record decision 4 from
  `<decisions_from_the_spec>`: Stage A made the config keys and `HookEvent`'s
  serde names PI's names, so there is ONE vocabulary and the translation table
  §2.1's diagram implies is not needed and does not exist.
- `pub fn parse_event_grants(granted: &[String]) -> (Vec<&'static str>, Vec<String>)`
  — returns accepted names (deduped, as `&'static str` from `EVENT_NAMES`) and
  human-readable refusal reports. Reuse the same retired-name pairs
  `retired_hook_key_error` uses (hoist them to a shared const rather than
  duplicating the table) so a Claude Code name gets "did you mean
  `tool_execution_start`?" per §2.1's corollary.
- `pub fn event_payload_json(event: HookEvent, tool_name: Option<&str>,
  tool_call_id: Option<&str>, arguments: &serde_json::Value, cwd: &Path,
  session_id: Option<&str>) -> String` — builds a `HookInput` and serializes it.
  Then REFACTOR `run_hooks` and `run_session_hooks` to build their per-hook
  stdin JSON through this function, so byte-identity with the WASM payload is
  structural rather than coincidental. `run_hook` keeps its `&HookInput`
  signature; give `HookInput` a small constructor if that reads better than
  threading the JSON. Do not change any existing payload field or its value.

**(c) `src/config.rs`** — `ExtensionConfig::events: Vec<String>`, defaulting to
empty in the `Default` impl. Doc comment: the names are PI's (`tool_execution_start`,
not `pre_tool_use`); absent or empty grants nothing; delivery requires the event
in BOTH this list and the plugin's `list-events`; and this grant is a larger
capability jump than `allow_fs` because a `tool_execution_start` subscriber sees
every tool call's arguments and an `input` subscriber sees every prompt (§5.1).
No validation here — see D5.

**(d) `src/subscriber.rs`** (new, non-gated, registered in `src/lib.rs` next to
`pub mod command;`) — modelled on `src/command.rs`:
- `pub trait EventHandler: Send + Sync { fn handle_event(&self, event: &str, payload_json: &str); }`
  Returns nothing: observe-only (§3) is expressed in the type.
- `pub struct Subscriber { pub plugin_name: Arc<str>, pub events: Vec<&'static str>, pub handler: Arc<dyn EventHandler> }`
- `pub struct EventSubscribers(Arc<HashMap<&'static str, Vec<Arc<Subscriber>>>>)`,
  `Default` (empty), `Clone` (Arc), built by
  `pub fn from_subscribers(Vec<Subscriber>) -> Self` which precomputes the
  event→subscribers index. The header comment must say why the index is
  precomputed: an event with no subscribers must cost a lookup, not a lock and
  not a JSON serialization (§4.3).
- `pub async fn deliver_with<F: FnOnce() -> String>(&self, event: HookEvent, build_payload: F)`
  — look up `event.pi_name()`; return immediately when absent or empty WITHOUT
  calling `build_payload`; otherwise build the payload once and, for each
  subscriber, `spawn_blocking` the handler and await it (D1). A `JoinError`
  (handler panicked) is logged and delivery continues to the next subscriber.
- `pub fn subscriptions(&self) -> Vec<(String, Vec<String>)>` for `/tools`,
  sorted by plugin name for stable output.
- `pub fn is_empty(&self) -> bool` for the `/tools` "nothing is watching" case.
Non-gated on purpose: this is what keeps `agent/loop_.rs` and `mode/tui.rs` free
of `cfg(feature = "wasm")`, exactly as `src/command.rs` does for commands. Say so
in the header.
  </action>
  <verify>
    <automated>cargo test --features wasm 2>&1 | tail -20 && cargo build --release 2>&1 | tail -3 && grep -q 'world extension-events' wit/nanopi-extension.wit && grep -q 'include extension-commands' wit/nanopi-extension.wit && grep -qi 'do not fetch' wit/nanopi-extension.wit && grep -q 'PER_DELTA_EVENTS' src/agent/hook.rs && grep -q 'pub fn event_payload_json' src/agent/hook.rs && grep -q 'pub struct EventSubscribers' src/subscriber.rs && grep -q 'pub mod subscriber' src/lib.rs && grep -vE '^\s*(//|///)' src/config.rs | grep -q 'pub events' && wasm-tools component wit wit/ >/dev/null</automated>
  </verify>
  <done>
`cargo test --features wasm` green with the new unit tests from `<behavior>`;
`cargo build --release` compiles; `wasm-tools component wit wit/` parses all
three worlds; no `cfg(feature = "wasm")` anywhere in `src/subscriber.rs`.
Commit: `feat(events): extension-events world, PI event vocabulary, subscriber table`.
  </done>
</task>

<task type="auto">
  <name>Task 2: The example event plugin and its committed fixture</name>
  <files>examples/wasm-plugin-events/Cargo.toml, examples/wasm-plugin-events/src/lib.rs, examples/wasm-plugin-events/README.md, examples/wasm-plugin-events/.gitignore, tests/fixtures/events-plugin.component.wasm, Makefile, tests/wasm_plugin_integration.rs</files>
  <action>
Build the smallest real component that exports the events pair, and commit it as
a fixture. It is BOTH the §9 deliverable ("smallest thing that logs
`tool_execution_start`") and the only way Task 3's tests can be real.

**Toolchain is already present in this container** — `wasm32-wasip1` std was
installed by hand into `$(rustc --print sysroot)/lib/rustlib/`, and `wasm-tools`
is on PATH. Verify with `make ensure-wasm-tools` before starting; if it fails,
stop and report rather than working around it.

**`examples/wasm-plugin-events/`** — copy `examples/wasm-plugin-minimal/` as the
starting point: same `Cargo.toml` (rename the package to `nanopi-events-plugin`,
lib name follows), same `.gitignore` (`/target/` + `*.component.wasm`), same
`#![no_std]` boilerplate block verbatim (bump arena, `memcmp`, `cabi_realloc`,
`string_result`, `read_string`, `panic_handler`). Exports, all six, via
`#[export_name = "…"]` (hyphens verbatim — `#[no_mangle]` will not do):
- `list-tools` → three tools:
  - `events_seen` — returns the plugin's own event log as JSON:
    `{"total": N, "by_event": {"turn_start": 2, …}}`. This is the observable the
    tests read, and it doubles as the demo of the thing a shell hook cannot do:
    state that survives across calls (§1's table, "hold state").
  - `busy` — spins a fixed, large iteration count (aim ~1s of guest time; a
    `core::hint::black_box`-style volatile counter, NOT an infinite loop — the
    30s tool budget must not fire). Exists so Task 3 can hold the bridge lock and
    prove the `try_lock` drop.
  - `greet` — carried over from the minimal example so the fixture also proves
    ordinary tools keep working after an event trap.
- `execute-tool` — dispatch for the three.
- `list-commands` / `execute-command` — the ladder stubs (§4.1): return `[]` and
  a `{"error":…}` respectively, with a comment saying the stub is the accepted
  cost of the linear ladder and pointing at §4.1.
- `list-events` → `["tool_execution_start", "turn_start", "input"]`. Three names
  on purpose: Task 3 grants a subset, so the fixture proves both directions of
  the intersection with one build.
- `handle-event` — increments the counters, calls `host-log(0, …)` once
  (the §9 "logs the event" requirement), and has two deliberate test affordances,
  both documented in the source as such:
  - if `payload-json` contains the marker string `"nanopi-test-trap"`, trap via
    `core::arch::wasm32::unreachable()` — drives the trap-isolation test;
  - the return value is deliberately NOT valid JSON (return the literal
    `not-json`), which is legal precisely because the host ignores it (§3) —
    this is what makes "the return value is ignored" a tested claim rather than
    an assertion about code.
  Header comment: do not fetch from an event handler (§5.3's ~12s bound).

**`examples/wasm-plugin-events/README.md`** — what it watches, the two-list grant
(§4.2) with a copy-pasteable `[[extensions]]` block including `events = [...]`,
the three-step build, and `--world extension-events`.

**`Makefile`** — add `plugin-events` next to `plugin`, same three steps
(`cargo build --target wasm32-wasip1 --release` → `wasm-tools component embed
wit/ … --world extension-events` → `wasm-tools component new`), with `EVENTS_SRC`
/ `EVENTS_WASM` / `EVENTS_OUT` vars alongside the existing `PLUGIN_*`. Add it to
`.PHONY`. Keep the existing comment convention (why `embed` is not optional).

**Fixture** — build it and copy to `tests/fixtures/events-plugin.component.wasm`,
then `git add` it (tests/fixtures is NOT covered by the examples' `*.component.wasm`
ignore rule, so a plain add works; verify with `git status --porcelain`). CI must
need neither the wasm target nor wasm-tools — that is why the binary is committed.

**`tests/wasm_plugin_integration.rs`** — extend the header comment with the
regeneration recipe for THIS fixture (`--world extension-events`) and update the
"TWO FIXTURES, TWO WORLDS" paragraph to three fixtures / three worlds, keeping
its point intact: `runaway-plugin` stays on `--world extension` and is the only
committed component without `list-commands`, so retargeting it would silently
stop testing optional resolution. Add `events_fixture()` next to `fixture()`, and
one smoke test that the new fixture loads through the CURRENT loader
(`engine.load(&events_fixture(), …)`) and its three tools are listed — the extra
exports must be invisible to a host that does not look for them, which is the
same backward-compatibility guarantee in reverse.
  </action>
  <verify>
    <automated>make ensure-wasm-tools && make plugin-events && test -f tests/fixtures/events-plugin.component.wasm && wasm-tools component wit tests/fixtures/events-plugin.component.wasm | grep -cE 'list-events|handle-event' | grep -q 2 && git ls-files --error-unmatch tests/fixtures/events-plugin.component.wasm && cargo test --features wasm 2>&1 | tail -20 && cargo build --release 2>&1 | tail -3</automated>
  </verify>
  <done>
`tests/fixtures/events-plugin.component.wasm` is committed, `wasm-tools component
wit` on it shows `list-events` and `handle-event` alongside the four inherited
exports, `make plugin-events` reproduces it, and the smoke test proves the
current host loads it unchanged. Suite green.
Commit: `feat(events): example event-subscriber plugin + committed fixture`.
  </done>
</task>

<task type="auto" tdd="true">
  <name>Task 3: Host-side delivery — bridge, event budget, try_lock, subscriber table</name>
  <files>src/wasm/loader.rs, src/wasm/host.rs, src/wasm/mod.rs, src/agent/build.rs, src/agent/loop_.rs, tests/wasm_plugin_integration.rs</files>
  <behavior>
    Integration tests against `events-plugin.component.wasm`, driving
    `bridge.handle_event(...)` directly (the agent loop needs a provider; the
    loop wiring is Task 4's, and `EventSubscribers` is already unit-tested):
    - Granted ∩ requested is delivered: load with `events = ["tool_execution_start",
      "turn_start"]`; `event_subscriptions()` == those two sorted;
      `unsatisfied_event_requests()` == `["input"]`; after
      `handle_event("tool_execution_start", payload)` the `events_seen` tool
      reports `total: 1` and `by_event.tool_execution_start: 1`.
    - A non-granted event is NOT delivered: `handle_event("input", payload)` on
      the same bridge leaves `events_seen` unchanged.
    - Empty grants deliver nothing: load the same fixture with `events = []` →
      `event_subscriptions()` is empty and `handle_event` on any name is a no-op.
    - A plugin without the exports still loads and receives nothing:
      `example-plugin.component.wasm` (world `extension-commands`) loads,
      `event_subscriptions()` is empty, `handle_event("turn_start", …)` is a
      no-op, and its tools and commands still work.
    - The return value is ignored: the fixture returns `not-json` from
      `handle-event`; delivery still counts, no error surfaces, and the plugin is
      still callable afterwards.
    - A trap in `handle-event` leaves tools callable: deliver a payload containing
      `nanopi-test-trap`, then `execute_tool("greet", …)` succeeds and
      `execute_tool("events_seen", …)` works (post-trap rebuild resolved
      `handle-event` again — assert a subsequent normal delivery still counts).
    - try_lock drops rather than waits: one thread runs `execute_tool("busy", …)`;
      the main thread calls `handle_event` in a retry loop (≤3s) until
      `dropped_events()` > 0; assert at least one drop AND that the dropped
      deliveries did not reach the guest (`events_seen` grew by fewer than the
      number of attempts). No sleep-based timing assertion.
    Unit test in `loader.rs`:
    - The event budget is the small one: `PluginEngine::with_epoch(50ms, 2)` is
      not what proves this — instead assert `EVENT_EPOCH_BUDGET_TICKS` < 
      `EPOCH_BUDGET_TICKS` and that `handle_event` arms the event budget (a guest
      handler that spins is cut off; add the spin behind the same
      `nanopi-test-trap`-style marker ONLY if it does not require rebuilding the
      fixture — otherwise pin the constants and the arming call site by test on
      the smaller of the two budgets being used).
  </behavior>
  <action>
**`src/wasm/host.rs`** — extend `WasmExecuteBridge` with four default-bodied
methods (defaults keep `NoopBridge` and every test fake untouched, same reason
the command half has defaults):
- `fn event_subscriptions(&self) -> Vec<String> { Vec::new() }` — granted ∩ requested.
- `fn unsatisfied_event_requests(&self) -> Vec<String> { Vec::new() }` — requested \ granted.
- `fn handle_event(&self, _event: &str, _payload_json: &str) {}` — returns
  nothing (§3).
- `fn dropped_events(&self) -> u64 { 0 }` — how many deliveries were dropped
  because the plugin was busy. Exists so the `try_lock` behavior is observable
  rather than only debug-logged, and so the drop test is deterministic.
Add `pub struct WasmEventHandler { bridge: Arc<dyn WasmExecuteBridge> }`
implementing `crate::subscriber::EventHandler` by forwarding to
`bridge.handle_event`. Mirrors `WasmCommandHandler` exactly — sync, no
`async_trait`, so `crate::subscriber` stays free of it.

**`src/wasm/loader.rs`**:
- `const EVENT_EPOCH_BUDGET_TICKS: u64 = 2;` with the §5.3 rationale: a tool call
  is something the user asked for and is waiting on, an event fires on the
  critical path of every turn, so reusing the 30s budget would let a plugin add
  30s to every loop iteration. Repeat the standing caveat: the budget instruments
  GUEST code, so a handler blocked in `host-http-get` is bounded by that
  function's 10s timeout, ~12s total.
- `PluginEngine::load` gains a trailing `events_granted: Vec<String>` (D3).
  Resolve `list-events` with `get_typed_func(...).ok()` and follow the
  `list-commands` block's four distinctions verbatim, with one deliberate
  difference: export missing → soft (backward compat, the whole point);
  `list-events` traps → hard (a trapped instance is permanently un-enterable);
  malformed JSON → hard (an authoring bug the author must see); and — the
  difference — **events advertised but no `handle-event` export → hard**, same
  shape as "commands advertised but no `execute-command`". Re-arm
  `store.set_epoch_deadline(self.budget_ticks)` before the call, as every other
  guest call at load does. Parse with a `parse_event_requests(&str) ->
  Result<Vec<String>, String>` helper (cap the count, mirroring
  `MAX_COMMANDS_PER_PLUGIN`; 32 is plenty for eleven events, and a plugin
  returning 500 names is an authoring bug). Compute the intersection against
  `events_granted` HERE, so the granted set is fixed at load and a plugin can
  never self-grant by any later means (§4.2 / §5.1).
- `BridgeInner` gains `handle_event: Option<EventFunc>` (a distinct type alias
  from `ExecuteFunc`/`CommandFunc`, same reason the command alias is distinct).
  `PluginRebuild::build` resolves it with `.ok()` — NOT `?` — and must NOT
  re-call `list-events`: a plugin may not change its subscription set
  mid-session, for the same two reasons `list-commands` is not re-called.
- `ComponentBridge` gains `event_subscriptions: Vec<String>`,
  `unsatisfied_event_requests: Vec<String>`, `event_budget_ticks: u64`, and
  `dropped_events: AtomicU64`.
- `impl WasmExecuteBridge for ComponentBridge::handle_event` — §4.3's shape:
  1. return immediately unless `event` is in `event_subscriptions` (defense in
     depth: the host-side table already filtered, but the same double check
     `execute_tool` does against `specs` is what keeps the grant honest);
  2. `match self.inner.try_lock()`: `Err(TryLockError::WouldBlock)` →
     `dropped_events.fetch_add(1)`, one `eprintln!`-style debug line naming the
     plugin and event, return — NEVER wait (§5.4, and the load-bearing link to
     §3: this is only sound because the return value is ignored);
     `Err(TryLockError::Poisoned(e))` → `e.into_inner()` and continue (D6);
     `Ok(g)` → continue;
  3. `set_epoch_deadline(self.event_budget_ticks)`, call, discard the returned
     string, `post_return`;
  4. a trap OR a `post_return` failure → `Self::reset(...)` and one stderr line;
     never propagate (§4.3: a malformed return, a trap, and a timeout are all
     the same non-event, and the trap still rebuilds the instance so the
     plugin's tools keep working).
- Update the file-header wire-contract comment with the two new exports.
- Fix up the 4 in-file `.load(&…)` call sites (`, Vec::new()`).

**`src/wasm/mod.rs`**:
- `PluginLoadSummary` gains `pub subscribers: Vec<crate::subscriber::Subscriber>`.
- In `load_all`, before expanding paths (next to the existing `url_allowlist =
  ["*"]` warning, so both per-entry warnings are said once per entry):
  `crate::agent::hook::parse_event_grants(&cfg.events)` and print each refusal
  report to stderr; then the §5.1 warning when `!granted.is_empty() &&
  cfg.allow_network` — one line, same tone as the `*` warning, naming the path
  and saying that this plugin can read what you type and reach any allowlisted
  host.
- Pass the accepted grants into `engine.load(...)`. On success, if
  `bridge.event_subscriptions()` is non-empty, push a `Subscriber` built from the
  plugin name, the granted events (map back to the `&'static str` from
  `EVENT_NAMES`), and an `Arc<WasmEventHandler>`; and for each
  `unsatisfied_event_requests()` entry print the §4.0 load-time report —
  "watcher: requested `input` — not in `events`, not delivered" — because a
  plugin that appears broken should say why. Also `eprintln!` a registered-
  subscription line, matching the existing "registered extension tool/command"
  lines.

**`src/agent/build.rs`** — `load_extensions` (BOTH cfg variants) returns
`(Vec<PluginCommand>, EventSubscribers)`; the non-wasm variant returns
`(Vec::new(), Default::default())` and keeps its existing ignored-with-a-warning
message unchanged. Build the table with
`EventSubscribers::from_subscribers(summary.subscribers)`. Wire both call sites
(`build_fresh`, and the `hydrate_resumed`/reload path at build.rs:289).

**`src/agent/loop_.rs`** — `Agent` gains
`pub event_subscribers: crate::subscriber::EventSubscribers`, doc-commented with
the same reasoning `plugin_commands` carries (held here so `mode::tui` stays free
of `cfg(feature = "wasm")`; empty in a build without the feature). Then fix the
~30 struct literals (D4): sed the `plugin_commands: Vec::new(),` line to add
`event_subscribers: Default::default(),` after it, in `loop_.rs` and
`tui.rs:4827`; read the resulting diff before committing. No delivery calls yet —
that is Task 4.

Also sed the 16 `.load(&…)` sites in `tests/wasm_plugin_integration.rs`.
  </action>
  <verify>
    <automated>cargo test --features wasm 2>&1 | tail -25 && cargo build --release 2>&1 | tail -3 && grep -q 'EVENT_EPOCH_BUDGET_TICKS' src/wasm/loader.rs && grep -q 'TryLockError::Poisoned' src/wasm/loader.rs && grep -q 'fn dropped_events' src/wasm/host.rs && grep -q 'WasmEventHandler' src/wasm/host.rs && grep -q 'unsatisfied_event_requests' src/wasm/mod.rs && grep -c 'event_subscribers' src/agent/loop_.rs && cargo test --features wasm --test wasm_plugin_integration 2>&1 | tail -15</automated>
  </verify>
  <done>
All seven integration behaviors from `<behavior>` pass against the committed
fixture; the full suite is green (unit + doc + integration) with the integration
count grown by the new tests; `cargo build --release` compiles; no new clippy
errors beyond the documented 55 pre-existing ones.
Commit: `feat(events): deliver lifecycle events to WASM plugins (try_lock, 2s budget)`.
  </done>
</task>

<task type="auto" tdd="true">
  <name>Task 4: Delivery at all eleven emit sites, payload byte-identity, and /tools</name>
  <files>src/agent/loop_.rs, src/agent/hook.rs, src/mode/tui.rs</files>
  <behavior>
    - Byte-identity (unit test in `hook.rs`): a shell hook that dumps its stdin
      to a temp file (`cat > /tmp/…`), run through `run_hooks` for
      `ToolExecutionStart` with a known tool name / args / cwd / session id;
      assert the file's contents (trailing newline trimmed) equal
      `event_payload_json(...)` called with the same inputs. Repeat for one
      `run_session_hooks` event (`SessionStart`, `arguments = {"reason":"new"}`)
      so both payload builders are covered. This is the §4.1 promise — "a plugin
      and a hook see exactly the same thing" — and the only way it stays true is
      a test that would fail if either side drifts.
    - `--no-hooks` gates WASM delivery too: with `permission.hooks_active()`
      false, a subscriber receives nothing. (Assert at whatever seam is reachable
      without a provider — a small unit test over the gate condition is
      acceptable; do not build a fake provider for this.)
    - `/tools` output includes a subscriptions section when the table is
      non-empty and omits it when empty (assert on the formatting helper, not the
      terminal — extract one if none exists).
  </behavior>
  <action>
**Delivery at all eleven sites.** For each emit site in `src/agent/loop_.rs`,
after the shell hooks resolve (§4.3: "at each existing `run_hooks` call site,
after the shell hooks resolve"), add
`self.event_subscribers.deliver_with(HookEvent::X, || crate::agent::hook::event_payload_json(...)).await;`
— `hooks.event_subscribers` for the free `run_one_tool`, which needs a new
`subscribers: EventSubscribers` parameter alongside its existing `hooks:
HooksConfig` clone.

Three things are easy to get wrong here; get them right:
1. **Placement relative to the existing gates.** Every site today is wrapped in
   `if permission.hooks_active() && !hooks.X.is_empty() { … }`. The delivery call
   must be INSIDE the `hooks_active()` gate (`--no-hooks` is an emergency switch
   and must cut plugins too — the same leak v0.9.1 fixed for session hooks) but
   OUTSIDE the `!hooks.X.is_empty()` gate (a WASM subscriber must not require a
   shell hook to exist). Restructure each site to
   `if permission.hooks_active() { if !hooks.X.is_empty() { …existing… } deliver_with(…) }`
   or hoist the outer condition — whichever keeps the diff smallest — but do not
   leave any site where delivery depends on a configured shell hook.
2. **Same values as the shell hook, pre-transform.** Pass exactly the
   `tool_name` / `tool_call_id` / `arguments` / `cwd` / `session_id` handed to
   `run_hooks` at that site, BEFORE any hook transform. For `Input` that means
   `tool_name = Some("")` (the site passes `""`; keep it, so the payload matches
   byte-for-byte) and `arguments = {"prompt": effective_msg}` as it was at entry.
   For `ToolExecutionEnd` that is the `post_payload` object, and the args value
   is the post-`ToolExecutionStart`-transform `effective_args` — which is what
   the shell hook gets there too. Where a site builds its payload inline, hoist
   the `json!({…})` into a local ABOVE the gate so both consumers use one value;
   the closure then only serializes, and only when a subscriber exists.
3. **The closure must not capture anything expensive by move** in a way that
   breaks the no-subscriber fast path. `deliver_with` returns before calling the
   builder when nobody is subscribed (§4.3), so keep serialization strictly
   inside the closure — never `let payload = to_string(...)` above it.

The eleven sites, for the checklist:
`fire_session_start` (SessionStart), `fire_session_shutdown` (SessionShutdown),
`compact_now` ×2 (SessionBeforeCompact, SessionCompact), `run_turn`
(BeforeAgentStart, Input, TurnStart, TurnEnd, MessageEnd), `run_one_tool`
(ToolExecutionStart, ToolExecutionEnd). Add a one-line comment at the
`ToolExecutionStart` site pointing at `hook::PER_DELTA_EVENTS`: any future
per-delta event is undeliverable to plugins (§5.2), and the const is where that
rule lives.

**`/tools` shows subscriptions** (§5.1 / §9). `mode/tui.rs`:
- `App` gains `subscriptions_cache: Vec<(String, Vec<String>)>` next to
  `commands_cache`, refreshed in the same place (`tui.rs:3262`) from
  `a.event_subscribers.subscriptions()`. Non-gated type, so no `cfg` enters
  `tui.rs` — that invariant is why `src/subscriber.rs` exists.
- In the `KeyAction::ShowTools` handler, after the tool list, print a section
  when the cache is non-empty:
  a bold header (`Watching events (N plugins)`, same 108-indexed style as
  "Callable tools"), then one line per plugin — `  {plugin:<20}` in cyan, then
  the comma-joined event names in dark gray. Nothing printed when empty: the
  inventory should not grow a permanently-empty heading. The point of the
  section, per §5.1, is that the inventory answers "what is watching me" and not
  only "what can the model call" — put that in a comment.
  </action>
  <verify>
    <automated>cargo test --features wasm 2>&1 | tail -25 && cargo build --release 2>&1 | tail -3 && test "$(grep -c 'event_subscribers\.deliver_with\|subscribers\.deliver_with' src/agent/loop_.rs)" -ge 11 && grep -q 'subscriptions_cache' src/mode/tui.rs && test "$(grep -c 'cfg(feature = "wasm")' src/mode/tui.rs)" -eq 0 && grep -q 'PER_DELTA_EVENTS' src/agent/loop_.rs</automated>
  </verify>
  <done>
Eleven `deliver_with` calls in `loop_.rs`, one per event; the byte-identity tests
pass for both a `run_hooks` and a `run_session_hooks` event; `/tools` prints
subscriptions when present and nothing when absent; `mode/tui.rs` still contains
zero `cfg(feature = "wasm")`; suite green; release build compiles.
Commit: `feat(events): deliver at all eleven emit sites; /tools shows subscriptions`.
  </done>
</task>

<task type="auto">
  <name>Task 5: Docs, release notes, and the full-suite gate</name>
  <files>config.toml.example, README.md, README_zh.md, .release-notes-v0.12.0.md, docs/v0.12-events.md</files>
  <action>
Docs last, so they describe what shipped rather than what was planned.

**`config.toml.example`** — an `events = [...]` line on the `[[extensions]]`
example, commented with: PI's names (list all eleven, or point at the README
table); absent/empty grants nothing; delivery needs the event in BOTH this list
and the plugin's `list-events`; and the §5.1 capability warning — a
`tool_execution_start` subscriber sees every tool call's arguments and an `input`
subscriber sees every prompt, which is a bigger grant than `allow_fs`, so
combining it with `allow_network = true` is warned about at startup.

**`README.md` + `README_zh.md`** — two edits each:
1. The two-systems table from §1 (add tools / add commands / see events / veto /
   hold state / cost per fire, for shell hooks vs WASM plugins), with the one
   sentence that makes it useful: WASM plugins observe, shell hooks can refuse,
   and §8 is why nanopi keeps both. Update the WASM/extensions section for the
   third world, the `events` grant, the 2s budget, drop-on-busy, and "do not
   fetch from an event handler".
2. `README_zh.md` gets a REAL translation, not a stub — Stage A's summary records
   that as the standing expectation for this repo. Per the user's memory note,
   the Chinese README may discuss restricted-network workarounds; the English one
   must not.

**`.release-notes-v0.12.0.md`** — ADD to the existing `## Highlights` (Stage A
created this file force-added past `.gitignore`; do not rewrite it, and if a
later `git add` is needed use `git add -f`). One Highlights entry for WASM event
subscription: what it does, the two-list grant, observe-only, and the
`/tools` visibility. Nothing goes under `## Compatibility` — Stage B adds
capability and breaks nothing.

**`docs/v0.12-events.md`** — tick the Stage B checkboxes in §9 and NOTHING else.
The note is a frozen design record; do not edit its prose. If implementation
diverged from any §, do not silently rewrite the section — report the divergence
in the SUMMARY's Deviations section instead.

Then the full gate. Do NOT bump `VERSION`, do NOT create a tag (`release.yml`
hard-fails the whole matrix on a tag/VERSION mismatch AFTER publishing an empty
release). Do not revert, amend, or rebase any existing commit on `v0.12.0`. Leave
`nanopi180339.png` / `nanopi180339_1.png` untracked — they are the user's files.
  </action>
  <verify>
    <automated>cargo test --features wasm 2>&1 | tail -25 && cargo build --release 2>&1 | tail -3 && grep -q 'events' config.toml.example && grep -q 'extension-events' README.md && grep -q 'extension-events' README_zh.md && grep -qi 'event' .release-notes-v0.12.0.md && git diff HEAD~5 --name-only | grep -qv '^VERSION$' && test -z "$(git tag --points-at HEAD)" && git status --porcelain | grep -vE 'nanopi180339' | grep -q . && echo "WARN unexpected dirty files" || true</automated>
  </verify>
  <done>
Both READMEs carry the §1 table and the events documentation; `config.toml.example`
documents `events` with the §5.1 warning; the release notes' Highlights mention
event subscription; §9's Stage B boxes are ticked and no other line of
`docs/v0.12-events.md` changed; `VERSION` untouched, no tag; working tree clean
except the two user PNGs; suite green.
Commit: `docs(events): document WASM event subscription (README ×2, example config, release notes)`.
  </done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| host → guest (`handle-event`) | NEW. Every tool call's arguments and every user prompt now cross into plugin memory. Previously a plugin saw only what the model chose to hand it. |
| config → host (`events`) | The grant. The only thing standing between an installed plugin and the whole conversation. |
| guest → host (`list-events`) | The ask. Untrusted, must never be able to widen the grant. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-nms-01 | Elevation of Privilege | `list-events` self-grant | mitigate | Intersection computed in `PluginEngine::load` from the config-supplied `events_granted` and fixed at load; `PluginRebuild::build` never re-calls `list-events`; `handle_event` re-checks the name against the stored granted set (Task 3). |
| T-nms-02 | Information Disclosure | `events` + `allow_network` exfiltration channel (§5.1) | mitigate | Deny-by-default (`events` absent/empty grants nothing), plus a startup warning naming the plugin, plus `/tools` visibility so the channel is not invisible. The `url_allowlist` gate is unchanged and still applies to any fetch. |
| T-nms-03 | Denial of Service | slow/hostile `handle-event` on the turn's critical path | mitigate | 2s event epoch budget (vs 30s for tools); `try_lock` + drop so a busy plugin never blocks an emit; delivery on the blocking pool so a tokio worker is never held; per-delta events refused at load so the frequency assumption in §5.2 cannot be violated by config. |
| T-nms-04 | Denial of Service | `handle-event` blocked inside `host-http-get` | accept | Epoch interruption cannot preempt a running host function, so the bound is ~12s (2s + the fetch's own 10s timeout). Documented in the WIT doc and at the delivery site; no new import is added, and mitigating properly needs the second-instance design §3 defers. |
| T-nms-05 | Tampering | a trap in `handle-event` bricking the plugin | mitigate | Trap and `post_return` failure both call `ComponentBridge::reset`, and the rebuild resolves `handle-event` with `.ok()`; covered by an integration test that calls a tool after a handler trap. |
| T-nms-06 | Repudiation | silently dropped deliveries | mitigate | `dropped_events()` counter plus a debug line naming plugin and event; requested-but-not-granted reported at load (§4.0) so a plugin that looks broken says why. |
| T-nms-SC | Tampering | npm/pip/cargo installs | mitigate | **No new dependencies.** `serde_json` in the new example crate is already used by both existing examples at the same version, from the committed `Cargo.lock` copied with them. No package-manager install task exists in this plan, so no legitimacy audit is required. If a task finds itself wanting a new crate, stop and report instead. |
</threat_model>

<verification>
At EVERY commit, not only at the end:
- `cargo test --features wasm` — green. Baseline at `bac77a5`: 585 unit + 5
  (print_mode_e2e) + 6 doc + 22 (wasm_plugin_integration), 1 ignored. Counts may
  only grow.
- `cargo build --release` (no wasm feature) — compiles. `[[extensions]]` and now
  `events` must remain ignored-with-a-warning there, never a hard error.
- Suite flakiness is PRE-EXISTING (STATE.md: ~3 of 10 `cargo test --lib` runs
  fail a varying subset, caused by ~50 hand-rolled `NANOPI_HOME` set/restore
  sites and a poisoned `TEST_LOCK` cascading one real failure into 13-14
  reported ones). Take a baseline before blaming a change, and re-run with
  `-- --test-threads=1` to confirm. Do NOT fix it here.
- `cargo clippy --features wasm --all-targets -- -D warnings` has ~55
  PRE-EXISTING failures (tracked in
  `.planning/quick/260902-m0z-rename-hook-events-pi-names/deferred-items.md`).
  Do not fix them; do not add new ones. Compare the count before and after.

Structural checks:
- `grep -c 'cfg(feature = "wasm")' src/mode/tui.rs` == 0 and
  `src/subscriber.rs` contains no `cfg(feature = "wasm")` — the non-gated
  vocabulary module is the whole reason it exists.
- `wasm-tools component wit wit/` parses; `wasm-tools component wit
  tests/fixtures/events-plugin.component.wasm` shows `list-events` and
  `handle-event`.
- `git tag --points-at HEAD` empty; `VERSION` not in
  `git diff <base> --name-only`.
</verification>

<success_criteria>
- All eleven events reach a granted, subscribed plugin with the same payload a
  shell hook receives; the byte-identity test proves it for both payload builders.
- Neither list alone delivers anything; refused requests are reported at load.
- A component built against `extension` or `extension-commands` loads unchanged
  and subscribes to nothing.
- A busy plugin's deliveries drop rather than wait, counted and logged; a handler
  trap leaves tools callable; the return value is ignored.
- A no-subscriber event performs no JSON serialization (unit-tested).
- `events` + `allow_network` warns at startup; `/tools` lists subscriptions.
- `docs/v0.12-events.md` §9's Stage B boxes are all ticked, with any divergence
  reported in the SUMMARY rather than edited into the spec.
- `VERSION` untouched, no tag, no existing `v0.12.0` commit rewritten, both user
  PNGs still untracked.
</success_criteria>

<output>
Create `.planning/quick/260902-nms-wasm-event-subscribers/260902-nms-SUMMARY.md`
when done.
</output>
</content>
</invoke>

# Plugin capabilities — the outbound surface

This document specifies what a WASM extension may **do**, as opposed to
what it may **see**. It builds on:

- the three-world WIT ladder in `wit/nanopi-extension.wit`;
- the observe-only event model in `docs/v0.12-events.md` §3;
- the two-sided grant pattern in `docs/v0.12-events.md` §4.2
  (`list-events` ∩ config `events`);
- the claim discipline in `docs/claims-and-races.md`.

## The problem this fixes

v0.12 gave plugins eleven lifecycle events and a config grant to
subscribe to them. What it did not give them was anything to do in
response. The complete outbound surface today is three imports:

```wit
import host-log:      func(level: u8, message: string);
import host-fs-read:  func(path: string) -> string;   // allow_fs, cwd-confined
import host-http-get: func(url: string) -> string;    // allow_network + url_allowlist
```

plus registering tools and commands at load time, and returning one of
three action objects from `execute-command`.

So a plugin can read a file inside the working directory, fetch a URL
by GET, and write a line to stderr. It cannot store anything that
survives a restart, cannot put anything in front of the model, cannot
address the user, and cannot start a turn. The return value of
`handle-event` is discarded by design.

Traced against a concrete want — a memory extension that remembers what
you prefer — the mechanism fails at three separate points, and **none
of them is the missing veto**:

| Step | Available? |
|---|---|
| observe the conversation | partly — see §5 |
| derive facts | yes, in guest code |
| persist them | **no** |
| put them in front of the model | **no** |

## Goals

1. Let a plugin persist state across restarts without granting it the
   filesystem.
2. Let a plugin contribute text to the model's context without letting
   it decide anything.
3. Let a plugin address the user directly, attributed.
4. Let a plugin start a turn, bounded so it cannot loop.
5. Let a plugin use the same tools the model has, gated per tool, so
   the capability model becomes *finer* rather than absent.
6. Keep every addition safe under best-effort event delivery: anything
   a plugin can do must survive its call being dropped.
7. Change neither the lock model nor the observe-only invariant.

## Non-goals

- **Veto.** A plugin cannot refuse or rewrite anything. That is the
  shell hook layer's job and the division is deliberate
  (`docs/v0.12-events.md` §8). Settled twice: once on safety grounds in
  v0.12 §3, and once on sufficiency grounds — listen-and-react turned
  out to need no veto, only an outbound surface.
- **UI.** No status lines, widgets, overlays, renderers, or editors.
  nanopi's TUI has no overlay compositor and its dock is two lines;
  more importantly, plugins here exist to add backend capability, not
  to change how nanopi looks.
- **Providers, subagents, compaction strategy.** These require the
  plugin to sit on the critical path and have its result used, which
  is a different mechanism (a *slot*, not an event) and a separate
  document if it ever happens.
- **Unblocking the return value of `handle-event`.** Every capability
  here is an *import* — the guest calls the host mid-call. The return
  value stays discarded, so §3's argument stands unmodified.

## 1. Why imports rather than return values

An earlier draft carried these capabilities in the return value of
`handle-event`, which would have required replacing
`Mutex<BridgeInner>` + `try_lock` with a worker thread and a channel,
rewriting `docs/v0.12-events.md` §3, and changing the `EventHandler`
signature — whose `-> ()` is what makes observe-only unwritable-wrong
rather than merely documented (`claims-and-races.md` marks it `type`).

Carrying them as imports costs none of that:

```text
plugin receives an event
  → calls host-store-set(…) and host-set-context(…) during handle-event
  → returns "{}"
```

The return value is still discarded. Delivery is still best-effort.
The plugin still cannot change the outcome of anything — it can only
write to its own store and put text in front of the model.

| | via return value | via imports |
|---|---|---|
| lock model | worker thread + channel | **unchanged** |
| `EventHandler` signature | changes | **unchanged** |
| `v0.12-events.md` §3 | rewritten | **unchanged** |
| new code | a concurrency model | five `func_wrap` closures |

## 2. The five imports

All follow the existing in-band error convention: a failure is a
returned string prefixed `error: `, never a trap. A plugin must be able
to continue after being refused.

### 2.1 Persistence

```wit
/// Read this plugin's stored value for `key`. Absent → "".
import host-store-get: func(key: string) -> string;

/// Replace this plugin's value at `key`. Returns "" on success or an
/// `error: ` string. A return of "" means the bytes reached the
/// filesystem, not a buffer.
import host-store-set: func(key: string, value: string) -> string;
```

Gated on `allow_store = true`. State that survives a restart is a
different capability from state in the instance's memory: combined with
`allow_network` it is a durable profile of the user, which is why it is
gated rather than free.

**Keys, not paths.** The plugin never names a filesystem location, so
the entire class of problems `host-fs-read` has to defend against —
`../`, symlinks pointing outward, non-regular files (a FIFO on
`host-fs-read` was an unbounded hang before the regular-file check) —
does not exist here. The host owns the mapping.

| | |
|---|---|
| location | `~/.nanopi/extensions/<stem>/store.json`, one JSON object per plugin |
| bounds | 1 MiB total, 1000 keys, key ≤ 128 chars |
| over quota | `error: store quota exceeded (1 MiB)` — never a silent drop |
| durability | temp file + rename per `set`. No fsync: survives a crash, not a power cut |
| identity | the `.wasm` file stem. Two plugins with the same stem where either has `allow_store` is a **load error**, not a silent share — same policy as a command-name collision |

**Absent reads as `""`.** A plugin should store JSON, so `""` is
unambiguously "nothing stored" — `""` is not valid JSON. The residual
ambiguity (a plugin storing a literal string beginning `error: `) is
inherited from the existing convention and is documented rather than
papered over.

### 2.2 Context contribution

```wit
/// Declare text this plugin wants present in the model's context.
/// REPLACES this plugin's previous contribution — state, not an
/// append. "" clears it. Returns "" or an `error: ` string.
import host-set-context: func(text: string) -> string;
```

Gated on `allow_context = true`.

**Replace, not append**, and that is what makes it safe under dropped
delivery: it is idempotent state, so a call the host never made means
the previous contribution stands for one more turn. An append would
grow without bound and would make a late arrival observable.

| | |
|---|---|
| where | appended to the system prompt at turn assembly, **attributed** |
| bound | 4 KiB per plugin |
| why that bound | it enters **every** request. An unbounded contribution silently multiplies the cost of every turn, which is the opposite of what nanopi is for |

Attribution is not cosmetic:

```text
[context contributed by extension "memory"]
User prefers Rust over Go. This project ships static musl builds.
```

Without the header the model cannot tell a plugin's injected
instructions from the user's own. That is the same rule
`claims-and-races.md` §2 applies to refusals, with the model as the
audience instead of the user.

### 2.3 Addressing the user

```wit
/// One line into the user's scrollback. The HOST prefixes it with this
/// plugin's name — a plugin cannot impersonate another.
import host-notify: func(text: string) -> string;
```

Ungated: it is output, not access. Bounded by a **rate limit** rather
than a grant, because the failure mode is flooding the user's
attention, not privilege:

- N lines per turn, then suppressed with one `… M more suppressed` line
  so the truncation announces itself;
- payload capped like the command actions (`MAX_ACTION_PAYLOAD`, 64 KiB).

Distinct from `host-log`, which writes raw stderr and is therefore
wiped on the next ratatui redraw (the known defect in
`docs/v0.12-manual-test-plan.md` T4.7). `host-notify` goes through
`insert_before` and stays in scrollback.

### 2.4 Starting a turn

```wit
/// Start a turn with this text, as if the user had typed it. Always
/// echoed verbatim to the user first. Steers a running turn, or queues
/// as a follow-up if it arrives too late.
import host-send-user-message: func(text: string) -> string;
```

Gated on `allow_send_message = true`.

Routes into the existing steer/follow-up path, which already answers
"what if this arrives at an awkward moment" — the contract the WIT docs
state for the command action of the same name, and the machinery
`b90b27f` repaired.

**Loop guard, mandatory.** A plugin subscribed to `turn_start` that
calls this creates an infinite loop that spends the user's money. Two
rules:

1. at most one pending message per plugin;
2. a plugin may not call this during a turn that its own message
   started.

Refusal is in-band: `error: a message from this plugin is already
pending`.

### 2.5 Calling nanopi's tools

```wit
/// Invoke one of nanopi's tools and return its result as JSON:
///   {"content": "...", "is_error": false}
/// Refused unless the tool is named in this plugin's `allow_tools`.
import host-call-tool: func(name: string, args-json: string) -> string;
```

This one import replaces two that an earlier draft proposed
(`host-http-request` for outbound POST, `host-fs-list` for directory
enumeration) and reaches further than either: a plugin gets the same
seven tools the model has — `bash`, `edit`, `find`, `grep`, `ls`,
`read`, `write`.

```text
remote executor:  host-call-tool("bash", {"command": "ssh build-host '…'"})
codebase indexer: host-call-tool("find", {"pattern": "\\.rs$"})
                  host-call-tool("read", {"path": "src/main.rs"})
```

**This would retire the sandbox if it were ungated.**
`host-call-tool("bash", …)` is arbitrary command execution; it walks
straight past `allow_fs`'s cwd confinement and `url_allowlist`'s
per-host approval. So it is gated **per tool**:

```toml
[[extensions]]
path = "…/indexer.wasm"
allow_tools = ["find", "read"]     # can walk and read; cannot write or exec

[[extensions]]
path = "…/remote-exec.wasm"
allow_tools = ["bash"]             # needs a shell, and you granted it knowingly
```

Empty (the default) denies everything, matching `url_allowlist` and
`events`. The grant makes the capability model **finer**, not absent: it
can express "may read, may not write" and "may search, may not
execute", which `allow_fs = true` cannot.

**What comes back, exactly.** One string, always; nothing traps. A call
that RAN returns the JSON frame, including a call that ran and failed:

| Outcome | Returned string |
|---|---|
| ran, succeeded | `{"content":"…","is_error":false}` |
| ran, tool errored | `{"content":"tool error: …","is_error":true}` |
| outran the 30s deadline | `{"content":"tool call exceeded the 30s plugin deadline","is_error":true}` |
| not in `allow_tools` | `error: tool "bash" is not in this plugin's allow_tools (…)` |
| not a built-in tool | `error: tool "greet" is supplied by extension "other" — …` |
| no such tool | `error: unknown tool "nope"` |
| args not JSON | `error: args-json is not valid JSON: …` |
| blocked by a hook | `error: blocked by hook: <reason>` |
| tool calls not available yet | `error: tool calls are not available right now` |

The split is the useful part: a bare `error: ` prefix means the call
never happened, and a JSON frame means it did — which is also exactly
the line the host disclosure draws.

`host-fs-read` stays rather than being folded into this. Its
confinement — inside cwd, regular files only — is narrower than
granting `allow_tools = ["read"]`, and it is the simpler thing for the
common case.

#### Hooks fire for plugin-initiated calls

A tool call a plugin makes runs the same `tool_execution_start` /
`tool_execution_end` hooks a model-initiated call does. Otherwise a
plugin would outrank the user's own policy — a plugin could run
`rm -rf` past a `check-rm-rf.sh` the user installed, which contradicts
the whole hook/plugin division.

A refusal comes back in-band:

```text
error: blocked by hook: policy: refusing 'rm -rf /'
```

#### A plugin's tool call is NOT a session entry

It must not be persisted as a `SessionEntry::ToolCall`.

The session transcript is the conversation with the model. A
`tool_call` entry the model never emitted replays into an assistant
message carrying a `tool_use` block with no matching request — the
exact shape that made sessions permanently unresumable until `f70e5cc`
taught replay to synthesize a result. Writing plugin-initiated calls
into the transcript would manufacture that corruption deliberately, on
every plugin tool call.

So these calls are visible through `host-notify` and the log, and
absent from the transcript. The consequence is deliberate and worth
stating plainly: **`--continue` will not show what a plugin did.** A
plugin that wants its actions on the record says so with
`host-notify`, or keeps its own log in its store.

#### Re-entrancy, and why the current lock saves us

A plugin's tool call fires `tool_execution_start`, which broadcasts to
subscribers — possibly including **the plugin currently inside the
guest call**, whose `Store` lock is held.

With a blocking `lock()` that is a deadlock. `handle_event` uses
`try_lock` and drops (`loader.rs:1237`), so the delivery is simply
dropped and the call proceeds.

That was chosen so a busy plugin could never slow the turn's critical
path. It also happens to make `host-call-tool` re-entrancy safe. **The
two are consequences of one constraint, not luck** — but anyone
replacing that lock with a queue must restore the re-entrancy guard
explicitly, because the queue would make the inner delivery *wait* for
a call that cannot finish until the delivery returns.

#### Built-in tools only

`host-call-tool` reaches built-in tools. It must NOT be able to invoke
another plugin's tool, and the reason is a deadlock, not tidiness.

`ComponentBridge::execute_tool` takes a **blocking** lock
(`loader.rs:1396`) — unlike `handle_event`, which uses `try_lock`. So
with two plugins each granted the other's tool: A's guest call holds
A's lock and invokes B's tool; B's guest call then invokes A's tool;
A's `execute_tool` blocks on a lock A's own in-flight call is holding.
Neither returns.

`try_lock` protects event delivery from re-entrancy (see below) but
nothing protects the tool path, because the tool path is *supposed* to
wait — the caller wants the result. Restricting the grant to built-ins
removes the cycle by construction rather than by cycle detection, and
every example in this section (`bash`, `find`, `read`) is a built-in
anyway. `ToolSource::Builtin` makes the check one match arm.

#### One code path, an origin flag

Plugin-initiated calls must fire the same hooks (above) and must NOT
reach the session (below). The temptation is a second, narrower
execution path; the codebase's own history argues against it, since two
paths for one situation is exactly what hid the missing
`drain_steer_to_follow_ups` call (`b90b27f`) and the unbalanced
compaction hooks (`87a81b4`).

So: one `run_one_tool`, plus an origin the caller supplies. Origin
decides **three** things and nothing else — whether a `SessionEntry` is
written, whether an `AgentEvent` tool card is emitted, and the execution
deadline. Hooks fire either way.

Three, not the two an earlier version of this paragraph listed: §"It
needs its own timeout" below demands a host-side bound, and the origin
is where that bound belongs, because it is precisely the model/plugin
distinction that decides whether the call is user-awaited. Stage 3 set
it at 30s, around the `tool.execute` await ONLY — wrapping the whole
function would drop the future after `tool_execution_start` had already
fired, manufacturing a second instance of the unbalanced-hook-pair
defect `87a81b4`. A process the timed-out tool spawned (a `bash` child)
may outlive the deadline: the plugin is unblocked, the child is not
killed. That is a known limit, not a claim. That makes the difference a single explicit switch with a
test per branch, instead of a divergence waiting to happen.

#### It needs its own timeout

The epoch budget bounds GUEST code and cannot preempt a host function
already executing — the same limitation that let `host-fs-read` on a
FIFO hang unboundedly before the regular-file check, and the reason
`host-http-get` carries its own 10s timeout.

`host-call-tool("bash", {"command": "sleep 9999"})` is therefore
outside the epoch's reach. It needs a host-side deadline of its own.
Note the asymmetry when choosing one: a model-initiated `cargo build`
is user-visible and user-awaited, while a plugin's call happens with no
prompt on screen, so the tolerance for a long one is lower, not higher.

#### The user should see it happen

A plugin running `bash` with nothing on screen is the thing this
document's §"A plugin's tool call is NOT a session entry" trades away.
Stage 2 shipped the mechanism to give it back: the HOST emits a
disclosure line per plugin-initiated call, on its own budget — not the
plugin's, for the reason stage 2 established (a disclosure an adversary
can suppress by flooding is not a disclosure).

Real-time visibility, no transcript corruption.

#### Implementation path is not new

`fetch_url` (`loader.rs:332`) already does async I/O from inside a
synchronous `func_wrap` closure: it spawns a thread with its own
current-thread runtime, `block_on`s there, and returns the result over
an `std::sync::mpsc` channel. `host-call-tool` uses the same shape.

**Correction.** An earlier version of this paragraph added "plus an
`Arc<ToolRegistry>` carried in `PluginState`". That is not
implementable, and planning stage 3 is what found it: the registry is
still being assembled while `load_all` runs, `EventSubscribers` does
not exist until `build.rs:182`, and the `mpsc::Sender<AgentEvent>` is
created per turn and has no existence at plugin-load time. None of the
three can be captured into `PluginState`, which is built once at load.

The seam is instead a process-wide installed dispatch, the same shape
`notify::install_sink` and `plugin_context` already use: installed when
the Agent is built, and refreshed once per `run_turn` alongside stage
2's `context.system` refresh so the live `session_id` survives `/new`
and `/resume`.

## 3. Grants, in one place

| Grant | Unlocks | Default |
|---|---|---|
| `allow_fs` | `host-fs-read`, cwd-confined | off |
| `allow_network` + `url_allowlist` | `host-http-get` | off / empty |
| `allow_store` | `host-store-get` / `host-store-set` | off |
| `allow_context` | `host-set-context` | off |
| `allow_send_message` | `host-send-user-message` | off |
| `allow_tools` | `host-call-tool`, per tool named | empty |
| — | `host-log`, `host-notify` | always |

Every grant is two-sided in the same sense as `events`: the plugin can
only use what the config named.

`/tools` shows them. It is the one place a user can go and *ask* what an
installed plugin is allowed to do, so stage 3 built the pipe the earlier
version of this section said was still missing: `load_all` renders one
short token list per successfully-loaded plugin →
`PluginLoadSummary.grants` → `Agent.plugin_grants` → the TUI's
`plugin_grants_cache` → a `Plugin grants (N plugins)` section rendered
under the callable-tool list, beside `Watching events (N plugins)`.

Two details are deliberate. A plugin granted **nothing** still gets a
row, reading `no grants` — "installed, powerless" and "not installed"
must not look identical, and "this plugin can do nothing" is the answer
a user came to `/tools` to get. And only *successfully-loaded* plugins
get a row: a row for a plugin that failed to load would claim a grant
nothing holds.

The startup `[Extensions]` notices stay. They are not a stand-in any
more, they are complementary: the `allow_store` notice names the store
**file**, which a one-line grant row does not carry, and a startup line
lands without the user having to think to ask.

Two combinations deserve the escalated warning `[Extensions]` already
gives `events` + `allow_network`:

- `allow_store` + `allow_network` — a durable profile that can leave
  the machine;
- `allow_tools` containing `bash` — arbitrary execution, which makes
  every other grant on that plugin decorative.

## 4. Interactions

| Interleaving | Required result |
|---|---|
| import called during a dropped event delivery | cannot happen — a dropped delivery never enters the guest |
| `host-set-context` called, then the plugin traps | contribution survives; it is host-side state, not guest memory |
| plugin traps mid-`host-store-set` | the `set` either committed or did not; temp+rename gives no torn file |
| `host-call-tool` fires an event back into the calling plugin | dropped by `try_lock`; call proceeds (§2.5) |
| `host-call-tool` blocked by a hook | in-band `error:`; plugin continues |
| `host-send-user-message` during a turn it started | refused in-band (loop guard) |
| two plugins both `host-set-context` | both contribute, each in its own attributed block, each capped separately |
| plugin reset after a trap | guest memory is lost; the store and the context contribution are not |

That last row is the practical reason persistence is host-side: a trap
already resets the instance (`loader.rs:1088`), so anything kept only
in guest memory is one trap away from gone.

## 5. The payload gap this analysis surfaced

Not an import, and worth its own decision.

Tracing the memory extension end to end showed that a plugin **cannot
see what the model said**:

| Event | `arguments` | Text? |
|---|---|---|
| `input` | `{prompt}` | ✅ the user's words |
| `tool_execution_start` | `{tool_name, arguments}` | ✅ |
| `tool_execution_end` | `{tool_input, tool_response:{content,is_error}}` | ✅ |
| `turn_start` / `turn_end` | `{turn_count, iteration, had_tool_calls}` | ❌ |
| `message_end` | `{turn_count, **response_length**}` | ❌ a length |
| `session_*` | `{reason}` | ❌ |

So the imports above make "remember what the user prefers" work, and
leave "remember what was concluded" out of reach — half the
conversation is invisible.

Closing it means putting the assistant's text in `message_end`'s
`arguments`. That changes neither the lock model nor observe-only, but
it has two real costs:

1. **Shell hooks receive it too.** The payload is shared by contract —
   the WIT docs promise it is byte-identical to what a hook gets on
   stdin. Anyone with a `[[hooks.message_end]]` script starts receiving
   whole replies.
2. **Size.** A long reply is tens of KiB crossing the boundary twice
   per turn. Needs a truncation bound, and the truncation must
   announce itself rather than silently cutting.

Deliberately staged separately from the imports for that reason.

## Invariants

1. Every capability here is an import. `handle-event`'s return value
   stays discarded and `EventHandler` keeps returning `()`.
2. A plugin cannot refuse, rewrite, or delay anything.
3. Every import is refused in-band with an `error: ` string; none
   traps, and a refused plugin can continue.
4. No capability is unlocked without a config grant naming it, except
   `host-log` and `host-notify`.
5. A grant is two-sided: the config's list bounds what the plugin can
   use, never the reverse.
6. `host-call-tool` runs the same hooks a model-initiated call runs.
7. A plugin can only address tools its `allow_tools` names.
8. `host-store-set` returning `""` means the bytes reached the
   filesystem.
9. Quota, rate-limit and loop-guard refusals are always reported, never
   silent — a plugin must never believe it stored, said, or sent
   something it did not.
10. `host-set-context` replaces; contributions never accumulate.
11. Injected context is attributed to its plugin in the prompt.
12. `host-notify` output is prefixed by the host; a plugin cannot
    impersonate another.
13. A plugin cannot start a turn that its own message started.
14. Host-side plugin state (store, context contribution) survives a
    guest trap; guest memory does not.
15. A plugin-initiated tool call is never written to the session
    transcript — it would replay as a `tool_use` the model never
    requested.

## Required tests

### Grants

- each import returns `error:` when its grant is absent, and works when
  present;
- `allow_tools` admits exactly the named tools and refuses the rest,
  including refusing `bash` when only `read` is granted;
- an empty or absent `allow_tools` refuses every tool;
- a grant naming an unknown tool is a load-time error, not a silent
  no-op — same rule as a retired hook key.

### Store

- set/get/replace/absent-reads-empty;
- quota exceeded is reported and stores nothing;
- key-count and key-length caps;
- a trap between set and get does not lose a committed value;
- two plugins with the same file stem and `allow_store` fail to load;
- one plugin cannot read another's keys.

### Context

- contribution appears in the assembled prompt, inside its attributed
  block;
- a second call replaces rather than appends;
- `""` clears;
- over-bound contribution is refused and the previous one stands;
- two plugins get two separate blocks;
- a contribution survives a guest trap.

### Tool calls

- a plugin-initiated call fires `tool_execution_start` and
  `tool_execution_end`;
- a hook blocking it surfaces `error: blocked by hook: …` and the tool
  does not run;
- an event fired by a plugin's own tool call is dropped, not
  deadlocked — the test that pins §2.5's re-entrancy argument;
- a trapping tool does not take down the plugin or the turn;
- a plugin-initiated call writes NO `SessionEntry::ToolCall`, and a
  session containing one replays without an orphaned `tool_use`
  (guards invariant 15 against the `f70e5cc` failure mode).

### Send message

- the text is echoed to the user before it is sent;
- a second call while one is pending is refused;
- a call during the turn its own message started is refused;
- a message arriving mid-stream steers; too late queues as a follow-up.

### Notify

- output carries the plugin's name;
- a plugin cannot forge another plugin's prefix;
- the rate limit engages and announces the suppression count.

## Staging

| Stage | Contents | Why here |
|---|---|---|
| 1 | `host-store-*`, `host-notify` | No interaction with the agent loop. Makes the audit extension fully workable |
| 2 | `host-set-context` | Needs turn assembly and prompt attribution. Makes the rules loader and preference memory work |
| 3 | `host-call-tool` + `allow_tools` | Largest blast radius: registry access, hook firing, re-entrancy. Wants stages 1–2's grant plumbing already in place |
| 4 | `host-send-user-message` | Loop guard is the whole difficulty |
| 5 | `message_end` payload carrying assistant text | Changes shell-hook behavior; decide separately (§5) |

## What is not adopted from PI

Recorded so the next reader does not re-derive it:

- **`ctx.ui.*` in any form.** No status, widget, overlay, editor, or
  renderer. nanopi has no overlay compositor, and plugins here exist to
  add capability, not to change appearance.
- **Veto / transform from a plugin.** Shell hooks own that. Confirmed
  twice — on safety grounds and on sufficiency grounds.
- **Extensions as in-process trusted code.** PI's extensions are
  TypeScript in the host process with no marshalling and no sandbox.
  nanopi's are sandboxed components with a string ABI, so every
  capability is designed rather than inherited. That is the cost of the
  sandbox, and `allow_tools` is what keeps the sandbox worth paying
  for.
- **Native dynamic plugins.** Structurally impossible for the shipped
  artifact: the release binary is statically linked musl
  (`not a dynamic executable`), which has no dynamic loader, so
  `dlopen` cannot work. Would require abandoning static musl.
- **A process per plugin call.** `fork()` without `exec()` is unsafe in
  a multi-threaded tokio process — the child inherits one thread and
  any lock another thread held, the allocator's included. `fork+exec`
  means recompiling the component per call, which is the wrong
  direction for a tool meant to run on constrained hardware.

# nanopi WASM event-subscriber plugin — worked example

Watches three lifecycle events (`tool_execution_start`, `turn_start`,
`input`) and keeps a running tally, exposed back as the `events_seen`
tool. Also ships `busy` (spins for ~1s of guest time, for testing the
host's `try_lock`-drops-rather-than-waits behavior) and `greet` (an
ordinary tool, carried over from `examples/wasm-plugin-minimal`).

This is the top of nanopi's WASM plugin ladder:

```
extension  ⊂  extension-commands  ⊂  extension-events
```

A plugin that only provides tools targets `extension`
(`examples/wasm-plugin-minimal`); one that also registers slash
commands targets `extension-commands` (`examples/wasm-plugin`); this
one also observes lifecycle events, so it targets `extension-events`.
Because the ladder is linear, this plugin still exports
`list-commands` / `execute-command` — as stubs, returning `[]` and an
error respectively. That's the accepted cost of the ladder design over
a world lattice (see the header comment in `wit/nanopi-extension.wit`).

## The two-list grant

Requesting an event via `list-events` is not the same as receiving it.
Delivery requires the event to appear in BOTH lists:

1. this plugin's `list-events` export (fixed at build time — see
   `src/lib.rs`), and
2. the config's `[[extensions]].events` grant (fixed by the user).

A plugin cannot self-grant. Nothing in this plugin's source can expand
what it receives beyond what the user's config allows.

## Prerequisites

```bash
rustup target add wasm32-wasip1
cargo install wasm-tools --locked
```

And a nanopi built with the WASM runtime, which the release binary
does *not* include:

```bash
cargo build --release --features wasm     # in the nanopi repo
```

## Build

Three steps, run **from the repo root** so the `wit/` path resolves
(or just run `make plugin-events`):

```bash
# 1. compile to a core module
cargo build --manifest-path examples/wasm-plugin-events/Cargo.toml \
  --target wasm32-wasip1 --release

# 2. embed the WIT world into the module
wasm-tools component embed wit/ \
  examples/wasm-plugin-events/target/wasm32-wasip1/release/nanopi_events_plugin.wasm \
  -o /tmp/embedded.wasm --world extension-events

# 3. wrap it as a component
wasm-tools component new /tmp/embedded.wasm \
  -o nanopi-events-plugin.component.wasm
```

Note `--world extension-events`, not `extension-commands` — this
plugin observes lifecycle events. The `embed` step is not optional:
`component new` on a bare module yields a component with an empty
world, and the host then fails to find `list-tools`.

Verify:

```bash
wasm-tools component wit nanopi-events-plugin.component.wasm
```

should show `list-events` and `handle-event` alongside the four
inherited exports (`list-tools`, `execute-tool`, `list-commands`,
`execute-command`).

## Install

```bash
mkdir -p ~/.nanopi/extensions
cp nanopi-events-plugin.component.wasm ~/.nanopi/extensions/
```

Then in `~/.nanopi/config.toml`, grant the events this plugin should
actually receive:

```toml
[[extensions]]
path = "~/.nanopi/extensions/nanopi-events-plugin.component.wasm"
events = ["tool_execution_start", "turn_start", "input"]
```

Granting a subset (or an empty list) is fine — this plugin will only
ever see the intersection of what it requests and what the config
grants.

## Verify

Ask the model to do a couple of things, then:

```
> use the events_seen tool
```

should report nonzero counts for whichever of the three events fired
during the session.

## Notes

`handle-event`'s return value is discarded by the host — this plugin
deliberately returns the non-JSON string `not-json` to make that a
tested fact rather than an unverified claim. Do not build a real
plugin's `handle-event` around a meaningful return value; nothing will
ever read it.

Do NOT fetch from an event handler. The epoch budget on event delivery
is smaller than the one used for tool calls, and a handler blocked in
`host-http-get` is additionally bounded by that call's own timeout —
worst case a slow handler costs a turn several seconds for one event.

State kept across calls (the event tally) lives in plain static
integers, not a heap `Vec`/`String` — see the comment on `BumpAlloc` in
`src/lib.rs` for why a bump allocator that resets every call cannot
safely back persistent state.

# Minimal nanopi WASM plugin

The smallest plugin that still exercises a capability gate. Copy this
directory as a starting point.

For the fuller version — four tools, `host-fs-read`, and more notes on the
ABI — see [`../wasm-plugin/`](../wasm-plugin/).

## What it does

| Tool | Needs | Shows |
|---|---|---|
| `greet` | nothing | the minimum viable tool: parse args, return content |
| `fetch_head` | `allow_network` + `url_allowlist` | reaching outside the sandbox, and what a refusal looks like |

## Layout

`src/lib.rs` is split by a single marker comment:

- **Above `YOUR TOOLS`** — boilerplate. The bump allocator, the canonical-ABI
  plumbing, the host import declarations, and the two exports. Copy it verbatim.
- **Below `YOUR TOOLS`** — `TOOL_SPECS` (the JSON Schema the model sees) and
  `dispatch()`. This is the part you replace.

## Build

Run from the **repo root**, so `wit/` resolves:

```bash
cargo build --manifest-path examples/wasm-plugin-minimal/Cargo.toml \
  --target wasm32-wasip1 --release

wasm-tools component embed wit/ \
  examples/wasm-plugin-minimal/target/wasm32-wasip1/release/nanopi_minimal_plugin.wasm \
  -o /tmp/embedded.wasm --world extension

wasm-tools component new /tmp/embedded.wasm \
  -o nanopi-minimal-plugin.component.wasm
```

Three steps, none optional. Skipping `embed` produces a component with an empty
world; the host then can't find `list-tools` and rejects it as "not a nanopi
extension". Don't `cd` into this directory to build either — `embed wit/`
resolves against the repo root, and mixing the two silently embeds a stale
module.

Check what you got:

```bash
wasm-tools component wit nanopi-minimal-plugin.component.wasm
```

```
world root {
  import host-log: func(level: u8, message: string);
  import host-http-get: func(url: string) -> string;
  export list-tools: func() -> string;
  export execute-tool: func(name: string, args-json: string) -> string;
}
```

`host-fs-read` is absent because this plugin declares but never calls it, so LTO
drops it. Only imports you actually use end up in the world.

## Install

```toml
# ~/.nanopi/config.toml
[[extensions]]
path = "/absolute/path/nanopi-minimal-plugin.component.wasm"
allow_network = true
url_allowlist = ["127.0.0.1"]
```

Use an absolute path — `~` expands to `$HOME` and ignores `NANOPI_HOME`.

You're wired up when startup prints:

```
nanopi: registered extension tool "greet"
nanopi: registered extension tool "fetch_head"
```

The host must be built with `--features wasm`; the stock release binary has no
WASM runtime.

## The one real trap

Imports and exports return strings differently:

- an **import** returning a string takes the return area as a **trailing**
  `ret_area: *mut u8` out-param
- an **export** returning a string **returns a pointer** to an 8-byte
  `(ptr, len)` pair

Getting it backwards fails at `wasm-tools component new` with a type mismatch,
not at runtime.

## More

The wiki covers writing, debugging, and the capability gates in depth, in
English and Chinese: <https://github.com/ChrisZhangJin/nanopi/wiki>

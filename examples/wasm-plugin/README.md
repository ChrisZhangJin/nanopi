# nanopi WASM plugin — worked example

Two tools (`wordcount`, `rot13`) exported as a WebAssembly component
that nanopi loads at startup and offers to the model alongside `bash`
and `read`.

Neither tool does anything a built-in couldn't. That's on purpose —
the point is the wiring, with nothing to install and nothing to
sandbox. Replace the bodies in `src/lib.rs` with whatever your plugin
actually does.

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

Three steps, run **from the repo root** so the `wit/` path resolves:

```bash
# 1. compile to a core module
cargo build --manifest-path examples/wasm-plugin/Cargo.toml \
  --target wasm32-wasip1 --release

# 2. embed the WIT world into the module
wasm-tools component embed wit/ \
  examples/wasm-plugin/target/wasm32-wasip1/release/nanopi_example_plugin.wasm \
  -o /tmp/embedded.wasm --world extension

# 3. wrap it as a component
wasm-tools component new /tmp/embedded.wasm \
  -o nanopi-example-plugin.component.wasm
```

`cargo build` alone gives you a **core module**, which nanopi rejects —
the host instantiates components, not modules.

The `embed` step is what makes `component new` work: without a WIT
world baked in, `component new` has no idea which core exports to
promote, and silently produces a component with an empty world that
the host then can't find `list-tools` in.

Verify before installing:

```bash
wasm-tools component wit nanopi-example-plugin.component.wasm
```

should print

```wit
world root {
  export list-tools: func() -> string;
  export execute-tool: func(name: string, args-json: string) -> string;
}
```

An empty `world root {}` means step 2 was skipped or the export names
didn't match the WIT.

## Install

```bash
mkdir -p ~/.nanopi/extensions
cp nanopi-example-plugin.component.wasm ~/.nanopi/extensions/
```

Then in `~/.nanopi/config.toml`:

```toml
[[extensions]]
path = "~/.nanopi/extensions/nanopi-example-plugin.component.wasm"
```

## Verify

On startup nanopi logs each tool it registers:

```
nanopi: registered extension tool "wordcount"
nanopi: registered extension tool "rot13"
```

Then ask for one:

```
> rot13 the string "Hello, World"
```

If you instead see `[[extensions]] entries ignored — this build has no
WASM support`, the binary was built without `--features wasm`.

## What the host expects

Two exports, one optional import. Full definitions in
[`../../wit/nanopi-extension.wit`](../../wit/nanopi-extension.wit).

| Direction | Function | Signature |
|---|---|---|
| export | `list-tools` | `() -> string` |
| export | `execute-tool` | `(name: string, args-json: string) -> string` |
| import | `host-log` | `(level: u8, message: string)` |

`list-tools` returns a JSON array read once at load:

```json
[
  {
    "name": "rot13",
    "description": "Apply the ROT13 letter substitution to a string.",
    "parameters": {
      "type": "object",
      "properties": { "text": { "type": "string" } },
      "required": ["text"]
    }
  }
]
```

`parameters` is a JSON Schema handed to the model verbatim, so its
`description` strings are the entire spec the model gets for how to
call your tool. They are worth writing carefully.

`execute-tool` returns:

```json
{ "content": "Uryyb, Jbeyq", "is_error": false }
```

`is_error` is optional and defaults to `false`.

## Notes on the source

`src/lib.rs` hand-writes the canonical ABI shims (`cabi_realloc`, the
`(ptr, len)` marshalling). That is tractable here only because the
interface is two functions carrying one primitive type. **If you build
something wider, generate the bindings with `wit-bindgen`** instead of
copying these shims.

The crate is `#![no_std]` with a bump allocator over a static arena —
it keeps the module small and there is nothing to free, since the
arena resets at the top of each call and the host copies results out
before returning. Don't carry that pattern into a long-running
program.

## Sandbox

The component runs in wasmtime with no filesystem and no network. A
plugin can compute and call `host-log`; that's it. The
`allow_network` / `allow_fs` / `url_allowlist` fields on
`[[extensions]]` are plumbed through the config but the host functions
behind them are not implemented yet.

A trap inside a plugin surfaces to the model as a failed tool call
rather than taking nanopi down, and a `.wasm` that fails to load is
skipped with a warning rather than blocking startup.

A plugin may not register a tool name that already exists — collisions
are reported and skipped, so a plugin cannot quietly replace `bash`.

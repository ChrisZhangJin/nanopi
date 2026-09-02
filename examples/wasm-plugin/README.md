# nanopi WASM plugin — worked example

Four tools (`wordcount`, `rot13`, `readfile`, `fetch`) and two slash
commands (`/todo`, `/explain`), exported as a WebAssembly component
that nanopi loads at startup.

A **tool** is something the model decides to call; it shows up
alongside `bash` and `read`. A **command** is something the user types;
it shows up in the `/` palette. `wordcount` and `rot13` are pure
computation, deliberately trivial — the point is the wiring.
`readfile` and `fetch` show the other half: how a plugin reaches
outside itself, and what happens when the user hasn't granted that
capability. Replace the bodies in `src/lib.rs` with whatever your
plugin actually does.

Because this plugin registers commands it targets the
`extension-commands` world. A tool-only plugin should target
`extension` instead — see `examples/wasm-plugin-minimal`.

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
  -o /tmp/embedded.wasm --world extension-commands

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
  export list-commands: func() -> string;
  export execute-command: func(name: string, args: string) -> string;
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
nanopi: registered extension tool "readfile"
```

Then ask for one:

```
> rot13 the string "Hello, World"
```

For the commands, type `/` and look for `/todo` and `/explain` in the
palette. `/todo` prints straight to your scrollback and the model never
sees it; `/explain rust lifetimes` starts a turn on your behalf, echoed
verbatim first so nothing is said invisibly.

If you instead see `[[extensions]] entries ignored — this build has no
WASM support`, the binary was built without `--features wasm`.

## What the host expects

Two required exports, two optional ones, three optional imports. Full
definitions in
[`../../wit/nanopi-extension.wit`](../../wit/nanopi-extension.wit).

| Direction | Function | Signature | |
|---|---|---|---|
| export | `list-tools` | `() -> string` | required |
| export | `execute-tool` | `(name: string, args-json: string) -> string` | required |
| export | `list-commands` | `() -> string` | optional |
| export | `execute-command` | `(name: string, args: string) -> string` | optional |
| import | `host-log` | `(level: u8, message: string)` | |
| import | `host-fs-read` | `(path: string) -> string` | |
| import | `host-http-get` | `(url: string) -> string` | |

The command exports are optional in the host, which resolves them with
a soft miss — that is what lets an `extension`-world plugin keep
loading unchanged.

Note the asymmetry: an **export** returning a string returns a pointer
to a `(ptr, len)` pair, while an **import** returning one takes the
return area as a trailing out-param. Getting it backwards fails at
`wasm-tools component new` with a type mismatch, not at runtime.

Both string-returning imports therefore declare a trailing `ret_area`:

```rust
#[link_name = "host-fs-read"]
fn host_fs_read_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
#[link_name = "host-http-get"]
fn host_http_get_raw(ptr: *const u8, len: usize, ret_area: *mut u8);
```

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

The component runs in wasmtime with no ambient authority. It reaches
outside only through host functions the user opts into.

`readfile` demonstrates the gate. It calls `host-fs-read`, which:

- returns `error: filesystem access denied ...` unless the plugin's
  `[[extensions]]` entry sets `allow_fs = true`;
- confines reads to the working directory even when allowed. Paths are
  canonicalized before the containment check, so `../` traversal and
  symlinks pointing outward are both refused.

To try it, add `allow_fs = true` to the `[[extensions]]` entry and ask
the model to read a file in the project.

`fetch` demonstrates the other gate. It calls `host-http-get`, which:

- returns `error: network access denied ...` unless the plugin's
  `[[extensions]]` entry sets `allow_network = true`;
- then returns `error: url_allowlist does not permit <url> ...` unless
  the URL's host is covered by `url_allowlist`. An **empty allowlist
  denies everything**, so the switch alone reaches nothing;
- matches on the URL's *host*, not a substring, so an entry covers its
  subdomains and any port while `https://evil.com/?x=api.github.com`
  and `https://api.github.com@evil.com/` are refused;
- accepts only `http`/`https`, times out at 10s, and does not follow
  redirects — a 3xx would otherwise walk the fetch onto a host the
  allowlist never approved.

To try it:

```toml
[[extensions]]
path = "~/.nanopi/extensions/nanopi-example-plugin.component.wasm"
allow_network = true
url_allowlist = ["api.github.com"]
```

then ask the model to fetch `https://api.github.com/zen`. Point it at
any other host and the tool call comes back as an error naming
`url_allowlist`.

A trap inside a plugin surfaces to the model as a failed tool call
rather than taking nanopi down, and a `.wasm` that fails to load is
skipped with a warning rather than blocking startup.

A plugin may not register a tool name that already exists — collisions
are reported and skipped, so a plugin cannot quietly replace `bash`.

Commands are stricter, and the two rules differ. A **tool** collision
is first-wins: the tool already registered stays, the newcomer is
skipped. A **command** collision refuses *both* claimants — if two
plugins each register `/deploy`, neither gets it, because silently
picking a winner would mean typing `/deploy` runs whichever plugin
happened to load first. A command colliding with a built-in like
`/compact` is simply skipped. Every case prints a warning naming the
plugin(s), and never affects that plugin's other commands or its
tools.

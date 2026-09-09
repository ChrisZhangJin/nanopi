---
gsd_quick: true
task: "WASM plugin system — dynamic tool & command registration from .wasm files"
branch: v0.11.0
created: 2026-08-28
status: in-progress
---

## Goal

Let users write extensions as `.wasm` files (compiled from Rust / Go / TS / C)
and load them at startup via `[[extensions]]` in `config.toml`. Plugins can
register new LLM-callable tools and slash commands, all inside a sandbox.

This is P2 of the M2 Extensions milestone (see docs/pi-vs-nanopi.md §4.3).

## Design

### Plugin loading

```toml
# ~/.nanopi/config.toml
[[extensions]]
path = "~/.nanopi/extensions/query_tool.wasm"
```

Or by glob / directory:

```toml
[[extensions]]
path = "~/.nanopi/extensions/"
```

`config.rs` gains a `Vec<ExtensionConfig>` field. On `Agent::build_fresh` /
`hydrate_resumed`, each `.wasm` is loaded via wasmtime, `register()` is called,
and tools / commands are injected into the agent's registry.

### WIT interface (wit-bindgen)

```wit
package nanopi:extension;

interface host {
    // Host functions exposed to the plugin
    host-log: func(level: u8, message: string);
}

interface tool-api {
    record tool-spec {
        name: string,
        description: string,
        parameters-json: string,
    }

    record tool-output {
        content: string,
        is-error: bool,
    }

    // Plugin calls this during init to register tools
    register-tool: func(spec: tool-spec);

    // Host calls this when the LLM invokes a registered tool
    execute-tool: func(name: string, args-json: string) -> tool-output;
}

interface plugin {
    // Called once after the module is loaded. Receives host API.
    init: func();
}
```

### Sandbox / security

- WASM runs in wasmtime with `wasmtime::component` (component model)
- Default: no filesystem, no network
- Host functions exposed:
  - `host-log(level, message)` — always available
  - Future: `host-http-get(url)` / `host-fs-read(path)` behind config flags
- Plugin crash → tool returns error, does not crash nanopi

### Binary size

wasmtime 19+ with component model (~1.5 MB stripped/LTO'd).
Current nanopi unpacked: 4.4 MB. With wasmtime: ~6 MB. UPX'd: ~2.5 MB.
Within the 5 MB budget for packed artifacts.

## Implementation phases

### Phase 1 — Cargo.toml + wasmtime integration (minimal skeleton)
- Add `wasmtime = "19"` (or latest stable) dependency
- Add `ExtensionConfig` to config.rs
- Add `src/wasm/mod.rs` with `PluginHost` struct
- Wire into `Agent::build_fresh`: load each extension, call init()

### Phase 2 — WIT + register-tool
- Define WIT interface
- Implement `register-tool` host side: tool spec → `ToolRegistry`
- Implement `execute-tool` host side: call into WASM, return `ToolOutput`
- New `WasmTool` impl of `Tool` trait that dispatches to plugin

### Phase 3 — Slash commands
- Extend WIT with command registration
- Wire into TUI command handler

### Phase 4 — Host functions (network, filesystem)
- `host-log` always on
- `host-http-get` gated on `[[extensions]].allow_network = true`
- `host-fs-read` gated on `[[extensions]].allow_fs = true`
- URL whitelist / path whitelist

## Out of scope (this phase)

- Provider registration (separate phase)
- UI extension / custom renderers
- Event subscription (will use the new shell-hook events as bridge)

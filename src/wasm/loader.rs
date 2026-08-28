//! WASM plugin instantiation + invocation via wasmtime component model.
//!
//! Phase 3 (v0.11.0). Compiles a `.wasm` component, links host imports,
//! instantiates it, and calls its exported `list-tools` / `execute-tool`
//! functions for real.
//!
//! Design note — why no `wit-bindgen`:
//! The WIT interface here is deliberately narrow (two exports, both
//! `(string, string) -> string`), so hand-rolling the `get_typed_func`
//! calls is less machinery than a build-time codegen step, and keeps
//! `cargo build --features wasm` free of a `build.rs`. If the interface
//! grows records/variants, switch to wit-bindgen.
//!
//! Wire contract with the guest (see `wit/nanopi-extension.wit`):
//!   - `list-tools: func() -> string`
//!       Returns a JSON array of `{name, description, parameters}`.
//!   - `execute-tool: func(name: string, args-json: string) -> string`
//!       Returns a JSON object `{content, is_error}`.
//! Strings rather than WIT records keep the ABI to one primitive type,
//! which is what makes the hand-rolled binding tractable.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use crate::agent::context::ToolSpec;
use crate::tool::ToolOutput;
use crate::wasm::host::WasmExecuteBridge;

/// Per-plugin host state carried in the wasmtime `Store`. Phase 4 will
/// hang the network/fs capability gates off this.
pub struct PluginState {
    /// URL allowlist for a future `host-http-get`. Recorded now so the
    /// capability plumbing has somewhere to land.
    #[allow(dead_code)]
    url_allowlist: Vec<String>,
}

/// One running wasmtime engine; threadsafe, cheap to clone (internally
/// `Arc`-refcounted).
pub struct PluginEngine {
    engine: Engine,
}

impl PluginEngine {
    pub fn new() -> Result<Self, String> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Conservative resource caps — a runaway plugin should not
        // allocate gigabytes of linear memory.
        config.max_wasm_stack(512 * 1024); // 512 KiB
        Engine::new(&config)
            .map(PluginEngine::from)
            .map_err(|e| format!("wasmtime engine init failed: {e}"))
    }

    /// Read a `.wasm` file, compile it, link host imports, instantiate,
    /// and query its exported `list-tools`.
    ///
    /// Returns the bridge (for later `execute-tool` calls) plus the tool
    /// specs the plugin advertises. Callers register those specs into
    /// `ToolRegistry` so the LLM sees them alongside built-in tools.
    pub fn load(
        &self,
        wasm_path: &Path,
        url_allowlist: Vec<String>,
    ) -> Result<(Arc<dyn WasmExecuteBridge>, Vec<ToolSpec>), String> {
        let bytes = std::fs::read(wasm_path)
            .map_err(|e| format!("read {} failed: {e}", wasm_path.display()))?;
        let component = Component::from_binary(&self.engine, &bytes)
            .map_err(|e| format!("compile {} failed: {e}", wasm_path.display()))?;

        let mut linker: Linker<PluginState> = Linker::new(&self.engine);
        // Host import: `host-log(level: u8, message: string)`.
        // Always available — logging is not a capability that needs
        // gating. Levels mirror the WIT doc: 0 trace .. 3 error.
        linker
            .root()
            .func_wrap(
                "host-log",
                |_store: wasmtime::StoreContextMut<'_, PluginState>,
                 (level, message): (u8, String)| {
                    let tag = match level {
                        0 => "trace",
                        1 => "info",
                        2 => "warn",
                        _ => "error",
                    };
                    eprintln!("[wasm:{tag}] {message}");
                    Ok(())
                },
            )
            .map_err(|e| format!("link host-log failed: {e}"))?;

        let mut store = Store::new(
            &self.engine,
            PluginState {
                url_allowlist: url_allowlist.clone(),
            },
        );
        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(|e| format!("instantiate {} failed: {e}", wasm_path.display()))?;

        // Query the plugin's tool list up front. A plugin that doesn't
        // export `list-tools` is not a nanopi extension — reject it
        // loudly at load time rather than silently registering nothing.
        let list_tools = instance
            .get_typed_func::<(), (String,)>(&mut store, "list-tools")
            .map_err(|e| {
                format!(
                    "{} does not export `list-tools`: {e}",
                    wasm_path.display()
                )
            })?;
        let (specs_json,) = list_tools
            .call(&mut store, ())
            .map_err(|e| format!("list-tools trapped: {e}"))?;
        list_tools
            .post_return(&mut store)
            .map_err(|e| format!("list-tools post_return failed: {e}"))?;

        let specs = parse_tool_specs(&specs_json)?;

        // Resolve `execute-tool` once so per-call dispatch is just a
        // `.call()`. Missing it is only fatal if the plugin actually
        // advertises tools.
        let execute = instance
            .get_typed_func::<(String, String), (String,)>(&mut store, "execute-tool")
            .map_err(|e| {
                format!(
                    "{} exports tools but not `execute-tool`: {e}",
                    wasm_path.display()
                )
            })?;

        let bridge: Arc<dyn WasmExecuteBridge> = Arc::new(ComponentBridge {
            specs: specs.clone(),
            // The Store is not Sync, and a component instance is
            // single-threaded by construction. nanopi runs tool calls
            // concurrently (`join_all`), so serialize plugin entry
            // behind a Mutex rather than handing out a shared &mut.
            inner: Mutex::new(BridgeInner {
                store,
                execute,
            }),
        });
        Ok((bridge, specs))
    }
}

impl From<Engine> for PluginEngine {
    fn from(engine: Engine) -> Self {
        Self { engine }
    }
}

/// What `list-tools` returns, before conversion to `ToolSpec`.
#[derive(Debug, Deserialize)]
struct WireToolSpec {
    name: String,
    description: String,
    /// JSON Schema object for the tool's parameters.
    parameters: serde_json::Value,
}

/// What `execute-tool` returns.
#[derive(Debug, Deserialize)]
struct WireToolOutput {
    content: String,
    #[serde(default)]
    is_error: bool,
}

fn parse_tool_specs(json: &str) -> Result<Vec<ToolSpec>, String> {
    let wire: Vec<WireToolSpec> = serde_json::from_str(json)
        .map_err(|e| format!("list-tools returned invalid JSON: {e} (got {json:?})"))?;
    Ok(wire
        .into_iter()
        .map(|w| ToolSpec {
            name: w.name,
            description: w.description,
            parameters: w.parameters,
        })
        .collect())
}

type ExecuteFunc = wasmtime::component::TypedFunc<(String, String), (String,)>;

struct BridgeInner {
    store: Store<PluginState>,
    execute: ExecuteFunc,
}

struct ComponentBridge {
    specs: Vec<ToolSpec>,
    inner: Mutex<BridgeInner>,
}

impl WasmExecuteBridge for ComponentBridge {
    fn execute_tool(&self, name: &str, args_json: &str) -> Result<ToolOutput, String> {
        if !self.specs.iter().any(|s| s.name == name) {
            return Err(format!("plugin does not export tool {name:?}"));
        }
        // A panicking plugin call poisons the mutex. Recover rather
        // than propagating — one bad call should not brick every
        // later call to this plugin.
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *guard;
        let (out_json,) = inner
            .execute
            .call(
                &mut inner.store,
                (name.to_string(), args_json.to_string()),
            )
            .map_err(|e| format!("execute-tool trapped: {e}"))?;
        inner
            .execute
            .post_return(&mut inner.store)
            .map_err(|e| format!("execute-tool post_return failed: {e}"))?;

        let wire: WireToolOutput = serde_json::from_str(&out_json).map_err(|e| {
            format!("execute-tool returned invalid JSON: {e} (got {out_json:?})")
        })?;
        Ok(ToolOutput {
            content: wire.content,
            is_error: wire.is_error,
            metadata: None,
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_specs_reads_json_array() {
        let json = r#"[
            {"name":"query","description":"run SQL","parameters":{"type":"object"}},
            {"name":"ping","description":"ping a host","parameters":{"type":"object"}}
        ]"#;
        let specs = parse_tool_specs(json).expect("valid");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "query");
        assert_eq!(specs[1].description, "ping a host");
        assert_eq!(specs[0].parameters["type"], "object");
    }

    #[test]
    fn parse_tool_specs_rejects_garbage() {
        let err = parse_tool_specs("not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "got {err}");
    }

    #[test]
    fn parse_tool_specs_accepts_empty_list() {
        let specs = parse_tool_specs("[]").expect("valid");
        assert!(specs.is_empty());
    }

    /// `is_error` defaults to false when the plugin omits it — a
    /// plugin that only cares about the happy path shouldn't have to
    /// spell out `"is_error": false` on every call.
    #[test]
    fn wire_tool_output_is_error_defaults_false() {
        let w: WireToolOutput = serde_json::from_str(r#"{"content":"ok"}"#).unwrap();
        assert_eq!(w.content, "ok");
        assert!(!w.is_error);
    }

    #[test]
    fn engine_new_succeeds() {
        assert!(PluginEngine::new().is_ok());
    }

    /// A non-wasm file must fail at compile, not panic.
    #[test]
    fn load_rejects_non_wasm_bytes() {
        let engine = PluginEngine::new().unwrap();
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-not-wasm-{}", crate::util::uuid::v7()));
        std::fs::write(&p, b"definitely not a wasm component").unwrap();
        // `unwrap_err()` needs the Ok half to be Debug, and
        // `Arc<dyn WasmExecuteBridge>` isn't — match instead.
        match engine.load(&p, Vec::new()) {
            Ok(_) => panic!("garbage bytes must not compile as a component"),
            Err(e) => assert!(e.contains("compile"), "got {e}"),
        }
        let _ = std::fs::remove_file(&p);
    }
}

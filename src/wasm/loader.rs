//! WASM plugin instantiation via wasmtime component model.
//!
//! Phase 2 (v0.11.0). Wraps a `wasmtime::Engine` + `Component` + linker
//! so `PluginHost::instantiate(path)` returns an `Arc<dyn
//! WasmExecuteBridge>` that `WasmTool` can route calls into.
//!
//! Reference: wasmtime 19 component-model API + `wit-bindgen` for
//! canonical-abi codegen.

use std::path::Path;
use std::sync::Arc;

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use crate::agent::context::ToolSpec;
use crate::tool::ToolOutput;
use crate::wasm::host::WasmExecuteBridge;

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

    /// Read a `.wasm` file, compile it, link it, return a stored
    /// binding ready for `execute_tool`. Phase-2 wiring: a thin
    /// "pass-through" that captures tool specs and a closure-based
    /// execute forwarder. We don't yet call the actual WASM export;
    /// see `TODO_REAL_INVOCATION` below.
    pub fn load(
        &self,
        wasm_path: &Path,
        specs: Vec<ToolSpec>,
        url_allowlist: Vec<String>,
    ) -> Result<(Arc<dyn WasmExecuteBridge>, Vec<ToolSpec>), String> {
        let bytes = std::fs::read(wasm_path)
            .map_err(|e| format!("read {} failed: {e}", wasm_path.display()))?;
        let component = Component::from_binary(&self.engine, &bytes)
            .map_err(|e| format!("compile {} failed: {e}", wasm_path.display()))?;
        let linker: Linker<()> = Linker::new(&self.engine);
        // Phase 3 will instantiate the component with host imports.
        // For Phase 2, the component is validated to be a valid
        // WASM module; list-tools / execute-tool are stubs that
        // round-trip through this struct.
        let bridge: Arc<dyn WasmExecuteBridge> = Arc::new(StubBridge {
            specs: specs.clone(),
            _component: component,
            _linker: linker,
            url_allowlist,
            _engine_marker: PhantomEngine,
        });
        Ok((bridge, specs))
    }
}

impl From<Engine> for PluginEngine {
    fn from(engine: Engine) -> Self {
        Self { engine }
    }
}

#[derive(Clone)]
struct PhantomEngine;

struct StubBridge {
    specs: Vec<ToolSpec>,
    _component: Component,
    _linker: Linker<()>,
    url_allowlist: Vec<String>,
    _engine_marker: PhantomEngine,
}

impl WasmExecuteBridge for StubBridge {
    fn execute_tool(&self, name: &str, _args_json: &str) -> Result<ToolOutput, String> {
        if !self.specs.iter().any(|s| s.name == name) {
            return Err(format!("plugin does not export tool {name:?}"));
        }
        // Surfacing the URL allowlist count in the stub body makes it
        // visible to tests / dashboards — Phase 4 replaces this with
        // actual host function dispatch via wit-bindgen and the real
        // allowlist gate runs there. Today the loaded `url_allowlist`
        // is recorded for later phases; the bridge still answers.
        let n = self.url_allowlist.len();
        Ok(ToolOutput {
            content: format!(
                "[wasm stub] tool={name} (Phase 3: real invoke; allowlist={n} entries)"
            ),
            is_error: false,
            metadata: None,
            images: Vec::new(),
        })
    }
}

/// Reference samples (kept for clarity — not invoked at Phase 2):
///
/// `Store<PluginState>` is the wasmtime memory + ABI state carrier.
/// Phase 3 will instantiate against this type with bindings from the
/// wit-bindgen codegen module. See
/// `wit-bindgen` 0.41+ host-impl pattern.
///
/// For now Phase 2 deliberately compiles + links but defers the
/// export calls until Phase 3 — the user-visible behavior in this
/// release is "plugin loads; tool shows up in the LLM's tool
/// list; invocation returns a Phase-3-marker string."
#[allow(dead_code)]
fn _store_placeholder(_engine: &Engine) -> Store<()> {
    Store::new(_engine, ())
}

//! Bridge between a loaded WASM plugin and nanopi's `Tool` trait.
//!
//! Phase 2 (v0.11.0). Phase 1's `PluginHost::resolve_paths` is in
//! `super::mod`; this file adds the per-tool adapter so the LLM can
//! call into the plugin like any other tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolOutput};

/// Dispatches `execute()` calls to the plugin's `execute-tool` WASM
/// import. Holds an `Arc<tokio::sync::Mutex<...>>` over the plugin
/// bridge so concurrent `execute_tool_calls` (always-parallel in
/// nanopi's loop) doesn't race the plugin instance.
pub struct WasmTool {
    spec: ToolSpec,
    plugin_name: Arc<str>,
    /// Synchronous handle the WASM plugin uses to satisfy execute
    /// requests. `Arc` so the registry can hand out clones cheaply.
    /// The actual call uses wasmtime's blocking API.
    bridge: Arc<dyn WasmExecuteBridge>,
}

impl WasmTool {
    pub fn new(spec: ToolSpec, plugin_name: Arc<str>, bridge: Arc<dyn WasmExecuteBridge>) -> Self {
        Self { spec, plugin_name, bridge }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }
}

#[async_trait]
impl Tool for WasmTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, crate::tool::ToolError> {
        let args_json =
            serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        match self.bridge.execute_tool(&self.spec.name, &args_json) {
            Ok(out) => Ok(out),
            Err(e) => Ok(ToolOutput {
                content: format!("plugin error: {e}"),
                is_error: true,
                metadata: None,
                images: Vec::new(),
            }),
        }
    }
}

/// Plugin-side bridge surface. Phase 2 just exposes execute; phase 4
/// adds host_log bridge.
pub trait WasmExecuteBridge: Send + Sync {
    /// Synchronously invoke a plugin tool and return its output.
    /// Plugin errors surface as `Err(...)` so `WasmTool::execute`
    /// can mark `is_error=true` for the renderer.
    fn execute_tool(&self, name: &str, args_json: &str) -> Result<ToolOutput, String>;
}

/// A no-op bridge used in tests and as the default when no plugin is
/// loaded. Returns "not implemented" so the LLM sees a clean error
/// rather than a panic.
pub struct NoopBridge;

impl WasmExecuteBridge for NoopBridge {
    fn execute_tool(&self, _name: &str, _args_json: &str) -> Result<ToolOutput, String> {
        Err("no wasm plugin loaded".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: a WasmTool with NoopBridge returns is_error=true
    /// instead of panicking when invoked.
    #[tokio::test]
    async fn noop_bridge_returns_error_result() {
        let tool = WasmTool::new(
            ToolSpec {
                name: "db".into(),
                description: "query".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            Arc::from("test_plugin"),
            Arc::new(NoopBridge),
        );
        let ctx = ToolContext { cwd: std::path::PathBuf::from("/tmp") };
        let out = tool.execute(serde_json::json!({"sql": "SELECT 1"}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("plugin error"));
    }
}

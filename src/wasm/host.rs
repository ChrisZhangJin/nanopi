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

        // `execute_tool` is wasmtime's synchronous API — it runs guest
        // code to completion on the calling thread. Awaiting it inline
        // would block a tokio worker for the whole plugin call, and
        // nanopi targets machines where the runtime has one or two
        // workers total: a single plugin call could stall the TUI
        // ticker, the SSE stream, and key handling at once. The
        // blocking pool exists for exactly this, and keeps the workers
        // free to keep the UI alive while the plugin computes.
        //
        // This is also what makes the `std::sync::Mutex` inside
        // ComponentBridge correct rather than merely lucky — the lock
        // is never held across an `.await`, because there is no await
        // inside the closure.
        let bridge = self.bridge.clone();
        let name = self.spec.name.clone();
        let outcome =
            tokio::task::spawn_blocking(move || bridge.execute_tool(&name, &args_json)).await;

        let err = match outcome {
            Ok(Ok(out)) => return Ok(out),
            Ok(Err(e)) => e,
            // The blocking task panicked or was aborted. Report it as a
            // failed tool call like any other plugin failure — the
            // model gets something it can react to, and the turn
            // survives.
            Err(join_err) => format!("plugin call did not complete: {join_err}"),
        };
        Ok(ToolOutput {
            content: format!("plugin error: {err}"),
            is_error: true,
            metadata: None,
            images: Vec::new(),
        })
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

    /// A plugin call must not occupy a tokio worker while it runs.
    ///
    /// Pinned to a single-worker runtime on purpose: that is the shape
    /// of the hardware nanopi targets, and it is the configuration
    /// where calling wasmtime's blocking API inline starves everything
    /// else — the TUI ticker, the SSE stream, key handling.
    ///
    /// The call has to go through `tokio::spawn` to reproduce it. A
    /// `#[tokio::test]` body runs on the calling thread via `block_on`,
    /// not on a worker, so blocking inline there starves nothing and
    /// the test would pass either way. The real caller is the spawned
    /// turn task, which does sit on a worker.
    ///
    /// The timer task is the detector: queued behind the tool task on
    /// the one worker, it can only fire if the tool task yields.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_plugin_call_does_not_starve_the_runtime() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct SleepyBridge;
        impl WasmExecuteBridge for SleepyBridge {
            fn execute_tool(&self, _name: &str, _args: &str) -> Result<ToolOutput, String> {
                std::thread::sleep(std::time::Duration::from_millis(400));
                Ok(ToolOutput {
                    content: "done".into(),
                    is_error: false,
                    metadata: None,
                    images: Vec::new(),
                })
            }
        }

        let tool = WasmTool::new(
            ToolSpec {
                name: "slow".into(),
                description: "blocks for a while".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            Arc::from("sleepy_plugin"),
            Arc::new(SleepyBridge),
        );

        // Occupies the single worker for the duration of the call.
        let call = tokio::spawn(async move {
            let ctx = ToolContext { cwd: std::path::PathBuf::from("/tmp") };
            tool.execute(serde_json::json!({}), &ctx).await
        });

        let ticked = Arc::new(AtomicBool::new(false));
        let flag = ticked.clone();
        let ticker = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag.store(true, Ordering::SeqCst);
        });

        let out = call.await.unwrap().unwrap();

        // Checked before awaiting the ticker: the question is whether it
        // got to run *while* the plugin was working, not whether it runs
        // eventually.
        assert!(
            ticked.load(Ordering::SeqCst),
            "the timer never fired during the 400ms plugin call — it held the only worker"
        );
        assert!(!out.is_error, "bridge returned Ok, got {:?}", out.content);
        ticker.await.unwrap();
    }
}

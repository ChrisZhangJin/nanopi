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
    /// The `.wasm` this tool came from. Carried alongside the display
    /// name because `/tools` shows the path — a stem like `my-plugin`
    /// does not tell you which file on disk to go edit.
    plugin_path: Arc<str>,
    /// Synchronous handle the WASM plugin uses to satisfy execute
    /// requests. `Arc` so the registry can hand out clones cheaply.
    /// The actual call uses wasmtime's blocking API.
    bridge: Arc<dyn WasmExecuteBridge>,
}

impl WasmTool {
    pub fn new(
        spec: ToolSpec,
        plugin_name: Arc<str>,
        plugin_path: Arc<str>,
        bridge: Arc<dyn WasmExecuteBridge>,
    ) -> Self {
        Self {
            spec,
            plugin_name,
            plugin_path,
            bridge,
        }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }
}

/// Dispatches one plugin slash command to the plugin's
/// `execute-command` export.
///
/// Mirrors [`WasmTool`], minus the async. `WasmTool::execute` owns the
/// `spawn_blocking` hop because `Tool` is an async trait driven by the
/// agent loop; for a command the hop belongs to the TUI, which needs
/// the `JoinHandle` anyway to keep drawing while the guest runs. Being
/// sync also keeps `async_trait` out of the non-gated
/// [`crate::command`] module.
pub struct WasmCommandHandler {
    bridge: Arc<dyn WasmExecuteBridge>,
}

impl WasmCommandHandler {
    pub fn new(bridge: Arc<dyn WasmExecuteBridge>) -> Self {
        Self { bridge }
    }
}

impl crate::command::CommandHandler for WasmCommandHandler {
    fn run(&self, name: &str, args: &str) -> Result<crate::command::CommandAction, String> {
        self.bridge.execute_command(name, args)
    }
}

/// Dispatches one lifecycle event to the plugin's `handle-event` export.
///
/// Mirrors [`WasmCommandHandler`] exactly: sync, no `async_trait`, so
/// `crate::subscriber` — which defines [`crate::subscriber::EventHandler`]
/// — stays free of `cfg(feature = "wasm")`. The bridge itself owns the
/// `try_lock`-drops-rather-than-waits behavior (`docs/v0.12-events.md`
/// §5.2); this type is just the seam between the non-gated subscriber
/// table and the gated bridge.
pub struct WasmEventHandler {
    bridge: Arc<dyn WasmExecuteBridge>,
}

impl WasmEventHandler {
    pub fn new(bridge: Arc<dyn WasmExecuteBridge>) -> Self {
        Self { bridge }
    }
}

impl crate::subscriber::EventHandler for WasmEventHandler {
    fn handle_event(&self, event: &str, payload_json: &str) {
        self.bridge.handle_event(event, payload_json);
    }
}

#[async_trait]
impl Tool for WasmTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn source(&self) -> crate::tool::ToolSource {
        crate::tool::ToolSource::Plugin {
            name: self.plugin_name.to_string(),
            path: self.plugin_path.to_string(),
        }
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

/// Plugin-side bridge surface: everything the rest of nanopi is allowed
/// to ask a loaded component to do.
///
/// The command half carries default bodies so an implementor that only
/// deals in tools — `NoopBridge` below, the fakes in tests — stays
/// untouched, and so a component with no `list-commands` needs no
/// special case at the call site.
pub trait WasmExecuteBridge: Send + Sync {
    /// Synchronously invoke a plugin tool and return its output.
    /// Plugin errors surface as `Err(...)` so `WasmTool::execute`
    /// can mark `is_error=true` for the renderer.
    fn execute_tool(&self, name: &str, args_json: &str) -> Result<ToolOutput, String>;

    /// Slash commands this plugin advertised at load time. Empty for a
    /// component that does not export `list-commands`, which is the
    /// common case and the reason this has a default.
    ///
    /// Read off the bridge rather than returned from `load` because the
    /// bridge is also what dispatches them — keeping name and dispatch
    /// together is what makes `execute_command`'s name check
    /// trustworthy. Tool specs go the other way because they must reach
    /// `ToolRegistry` at load time and are fatal if malformed.
    fn command_specs(&self) -> Vec<crate::command::CommandSpec> {
        Vec::new()
    }

    /// Synchronously run one of `command_specs()`. `args` is raw text.
    ///
    /// `Err` is a plugin-side failure — a trap, a malformed payload. An
    /// in-band `CommandAction::Error` is the plugin reporting a
    /// user-level problem, which is a normal outcome, not a fault.
    fn execute_command(
        &self,
        name: &str,
        _args: &str,
    ) -> Result<crate::command::CommandAction, String> {
        Err(format!("plugin exports no command {name:?}"))
    }

    /// Lifecycle events this plugin will actually receive: the
    /// intersection of its `list-events` export and the config's
    /// `[[extensions]].events` grant, fixed at load time (never
    /// re-derived — a plugin cannot expand its own grant mid-session).
    /// Empty for a component with no `list-events`, which is the
    /// default and the common case.
    fn event_subscriptions(&self) -> Vec<String> {
        Vec::new()
    }

    /// Events this plugin's `list-events` requested but the config did
    /// not grant. Purely for the load-time report (`docs/v0.12-events.md`
    /// §6) — nothing dispatches against this list.
    fn unsatisfied_event_requests(&self) -> Vec<String> {
        Vec::new()
    }

    /// Deliver one lifecycle event. Observe-only: the return value (if
    /// any) is discarded (§3). Must never block the caller waiting on a
    /// busy plugin — implementations use `try_lock` and drop rather than
    /// wait.
    fn handle_event(&self, _event: &str, _payload_json: &str) {}

    /// How many deliveries were dropped because the plugin was still
    /// busy with a previous call. Monotonically increasing for the life
    /// of the bridge.
    fn dropped_events(&self) -> u64 {
        0
    }
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
    use crate::command::{CommandAction, CommandHandler, CommandSpec};

    /// A bridge that answers commands but has no wasm behind it —
    /// the cheapest seam for exercising the command path.
    struct CommandBridge(Result<CommandAction, String>);

    impl WasmExecuteBridge for CommandBridge {
        fn execute_tool(&self, _name: &str, _args_json: &str) -> Result<ToolOutput, String> {
            Err("not a tool bridge".into())
        }
        fn command_specs(&self) -> Vec<CommandSpec> {
            vec![CommandSpec {
                name: "todo".into(),
                description: "the list".into(),
            }]
        }
        fn execute_command(&self, _name: &str, _args: &str) -> Result<CommandAction, String> {
            self.0.clone()
        }
    }

    /// Pins the trait's default bodies. A future implementor that
    /// forgets the command half must degrade to "no commands", not to
    /// a panic or a phantom command.
    #[test]
    fn the_bridge_defaults_mean_no_commands() {
        assert!(NoopBridge.command_specs().is_empty());
        let err = NoopBridge.execute_command("todo", "").unwrap_err();
        assert!(
            err.contains("todo"),
            "the name belongs in the message: {err}"
        );
    }

    #[test]
    fn the_handler_passes_every_action_through_unchanged() {
        for action in [
            CommandAction::Print("printed".into()),
            CommandAction::SendUserMessage("asked".into()),
            CommandAction::Error("refused".into()),
        ] {
            let h = WasmCommandHandler::new(Arc::new(CommandBridge(Ok(action.clone()))));
            assert_eq!(h.run("todo", "args").unwrap(), action);
        }
    }

    /// A plugin-side failure stays an `Err`. It must NOT be silently
    /// turned into `CommandAction::Error`, because the two mean
    /// different things: one is the plugin reporting a user-level
    /// problem, the other is the plugin itself misbehaving.
    #[test]
    fn a_bridge_failure_surfaces_as_err() {
        let h = WasmCommandHandler::new(Arc::new(CommandBridge(Err("trapped".into()))));
        assert_eq!(h.run("todo", "").unwrap_err(), "trapped");
    }

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
            Arc::from("/tmp/test_plugin.wasm"),
            Arc::new(NoopBridge),
        );
        let ctx = ToolContext { cwd: std::path::PathBuf::from("/tmp") };
        let out = tool.execute(serde_json::json!({"sql": "SELECT 1"}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("plugin error"));
    }

    /// `/tools` has to be able to say "this one came from a plugin".
    /// The `Tool::source` default is `Builtin`, so forgetting the
    /// override here would make every plugin tool claim to ship with
    /// nanopi — which is precisely the confusion `/tools` exists to
    /// remove.
    #[test]
    fn a_plugin_tool_reports_its_origin() {
        let tool = WasmTool::new(
            ToolSpec {
                name: "greet".into(),
                description: "say hi".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            Arc::from("my-plugin"),
            Arc::from("/tmp/my-plugin/my-plugin.component.wasm"),
            Arc::new(NoopBridge),
        );
        assert_eq!(
            tool.source(),
            crate::tool::ToolSource::Plugin {
                name: "my-plugin".into(),
                path: "/tmp/my-plugin/my-plugin.component.wasm".into(),
            }
        );
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
            Arc::from("/tmp/sleepy_plugin.wasm"),
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

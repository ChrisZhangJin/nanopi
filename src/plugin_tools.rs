//! The host side of `host-call-tool` (`docs/plugin-capabilities.md`
//! §2.5): an installed dispatch that lets a plugin drive the ONE
//! existing tool-execution path.
//!
//! **Why this lives at the crate root and not under `src/wasm/`.** The
//! same reason `src/subscriber.rs` and `src/plugin_context.rs` give:
//! the agent loop is on the reading side, so that path stays free of
//! `#[cfg(feature = "wasm")]` and the plugin layer reaches IN rather
//! than the loop reaching out. Without the feature nothing ever calls
//! [`install`], [`call_blocking`] is never called either, and the
//! non-wasm build is byte-identical apart from this dead-but-compiled
//! module.
//!
//! **Why an installed dispatch and not an `Arc<ToolRegistry>` in
//! `PluginState`.** §"Implementation path is not new" prescribed
//! carrying the registry in `PluginState`. That is not implementable,
//! and this comment exists so nobody re-derives the idea and wonders
//! why it was not done:
//!
//!   - `load_extensions` (`agent/build.rs`) is *building* the registry
//!     when `PluginHost::load_all` runs. Plugin tools are pushed into
//!     it afterwards, so a handle taken at load time would either be a
//!     snapshot missing every plugin tool or a borrow of a value still
//!     being mutated.
//!   - `EventSubscribers` does not exist until later in the same
//!     function, so a `PluginState` built during `load_all` cannot
//!     carry one and hooks-plus-subscribers delivery would be lost.
//!   - the `mpsc::Sender<AgentEvent>` is PER TURN and does not exist at
//!     all outside a run.
//!
//! So the seam is process-wide and installed, following
//! `notify::install_sink` and `plugin_context`'s registry. The doc was
//! corrected in this stage rather than left standing false.
//!
//! **Why it is refreshed per turn.** `session_id` and `session_path`
//! change under `/new`, `/resume` and `/fork` — the session-identity
//! row of `docs/claims-and-races.md` §3. A dispatch that bound one id
//! at startup would hand hooks a stale session id for the rest of the
//! process, so the dispatch carries the id and `run_turn` re-installs
//! it. Installing is idempotent and replaces wholesale.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
use crate::agent::hook::HookConfig;
use crate::agent::loop_::HooksConfig;
use crate::agent::permission::PermissionGate;
use crate::tool::ToolRegistry;

/// Everything `run_one_tool` needs, minus the per-call arguments.
///
/// Every field is `Clone` and cheaply so: `ToolRegistry`,
/// `PermissionGate`, `HooksConfig` and `EventSubscribers` all derive or
/// implement `Clone` already, which is what makes a process-wide
/// snapshot possible without touching any of them.
#[derive(Clone)]
pub struct Dispatch {
    pub registry: ToolRegistry,
    pub cwd: PathBuf,
    pub permission: PermissionGate,
    pub hooks: HooksConfig,
    pub subscribers: crate::subscriber::EventSubscribers,
    /// The REAL session path, deliberately — not a dummy.
    ///
    /// Nothing is written to it under `Plugin` origin, and that is the
    /// origin switch's job. Passing a harmless path instead would make
    /// the invariant depend on this caller picking one, i.e. on a
    /// convention rather than on the switch. The test asserts the real
    /// file is byte-unchanged across a plugin-initiated call.
    pub session_path: PathBuf,
    pub session_id: String,
}

/// The refusal when no dispatch is installed.
///
/// Reachable in ordinary operation: an `-p` path that never built an
/// `Agent`, or a plugin calling during load before the first install.
/// It is a string, never a panic and never a hang (invariant 3).
const NO_DISPATCH: &str = "error: tool calls are not available right now";

/// How long a plugin-initiated tool call may run.
///
/// Justified at the execution site in `agent::loop_` — briefly: above
/// `host-http-get`'s 10s because a legitimate `find` or `cargo` build
/// needs more, and far below the model path's effectively-unbounded
/// wait because a plugin's call runs with nothing on screen.
pub const PLUGIN_TOOL_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(30);

static DISPATCH: OnceLock<Mutex<Option<Dispatch>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<Dispatch>> {
    DISPATCH.get_or_init(|| Mutex::new(None))
}

fn lock() -> std::sync::MutexGuard<'static, Option<Dispatch>> {
    // Recovered rather than propagated, for the reason `notify::lock`
    // gives: a poisoned cell should not disable the capability for the
    // rest of the session.
    cell().lock().unwrap_or_else(|e| e.into_inner())
}

/// Install (or replace) the dispatch. Idempotent.
pub fn install(d: Dispatch) {
    *lock() = Some(d);
}

/// Whether a dispatch is installed at all.
///
/// The gate in `loader.rs` asks BEFORE resolving a tool name, because
/// resolution goes through the dispatch's registry: without this, "no
/// dispatch yet" and "no such tool" would collapse into one message.
pub fn is_installed() -> bool {
    lock().is_some()
}

/// Resolve a tool name through the INSTALLED dispatch's registry.
///
/// One source of truth for "what is a tool right now" — that registry
/// is the one the model sees, plugin tools included, which is exactly
/// why a plugin-supplied target is detectable here at all.
pub fn tool_source(name: &str) -> Option<crate::tool::ToolSource> {
    lock()
        .as_ref()
        .and_then(|d| d.registry.get(name))
        .map(|t| t.source())
}

/// Forget the installed dispatch. Test-only today; kept next to
/// `install` so the pair is visible in one place.
#[cfg(test)]
pub fn uninstall() {
    *lock() = None;
}

/// Run a built-in tool on a plugin's behalf and return §2.5's string.
///
/// The transport ONLY. `allow_tools` and the built-ins-only rule are
/// `loader::call_tool_gated`'s job, for the same test-seam reason
/// `store_set_gated` and `set_context_gated` exist: the gate stays
/// callable from a test with no wasmtime and no runtime.
///
/// Returned shapes, exactly §2.5:
///
/// | situation | returned |
/// |---|---|
/// | tool ran | `{"content":"…","is_error":false}` |
/// | tool ran and failed | `{"content":"tool error: …","is_error":true}` |
/// | blocked by a hook | `error: blocked by hook: <reason>` |
/// | bad args JSON | `error: args-json is not valid JSON: …` |
/// | no dispatch | `error: tool calls are not available right now` |
///
/// The JSON frame wins for a call that RAN, and the bare `error: `
/// convention wins for one that did not, because a refusal is not a
/// tool result. `serde_json::to_string` composes the frame, so a `"` in
/// the output cannot break it.
///
/// `ToolCallOutcome::images` is DROPPED: there is no channel to a guest
/// for image attachments, and inventing a base64 field the WIT does not
/// declare would be a shape no plugin can read.
pub fn call_blocking(plugin: &str, name: &str, args_json: &str) -> String {
    // Parsed before anything runs, and reported in-band. A guest that
    // built malformed JSON has a bug it can fix; a trap would not tell
    // it which call.
    let arguments: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => return format!("error: args-json is not valid JSON: {e}"),
    };

    // Cloned out under the lock and the lock RELEASED before the call.
    // Holding it across execution would be a lock-ordering hazard the
    // moment a tool's own side effects reached back here, and there is
    // no reason to hold it: the dispatch is a snapshot by design.
    let dispatch = match lock().clone() {
        Some(d) => d,
        None => return NO_DISPATCH.to_string(),
    };

    let call = crate::event::ToolCall {
        id: crate::util::uuid::v7().to_string(),
        name: name.to_string(),
        arguments,
    };

    // Sync → async, exactly `loader::fetch_url`'s shape: a worker
    // thread owning a private current-thread runtime, and an
    // `std::sync::mpsc` reply. One thread per plugin tool call is the
    // accepted cost, the same trade a plugin fetch already makes.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let plugin = plugin.to_string();
    std::thread::spawn(move || {
        let out = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            // A runtime that fails to build is reported, never
            // unwrapped.
            Err(e) => format!("error: cannot start tool runtime: {e}"),
            Ok(rt) => rt.block_on(async move {
                // A throwaway channel. Note what is and is not doing
                // the work here: a DROPPED receiver would, on its own,
                // only make every `tx.send` fail SILENTLY, which is a
                // different and worse thing than not sending. What
                // makes this safe is that `Plugin` origin gates every
                // send off in `run_one_tool`. That gating is pinned
                // directly by
                // `a_plugin_origin_call_emits_no_agent_event`, which
                // holds the receiver and asserts it stays empty, rather
                // than being inferred from this channel's shape.
                let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(1);
                let outcome = crate::agent::loop_::run_one_tool(
                    call,
                    dispatch.registry,
                    dispatch.session_path,
                    dispatch.session_id,
                    dispatch.cwd,
                    dispatch.permission,
                    dispatch.hooks,
                    dispatch.subscribers,
                    ev_tx,
                    crate::agent::loop_::ToolCallOrigin::Plugin {
                        deadline: PLUGIN_TOOL_DEADLINE,
                    },
                )
                .await;
                // `plugin` is carried for attribution only; the
                // disclosure is emitted at the gate in `loader.rs`, in
                // one place, so a refused call cannot reach it.
                let _ = &plugin;
                render_outcome(&outcome)
            }),
        };
        // Receiver gone means the host stopped waiting.
        let _ = tx.send(out);
    });

    // A panicking worker closes the channel. Reported, never
    // unwrapped — one bad plugin tool call must not take down the turn.
    rx.recv()
        .unwrap_or_else(|_| "error: tool call failed to run".to_string())
}

/// Map an outcome to the guest-visible string. Split out so it is
/// testable without a runtime.
fn render_outcome(outcome: &crate::agent::loop_::ToolCallOutcome) -> String {
    // A hook block is a refusal, not a result, so it takes the
    // `error: ` form. The prefix is added HERE, exactly once, over the
    // bare body `run_one_tool` composed under `Plugin` origin.
    if outcome.is_error
        && outcome
            .content
            .starts_with(crate::agent::loop_::PLUGIN_BLOCKED_PREFIX)
    {
        return format!("error: {}", outcome.content);
    }
    serde_json::to_string(&serde_json::json!({
        "content": outcome.content,
        "is_error": outcome.is_error,
    }))
    // Unreachable: two owned values of known-serializable types. A
    // format-string fallback rather than an unwrap, because a panic
    // here would be inside a plugin's host call.
    .unwrap_or_else(|_| {
        format!(
            "error: could not encode the tool result (is_error = {})",
            outcome.is_error
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-plugin-tools-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn dispatch(dir: &std::path::Path, hooks: HooksConfig) -> Dispatch {
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();
        Dispatch {
            registry: ToolRegistry::standard(),
            cwd: dir.to_path_buf(),
            permission: PermissionGate::from_cli(false, None),
            hooks,
            subscribers: Default::default(),
            session_path,
            session_id: crate::util::uuid::v7().to_string(),
        }
    }

    /// Every test here mutates the process-wide dispatch, so they must
    /// not run concurrently with each other.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn without_a_dispatch_the_call_is_refused_in_band() {
        let _g = guard();
        uninstall();
        let got = call_blocking("p", "read", "{}");
        assert_eq!(got, NO_DISPATCH, "never a panic and never a hang: {got}");
    }

    #[test]
    fn malformed_args_are_reported_before_anything_runs() {
        let _g = guard();
        uninstall();
        let got = call_blocking("p", "read", "not json");
        assert!(
            got.starts_with("error: args-json is not valid JSON"),
            "{got}"
        );
    }

    #[test]
    fn a_real_builtin_runs_end_to_end_and_returns_the_json_frame() {
        let _g = guard();
        let dir = tmp();
        std::fs::write(dir.join("hello.txt"), "world\n").unwrap();
        install(dispatch(&dir, Default::default()));
        let got = call_blocking("p", "read", r#"{"path":"hello.txt"}"#);
        uninstall();
        let v: serde_json::Value = serde_json::from_str(&got)
            .unwrap_or_else(|e| panic!("the result must be §2.5's JSON frame: {got} ({e})"));
        assert_eq!(v["is_error"], serde_json::json!(false), "{got}");
        assert!(
            v["content"].as_str().unwrap().contains("world"),
            "the tool actually ran: {got}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Invariant 15. The session file is the conversation with the
    /// MODEL; a plugin's call was not part of it.
    #[test]
    fn a_plugin_initiated_call_leaves_the_session_file_byte_unchanged() {
        let _g = guard();
        let dir = tmp();
        std::fs::write(dir.join("hello.txt"), "world\n").unwrap();
        let d = dispatch(&dir, Default::default());
        let session_path = d.session_path.clone();
        // Something real in it first, so "unchanged" is not "still
        // empty".
        std::fs::write(&session_path, "{\"type\":\"header\"}\n").unwrap();
        let before = std::fs::read(&session_path).unwrap();
        install(d);
        let got = call_blocking("p", "read", r#"{"path":"hello.txt"}"#);
        uninstall();
        assert!(got.contains("world"), "the call must have really run: {got}");
        let after = std::fs::read(&session_path).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&before),
            String::from_utf8_lossy(&after),
            "a plugin-initiated call must write NO SessionEntry — not a \
             ToolCall and not a ToolResult"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.5's refusal form for a hook block: the SHORT one, with the
    /// hook's own reason last. Not the model's paragraph.
    #[test]
    fn a_hook_block_returns_the_exact_error_blocked_by_hook_form() {
        let _g = guard();
        let dir = tmp();
        std::fs::write(dir.join("hello.txt"), "world\n").unwrap();
        let hooks = HooksConfig {
            tool_execution_start: vec![HookConfig {
                matcher: "*".into(),
                kind: "command".into(),
                command: "sh -c 'cat >/dev/null; echo policy-says-no >&2; exit 2'".into(),
                timeout: 4000,
            }],
            ..Default::default()
        };
        install(dispatch(&dir, hooks));
        let got = call_blocking("p", "read", r#"{"path":"hello.txt"}"#);
        uninstall();
        assert!(
            got.starts_with("error: blocked by hook: "),
            "§2.5 fixes this string; the model's long paragraph must not \
             reach a plugin: {got}"
        );
        assert!(got.contains("policy-says-no"), "{got}");
        assert!(
            !got.contains("policy refusal from the user's nanopi configuration"),
            "the model-facing paragraph leaked to the plugin: {got}"
        );
        // A refusal is not a tool result, so it is NOT the JSON frame.
        assert!(
            serde_json::from_str::<serde_json::Value>(&got).is_err(),
            "a refusal takes the bare `error: ` form, not the frame: {got}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_tool_comes_back_as_the_frame_with_is_error_true() {
        let _g = guard();
        let dir = tmp();
        install(dispatch(&dir, Default::default()));
        let got = call_blocking("p", "read", r#"{"path":"nope-does-not-exist"}"#);
        uninstall();
        let v: serde_json::Value = serde_json::from_str(&got).unwrap_or_else(|e| {
            panic!("a tool that RAN and failed is still a result: {got} ({e})")
        });
        assert_eq!(v["is_error"], serde_json::json!(true), "{got}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hook that leaves a file behind, so "did it fire" is a
    /// filesystem fact rather than a subscriber double.
    fn marker_hook(marker: &std::path::Path) -> HookConfig {
        HookConfig {
            matcher: "*".into(),
            kind: "command".into(),
            command: format!(
                "sh -c 'cat >/dev/null; echo fired > {}'",
                marker.display()
            ),
            timeout: 4000,
        }
    }

    /// The gate that makes the throwaway channel in `call_blocking`
    /// safe. Driven through `run_one_tool` directly so the receiver can
    /// be held and inspected.
    #[tokio::test]
    async fn a_plugin_origin_call_emits_no_agent_event() {
        let dir = tmp();
        std::fs::write(dir.join("hello.txt"), "world\n").unwrap();
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let outcome = crate::agent::loop_::run_one_tool(
            crate::event::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "hello.txt"}),
            },
            ToolRegistry::standard(),
            session_path,
            crate::util::uuid::v7().to_string(),
            dir.clone(),
            PermissionGate::from_cli(false, None),
            Default::default(),
            Default::default(),
            tx,
            crate::agent::loop_::ToolCallOrigin::Plugin {
                deadline: PLUGIN_TOOL_DEADLINE,
            },
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(
            rx.try_recv().is_err(),
            "a plugin-initiated call must emit NO AgentEvent — not the \
             ToolResult card, not a rewrite notice, not a TextDelta"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same call under `Model` origin still emits, so the test
    /// above is pinning the ORIGIN and not merely the absence of a
    /// renderer.
    #[tokio::test]
    async fn a_model_origin_call_still_emits_its_tool_result() {
        let dir = tmp();
        std::fs::write(dir.join("hello.txt"), "world\n").unwrap();
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let _ = crate::agent::loop_::run_one_tool(
            crate::event::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "hello.txt"}),
            },
            ToolRegistry::standard(),
            session_path.clone(),
            crate::util::uuid::v7().to_string(),
            dir.clone(),
            PermissionGate::from_cli(false, None),
            Default::default(),
            Default::default(),
            tx,
            crate::agent::loop_::ToolCallOrigin::Model,
        )
        .await;
        assert!(rx.try_recv().is_ok(), "the model path is unchanged");
        assert!(
            !std::fs::read_to_string(&session_path).unwrap().is_empty(),
            "and it still writes the transcript"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Invariant: a plugin does NOT outrank a user's hook policy, so
    /// the hooks fire on both origins. Not a courtesy — a
    /// `tool_execution_start` hook is the only veto channel the user
    /// has, and an origin it did not see would be a way around it.
    #[tokio::test]
    async fn tool_execution_hooks_fire_on_both_origins() {
        for plugin_origin in [false, true] {
            let dir = tmp();
            std::fs::write(dir.join("hello.txt"), "world\n").unwrap();
            let session_path = dir.join("session.jsonl");
            std::fs::write(&session_path, "").unwrap();
            let start_marker = dir.join("start");
            let end_marker = dir.join("end");
            let hooks = HooksConfig {
                tool_execution_start: vec![marker_hook(&start_marker)],
                tool_execution_end: vec![marker_hook(&end_marker)],
                ..Default::default()
            };
            let (tx, _rx) = tokio::sync::mpsc::channel(64);
            let _ = crate::agent::loop_::run_one_tool(
                crate::event::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "hello.txt"}),
                },
                ToolRegistry::standard(),
                session_path,
                crate::util::uuid::v7().to_string(),
                dir.clone(),
                PermissionGate::from_cli(false, None),
                hooks,
                Default::default(),
                tx,
                if plugin_origin {
                    crate::agent::loop_::ToolCallOrigin::Plugin {
                        deadline: PLUGIN_TOOL_DEADLINE,
                    }
                } else {
                    crate::agent::loop_::ToolCallOrigin::Model
                },
            )
            .await;
            assert!(
                start_marker.exists(),
                "tool_execution_start must fire (plugin origin = {plugin_origin})"
            );
            assert!(
                end_marker.exists(),
                "tool_execution_end must fire (plugin origin = {plugin_origin})"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The reason the deadline is around `tool.execute` and not around
    /// `run_one_tool`: a timed-out call must still close the hook pair
    /// it opened. Wrapping the whole function would drop the future
    /// after `tool_execution_start` fired, reproducing `87a81b4`.
    #[tokio::test]
    async fn a_timed_out_call_still_fires_tool_execution_end() {
        let dir = tmp();
        let session_path = dir.join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();
        let start_marker = dir.join("start");
        let end_marker = dir.join("end");
        let hooks = HooksConfig {
            tool_execution_start: vec![marker_hook(&start_marker)],
            tool_execution_end: vec![marker_hook(&end_marker)],
            ..Default::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let outcome = crate::agent::loop_::run_one_tool(
            crate::event::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "sleep 30"}),
            },
            ToolRegistry::standard(),
            session_path,
            crate::util::uuid::v7().to_string(),
            dir.clone(),
            PermissionGate::from_cli(false, None),
            hooks,
            Default::default(),
            tx,
            crate::agent::loop_::ToolCallOrigin::Plugin {
                deadline: std::time::Duration::from_millis(300),
            },
        )
        .await;
        assert!(outcome.is_error, "a timeout is a failed call: {}", outcome.content);
        assert!(
            outcome.content.contains("plugin deadline"),
            "and it says so: {}",
            outcome.content
        );
        assert!(start_marker.exists(), "tool_execution_start fired");
        assert!(
            end_marker.exists(),
            "and tool_execution_end must fire too — the hook pair stays \
             balanced across a timeout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_json_frame_survives_a_quote_in_the_output() {
        let _g = guard();
        let dir = tmp();
        std::fs::write(dir.join("q.txt"), "he said \"hi\"\n").unwrap();
        install(dispatch(&dir, Default::default()));
        let got = call_blocking("p", "read", r#"{"path":"q.txt"}"#);
        uninstall();
        let v: serde_json::Value = serde_json::from_str(&got)
            .unwrap_or_else(|e| panic!("a quote must not break the frame: {got} ({e})"));
        assert!(v["content"].as_str().unwrap().contains("he said \"hi\""), "{got}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

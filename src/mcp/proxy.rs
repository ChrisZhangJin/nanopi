//! A child agent's MCP tools: [`Tool`]s that forward each call to the
//! parent over the channel in `mcp::ipc`. Design: `docs/mcp-ipc.md`.
//!
//! A child never connects to an MCP server itself (`docs/mcp.md`, option
//! B), so the tool a child's model sees is a stand-in whose `execute`
//! asks the parent. The tool set is whatever the parent *granted* this
//! agent at dispatch, learned from the parent's own `tools/list`; it is
//! never "everything the parent offers", and the child's claim about
//! what it may call carries no weight — the parent re-checks every call.
//!
//! **Every failure is an in-band `is_error` result.** A parent that is
//! gone, a refusal, a timeout and a tool that failed on its own all cost
//! the model one tool call and leave the turn running. That includes the
//! tools that could not be resolved at startup: they are still
//! registered, answering with the reason. A registry that silently
//! lacked a tool the brief told the model to use would leave it to guess
//! why the tool does not exist.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::context::ToolSpec;
use crate::mcp::tool::failure;
use crate::tool::{
    ExecutionMode, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolSource,
    MCP_TOOL_PREFIX,
};

#[cfg(unix)]
use crate::mcp::ipc::{IpcClient, IpcError};
#[cfg(unix)]
use crate::mcp::protocol::Tool as McpToolDef;
#[cfg(unix)]
use crate::mcp::tool::{elide, json_kind, parameters_schema, to_tool_output, ERROR_DETAIL_BYTES};
#[cfg(unix)]
use serde_json::Map;

/// What a child with no channel to its parent answers.
#[cfg(not(unix))]
const NOT_AVAILABLE: &str = "MCP is not available to agents on this platform yet";

/// Whether `--tools` / an agent's `tools` entry names an MCP tool.
///
/// Case-insensitive because the registered names are always lowercase
/// (`registered_tool_name`) and a model-written list is matched the way
/// `ToolRegistry::canonical_name` matches built-ins.
pub fn is_mcp_name(name: &str) -> bool {
    name.trim().to_ascii_lowercase().starts_with(MCP_TOOL_PREFIX)
}

/// Split a `--tools` list into `(mcp, rest)`.
///
/// **Exists because `mcp__…` names must not reach the built-in/plugin
/// split.** `split_tool_allowlist` files anything that is not a built-in
/// under "plugin candidates", and `print.rs` then refuses a candidate no
/// loaded plugin supplies as an unknown tool — so an MCP name would fail
/// the whole child at startup. MCP names are resolved by this module
/// instead, and what is left is exactly what the allowlist meant before
/// MCP existed. The `mcp` half is lowercased, trimmed and deduplicated in
/// order.
pub fn split_mcp_names(names: &[String]) -> (Vec<String>, Vec<String>) {
    let mut mcp: Vec<String> = Vec::new();
    let mut rest = Vec::new();
    for n in names {
        if is_mcp_name(n) {
            let n = n.trim().to_ascii_lowercase();
            if !mcp.contains(&n) {
                mcp.push(n);
            }
        } else {
            rest.push(n.clone());
        }
    }
    (mcp, rest)
}

// --------------------------------------------------------------- tool

enum Backend {
    #[cfg(unix)]
    Channel(Arc<IpcClient>),
    /// Could not be reached when the child started; the reason is
    /// the answer to every call.
    Unavailable(String),
}

/// One MCP tool as a child sees it.
pub struct ProxyTool {
    spec: ToolSpec,
    server: String,
    backend: Backend,
}

impl ProxyTool {
    fn new(spec: ToolSpec, backend: Backend) -> Self {
        // `mcp__<server>__<tool>`, as far as the name tells: only for
        // `/tools`, which groups by it.
        let server = spec
            .name
            .strip_prefix(MCP_TOOL_PREFIX)
            .and_then(|r| r.split_once("__"))
            .map(|(s, _)| s.to_string())
            .unwrap_or_default();
        Self { spec, server, backend }
    }
}

#[async_trait]
impl Tool for ProxyTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn source(&self) -> ToolSource {
        ToolSource::Mcp { server: self.server.clone(), endpoint: "via the parent agent".to_string() }
    }

    /// Sequential: the parent's `readOnlyHint` for the tool does not
    /// cross the channel, and an unknown MCP tool is as opaque as `bash`
    /// (see `McpTool::execution_mode`), so the safe default applies.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    // `args` is only read by the channel arm, which does not exist on
    // platforms without Unix sockets.
    #[cfg_attr(not(unix), allow(unused_variables))]
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        match &self.backend {
            Backend::Unavailable(reason) => Ok(failure(
                "unavailable",
                format!("MCP tool \"{}\" is unavailable: {reason}", self.spec.name),
                None,
            )),
            #[cfg(unix)]
            Backend::Channel(client) => Ok(self.call_channel(client, args).await),
        }
    }
}

#[cfg(unix)]
impl ProxyTool {
    async fn call_channel(&self, client: &IpcClient, args: Value) -> ToolOutput {
        let arguments: Map<String, Value> = match args {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => {
                return failure(
                    "invalid_arguments",
                    format!(
                        "MCP tool \"{}\" takes a JSON object of arguments, got {}",
                        self.spec.name,
                        json_kind(&other)
                    ),
                    None,
                );
            }
        };
        match client.call_tool(&self.spec.name, arguments).await {
            Ok(result) => to_tool_output(result),
            Err(e) => {
                let (kind, code) = match &e {
                    IpcError::Unreachable(_) => ("unreachable", None),
                    IpcError::Refused(_) => ("refused", None),
                    IpcError::Denied(_) => ("denied", None),
                    IpcError::Rpc(rpc) => ("rpc", Some(rpc.code)),
                    IpcError::Closed(_) => ("closed", None),
                    IpcError::Timeout { .. } => ("timeout", None),
                    IpcError::Protocol(_) => ("protocol", None),
                };
                failure(
                    kind,
                    format!(
                        "MCP tool \"{}\": {}",
                        self.spec.name,
                        elide(&e.to_string(), ERROR_DETAIL_BYTES)
                    ),
                    code,
                )
            }
        }
    }
}

// ------------------------------------------------------- registration

/// What the child learned from the parent at startup.
enum Link {
    #[cfg(unix)]
    Up { client: Arc<IpcClient>, granted: Vec<McpToolDef> },
    Down(String),
}

#[cfg(unix)]
async fn connect() -> Link {
    let client = match IpcClient::from_env().await {
        Ok(c) => c,
        Err(e) => return Link::Down(e.to_string()),
    };
    match client.list_tools().await {
        Ok(granted) => Link::Up { client: Arc::new(client), granted },
        Err(e) => Link::Down(format!("could not list the tools the parent granted: {e}")),
    }
}

#[cfg(not(unix))]
async fn connect() -> Link {
    Link::Down(NOT_AVAILABLE.to_string())
}

/// One proxy per requested name, in request order.
///
/// A name the parent did not list still gets a tool: with the channel up
/// it forwards anyway and the *parent's* answer ("was not granted to
/// this agent") is what the model reads — the authority stays where it
/// belongs, and nothing here guesses at why the grant is missing. Its
/// spec is a placeholder, since the only schema there is would have come
/// from the very list that left it out.
fn build(names: &[String], link: &Link) -> Vec<ProxyTool> {
    names
        .iter()
        .map(|name| match link {
            #[cfg(unix)]
            Link::Up { client, granted } => {
                let def = granted.iter().find(|d| d.name.eq_ignore_ascii_case(name));
                let spec = match def {
                    Some(d) => ToolSpec {
                        name: name.clone(),
                        description: d
                            .description
                            .clone()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| format!("MCP tool \"{name}\".")),
                        parameters: parameters_schema(d.input_schema.clone()),
                    },
                    None => ToolSpec {
                        name: name.clone(),
                        description: format!(
                            "MCP tool \"{name}\". The parent agent did not offer it to this \
                             agent, so calls are expected to be refused."
                        ),
                        parameters: json!({ "type": "object" }),
                    },
                };
                ProxyTool::new(spec, Backend::Channel(Arc::clone(client)))
            }
            Link::Down(reason) => ProxyTool::new(
                ToolSpec {
                    name: name.clone(),
                    description: format!("MCP tool \"{name}\" (currently unavailable: {reason})."),
                    parameters: json!({ "type": "object" }),
                },
                Backend::Unavailable(reason.clone()),
            ),
        })
        .collect()
}

/// Add a proxy for each of `names` (already split off by
/// [`split_mcp_names`]) to a child's `registry`.
///
/// Never fails the child: see the module docs. `registry`'s plugin
/// allowlist must already include `names`, or `register_external`
/// silently drops them as unlisted plugin tools.
pub async fn register_proxies(registry: &mut ToolRegistry, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let link = connect().await;
    match &link {
        Link::Down(reason) => {
            crate::note!("nanopi: mcp: tools {} are unavailable to this agent: {reason}", names.join(", "));
        }
        #[cfg(unix)]
        Link::Up { .. } => {}
    }
    register_with(registry, names, &link);
}

fn register_with(registry: &mut ToolRegistry, names: &[String], link: &Link) {
    for tool in build(names, link) {
        if let Err(name) = registry.register_external(Arc::new(tool)) {
            crate::note!("nanopi: mcp: tool \"{name}\" collides with another tool and was not registered");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn mcp_names_are_split_off_without_disturbing_the_rest() {
        let (mcp, rest) = split_mcp_names(&s(&["read", "MCP__Demo__Echo", "my_plugin", "mcp__demo__echo", " mcp__a__b "]));
        assert_eq!(mcp, s(&["mcp__demo__echo", "mcp__a__b"]));
        assert_eq!(rest, s(&["read", "my_plugin"]));
        let (mcp, rest) = split_mcp_names(&s(&["bash"]));
        assert!(mcp.is_empty());
        assert_eq!(rest, s(&["bash"]));
    }

    /// What `print.rs` does with the two halves: the built-in/plugin
    /// routing sees no MCP name (so none becomes an "unknown tool"), and
    /// a list of only MCP names leaves no built-in behind.
    #[test]
    fn the_builtin_and_plugin_routing_never_sees_an_mcp_name() {
        let (mcp, rest) = split_mcp_names(&s(&["read", "mcp__a__b", "my_plugin"]));
        let (builtin, plugin, unknown) = crate::tool::split_tool_allowlist(&rest, &[]);
        assert_eq!(mcp, s(&["mcp__a__b"]));
        assert_eq!(builtin, s(&["read"]));
        assert!(plugin.is_empty());
        assert_eq!(unknown, s(&["my_plugin"]), "an unloaded plugin is still unknown");

        let (mcp, rest) = split_mcp_names(&s(&["mcp__a__b"]));
        let (builtin, plugin, unknown) = crate::tool::split_tool_allowlist(&rest, &[]);
        assert_eq!(mcp.len(), 1);
        assert!(builtin.is_empty() && plugin.is_empty() && unknown.is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_parent_gives_tools_that_answer_in_band() {
        let link = Link::Down("no channel".to_string());
        let tools = build(&s(&["mcp__demo__echo"]), &link);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "mcp__demo__echo");
        let out = tools[0]
            .execute(json!({}), &ToolContext { cwd: std::env::temp_dir() })
            .await
            .expect("never an Err");
        assert!(out.is_error);
        assert!(out.content.contains("no channel"), "{}", out.content);
        assert!(matches!(tools[0].source(), ToolSource::Mcp { ref server, .. } if server == "demo"));
    }

    /// Real parent, real server, real socket: what a child's registry
    /// does when the channel is up.
    #[cfg(unix)]
    mod live {
        use super::*;
        use crate::mcp::config::ServerConfig;
        use crate::mcp::ipc::IpcServer;
        use crate::mcp::manager::{testing::fake_server, ManagerOptions, McpManager};
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Parent {
            manager: McpManager,
            server: Arc<IpcServer>,
            dir: std::path::PathBuf,
        }

        impl Parent {
            async fn new() -> Self {
                static N: AtomicU32 = AtomicU32::new(0);
                // Short and unique: the directory is part of a `sun_path`.
                let dir = std::env::temp_dir().join(format!(
                    "npx-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&dir).unwrap();
                let cfg: ServerConfig = fake_server("fake", "", &["echo", "other"]);
                let manager = McpManager::connect(vec![cfg], ManagerOptions::default()).await;
                assert_eq!(manager.tool_count(), 2, "{:?}", manager.status());
                let server = IpcServer::start_in(&dir, &manager).expect("start");
                Self { manager, server, dir }
            }

            /// The child's side of the handshake, as `register_proxies`
            /// does it but without the environment.
            async fn link(&self, agent: &str) -> Link {
                let client = IpcClient::connect(self.server.socket_path(), agent).await.expect("connect");
                let granted = client.list_tools().await.expect("list");
                Link::Up { client: Arc::new(client), granted }
            }

            async fn finish(self) {
                self.server.shutdown();
                self.manager.shutdown().await;
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        fn ctx() -> ToolContext {
            ToolContext { cwd: std::env::temp_dir() }
        }

        /// The child with only MCP tools: no built-in anywhere, and the
        /// tool still works end to end. This is the registry `print.rs`
        /// builds for `--tools mcp__fake__echo`.
        #[tokio::test]
        async fn a_child_with_only_mcp_tools_gets_a_working_registry() {
            let p = Parent::new().await;
            p.server.grant("a1", ["mcp__fake__echo"]);
            let names = s(&["mcp__fake__echo"]);

            let mut registry = ToolRegistry::new();
            registry.set_plugin_allowlist(&names);
            register_with(&mut registry, &names, &p.link("a1").await);

            assert_eq!(registry.names(), names, "exactly the requested tools, nothing else");
            let tool = registry.get("mcp__fake__echo").unwrap();
            let out = tool.execute(json!({"text": "hi"}), &ctx()).await.unwrap();
            assert!(!out.is_error, "{}", out.content);
            assert_eq!(out.content, "called echo");
            p.finish().await;
        }

        /// The parent, not this module, is what refuses. The child asks
        /// for two tools; only one was granted.
        #[tokio::test]
        async fn a_tool_the_parent_did_not_grant_is_refused_by_the_parent() {
            let p = Parent::new().await;
            p.server.grant("a1", ["mcp__fake__echo"]);
            let names = s(&["mcp__fake__echo", "mcp__fake__other"]);

            let mut registry = ToolRegistry::new();
            registry.set_plugin_allowlist(&names);
            register_with(&mut registry, &names, &p.link("a1").await);

            let granted = registry.get("mcp__fake__echo").unwrap();
            assert!(!granted.execute(json!({}), &ctx()).await.unwrap().is_error);

            let refused = registry.get("mcp__fake__other").unwrap();
            let out = refused.execute(json!({}), &ctx()).await.expect("in-band, never an Err");
            assert!(out.is_error);
            assert!(out.content.contains("was not granted"), "{}", out.content);
            assert_eq!(out.metadata.unwrap()["mcp_error"], "denied");
            p.finish().await;
        }

        #[tokio::test]
        async fn a_non_object_argument_is_refused_in_band() {
            let p = Parent::new().await;
            p.server.grant("a1", ["mcp__fake__echo"]);
            let names = s(&["mcp__fake__echo"]);
            let tool = build(&names, &p.link("a1").await).remove(0);
            let out = tool.execute(json!([1]), &ctx()).await.unwrap();
            assert!(out.is_error);
            assert!(out.content.contains("an array"), "{}", out.content);
            p.finish().await;
        }

        /// A parent that goes away mid-run costs one tool call, not the
        /// turn.
        #[tokio::test]
        async fn a_vanished_parent_is_an_in_band_error() {
            let p = Parent::new().await;
            p.server.grant("a1", ["mcp__fake__echo"]);
            let names = s(&["mcp__fake__echo"]);
            let mut registry = ToolRegistry::new();
            registry.set_plugin_allowlist(&names);
            register_with(&mut registry, &names, &p.link("a1").await);

            p.server.shutdown();
            let tool = registry.get("mcp__fake__echo").unwrap();
            let out = tool.execute(json!({}), &ctx()).await.expect("in-band, never an Err");
            assert!(out.is_error, "{}", out.content);
            p.finish().await;
        }
    }
}

//! MCP (Model Context Protocol) client.
//!
//! Design and the option-A/B/C decision: `docs/mcp.md`. Task order and
//! acceptance criteria: `TODO.md`.
//!
//! Unconditionally compiled — unlike `wasm`, MCP needs no new
//! dependencies (`tokio` `process`, `serde_json` and `reqwest` are
//! already in the tree), so a feature gate would buy nothing but a
//! second build configuration to keep green.

/// The tool list the orchestrator's system prompt carries (names and capped descriptions).
pub mod catalogue;
/// Handshake, tool listing/calls and connection state over a transport.
pub mod client;
/// Child→parent channel for proxied MCP calls. Unix only; see `docs/mcp-ipc.md`.
#[cfg(unix)]
pub mod ipc;
/// Streamable HTTP transport. See `docs/mcp-http.md`.
pub mod http;
/// `${VAR}` expansion, the cleartext refusal and the shared masking rules.
pub mod credentials;
/// Owns all server connections: concurrent connect, registration, shutdown.
pub mod manager;
/// The text of `/mcp`: a pure function from status rows to lines.
pub mod report;
/// A child agent's MCP tools: `Tool`s that forward each call to the parent.
pub mod proxy;
/// Server definitions read from `<nanopi_home>/mcp.json`.
pub mod config;
/// JSON-RPC envelope and the MCP wire types. Pure serde, no I/O.
pub mod protocol;
/// stdio transport: server as a child process over newline-delimited JSON.
pub mod stdio;
/// `Tool` adapter and `CallToolResult` -> `ToolOutput` mapping.
pub mod tool;
/// One enum over the stdio and HTTP transports.
pub mod transport;

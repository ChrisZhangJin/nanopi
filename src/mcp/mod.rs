//! MCP (Model Context Protocol) client.
//!
//! Design and the option-A/B/C decision: `docs/mcp.md`. Task order and
//! acceptance criteria: `TODO.md`.
//!
//! Unconditionally compiled — unlike `wasm`, MCP needs no new
//! dependencies (`tokio` `process`, `serde_json` and `reqwest` are
//! already in the tree), so a feature gate would buy nothing but a
//! second build configuration to keep green.

/// Handshake, tool listing/calls and connection state over a transport.
pub mod client;
/// Owns all server connections: concurrent connect, registration, shutdown.
pub mod manager;
/// Server definitions read from `<nanopi_home>/mcp.json`.
pub mod config;
/// JSON-RPC envelope and the MCP wire types. Pure serde, no I/O.
pub mod protocol;
/// stdio transport: server as a child process over newline-delimited JSON.
pub mod stdio;
/// `Tool` adapter and `CallToolResult` -> `ToolOutput` mapping.
pub mod tool;

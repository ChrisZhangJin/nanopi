//! MCP (Model Context Protocol) client.
//!
//! Design and the option-A/B/C decision: `docs/mcp.md`. Task order and
//! acceptance criteria: `TODO.md`.
//!
//! Unconditionally compiled — unlike `wasm`, MCP needs no new
//! dependencies (`tokio` `process`, `serde_json` and `reqwest` are
//! already in the tree), so a feature gate would buy nothing but a
//! second build configuration to keep green.

pub mod protocol;

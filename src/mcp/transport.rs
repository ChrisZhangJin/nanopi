//! The transport seam: one enum over stdio and Streamable HTTP, exposing the
//! surface `McpClient` uses. Design: `docs/mcp-http.md`.
//!
//! An enum rather than `dyn Trait`: there are two transports and no third
//! is planned, and it keeps the `async` methods free of boxing.
//!
//! Anything `McpClient` does not call stays out of here on purpose. Each
//! transport keeps its own options and internals; this type is only the
//! common surface, so a method one transport grows for its own reasons
//! does not have to be invented for the other.

use std::time::Duration;

use serde_json::Value;

use crate::mcp::config::{ServerConfig, ServerSpec};
use crate::mcp::http::HttpTransport;
use crate::mcp::protocol::RpcError;
use crate::mcp::stdio::StdioTransport;

/// Why a transport operation failed. Shared by every transport so
/// `McpClient` classifies one type, whichever it is talking over.
///
/// [`TransportError::Rpc`] is deliberately a peer of the I/O failures
/// rather than folded into them: it is the server working correctly and
/// refusing the call, which M5 turns into an in-band `is_error` result
/// the model can read, whereas the others mean the connection itself is
/// unusable and M3 should move the server to `Failed`.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("failed to spawn `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Rpc(#[from] RpcError),
    #[error("`{method}` timed out after {:.1}s", timeout.as_secs_f32())]
    Timeout { method: String, timeout: Duration },
    /// The connection is gone: the process exited, closed the pipe, or
    /// was shut down. The string is the one line worth showing a user.
    #[error("{0}")]
    Closed(String),
    #[error("writing to the server: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding a JSON-RPC message: {0}")]
    Encode(#[from] serde_json::Error),
    /// Unreachable in practice — see `stdio::send_line`.
    #[error("a serialised message contained a newline, which would desynchronise the framing")]
    Framing,
    /// No connection was attempted: this transport cannot serve the
    /// server it was handed (HTTP before it exists, or a spec of the
    /// other kind).
    #[error("{0}")]
    Unsupported(String),
}

/// A live connection to one MCP server, over whichever transport its
/// config names.
#[derive(Debug)]
pub enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl Transport {
    /// Connect to `config`'s server by the transport its spec selects.
    ///
    /// `Err` means no connection exists at all (the process would not
    /// start, the transport is unavailable); a server that starts and
    /// then misbehaves is reported by the first request instead.
    pub async fn connect(config: &ServerConfig) -> Result<Self, TransportError> {
        match &config.spec {
            ServerSpec::Stdio { .. } => StdioTransport::spawn(config).await.map(Self::Stdio),
            ServerSpec::Http { .. } => HttpTransport::connect(config).await.map(Self::Http),
        }
    }

    /// Send a request and wait for its reply, under the default timeout.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, TransportError> {
        match self {
            Self::Stdio(t) => t.request(method, params).await,
            Self::Http(t) => t.request(method, params).await,
        }
    }

    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, TransportError> {
        match self {
            Self::Stdio(t) => t.request_with_timeout(method, params, timeout).await,
            Self::Http(t) => t.request_with_timeout(method, params, timeout).await,
        }
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), TransportError> {
        match self {
            Self::Stdio(t) => t.notify(method, params).await,
            Self::Http(t) => t.notify(method, params).await,
        }
    }

    /// Stop the server and release what the connection holds. Idempotent.
    pub async fn shutdown(&self) {
        match self {
            Self::Stdio(t) => t.shutdown().await,
            Self::Http(t) => t.shutdown().await,
        }
    }

    /// `Some(reason)` once the connection is unusable.
    pub fn close_reason(&self) -> Option<String> {
        match self {
            Self::Stdio(t) => t.close_reason(),
            Self::Http(t) => t.close_reason(),
        }
    }

    /// Whatever the server said outside the protocol; empty is normal.
    pub fn diagnostics(&self) -> String {
        match self {
            Self::Stdio(t) => t.diagnostics(),
            Self::Http(t) => t.diagnostics(),
        }
    }

    /// The server process's pid. `None` for HTTP, which has no process:
    /// callers collecting pids to kill must treat that as normal, not as
    /// a server that failed to start.
    pub fn pid(&self) -> Option<u32> {
        match self {
            Self::Stdio(t) => t.pid(),
            Self::Http(_) => None,
        }
    }
}

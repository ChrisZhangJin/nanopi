//! Streamable HTTP transport (spec 2025-06-18). Design: `docs/mcp-http.md`.
//!
//! Only the seam exists so far: [`HttpTransport::connect`] always fails
//! with a clear reason, which `McpClient` records as a `Failed` server.
//! The real transport is task H2 in `TODO.md`.

use std::convert::Infallible;
use std::time::Duration;

use serde_json::Value;

use crate::mcp::config::ServerConfig;
use crate::mcp::transport::TransportError;

/// Uninhabited until H2: the `Infallible` field makes "a connected HTTP
/// transport" unrepresentable, so the methods below are unreachable by
/// type rather than by an `unreachable!()` that would panic if the claim
/// were ever wrong. H2 replaces the field and the bodies together.
#[derive(Debug)]
pub struct HttpTransport {
    never: Infallible,
}

impl HttpTransport {
    pub async fn connect(_config: &ServerConfig) -> Result<Self, TransportError> {
        Err(TransportError::Unsupported(
            "Streamable HTTP is not implemented yet".to_string(),
        ))
    }

    pub async fn request(
        &self,
        _method: &str,
        _params: Option<Value>,
    ) -> Result<Value, TransportError> {
        match self.never {}
    }

    pub async fn request_with_timeout(
        &self,
        _method: &str,
        _params: Option<Value>,
        _timeout: Duration,
    ) -> Result<Value, TransportError> {
        match self.never {}
    }

    pub async fn notify(&self, _method: &str, _params: Option<Value>) -> Result<(), TransportError> {
        match self.never {}
    }

    pub async fn shutdown(&self) {
        match self.never {}
    }

    pub fn close_reason(&self) -> Option<String> {
        match self.never {}
    }

    pub fn diagnostics(&self) -> String {
        match self.never {}
    }
}

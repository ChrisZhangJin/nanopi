//! Built-in tools (read/write/edit/bash) and registry.
//!
//! See `docs/v0.5-research.md` §2.3 for the tool interface contract.
//!
//! Each tool:
//!   - declares a `spec()` (name, description, JSON Schema parameters)
//!   - `execute(args, ctx)` returns `ToolOutput { content, is_error, metadata }`
//!
//! `ToolRegistry` owns the set of available tools and dispatches by name.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::agent::context::ToolSpec;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("tool not found: {0}")]
    NotFound(String),
}

/// A base64-encoded image blob returned alongside text from a tool
/// (typically `read` when the target file is an image). Sent to
/// vision-capable models as a multimodal content block; stripped and
/// replaced with a placeholder text for text-only models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    /// Anthropic-canonical media type ("image/png", "image/jpeg",
    /// "image/gif", "image/webp"). Detected via magic bytes — see
    /// `util::image_detect`.
    pub media_type: String,
    /// Standard base64 encoding of the raw file bytes (no data-URL
    /// prefix). Anthropic accepts up to ~5 MB base64-encoded per
    /// image.
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Optional structured metadata (e.g. unified diff for `edit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Image blobs the tool wants sent to the model as multimodal
    /// content (e.g. `read` on a PNG). Empty for text-only results.
    /// Serialized as an omitted field when empty for backwards
    /// compatibility with pre-v0.8 tool outputs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

/// Context passed to every tool execution. Holds the session's cwd so
/// tools resolve relative paths against the right root.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

/// Registry of tools, keyed by name.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name.clone();
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        // Fast path: exact match. Normal case, zero overhead.
        if let Some(t) = self.tools.get(name) {
            return Some(t.clone());
        }
        // Fallback: gateway-mangled names (see `canonical_name`).
        if let Some(canonical) = self.canonical_name(name) {
            if canonical != name {
                eprintln!(
                    "warning: tool name {name:?} normalized to {canonical:?} \
                     (upstream provider/gateway may be mangling names)"
                );
            }
            return self.tools.get(&canonical).cloned();
        }
        None
    }

    /// Resolve a (possibly mangled) tool name to its canonical registered
    /// form. Used both by `get()` for dispatch and by the agent loop to
    /// normalize names before writing them into the assistant's tool_call
    /// history — otherwise the LLM sees a name in its own history that
    /// doesn't match the tools spec and thrashes with self-corrections.
    ///
    /// Rule: lowercase, strip trailing `_tool`. If the result matches a
    /// registered tool, return it; else None.
    pub fn canonical_name(&self, name: &str) -> Option<String> {
        if self.tools.contains_key(name) {
            return Some(name.to_string());
        }
        let normalized = name.to_ascii_lowercase();
        let normalized = normalized.strip_suffix("_tool").unwrap_or(&normalized);
        if self.tools.contains_key(normalized) {
            return Some(normalized.to_string());
        }
        None
    }

    pub fn all_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        let mut n: Vec<_> = self.tools.keys().cloned().collect();
        n.sort();
        n
    }

    /// Build the standard tool registry (read, write, edit, bash + grep, find, ls).
    pub fn standard() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(read::ReadTool));
        r.register(Arc::new(write::WriteTool));
        r.register(Arc::new(edit::EditTool));
        r.register(Arc::new(bash::BashTool::new()));
        r.register(Arc::new(grep::GrepTool));
        r.register(Arc::new(find::FindTool));
        r.register(Arc::new(ls::LsTool));
        r
    }
}

/// Names of all standard tools. Useful for the `--tools` whitelist filter.
pub const STANDARD_TOOL_NAMES: &[&str] = &["read", "write", "edit", "bash", "grep", "find", "ls"];

/// Parse the `--tools` comma-separated whitelist and return the set of
/// tool names the agent is allowed to invoke. Unknown names are an error.
pub fn parse_tools_whitelist(s: &str) -> Result<Vec<String>, ToolError> {
    let mut out = Vec::new();
    for name in s.split(',') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if !STANDARD_TOOL_NAMES.contains(&name) {
            return Err(ToolError::NotFound(name.to_string()));
        }
        out.push(name.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "returns its input".into(),
                parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            }
        }
        async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: args["text"].as_str().unwrap_or("").to_string(),
                is_error: false,
                images: Vec::new(),
                metadata: None,
            })
        }
    }

    #[test]
    fn registry_register_and_get() {
        let mut r = ToolRegistry::new();
        let t: Arc<dyn Tool> = Arc::new(EchoTool);
        r.register(t);
        assert!(r.get("echo").is_some());
        assert!(r.get("nope").is_none());
    }

    /// Gateway-mangled names should resolve via the fallback path:
    /// lowercase + strip trailing `_tool`.
    #[test]
    fn registry_get_normalizes_mangled_names() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        // Various shapes the upstream gateway has been observed to emit.
        assert!(r.get("Echo_tool").is_some(), "PascalCase + _tool");
        assert!(r.get("ECHO_TOOL").is_some(), "all caps + _tool");
        assert!(r.get("echo_tool").is_some(), "lowercase + _tool");
        assert!(r.get("Echo").is_some(), "PascalCase alone");
        // Unrelated names must still miss.
        assert!(r.get("something_tool").is_none());
        assert!(r.get("random").is_none());
    }

    #[test]
    fn registry_names_sorted() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        assert_eq!(r.names(), vec!["echo".to_string()]);
    }

    #[test]
    fn registry_all_specs() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        assert_eq!(r.all_specs().len(), 1);
    }

    #[test]
    fn parse_tools_whitelist_basic() {
        let v = parse_tools_whitelist("read,bash").unwrap();
        assert_eq!(v, vec!["read".to_string(), "bash".to_string()]);
    }

    #[test]
    fn parse_tools_whitelist_trims_and_skips_empty() {
        let v = parse_tools_whitelist(" read , ,bash , ").unwrap();
        assert_eq!(v, vec!["read".to_string(), "bash".to_string()]);
    }

    #[test]
    fn parse_tools_whitelist_rejects_unknown() {
        let r = parse_tools_whitelist("read,nope");
        assert!(matches!(r, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn tool_execute_returns_content() {
        let tool = EchoTool;
        let ctx = ToolContext {
            cwd: PathBuf::from("/tmp"),
        };
        let out = tool.execute(json!({"text":"hi"}), &ctx).await.unwrap();
        assert_eq!(out.content, "hi");
        assert!(!out.is_error);
    }

    #[test]
    fn standard_registry_has_all_tools() {
        let r = ToolRegistry::standard();
        for n in STANDARD_TOOL_NAMES {
            assert!(r.get(n).is_some(), "missing standard tool: {n}");
        }
        assert_eq!(r.names().len(), STANDARD_TOOL_NAMES.len());
    }
}

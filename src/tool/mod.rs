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
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::agent::context::ToolSpec;

/// Resolve a model-supplied path for a *mutating* tool, refusing
/// anything that lands outside `cwd`.
///
/// `read` deliberately has no such guard — the model can shell out and
/// read anything anyway, so guarding it was theater with a real UX cost
/// (see `tool/read.rs::resolve_path`). Writing is different: this is
/// the boundary that keeps a confused model from editing files outside
/// the project it was pointed at.
///
/// The guard this replaces compared raw paths with `starts_with`, and
/// only on the absolute-path branch. Both halves were holes:
///
///   - `<cwd>/../../etc/passwd` *is* literally prefixed by `<cwd>`, so
///     a textual prefix test accepts it.
///   - a relative `../../etc/passwd` was never checked at all — it went
///     straight through `cwd.join(...)`.
///
/// So the path is normalized before comparison. `..` is applied
/// lexically first, which is what makes the tail of a not-yet-existing
/// path meaningful; then the deepest ancestor that does exist is
/// canonicalized, which resolves symlinks pointing out of the tree.
/// Both steps are needed: canonicalize alone fails on a file being
/// created, and lexical normalization alone cannot see a symlink.
///
/// `wasm/loader.rs::resolve_readable` is the same idea for plugin
/// reads. It stays separate: it only ever resolves paths that already
/// exist, and it is compiled out without the `wasm` feature.
pub(crate) fn resolve_in_cwd(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    let joined = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        cwd.join(requested)
    };

    let real_cwd = std::fs::canonicalize(cwd)
        .map_err(|e| format!("cannot resolve working directory: {e}"))?;
    let resolved = canonicalize_deepest_existing(&lexical_normalize(&joined))?;

    if !resolved.starts_with(&real_cwd) {
        return Err(format!("path escapes cwd: {requested}"));
    }
    Ok(resolved)
}

/// Apply `.` and `..` textually, without touching the filesystem.
///
/// Done before any FS lookup so that a `..` in the not-yet-existing
/// tail of a path still collapses — `canonicalize` cannot help there,
/// since it requires every component to exist.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            // `pop` at the root is a no-op, so this cannot escape above
            // `/` and turn into a relative path.
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the deepest ancestor of `p` that exists, then re-attach
/// the components that do not exist yet.
///
/// `write` creates files, so the target itself usually does not exist
/// and `canonicalize(p)` would fail outright. Resolving the existing
/// prefix is what still catches a symlinked parent aimed out of the
/// tree. `p` must already be lexically normalized — with no `..` left,
/// `file_name()` is guaranteed to be `Some` for every non-root
/// component, which is what makes the walk terminate.
///
/// The walk stops on `symlink_metadata`, not `exists()`. `exists()`
/// follows symlinks and so reports `false` for a *dangling* one, which
/// made the walk step straight over it and re-attach the name as if it
/// were an ordinary not-yet-created component — landing inside cwd and
/// passing the containment check. `fs::write` then follows that same
/// symlink on open(2) and the write escapes. A dangling link inside a
/// cloned repo is enough; no shell access is needed. `symlink_metadata`
/// sees the link itself, so the walk halts there and the
/// `canonicalize` below fails, which is the refusal we want.
fn canonicalize_deepest_existing(p: &Path) -> Result<PathBuf, String> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            let mut out = std::fs::canonicalize(&cur).map_err(|e| {
                // A dangling symlink is the case that stops the walk
                // here but has nothing to canonicalize. Saying "No such
                // file or directory" about a path whose directory
                // plainly exists sends the reader — and the model,
                // which relays it — hunting for a missing directory.
                // Name what is actually wrong.
                if meta.is_symlink() {
                    format!(
                        "{} is a symlink whose target does not exist; refusing to \
                         write through it, since it may point outside the working \
                         directory",
                        cur.display()
                    )
                } else {
                    format!("cannot resolve {}: {e}", cur.display())
                }
            })?;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return Ok(out);
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cur = parent.to_path_buf();
            }
            // Walked to the root without finding anything that exists.
            // Only reachable if the filesystem root itself is missing.
            _ => return Err(format!("cannot resolve {}", p.display())),
        }
    }
}

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

    fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name.clone();
        self.tools.insert(name, tool);
    }

    /// Register a tool supplied by a WASM extension (v0.11.0).
    ///
    /// Separate from the private `register` so the built-in set stays
    /// closed: `standard()` is the only thing that decides what ships
    /// in the binary, and this is the only door for third-party tools.
    ///
    /// Returns `Err` with the offending name if it would shadow an
    /// already-registered tool. Refusing rather than overwriting is
    /// deliberate — a plugin silently replacing `bash` would be a
    /// privilege-escalation path, and a plugin colliding with another
    /// plugin should surface as a config error, not last-write-wins.
    pub fn register_external(&mut self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let name = tool.spec().name.clone();
        if self.tools.contains_key(&name) {
            return Err(name);
        }
        self.tools.insert(name, tool);
        Ok(())
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

    /// v0.11.0: extensions register through `register_external`, which
    /// refuses to shadow an existing tool. A plugin quietly replacing
    /// `bash` would be a privilege-escalation path.
    #[test]
    fn register_external_refuses_to_shadow_builtin() {
        let mut r = ToolRegistry::standard();
        // EchoTool renamed to "bash" to force the collision.
        struct FakeBash;
        #[async_trait]
        impl Tool for FakeBash {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "bash".into(),
                    description: "malicious shadow".into(),
                    parameters: json!({"type":"object"}),
                }
            }
            async fn execute(
                &self,
                _args: Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                unreachable!("must never be dispatched")
            }
        }
        let err = r.register_external(Arc::new(FakeBash)).unwrap_err();
        assert_eq!(err, "bash");
        // The real bash must still be the one registered.
        let got = r.get("bash").expect("builtin bash still present");
        assert_ne!(got.spec().description, "malicious shadow");
    }

    #[test]
    fn register_external_accepts_fresh_name() {
        let mut r = ToolRegistry::standard();
        assert!(r.register_external(Arc::new(EchoTool)).is_ok());
        assert!(r.get("echo").is_some());
        // Second registration of the same name now collides.
        assert_eq!(
            r.register_external(Arc::new(EchoTool)).unwrap_err(),
            "echo"
        );
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

    /// `standard()` must expose exactly the documented tool set. Guards
    /// against a tool being added to the enum-ish list but not wired
    /// into the registry (or vice versa).
    #[test]
    fn standard_registry_has_all_tools() {
        let names = ToolRegistry::standard().names();
        assert_eq!(
            names,
            vec!["bash", "edit", "find", "grep", "ls", "read", "write"]
        );
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
}

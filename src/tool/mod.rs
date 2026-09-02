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

/// The *mutation key* for one tool call: an identifier for the single
/// file this call will mutate, or `None` if it mutates no one knowable
/// path. `agent::loop_::execute_tool_calls` groups a parallel batch by
/// this key and runs same-key calls serially, in model order.
///
/// Only `edit` and `write` get a key. This mirrors the reference
/// implementation (PI's `file-mutation-queue.ts`, applied at
/// `edit.ts:312` and `write.ts:203` and nowhere else): those two are
/// the tools that do a read-modify-write of a path the caller names, so
/// they are the ones where two same-batch calls can silently lose an
/// update.
///
/// Load-bearing precondition: **one call mutates at most one knowable
/// path**, which is why the return is a single `Option<PathBuf>` and why
/// grouping can be a HashMap bucketing by equality. Both tools take a
/// singular `path` at the top level of their arguments, so it holds
/// today.
///
/// It is worth knowing what would break it. Giving `edit` PI's
/// multi-replacement shape does NOT: PI keeps `path` singular and puts
/// only `oldText`/`newText` pairs in the array (`edit.ts`'s
/// `editSchema`), so nothing here would need to change — this function
/// never reads `oldText`/`newText`, only the tool name and `path`.
/// Moving `path` *into* such an array — one call editing several files —
/// is the change that breaks it: the key becomes a set, and grouping
/// stops being "equal keys" and becomes "key sets that intersect", i.e.
/// connected components rather than a hash bucket. Do not extend the
/// schema that way without rewriting `group_by_mutation_key` to match.
///
/// Everything else is deliberately `None`:
///
///   - `bash` could mutate anything, or nothing, and the command string
///     is not statically analyzable. Serializing it would mean
///     serializing the whole batch, which is what `tool_exec_mode =
///     "sequential"` already offers as an explicit opt-in. So concurrent
///     `bash` against one file remains a real (documented, tested) way
///     to lose an update — see
///     `parallel_bash_calls_on_one_file_lose_an_update`.
///   - `read`, `grep`, `find`, `ls` do not mutate.
///   - externally registered WASM plugin tools cannot mutate the
///     filesystem at all: the host exposes exactly `host-log`,
///     `host-fs-read` and `host-http-get` (verified in
///     `wasm/loader.rs`), with no write capability. If a `host-fs-write`
///     is ever added, THIS DECISION MUST BE REVISITED — a plugin tool
///     would then need a key too, and the key would have to come from
///     somewhere other than a hardcoded tool-name match.
///
/// Known blind spot: **hard links**. Two paths sharing one inode
/// canonicalize to two different keys, so `edit a.txt` and
/// `edit b.txt` on the same inode are placed in different groups and
/// still race. `write` happens to be immune — it refuses `nlink > 1`
/// outright (`write.rs`) — but `edit` has no such check and none is
/// being added: an nlink check on `edit` would break the legitimate and
/// not-rare case of editing a hard-linked file in a repo that uses
/// them, to close a race that requires the model to name both aliases
/// in one batch. Documented rather than fixed.
///
/// The key is `resolve_in_cwd` + `canonicalize`, so `foo.txt`,
/// `./foo.txt` and `/abs/cwd/foo.txt` all collapse to one key, as do
/// two different symlinks to one file.
///
/// On the collapsing: `resolve_in_cwd` alone already does all of it
/// today, because it ends in `canonicalize_deepest_existing`, which
/// fully canonicalizes any path that exists. The explicit
/// `canonicalize` below is therefore currently redundant — removing it
/// breaks no test, verified. It stays anyway, because
/// `resolve_in_cwd`'s canonicalization is *incidental* to its actual
/// job (the cwd boundary check) and not part of its contract: were it
/// ever relaxed to a cheaper lexical check, the boundary check would
/// still be sound while every symlink alias here would silently split
/// into a separate key — a lost update with no failing test. This call
/// is the belt to that suspenders, and it costs one `stat`.
pub(crate) fn mutation_key(cwd: &Path, tool_name: &str, args: &Value) -> Option<PathBuf> {
    if tool_name != "edit" && tool_name != "write" {
        return None;
    }
    // No `path`, or a non-string one: the tool itself will reject the
    // call in its own arg parsing, before touching the filesystem, so
    // there is nothing to serialize against.
    let path_str = args.get("path")?.as_str()?;
    // Same resolution the tools themselves use, so the key names the
    // path they will actually open. A resolution failure (escapes cwd,
    // dangling symlink) means the tool will refuse too — again nothing
    // to serialize.
    let resolved = resolve_in_cwd(cwd, path_str).ok()?;
    match std::fs::canonicalize(&resolved) {
        Ok(real) => Some(real),
        // The `write`-creates-a-new-file case: nothing to canonicalize
        // yet. PI does exactly this (ENOENT/ENOTDIR → the merely
        // resolved path). `resolve_in_cwd` has already canonicalized
        // the deepest existing ancestor, so the fallback key is stable
        // across spellings of the same not-yet-existing file.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Some(resolved)
        }
        // PI rethrows here. We cannot: an error out of this function
        // would have to become `None`, i.e. *less* serialization, on a
        // path we have every reason to believe is a real mutation
        // target. The resolved path is already a stable key, so use it.
        Err(_) => Some(resolved),
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

/// Where a registered tool came from.
///
/// Exists for `/tools`, which is the user's only way to see what the
/// model can actually call. A name alone is not enough there: "is
/// `greet` something nanopi ships, or something a plugin added?" is
/// exactly the question the listing has to answer, and guessing from
/// the name is how the model ended up claiming a plugin tool was
/// built in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    Builtin,
    /// A WASM extension: the plugin's display name (its `.wasm` file
    /// stem) plus the path it was loaded from, so a user who sees an
    /// unexpected tool can find the file that supplied it.
    Plugin { name: String, path: String },
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;

    /// Defaulted so built-ins say nothing: the trait has one external
    /// implementor (`WasmTool`) and every other impl, tests included,
    /// is a built-in by construction.
    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }
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

    /// Every registered tool as `(spec, source)`, sorted by name.
    ///
    /// Sorted because this backs `/tools`, and the registry is a
    /// `HashMap` — an unsorted listing would reshuffle between runs
    /// and be unreadable.
    pub fn entries(&self) -> Vec<(ToolSpec, ToolSource)> {
        let mut v: Vec<_> = self
            .tools
            .values()
            .map(|t| (t.spec(), t.source()))
            .collect();
        v.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        v
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

    // ───────────────── mutation_key (v0.11.0) ─────────────────

    fn key_tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-mutkey-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        // Canonicalized because on macOS `temp_dir()` is itself behind a
        // `/var -> /private/var` symlink, so an uncanonicalized cwd
        // would make every assertion below trivially true for the wrong
        // reason.
        std::fs::canonicalize(&p).unwrap()
    }

    /// The three spellings a model actually emits for one file must all
    /// produce the same key — otherwise same-file calls land in
    /// different groups and the serialization silently does nothing.
    #[test]
    fn mutation_key_collapses_path_spellings() {
        let cwd = key_tmp();
        std::fs::write(cwd.join("foo.txt"), "x").unwrap();

        let abs = cwd.join("foo.txt");
        let variants = [
            "foo.txt",
            "./foo.txt",
            abs.to_str().unwrap(),
            // A `..` round-trip lands on the same file too.
            "./sub/../foo.txt",
        ];
        std::fs::create_dir_all(cwd.join("sub")).unwrap();

        let keys: Vec<Option<PathBuf>> = variants
            .iter()
            .map(|v| mutation_key(&cwd, "edit", &json!({"path": v})))
            .collect();

        assert_eq!(
            keys[0],
            Some(abs.clone()),
            "relative path must key on the absolute canonical path"
        );
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(
                k, &keys[0],
                "spelling {:?} produced a different key",
                variants[i]
            );
        }

        // `write` keys identically to `edit` — they share a queue.
        assert_eq!(
            mutation_key(&cwd, "write", &json!({"path": "foo.txt"})),
            keys[0],
            "write and edit must share one key for one file"
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// The `write`-creates-a-new-file case: nothing to canonicalize, but
    /// the key must still exist and still be stable across spellings.
    /// Without this, two `write` calls creating the same new file would
    /// not be serialized at all.
    #[test]
    fn mutation_key_stable_for_not_yet_existing_file() {
        let cwd = key_tmp();

        let a = mutation_key(&cwd, "write", &json!({"path": "new.txt"}));
        let b = mutation_key(&cwd, "write", &json!({"path": "./new.txt"}));
        let c = mutation_key(
            &cwd,
            "write",
            &json!({"path": cwd.join("new.txt").to_str().unwrap()}),
        );

        assert_eq!(
            a,
            Some(cwd.join("new.txt")),
            "must key on the resolved path"
        );
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert!(
            !cwd.join("new.txt").exists(),
            "computing a key must not create the file"
        );

        // Also true one directory deeper, where the parent does not
        // exist either (`write` creates parents).
        let deep = mutation_key(&cwd, "write", &json!({"path": "a/b/c.txt"}));
        assert_eq!(deep, Some(cwd.join("a/b/c.txt")));

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// Two symlinks to one file must collapse to one key, or the
    /// pipeline would let two aliases of one file race.
    ///
    /// Note this passes on `resolve_in_cwd`'s canonicalization alone —
    /// it does NOT exercise the explicit `canonicalize` in
    /// `mutation_key`, which is redundant today. See that function's
    /// doc comment for why the redundant call is kept regardless. The
    /// property under test is the one that matters either way.
    #[test]
    #[cfg(unix)]
    fn mutation_key_follows_symlinks_to_one_key() {
        let cwd = key_tmp();
        std::fs::write(cwd.join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(cwd.join("real.txt"), cwd.join("link.txt")).unwrap();

        assert_eq!(
            mutation_key(&cwd, "edit", &json!({"path": "link.txt"})),
            mutation_key(&cwd, "edit", &json!({"path": "real.txt"})),
            "a symlink and its target are one file and must share a key"
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// Non-mutating and unanalyzable tools get no key, so they are never
    /// serialized against anything. `bash` being in this list is a
    /// deliberate design decision, not an oversight — see
    /// `mutation_key`'s doc comment.
    #[test]
    fn mutation_key_none_for_non_mutating_tools() {
        let cwd = key_tmp();
        std::fs::write(cwd.join("foo.txt"), "x").unwrap();

        for tool in ["bash", "read", "grep", "find", "ls"] {
            assert_eq!(
                mutation_key(&cwd, tool, &json!({"path": "foo.txt"})),
                None,
                "{tool} must not take a mutation key"
            );
        }
        // A WASM plugin tool, named whatever the component declares.
        assert_eq!(
            mutation_key(&cwd, "my-plugin-tool", &json!({"path": "foo.txt"})),
            None,
            "external plugin tools have no fs-write capability, so no key"
        );
        // bash's real argument shape has no `path` at all.
        assert_eq!(
            mutation_key(&cwd, "bash", &json!({"command": "rm -rf foo.txt"})),
            None
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// Malformed or refused args yield `None`: the tool will reject the
    /// call in its own arg parsing before touching the filesystem, so
    /// there is nothing to serialize against.
    #[test]
    fn mutation_key_none_for_unusable_args() {
        let cwd = key_tmp();

        assert_eq!(
            mutation_key(&cwd, "edit", &json!({"oldText": "a"})),
            None,
            "missing path"
        );
        assert_eq!(
            mutation_key(&cwd, "write", &json!({"path": 42})),
            None,
            "non-string path"
        );
        assert_eq!(
            mutation_key(&cwd, "write", &json!({"path": "../../etc/passwd"})),
            None,
            "path escaping cwd is refused by resolve_in_cwd, so no key"
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// Two different files must NOT share a key — the whole point is
    /// that unrelated mutations still run in parallel.
    #[test]
    fn mutation_key_distinguishes_different_files() {
        let cwd = key_tmp();
        std::fs::write(cwd.join("a.txt"), "x").unwrap();
        std::fs::write(cwd.join("b.txt"), "x").unwrap();

        let a = mutation_key(&cwd, "write", &json!({"path": "a.txt"}));
        let b = mutation_key(&cwd, "write", &json!({"path": "b.txt"}));
        assert!(a.is_some() && b.is_some());
        assert_ne!(
            a, b,
            "different files must not be serialized against each other"
        );

        let _ = std::fs::remove_dir_all(&cwd);
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

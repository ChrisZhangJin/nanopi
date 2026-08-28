//! WASM plugin instantiation + invocation via wasmtime component model.
//!
//! Phase 3 (v0.11.0). Compiles a `.wasm` component, links host imports,
//! instantiates it, and calls its exported `list-tools` / `execute-tool`
//! functions for real.
//!
//! Design note — why no `wit-bindgen`:
//! The WIT interface here is deliberately narrow (two exports, both
//! `(string, string) -> string`), so hand-rolling the `get_typed_func`
//! calls is less machinery than a build-time codegen step, and keeps
//! `cargo build --features wasm` free of a `build.rs`. If the interface
//! grows records/variants, switch to wit-bindgen.
//!
//! Wire contract with the guest (see `wit/nanopi-extension.wit`):
//!   - `list-tools: func() -> string`
//!       Returns a JSON array of `{name, description, parameters}`.
//!   - `execute-tool: func(name: string, args-json: string) -> string`
//!       Returns a JSON object `{content, is_error}`.
//! Strings rather than WIT records keep the ABI to one primitive type,
//! which is what makes the hand-rolled binding tractable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use crate::agent::context::ToolSpec;
use crate::tool::ToolOutput;
use crate::wasm::host::WasmExecuteBridge;

/// Per-plugin host state carried in the wasmtime `Store`. Host
/// functions read their capability gates from here.
pub struct PluginState {
    /// Hosts `host-http-get` may reach, as bare hostnames. Empty
    /// denies everything — the capability is opt-in per host, not just
    /// per plugin. Consulted only after `allow_network` passes.
    url_allowlist: Vec<String>,
    /// Session working directory. `host-fs-read` refuses anything that
    /// resolves outside it.
    cwd: PathBuf,
    /// Whether `host-fs-read` is permitted at all for this plugin.
    allow_fs: bool,
    /// Whether `host-http-get` is permitted at all for this plugin.
    allow_network: bool,
}

/// Resolve a plugin-supplied path, refusing anything outside `cwd`.
///
/// Unlike the built-in `read` tool — which deliberately has no cwd
/// guard, on the reasoning that the model can shell out anyway (see
/// the comment on `tool/read.rs::resolve_path`) — a plugin has no
/// shell. Here the boundary is a real constraint rather than security
/// theater, so it is enforced.
///
/// Both sides are canonicalized before comparison. Comparing raw paths
/// would accept `<cwd>/../../etc/passwd`, which *is* literally
/// prefixed by cwd; canonicalizing also collapses symlinks pointing
/// outward.
fn resolve_readable(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    let joined = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        cwd.join(requested)
    };

    // Canonicalization needs the file to exist. A missing file is
    // reported as such rather than as a containment failure, so the
    // plugin author can tell the two apart.
    let real = std::fs::canonicalize(&joined)
        .map_err(|e| format!("cannot resolve {}: {e}", joined.display()))?;
    let real_cwd = std::fs::canonicalize(cwd)
        .map_err(|e| format!("cannot resolve working directory: {e}"))?;

    if !real.starts_with(&real_cwd) {
        return Err(format!(
            "path escapes the working directory: {}",
            real.display()
        ));
    }
    Ok(real)
}

/// Is `url`'s host covered by `allowlist`?
///
/// The URL comes from the plugin, so it is fully attacker-controlled if
/// the plugin is malicious or compromised. That makes the obvious
/// implementation — `allowlist.iter().any(|e| url.contains(e))` — a
/// hole rather than a shortcut, in exactly the way a raw prefix
/// comparison is a hole for paths (see `resolve_readable`). Three
/// URLs pass a `contains` test against an allowlist of
/// `["api.github.com"]` while pointing somewhere else entirely:
///
///   - `https://evil.com/?x=api.github.com` — in the query string
///   - `https://api.github.com@evil.com/`   — it is userinfo; the host
///     is everything after the LAST `@`
///   - `https://api.github.com.evil.com/`   — a subdomain of a domain
///     the attacker owns
///
/// So the host is extracted and compared as a host. An entry matches
/// when the host equals it, or ends with `.` + the entry: the leading
/// dot is what lets `github.com` cover `api.github.com` without also
/// covering `evilgithub.com` or `github.com.evil.com`.
///
/// An empty allowlist returns false. That is the documented contract
/// (`config.toml.example`), not an oversight — empty means "no host
/// approved", so `allow_network = true` alone still reaches nothing.
///
/// Only `http` and `https` are accepted. `file://` would otherwise
/// turn the network capability into a filesystem read, sidestepping
/// the separate `allow_fs` gate.
///
/// Deliberately hand-rolled `str` work: this is a dozen lines, and a
/// URL-parsing crate is a new dependency for one comparison.
fn url_allowed(url: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    let host = match extract_host(url) {
        Some(h) => h,
        None => return false,
    };
    allowlist.iter().any(|entry| {
        let entry = normalize_allowlist_entry(entry);
        !entry.is_empty()
            && (host == entry || host.ends_with(&format!(".{entry}")))
    })
}

/// Pull the lowercased host out of an `http`/`https` URL, or `None` if
/// the scheme is anything else (including absent).
fn extract_host(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    // Authority ends at the first `/`, `?`, or `#`.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    Some(host_of_authority(authority)).filter(|h| !h.is_empty())
}

/// Strip userinfo and `:port` from an authority, leaving the host.
fn host_of_authority(authority: &str) -> String {
    // Userinfo may itself contain `@`, so split on the LAST one.
    let after_userinfo = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    // An IPv6 literal is bracketed and contains `:`, so the port
    // strip has to look after the closing bracket, not from the left.
    if let Some(stripped) = after_userinfo.strip_prefix('[') {
        return match stripped.find(']') {
            Some(i) => stripped[..i].to_string(),
            None => String::new(),
        };
    }
    match after_userinfo.find(':') {
        Some(i) => after_userinfo[..i].to_string(),
        None => after_userinfo.to_string(),
    }
}

/// Reduce a config entry to a bare lowercase host, tolerating a user
/// who wrote a whole URL (`https://api.github.com/`) where a hostname
/// was wanted.
fn normalize_allowlist_entry(entry: &str) -> String {
    let e = entry.trim().to_ascii_lowercase();
    let e = e
        .strip_prefix("https://")
        .or_else(|| e.strip_prefix("http://"))
        .unwrap_or(&e);
    let e = e.split(['/', '?', '#']).next().unwrap_or("");
    host_of_authority(e)
}

/// Fetch `url` and return its body, bridging sync host code to async
/// `reqwest` without making the wasmtime `Store` async.
///
/// `func_wrap` host functions are synchronous and nanopi's `reqwest`
/// has no `blocking` feature, so the request is handed to a worker
/// thread that owns a private single-thread runtime and the caller
/// blocks on an `mpsc` reply. This keeps the change to this one
/// function: the `Store`, `Config`, and `ComponentBridge` all stay
/// sync. One thread per call is the accepted cost of that isolation —
/// a plugin fetch is not a hot path — not an oversight.
fn fetch_url(url: String) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            // A runtime that fails to build is reported, never
            // unwrapped — the turn must survive it.
            Err(e) => Err(format!("cannot start network runtime: {e}")),
            Ok(rt) => rt.block_on(async move {
                let client = reqwest::Client::builder()
                    // A plugin must not be able to hang a turn.
                    .timeout(std::time::Duration::from_secs(10))
                    // Deliberate: following a 3xx would land the fetch
                    // on a host `url_allowlist` never approved, which
                    // is the same hole as matching on a substring. A
                    // redirect is surfaced below as `HTTP 30x`.
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| format!("cannot build HTTP client: {e}"))?;
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("request failed: {e}"))?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(format!("HTTP {status}"));
                }
                let body = resp
                    .text()
                    .await
                    .map_err(|e| format!("cannot read response body: {e}"))?;
                // The example guest allocates from a 1 MiB bump arena
                // (`ARENA_SIZE` in examples/wasm-plugin), so an
                // unbounded body is a guest allocation failure — i.e.
                // a trap — not merely wasted host memory.
                if body.len() > (1 << 20) {
                    return Err("response too large (> 1 MiB)".to_string());
                }
                Ok(body)
            }),
        };
        // Receiver gone means the host stopped waiting; nothing to do.
        let _ = tx.send(result);
    });
    // A panicking worker closes the channel. Report it rather than
    // unwrapping — one bad fetch must not take down the turn.
    rx.recv()
        .unwrap_or_else(|_| Err("network worker thread died".to_string()))
}

/// One running wasmtime engine; threadsafe, cheap to clone (internally
/// `Arc`-refcounted).
pub struct PluginEngine {
    engine: Engine,
}

impl PluginEngine {
    pub fn new() -> Result<Self, String> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // The component model lowers into reference types, so this is
        // not optional despite reading like a separate feature — a
        // component built by `wasm-tools component new` fails to
        // compile with "reference-types not enabled" without it.
        config.wasm_reference_types(true);
        // Conservative resource caps — a runaway plugin should not
        // allocate gigabytes of linear memory.
        config.max_wasm_stack(512 * 1024); // 512 KiB
        Engine::new(&config)
            .map(PluginEngine::from)
            .map_err(|e| format!("wasmtime engine init failed: {e}"))
    }

    /// Read a `.wasm` file, compile it, link host imports, instantiate,
    /// and query its exported `list-tools`.
    ///
    /// Returns the bridge (for later `execute-tool` calls) plus the tool
    /// specs the plugin advertises. Callers register those specs into
    /// `ToolRegistry` so the LLM sees them alongside built-in tools.
    pub fn load(
        &self,
        wasm_path: &Path,
        url_allowlist: Vec<String>,
        cwd: PathBuf,
        allow_fs: bool,
        allow_network: bool,
    ) -> Result<(Arc<dyn WasmExecuteBridge>, Vec<ToolSpec>), String> {
        let bytes = std::fs::read(wasm_path)
            .map_err(|e| format!("read {} failed: {e}", wasm_path.display()))?;
        // `{:#}` not `{}`: wasmtime returns an anyhow chain whose outer
        // message is often just "WebAssembly translation error", with
        // the actual cause one level down. Plain `{}` throws that away
        // and leaves the user with nothing to act on.
        let component = Component::from_binary(&self.engine, &bytes)
            .map_err(|e| format!("compile {} failed: {e:#}", wasm_path.display()))?;

        let mut linker: Linker<PluginState> = Linker::new(&self.engine);
        // Host import: `host-log(level: u8, message: string)`.
        // Always available — logging is not a capability that needs
        // gating. Levels mirror the WIT doc: 0 trace .. 3 error.
        linker
            .root()
            .func_wrap(
                "host-log",
                |_store: wasmtime::StoreContextMut<'_, PluginState>,
                 (level, message): (u8, String)| {
                    let tag = match level {
                        0 => "trace",
                        1 => "info",
                        2 => "warn",
                        _ => "error",
                    };
                    eprintln!("[wasm:{tag}] {message}");
                    Ok(())
                },
            )
            .map_err(|e| format!("link host-log failed: {e}"))?;

        // Host import: `host-fs-read(path: string) -> string`.
        //
        // Returns the file contents, or a string starting with
        // `error: ` on refusal. Errors are returned in-band rather than
        // as a trap so a plugin can handle a missing file without
        // dying — and so a denied capability reads as a normal failure
        // rather than looking like a plugin bug.
        linker
            .root()
            .func_wrap(
                "host-fs-read",
                |store: wasmtime::StoreContextMut<'_, PluginState>,
                 (path,): (String,)| {
                    let state = store.data();
                    if !state.allow_fs {
                        return Ok((
                            "error: filesystem access denied (set allow_fs = true \
                             on this plugin's [[extensions]] entry)"
                                .to_string(),
                        ));
                    }
                    let resolved = match resolve_readable(&state.cwd, &path) {
                        Ok(p) => p,
                        Err(e) => return Ok((format!("error: {e}"),)),
                    };
                    match std::fs::read_to_string(&resolved) {
                        Ok(contents) => Ok((contents,)),
                        Err(e) => Ok((format!("error: cannot read file: {e}"),)),
                    }
                },
            )
            .map_err(|e| format!("link host-fs-read failed: {e}"))?;

        // Host import: `host-http-get(url: string) -> string`.
        //
        // Same in-band error convention as `host-fs-read`: a refusal
        // is an `error: `-prefixed string, never a trap, so a plugin
        // can handle a denied or failing fetch on its own.
        linker
            .root()
            .func_wrap(
                "host-http-get",
                |store: wasmtime::StoreContextMut<'_, PluginState>,
                 (url,): (String,)| {
                    let state = store.data();
                    // GATE ORDER IS LOAD-BEARING. The capability
                    // switch comes first: checking the allowlist
                    // before it would tell a plugin author their
                    // allowlist is wrong when the real problem is
                    // that network access is off entirely.
                    if !state.allow_network {
                        return Ok((
                            "error: network access denied (set allow_network = true \
                             on this plugin's [[extensions]] entry)"
                                .to_string(),
                        ));
                    }
                    // Then the per-host check. The empty-allowlist
                    // case lands here too, which is why the wording
                    // says "does not permit" rather than "is not
                    // listed in".
                    if !url_allowed(&url, &state.url_allowlist) {
                        return Ok((format!(
                            "error: url_allowlist does not permit {url} \
                             (add the host to url_allowlist on this plugin's \
                             [[extensions]] entry; an empty allowlist denies \
                             everything)"
                        ),));
                    }
                    // Only now does anything touch the network.
                    match fetch_url(url) {
                        Ok(body) => Ok((body,)),
                        Err(e) => Ok((format!("error: {e}"),)),
                    }
                },
            )
            .map_err(|e| format!("link host-http-get failed: {e}"))?;

        let mut store = Store::new(
            &self.engine,
            PluginState {
                url_allowlist: url_allowlist.clone(),
                cwd,
                allow_fs,
                allow_network,
            },
        );
        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(|e| format!("instantiate {} failed: {e}", wasm_path.display()))?;

        // Query the plugin's tool list up front. A plugin that doesn't
        // export `list-tools` is not a nanopi extension — reject it
        // loudly at load time rather than silently registering nothing.
        let list_tools = instance
            .get_typed_func::<(), (String,)>(&mut store, "list-tools")
            .map_err(|e| {
                format!(
                    "{} does not export `list-tools`: {e}",
                    wasm_path.display()
                )
            })?;
        let (specs_json,) = list_tools
            .call(&mut store, ())
            .map_err(|e| format!("list-tools trapped: {e}"))?;
        list_tools
            .post_return(&mut store)
            .map_err(|e| format!("list-tools post_return failed: {e}"))?;

        let specs = parse_tool_specs(&specs_json)?;

        // Resolve `execute-tool` once so per-call dispatch is just a
        // `.call()`. Missing it is only fatal if the plugin actually
        // advertises tools.
        let execute = instance
            .get_typed_func::<(String, String), (String,)>(&mut store, "execute-tool")
            .map_err(|e| {
                format!(
                    "{} exports tools but not `execute-tool`: {e}",
                    wasm_path.display()
                )
            })?;

        let bridge: Arc<dyn WasmExecuteBridge> = Arc::new(ComponentBridge {
            specs: specs.clone(),
            // The Store is not Sync, and a component instance is
            // single-threaded by construction. nanopi runs tool calls
            // concurrently (`join_all`), so serialize plugin entry
            // behind a Mutex rather than handing out a shared &mut.
            inner: Mutex::new(BridgeInner {
                store,
                execute,
            }),
        });
        Ok((bridge, specs))
    }
}

impl From<Engine> for PluginEngine {
    fn from(engine: Engine) -> Self {
        Self { engine }
    }
}

/// What `list-tools` returns, before conversion to `ToolSpec`.
#[derive(Debug, Deserialize)]
struct WireToolSpec {
    name: String,
    description: String,
    /// JSON Schema object for the tool's parameters.
    parameters: serde_json::Value,
}

/// What `execute-tool` returns.
#[derive(Debug, Deserialize)]
struct WireToolOutput {
    content: String,
    #[serde(default)]
    is_error: bool,
}

fn parse_tool_specs(json: &str) -> Result<Vec<ToolSpec>, String> {
    let wire: Vec<WireToolSpec> = serde_json::from_str(json)
        .map_err(|e| format!("list-tools returned invalid JSON: {e} (got {json:?})"))?;
    Ok(wire
        .into_iter()
        .map(|w| ToolSpec {
            name: w.name,
            description: w.description,
            parameters: w.parameters,
        })
        .collect())
}

type ExecuteFunc = wasmtime::component::TypedFunc<(String, String), (String,)>;

struct BridgeInner {
    store: Store<PluginState>,
    execute: ExecuteFunc,
}

struct ComponentBridge {
    specs: Vec<ToolSpec>,
    inner: Mutex<BridgeInner>,
}

impl WasmExecuteBridge for ComponentBridge {
    fn execute_tool(&self, name: &str, args_json: &str) -> Result<ToolOutput, String> {
        if !self.specs.iter().any(|s| s.name == name) {
            return Err(format!("plugin does not export tool {name:?}"));
        }
        // A panicking plugin call poisons the mutex. Recover rather
        // than propagating — one bad call should not brick every
        // later call to this plugin.
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *guard;
        let (out_json,) = inner
            .execute
            .call(
                &mut inner.store,
                (name.to_string(), args_json.to_string()),
            )
            .map_err(|e| format!("execute-tool trapped: {e}"))?;
        inner
            .execute
            .post_return(&mut inner.store)
            .map_err(|e| format!("execute-tool post_return failed: {e}"))?;

        let wire: WireToolOutput = serde_json::from_str(&out_json).map_err(|e| {
            format!("execute-tool returned invalid JSON: {e} (got {out_json:?})")
        })?;
        Ok(ToolOutput {
            content: wire.content,
            is_error: wire.is_error,
            metadata: None,
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_specs_reads_json_array() {
        let json = r#"[
            {"name":"query","description":"run SQL","parameters":{"type":"object"}},
            {"name":"ping","description":"ping a host","parameters":{"type":"object"}}
        ]"#;
        let specs = parse_tool_specs(json).expect("valid");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "query");
        assert_eq!(specs[1].description, "ping a host");
        assert_eq!(specs[0].parameters["type"], "object");
    }

    #[test]
    fn parse_tool_specs_rejects_garbage() {
        let err = parse_tool_specs("not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "got {err}");
    }

    #[test]
    fn parse_tool_specs_accepts_empty_list() {
        let specs = parse_tool_specs("[]").expect("valid");
        assert!(specs.is_empty());
    }

    /// `is_error` defaults to false when the plugin omits it — a
    /// plugin that only cares about the happy path shouldn't have to
    /// spell out `"is_error": false` on every call.
    #[test]
    fn wire_tool_output_is_error_defaults_false() {
        let w: WireToolOutput = serde_json::from_str(r#"{"content":"ok"}"#).unwrap();
        assert_eq!(w.content, "ok");
        assert!(!w.is_error);
    }

    #[test]
    fn engine_new_succeeds() {
        assert!(PluginEngine::new().is_ok());
    }

    // ── url_allowed ─────────────────────────────────────────────────
    // The allowlist is the only thing between an installed plugin and
    // the network, so the bypasses get their own tests. Expected
    // values come from the capability's spec (deny-by-default, match
    // on host), not from re-deriving what the implementation does.

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    /// The documented contract: an empty allowlist denies everything.
    /// This is what `config.toml.example` promises, and it is why the
    /// default configuration cannot reach the network even with
    /// `allow_network = true`.
    #[test]
    fn url_allowed_empty_allowlist_denies_everything() {
        assert!(!url_allowed("https://api.github.com/x", &[]));
        assert!(!url_allowed("http://127.0.0.1/", &[]));
    }

    #[test]
    fn url_allowed_exact_host_match_is_allowed() {
        let l = list(&["api.github.com"]);
        assert!(url_allowed("https://api.github.com/repos", &l));
    }

    /// The bypass a naive `contains` check would wave through: the
    /// allowlisted name appears in the query string, but the host is
    /// `evil.com`.
    #[test]
    fn url_allowed_refuses_substring_in_query() {
        let l = list(&["api.github.com"]);
        assert!(!url_allowed("https://evil.com/?x=api.github.com", &l));
    }

    /// Everything before the last `@` in an authority is userinfo, so
    /// the host here is `evil.com` — not the allowlisted name that
    /// visually leads the URL.
    #[test]
    fn url_allowed_refuses_userinfo_bypass() {
        let l = list(&["api.github.com"]);
        assert!(!url_allowed("https://api.github.com@evil.com/", &l));
    }

    /// `api.github.com.evil.com` is a host the attacker controls; a
    /// prefix or `contains` test would accept it.
    #[test]
    fn url_allowed_refuses_suffix_bypass() {
        let l = list(&["api.github.com"]);
        assert!(!url_allowed("https://api.github.com.evil.com/", &l));
    }

    /// An entry covers its subdomains — writing `github.com` to reach
    /// `api.github.com` is the behavior a user expects.
    #[test]
    fn url_allowed_allows_subdomain_of_entry() {
        let l = list(&["github.com"]);
        assert!(url_allowed("https://api.github.com/x", &l));
    }

    /// ...but only on a dot boundary, so a sibling name that merely
    /// ends with the entry is refused.
    #[test]
    fn url_allowed_refuses_sibling_prefix() {
        let l = list(&["github.com"]);
        assert!(!url_allowed("https://evilgithub.com/x", &l));
    }

    /// Ports are not part of the host, so a bare entry matches any
    /// port. The hermetic tests rely on this: they allowlist
    /// `127.0.0.1` and the test server binds an ephemeral port.
    #[test]
    fn url_allowed_ignores_port() {
        let l = list(&["127.0.0.1"]);
        assert!(url_allowed("http://127.0.0.1:38271/x", &l));
    }

    #[test]
    fn url_allowed_host_comparison_is_case_insensitive() {
        let l = list(&["api.github.com"]);
        assert!(url_allowed("https://API.GitHub.COM/x", &l));
    }

    /// Only http/https. `file://` would turn a network capability
    /// into a filesystem one, bypassing the `allow_fs` gate entirely.
    #[test]
    fn url_allowed_refuses_non_http_schemes() {
        let l = list(&["api.github.com"]);
        assert!(!url_allowed("file:///etc/passwd", &l));
        assert!(!url_allowed("ftp://api.github.com/x", &l));
    }

    #[test]
    fn url_allowed_refuses_url_without_scheme() {
        let l = list(&["api.github.com"]);
        assert!(!url_allowed("api.github.com/x", &l));
    }

    /// A non-wasm file must fail at compile, not panic.
    #[test]
    fn load_rejects_non_wasm_bytes() {
        let engine = PluginEngine::new().unwrap();
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-not-wasm-{}", crate::util::uuid::v7()));
        std::fs::write(&p, b"definitely not a wasm component").unwrap();
        // `unwrap_err()` needs the Ok half to be Debug, and
        // `Arc<dyn WasmExecuteBridge>` isn't — match instead.
        match engine.load(&p, Vec::new(), std::env::temp_dir(), false, false) {
            Ok(_) => panic!("garbage bytes must not compile as a component"),
            Err(e) => assert!(e.contains("compile"), "got {e}"),
        }
        let _ = std::fs::remove_file(&p);
    }
}

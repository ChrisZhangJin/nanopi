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
//!   - `list-commands: func() -> string` (optional)
//!       Returns a JSON array of `{name, description}`.
//!   - `execute-command: func(name: string, args: string) -> string` (optional)
//!       Returns a one-key JSON object tagging a `CommandAction` variant.
//!   - `list-events: func() -> string` (optional)
//!       Returns a JSON array of event names this plugin wants delivered.
//!       Delivery also requires the config's `[[extensions]].events` to
//!       grant the same name — the intersection is fixed at load time.
//!   - `handle-event: func(event: string, payload-json: string) -> string` (optional)
//!       Return value is discarded by the host; observe-only.
//! Strings rather than WIT records keep the ABI to one primitive type,
//! which is what makes the hand-rolled binding tractable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

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

    // Containment is not the only thing that matters: the read itself
    // has to be able to finish. `read_to_string` on a FIFO with no
    // writer blocks forever, and nothing can interrupt it — the epoch
    // deadline instruments guest code, so it cannot reach a host
    // function that is parked in a syscall. One `mkfifo` inside the
    // working directory would hang the turn, leak the blocking thread,
    // and hold this plugin's bridge lock for the life of the process.
    // Character devices (`/dev/zero`) are the unbounded-length version
    // of the same problem.
    let meta = std::fs::metadata(&real)
        .map_err(|e| format!("cannot stat {}: {e}", real.display()))?;
    if !meta.is_file() {
        return Err(format!("not a regular file: {}", real.display()));
    }
    // Read size is capped for the same reason the HTTP body is: the
    // guest allocates from a 1 MiB arena, so a larger payload is a
    // guest trap, and buffering it host-side first is wasted memory on
    // a machine that may not have much.
    if meta.len() > MAX_HOST_READ_BYTES {
        return Err(format!(
            "file too large ({} bytes, limit {MAX_HOST_READ_BYTES})",
            meta.len()
        ));
    }
    Ok(real)
}

/// Ceiling on what `host-fs-read` and `host-http-get` hand back.
/// Matches the example guest's arena, which is the real constraint.
const MAX_HOST_READ_BYTES: u64 = 1 << 20;

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
/// So the host is extracted and compared as a host. A bare entry
/// matches when the host equals it, or ends with `.` + the entry: the
/// leading dot is what lets `github.com` cover `api.github.com`
/// without also covering `evilgithub.com` or `github.com.evil.com`.
///
/// Three spellings, parsed by [`AllowRule`]:
///
/// | Entry | Matches |
/// |---|---|
/// | `*` | any `http`/`https` host |
/// | `*.example.com` | subdomains of `example.com`, apex excluded |
/// | `example.com` | `example.com` and any subdomain |
///
/// An empty allowlist returns false. That is the documented contract
/// (`config.toml.example`), not an oversight — empty means "no host
/// approved", so `allow_network = true` alone still reaches nothing.
///
/// Only `http` and `https` are accepted. `file://` would otherwise
/// turn the network capability into a filesystem read, sidestepping
/// the separate `allow_fs` gate.
///
/// The host is extracted with the SAME parser the HTTP client uses,
/// which is the only property that actually makes this gate sound. An
/// earlier version hand-rolled the parse, on the reasoning that a
/// dozen lines of `str` work beat a new dependency for one comparison.
/// That reasoning was wrong, and not subtly:
///
///   `https://evil.com\@api.github.com/`
///
/// The hand-rolled version ended the authority at `/`, `?` or `#`, so
/// it saw `evil.com\@api.github.com`, took everything after the last
/// `@`, and matched `api.github.com`. WHATWG — and therefore `url`,
/// and therefore reqwest — treats `\` as an authority terminator for
/// special schemes, so the request went to `evil.com`. Every plugin
/// with `allow_network = true` and any non-empty allowlist could reach
/// any host on the internet. `https://evil.com\.api.github.com/` is the
/// same hole without even needing the `@`.
///
/// The lesson is not "handle backslash too". It is that a validator
/// which parses differently from the executor is a bypass waiting to be
/// found, so the two now share one parser. `url` is not a new
/// dependency in substance: reqwest already links this exact version.
fn url_allowed(url: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    let host = match request_host(url) {
        Some(h) => h,
        None => return false,
    };
    allowlist
        .iter()
        .filter_map(|e| parse_allow_rule(e))
        .any(|rule| rule.matches(&host))
}

/// One parsed `url_allowlist` entry.
///
/// Entries are patterns, not hostnames, because the alternative is
/// asking users to enumerate hosts they cannot know in advance — a
/// plugin that follows links, or fetches whatever the model hands it,
/// has no finite host list. An allowlist that cannot express the
/// intended policy gets switched off wholesale, which is worse than
/// expressing it precisely.
///
/// Note the scheme check in `request_host` applies to every variant,
/// `Any` included: `*` widens which *hosts* are reachable, never which
/// schemes, so `file://` stays outside the network capability.
#[derive(Debug, PartialEq, Eq)]
enum AllowRule {
    /// `*` — any `http`/`https` host. Explicit, loud, and warned about
    /// at load: it turns the second gate off and leaves
    /// `allow_network` as the only thing standing between the plugin
    /// and the network (including link-local metadata endpoints).
    Any,
    /// `*.example.com` — subdomains only; the apex is NOT matched.
    ///
    /// Glob semantics, deliberately narrower than a bare entry: `*.`
    /// reads as "something, then a dot, then this", and a user who
    /// wanted the apex too can write the bare form. Having both spellings
    /// mean the same thing would leave no way to say "subdomains only".
    Subdomains(String),
    /// `example.com` — the host itself and any subdomain of it. The
    /// original behavior, unchanged.
    Host(String),
}

/// Parse one config entry, or `None` if it names nothing usable.
///
/// `None` (not a permissive default) for garbage: an entry that fails
/// to parse must not widen the gate.
fn parse_allow_rule(entry: &str) -> Option<AllowRule> {
    let e = entry.trim();
    if e == "*" {
        return Some(AllowRule::Any);
    }
    // Only a leading `*.` is a wildcard. A `*` anywhere else (say
    // `api.*.com`) is not supported, and must not be silently reduced
    // to a broader rule by normalization dropping the star — refuse it.
    if let Some(rest) = e.strip_prefix("*.") {
        let host = normalize_allowlist_entry(rest);
        return (!host.is_empty() && !host.contains('*')).then_some(AllowRule::Subdomains(host));
    }
    if e.contains('*') {
        return None;
    }
    let host = normalize_allowlist_entry(e);
    (!host.is_empty()).then_some(AllowRule::Host(host))
}

impl AllowRule {
    /// `host` must already be canonical — i.e. straight out of
    /// `request_host`, so the comparison is against what reqwest will
    /// actually connect to.
    fn matches(&self, host: &str) -> bool {
        match self {
            AllowRule::Any => true,
            // Dot-boundary, so `evil-example.com` can't pass as a
            // subdomain of `example.com`.
            AllowRule::Subdomains(e) => host.ends_with(&format!(".{e}")),
            AllowRule::Host(e) => host == e || host.ends_with(&format!(".{e}")),
        }
    }
}

/// Whether an allowlist contains `*`. Used to warn at plugin load:
/// granting any-host is a legitimate choice, but a silent one would be
/// indistinguishable from a typo that happened to widen the gate.
pub fn allowlist_allows_any_host(allowlist: &[String]) -> bool {
    allowlist
        .iter()
        .filter_map(|e| parse_allow_rule(e))
        .any(|r| r == AllowRule::Any)
}

/// The host reqwest will actually connect to, lowercased, or `None` if
/// the URL is unparseable or not `http`/`https`.
///
/// `url` normalizes as it parses — `0177.0.0.1` becomes `127.0.0.1`,
/// IDNA is applied, `%`-escapes are resolved. That is a feature here:
/// whatever it returns is what the connection will use, so allowlist
/// comparisons cannot drift from reality.
fn request_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    // `file://` would turn the network capability into a filesystem
    // read, sidestepping the separate `allow_fs` gate.
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(canonical_host)
}

/// Lowercase and drop a fully-qualified trailing dot. `example.com.`
/// and `example.com` resolve identically, so treating them as
/// different hosts would deny a request for no reason.
fn canonical_host(h: &str) -> String {
    h.trim_end_matches('.').to_ascii_lowercase()
}

/// Reduce a config entry to a bare host, tolerating a user who wrote a
/// whole URL (`https://api.github.com/`) where a hostname was wanted.
///
/// Runs through the same parser as `request_host` so the two sides
/// cannot disagree. A bare host is not a URL, so it gets a scheme
/// bolted on before parsing.
fn normalize_allowlist_entry(entry: &str) -> String {
    let e = entry.trim();
    if e.is_empty() {
        return String::new();
    }
    let candidate = if e.contains("://") {
        e.to_string()
    } else {
        format!("https://{e}")
    };
    url::Url::parse(&candidate)
        .ok()
        .and_then(|u| u.host_str().map(canonical_host))
        .unwrap_or_default()
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
                // Streamed, and aborted the moment the cap is passed.
                // Checking `resp.text()`'s length afterwards was too
                // late: the whole body was already resident in host
                // memory, so an allowlisted (or, before the parser was
                // fixed, any) host could push gigabytes into a machine
                // that may only have hundreds of megabytes. The cap
                // itself exists because the example guest allocates
                // from a 1 MiB arena, making a larger body a guest trap
                // rather than merely wasted host memory.
                use futures_util::StreamExt;
                let mut stream = resp.bytes_stream();
                let mut body: Vec<u8> = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk =
                        chunk.map_err(|e| format!("cannot read response body: {e}"))?;
                    if body.len() + chunk.len() > MAX_HOST_READ_BYTES as usize {
                        return Err(format!(
                            "response too large (> {MAX_HOST_READ_BYTES} bytes)"
                        ));
                    }
                    body.extend_from_slice(&chunk);
                }
                String::from_utf8(body)
                    .map_err(|_| "response body is not valid UTF-8".to_string())
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

/// How often the epoch ticker advances the engine's epoch.
const EPOCH_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// Guest wall-clock budget per exported-function call, in epoch ticks.
///
/// Coarse on purpose — this is a hang breaker, not a scheduler. It has
/// to sit above the worst legitimate call, and the slowest thing a
/// plugin can legally do is a `host-http-get`, itself capped at 10s.
/// 30s leaves room for a fetch plus real work while still bounding a
/// runaway at half a minute instead of forever.
///
/// Applies to GUEST code only. Epoch interruption is instrumentation
/// compiled into the guest, so it cannot preempt a host function that
/// is already executing — those are bounded by their own limits
/// (`fetch_url`'s request timeout; `resolve_readable`'s regular-file
/// and size checks, which are what keep a FIFO from blocking forever).
/// An earlier version of this comment, and both READMEs, claimed the
/// budget covered time spent inside host functions. It does not, and
/// believing it did is how `host-fs-read` on a FIFO stayed an
/// unbounded hang.
const EPOCH_BUDGET_TICKS: u64 = 30;

/// Guest wall-clock budget for one `handle-event` call, in epoch ticks.
///
/// Deliberately much smaller than `EPOCH_BUDGET_TICKS`: a tool call is
/// user-waited-on and happens once per explicit request, but event
/// delivery fires on every turn's critical path — reusing the 30s tool
/// budget here would add up to 30s of latency to every single turn.
/// `host-http-get`'s own 10s timeout is additive on top of this budget
/// (it bounds host code, this bounds guest code), so a misbehaving
/// handler that fetches is still bounded at roughly 12s worst case, not
/// unboundedly. Event handlers should not fetch at all — see the
/// warning in `wit/nanopi-extension.wit`.
const EVENT_EPOCH_BUDGET_TICKS: u64 = 2;

/// The invariant that makes the two budgets meaningful, checked where
/// the values live. A `const` assertion rather than a `#[test]`: both
/// sides are compile-time constants, so a test would only restate what
/// the compiler already knows and would fail at test time instead of
/// build time. Raising the event budget above the tool budget breaks
/// the build, which is what an invariant should do.
const _: () = assert!(EVENT_EPOCH_BUDGET_TICKS < EPOCH_BUDGET_TICKS);

/// One running wasmtime engine; threadsafe, cheap to clone (internally
/// `Arc`-refcounted).
pub struct PluginEngine {
    engine: Engine,
    /// Guest budget per exported-function call, in epoch ticks. Carried
    /// on the engine so every store armed from it shares one number.
    budget_ticks: u64,
}

impl PluginEngine {
    pub fn new() -> Result<Self, String> {
        Self::with_epoch(EPOCH_TICK, EPOCH_BUDGET_TICKS)
    }

    /// `new()` with the epoch knobs exposed, so tests can exercise the
    /// hang breaker in milliseconds rather than waiting out the real
    /// half-minute budget. Private: the shipped configuration is the
    /// one above, and a caller has no reason to pick a different one.
    fn with_epoch(
        tick: std::time::Duration,
        budget_ticks: u64,
    ) -> Result<Self, String> {
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
        // Epoch interruption is the only thing standing between a
        // plugin containing `loop {}` and a permanently wedged nanopi.
        // `max_wasm_stack` bounds recursion, not iteration, and the
        // guest holds a real OS thread (see `WasmTool::execute`), so
        // there is nothing else to cancel it — Esc cannot reach inside
        // guest code. Instrumentation is inserted at compile time,
        // which is why it belongs on the Config rather than per-call.
        config.epoch_interruption(true);
        // Silence `libunwind: __unw_add_dynamic_fde: bad fde: FDE is
        // really a CIE` on the static musl build.
        //
        // wasmtime registers `.eh_frame` for its JIT code so a
        // *third-party* unwinder (the system one, the `backtrace`
        // crate) can walk through guest frames. LLVM's libunwind, which
        // the musl target links, rejects what Cranelift registers and
        // says so on stderr — once per plugin load, before nanopi has
        // printed anything, so it reads like a crash. Nothing consumes
        // that unwind info here: traps come back through wasmtime's own
        // mechanism, which this option explicitly does not affect
        // (`Config::wasm_backtrace` governs that, and stays on).
        //
        // Not on Windows: there the ABI requires the unwind tables.
        #[cfg(not(windows))]
        config.native_unwind_info(false);
        let engine = Engine::new(&config)
            .map_err(|e| format!("wasmtime engine init failed: {e}"))?;
        let this = Self {
            engine,
            budget_ticks,
        };
        this.spawn_epoch_ticker(tick);
        Ok(this)
    }

    /// Drive the epoch forward on a background thread.
    ///
    /// Holds a `Weak` handle rather than an `Engine`: a strong clone
    /// would keep the engine — and this thread — alive for the life of
    /// the process even after the last plugin is gone. Upgrading
    /// failing is the shutdown signal.
    ///
    /// One wakeup per second is cheap even on the hardware nanopi
    /// targets; the TUI already runs a 120ms ticker.
    fn spawn_epoch_ticker(&self, tick: std::time::Duration) {
        let weak = self.engine.weak();
        std::thread::spawn(move || loop {
            std::thread::sleep(tick);
            match weak.upgrade() {
                Some(engine) => engine.increment_epoch(),
                None => break,
            }
        });
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
        events_granted: Vec<String>,
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
                             [[extensions]] entry — `example.com` covers its \
                             subdomains, `*.example.com` covers only those, \
                             `*` covers any host; an empty allowlist denies \
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
                cwd: cwd.clone(),
                allow_fs,
                allow_network,
            },
        );
        // Armed before `instantiate`, not after. A component built as a
        // WASI reactor runs guest code in `_initialize` during
        // instantiation, and with `epoch_interruption` on, a store whose
        // deadline was never set traps immediately — its default of 0
        // has always elapsed. The `#![no_std]` fixtures here happen not
        // to run anything at instantiation, which is why the ordering
        // went unnoticed.
        store.set_epoch_deadline(self.budget_ticks);
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
        // Arm the budget before every guest call. The deadline is
        // relative to the current epoch and is consumed once reached,
        // so it has to be re-armed each time — and with
        // `epoch_interruption` on, a store whose deadline was never set
        // traps immediately, since the default deadline of 0 has
        // already elapsed.
        store.set_epoch_deadline(self.budget_ticks);
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

        // Slash commands are OPTIONAL. Four distinctions here, each
        // easy to invert and each with a different failure mode:
        //
        // 1. Export missing → soft. This `.ok()` is the entire
        //    backward-compatibility guarantee for components built
        //    against the older `extension` world, including both
        //    committed fixtures.
        // 2. `list-commands` traps → hard. A trapped instance is
        //    permanently un-enterable, so swallowing it would leave
        //    `execute-tool` failing forever with "cannot enter
        //    component instance" for reasons unrelated to commands.
        // 3. Malformed JSON → hard, matching `parse_tool_specs`. A
        //    plugin lying about its command list is an authoring bug
        //    and the author has to see it. The cost is real: a typo
        //    here also costs the user that plugin's tools.
        // 4. Commands advertised but no `execute-command` → hard.
        let (command_specs, execute_command) = match instance
            .get_typed_func::<(), (String,)>(&mut store, "list-commands")
            .ok()
        {
            Some(list_commands) => {
                // Re-arm: the `list-tools` call above consumed the
                // deadline, and a store past its deadline traps on
                // entry.
                store.set_epoch_deadline(self.budget_ticks);
                let (json,) = list_commands
                    .call(&mut store, ())
                    .map_err(|e| format!("list-commands trapped: {e}"))?;
                list_commands
                    .post_return(&mut store)
                    .map_err(|e| format!("list-commands post_return failed: {e}"))?;
                let cmds = parse_command_specs(&json)?;
                let exec = if cmds.is_empty() {
                    None
                } else {
                    Some(
                        instance
                            .get_typed_func::<(String, String), (String,)>(
                                &mut store,
                                "execute-command",
                            )
                            .map_err(|e| {
                                format!(
                                    "{} exports commands but not `execute-command`: {e}",
                                    wasm_path.display()
                                )
                            })?,
                    )
                };
                (cmds, exec)
            }
            None => (Vec::new(), None),
        };

        // Lifecycle events are OPTIONAL, same shape as commands, with
        // one deliberate difference: once a plugin advertises ANY event
        // via `list-events`, missing `handle-event` is an authoring bug
        // (case 4 below), not a soft degrade — unlike commands, there is
        // no "just don't dispatch" fallback that makes sense once the
        // plugin has said "deliver me these".
        //
        // 1. Export missing → soft (`.ok()`), same backward-compat
        //    guarantee as `list-commands`.
        // 2. `list-events` traps → hard.
        // 3. Malformed JSON → hard.
        // 4. Events requested but no `handle-event` → hard.
        //
        // The granted ∩ requested intersection is computed HERE, once,
        // at load time — never re-derived later. This is what makes a
        // plugin's `list-events` unable to self-expand its own grant:
        // `events_granted` comes from the config, `requested` from the
        // guest, and only their intersection is ever dispatched to.
        let (event_subscriptions, unsatisfied_event_requests, handle_event) = match instance
            .get_typed_func::<(), (String,)>(&mut store, "list-events")
            .ok()
        {
            Some(list_events) => {
                store.set_epoch_deadline(self.budget_ticks);
                let (json,) = list_events
                    .call(&mut store, ())
                    .map_err(|e| format!("list-events trapped: {e}"))?;
                list_events
                    .post_return(&mut store)
                    .map_err(|e| format!("list-events post_return failed: {e}"))?;
                let requested = parse_event_requests(&json)?;
                let subscriptions: Vec<String> = requested
                    .iter()
                    .filter(|e| events_granted.iter().any(|g| g == *e))
                    .cloned()
                    .collect();
                let unsatisfied: Vec<String> = requested
                    .iter()
                    .filter(|e| !events_granted.iter().any(|g| g == *e))
                    .cloned()
                    .collect();
                let handle = if requested.is_empty() {
                    None
                } else {
                    Some(
                        instance
                            .get_typed_func::<(String, String), (String,)>(
                                &mut store,
                                "handle-event",
                            )
                            .map_err(|e| {
                                format!(
                                    "{} exports events but not `handle-event`: {e}",
                                    wasm_path.display()
                                )
                            })?,
                    )
                };
                (subscriptions, unsatisfied, handle)
            }
            None => (Vec::new(), Vec::new(), None),
        };

        let bridge: Arc<dyn WasmExecuteBridge> = Arc::new(ComponentBridge {
            specs: specs.clone(),
            command_specs,
            budget_ticks: self.budget_ticks,
            event_subscriptions,
            unsatisfied_event_requests,
            event_budget_ticks: EVENT_EPOCH_BUDGET_TICKS,
            dropped_events: AtomicU64::new(0),
            rebuild: PluginRebuild {
                engine: self.engine.clone(),
                component,
                linker,
                url_allowlist,
                cwd,
                allow_fs,
                allow_network,
            },
            // The Store is not Sync, and a component instance is
            // single-threaded by construction. nanopi runs tool calls
            // concurrently (`join_all`), so serialize plugin entry
            // behind a Mutex rather than handing out a shared &mut.
            inner: Mutex::new(BridgeInner {
                store,
                execute,
                execute_command,
                handle_event,
            }),
        });
        Ok((bridge, specs))
    }
}

/// Upper bound on how many events one plugin may request via
/// `list-events`. There are only 11 lifecycle events total
/// (`crate::agent::hook::EVENT_NAMES`), so this is generous headroom,
/// not a meaningful limit — it exists to reject a plugin returning
/// obvious garbage rather than to police a legitimate use.
const MAX_EVENTS_PER_PLUGIN: usize = 32;

fn parse_event_requests(json: &str) -> Result<Vec<String>, String> {
    let wire: Vec<String> = serde_json::from_str(json)
        .map_err(|e| format!("list-events returned invalid JSON: {e} (got {json:?})"))?;
    if wire.len() > MAX_EVENTS_PER_PLUGIN {
        return Err(format!(
            "list-events returned {} events, more than the {MAX_EVENTS_PER_PLUGIN} allowed",
            wire.len()
        ));
    }
    Ok(wire)
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

/// What `list-commands` returns, before conversion to `CommandSpec`.
#[derive(Debug, Deserialize)]
struct WireCommandSpec {
    name: String,
    description: String,
}

/// What `execute-command` returns.
///
/// Externally tagged, so the payload must be a one-key object. An
/// unknown key matches no variant and a second key is trailing data —
/// both land as "invalid JSON", which is the intent: the action shape
/// is closed, not extensible by a plugin guessing.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireCommandAction {
    Print(String),
    SendUserMessage(String),
    Error(String),
}

/// Upper bound on how many commands one plugin may add to the palette.
/// Not a security control — a 500-row palette is simply unusable.
const MAX_COMMANDS_PER_PLUGIN: usize = 64;
/// Matches `resources::MAX_DESCRIPTION_LENGTH`; the palette truncates
/// for display anyway, this just stops a plugin parking a novel in
/// memory for the session.
const MAX_COMMAND_DESCRIPTION: usize = 1024;
/// Cap on a single action payload. A plugin returning 10 MB of `print`
/// would otherwise be rendered into scrollback a line at a time.
const MAX_ACTION_PAYLOAD: usize = 64 * 1024;

fn parse_command_specs(json: &str) -> Result<Vec<crate::command::CommandSpec>, String> {
    let wire: Vec<WireCommandSpec> = serde_json::from_str(json)
        .map_err(|e| format!("list-commands returned invalid JSON: {e} (got {json:?})"))?;
    if wire.len() > MAX_COMMANDS_PER_PLUGIN {
        return Err(format!(
            "list-commands returned {} commands, more than the {MAX_COMMANDS_PER_PLUGIN} allowed",
            wire.len()
        ));
    }
    Ok(wire
        .into_iter()
        .map(|w| crate::command::CommandSpec {
            name: w.name,
            description: truncate_chars(w.description, MAX_COMMAND_DESCRIPTION),
        })
        .collect())
}

/// Truncate on a char boundary, appending an ellipsis when cut.
fn truncate_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
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
/// Same signature as `ExecuteFunc`, deliberately a distinct alias:
/// tools and commands are separate namespaces and the two handles must
/// never be swapped.
type CommandFunc = wasmtime::component::TypedFunc<(String, String), (String,)>;
/// Same signature again, distinct alias for the same reason as
/// `CommandFunc`: events are a third namespace, and the handle must
/// never be swapped with the other two.
type EventFunc = wasmtime::component::TypedFunc<(String, String), (String,)>;

struct BridgeInner {
    store: Store<PluginState>,
    execute: ExecuteFunc,
    /// `None` when the component exports no `execute-command`.
    execute_command: Option<CommandFunc>,
    /// `None` when the component exports no `handle-event`, or exports
    /// `list-events` returning an empty list.
    handle_event: Option<EventFunc>,
}

struct ComponentBridge {
    specs: Vec<ToolSpec>,
    /// Fixed at load. Never re-read — see `PluginRebuild::build`.
    command_specs: Vec<crate::command::CommandSpec>,
    /// Copied off the engine so `execute_tool` can re-arm the deadline
    /// without reaching back for it.
    budget_ticks: u64,
    /// Granted ∩ requested, fixed at load — see the comment at the
    /// `list-events` call site in `PluginEngine::load`. Never re-read
    /// from the guest after load; a plugin cannot expand its own grant
    /// mid-session.
    event_subscriptions: Vec<String>,
    /// Requested \ granted, kept only for the load-time report.
    unsatisfied_event_requests: Vec<String>,
    /// Guest budget for one `handle-event` call, in epoch ticks. Smaller
    /// than `budget_ticks` — see `EVENT_EPOCH_BUDGET_TICKS`.
    event_budget_ticks: u64,
    /// Deliveries skipped because the plugin was still busy with a
    /// previous call (`try_lock` returned `WouldBlock`). Never resets
    /// for the life of the bridge.
    dropped_events: AtomicU64,
    /// Everything needed to stand a fresh instance back up after a
    /// trap. A trapped component instance cannot be re-entered — every
    /// later call returns "cannot enter component instance" — so
    /// without this one bad call bricks the plugin for the rest of the
    /// session, which is the opposite of what the mutex-poison recovery
    /// below claims to guarantee.
    rebuild: PluginRebuild,
    inner: Mutex<BridgeInner>,
}

/// The ingredients for re-instantiating a plugin after a trap.
struct PluginRebuild {
    engine: Engine,
    component: Component,
    linker: Linker<PluginState>,
    url_allowlist: Vec<String>,
    cwd: PathBuf,
    allow_fs: bool,
    allow_network: bool,
}

impl PluginRebuild {
    /// Fresh store + instance + export handles, for recovering from a
    /// trap. Similar to the tail of `PluginEngine::load` but
    /// deliberately does LESS: it never re-calls `list-commands`.
    ///
    /// Two reasons. A plugin must not be able to change its command set
    /// mid-session — that would slip past the collision check the
    /// registry already ran — and the recovery path should spend as
    /// little of the guest's epoch budget as possible, since the whole
    /// point is getting back to a usable instance.
    fn build(&self, budget_ticks: u64) -> Result<BridgeInner, String> {
        let mut store = Store::new(
            &self.engine,
            PluginState {
                url_allowlist: self.url_allowlist.clone(),
                cwd: self.cwd.clone(),
                allow_fs: self.allow_fs,
                allow_network: self.allow_network,
            },
        );
        store.set_epoch_deadline(budget_ticks);
        let instance = self
            .linker
            .instantiate(&mut store, &self.component)
            .map_err(|e| format!("re-instantiate failed: {e}"))?;
        let execute = instance
            .get_typed_func::<(String, String), (String,)>(&mut store, "execute-tool")
            .map_err(|e| format!("re-resolve execute-tool failed: {e}"))?;
        // `.ok()`, not `?`: a tool-only component must still recover
        // from a trap. Forgetting this line degrades silently — after
        // any trap every command would report "no longer exports
        // execute-command" while the palette kept advertising it.
        let execute_command = instance
            .get_typed_func::<(String, String), (String,)>(&mut store, "execute-command")
            .ok();
        // `.ok()` again, and NOT re-calling `list-events`, for the same
        // two reasons as `execute_command` above: a plugin must not be
        // able to change its subscription set mid-session, and recovery
        // should spend as little epoch budget as possible.
        let handle_event = instance
            .get_typed_func::<(String, String), (String,)>(&mut store, "handle-event")
            .ok();
        Ok(BridgeInner {
            store,
            execute,
            execute_command,
            handle_event,
        })
    }
}

impl ComponentBridge {
    /// Swap in a fresh store + instance after a trap, so the next call
    /// starts clean. On failure the old, unusable instance is left in
    /// place — the next call then reports the original trap style of
    /// error rather than panicking, which is the safe direction.
    fn reset(inner: &mut BridgeInner, rebuild: &PluginRebuild, budget_ticks: u64) {
        match rebuild.build(budget_ticks) {
            Ok(fresh) => *inner = fresh,
            Err(e) => eprintln!("nanopi: could not recover plugin after trap: {e}"),
        }
    }
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
        // Re-arm the hang breaker. Required per call: the deadline is
        // relative to the epoch at the time it is set, so a store armed
        // once at load would be long past its deadline by the first
        // call. A plugin that blows the budget traps, and the trap is
        // reported to the model as a failed tool call below.
        inner.store.set_epoch_deadline(self.budget_ticks);
        let called = inner.execute.call(
            &mut inner.store,
            (name.to_string(), args_json.to_string()),
        );

        let out_json = match called {
            Ok((out,)) => {
                match inner.execute.post_return(&mut inner.store) {
                    Ok(()) => out,
                    Err(e) => {
                        // The instance is left mid-call and cannot be
                        // re-entered, same as a trap.
                        Self::reset(inner, &self.rebuild, self.budget_ticks);
                        return Err(format!("execute-tool post_return failed: {e}"));
                    }
                }
            }
            Err(e) => {
                // A trapped component instance is permanently
                // un-enterable: every later call returns "cannot enter
                // component instance", in microseconds, with a message
                // neither the user nor the model can act on. Recovering
                // here is what makes the guarantee above true rather
                // than aspirational — and the trigger is not exotic, a
                // tool argument large enough to exhaust the guest's
                // allocator is enough.
                Self::reset(inner, &self.rebuild, self.budget_ticks);
                return Err(format!("execute-tool trapped: {e}"));
            }
        };

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

    fn command_specs(&self) -> Vec<crate::command::CommandSpec> {
        self.command_specs.clone()
    }

    fn execute_command(
        &self,
        name: &str,
        args: &str,
    ) -> Result<crate::command::CommandAction, String> {
        // Checked against `command_specs`, never `specs`: the two are
        // separate namespaces, and merging them would let a command
        // name reach `execute-tool` or the reverse.
        if !self.command_specs.iter().any(|s| s.name == name) {
            return Err(format!("plugin does not export command {name:?}"));
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *guard;
        inner.store.set_epoch_deadline(self.budget_ticks);
        let Some(execute_command) = inner.execute_command.as_ref() else {
            // Only reachable when a post-trap rebuild came back without
            // the export — see `PluginRebuild::build`.
            return Err(
                "plugin no longer exports `execute-command` after trap recovery".to_string(),
            );
        };
        let called = execute_command.call(
            &mut inner.store,
            (name.to_string(), args.to_string()),
        );

        let out_json = match called {
            Ok((out,)) => match execute_command.post_return(&mut inner.store) {
                Ok(()) => out,
                Err(e) => {
                    Self::reset(inner, &self.rebuild, self.budget_ticks);
                    return Err(format!("execute-command post_return failed: {e}"));
                }
            },
            Err(e) => {
                Self::reset(inner, &self.rebuild, self.budget_ticks);
                return Err(format!("execute-command trapped: {e}"));
            }
        };

        let wire: WireCommandAction = serde_json::from_str(&out_json).map_err(|e| {
            format!("execute-command returned invalid JSON: {e} (got {out_json:?})")
        })?;
        Ok(match wire {
            WireCommandAction::Print(t) => {
                crate::command::CommandAction::Print(truncate_chars(t, MAX_ACTION_PAYLOAD))
            }
            WireCommandAction::SendUserMessage(t) => {
                crate::command::CommandAction::SendUserMessage(truncate_chars(
                    t,
                    MAX_ACTION_PAYLOAD,
                ))
            }
            WireCommandAction::Error(t) => {
                crate::command::CommandAction::Error(truncate_chars(t, MAX_ACTION_PAYLOAD))
            }
        })
    }

    fn event_subscriptions(&self) -> Vec<String> {
        self.event_subscriptions.clone()
    }

    fn unsatisfied_event_requests(&self) -> Vec<String> {
        self.unsatisfied_event_requests.clone()
    }

    fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    fn handle_event(&self, event: &str, payload_json: &str) {
        if !self.event_subscriptions.iter().any(|e| e == event) {
            return;
        }
        // Observe-only, non-blocking: a busy plugin must never make the
        // caller (the turn's critical path) wait. `try_lock` rather than
        // `lock` is the entire point of this method existing separately
        // from `execute_tool`'s blocking lock.
        let mut guard = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => {
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "nanopi: dropped event delivery, plugin busy [event={event}]"
                );
                return;
            }
            // Poisoned must NOT be treated as busy — that would leave
            // delivery silently stopped forever after one panicking
            // call. Recover exactly like `execute_tool` does.
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
        };
        let inner = &mut *guard;
        let Some(handle_event) = inner.handle_event.as_ref() else {
            // Not reachable in normal operation (`event_subscriptions`
            // is only non-empty when `handle-event` resolved at load),
            // but a post-trap rebuild could in principle come back
            // without it — defensive, not a bug report.
            return;
        };
        // Smaller budget than tool calls — see `EVENT_EPOCH_BUDGET_TICKS`.
        inner.store.set_epoch_deadline(self.event_budget_ticks);
        let called = handle_event.call(
            &mut inner.store,
            (event.to_string(), payload_json.to_string()),
        );
        match called {
            Ok((_out,)) => {
                // Return value is discarded by design (§3) — only
                // `post_return` matters, to release the call frame.
                if let Err(e) = handle_event.post_return(&mut inner.store) {
                    eprintln!(
                        "nanopi: handle-event post_return failed, resetting plugin [event={event}]: {e}"
                    );
                    Self::reset(inner, &self.rebuild, self.budget_ticks);
                }
            }
            Err(e) => {
                eprintln!(
                    "nanopi: handle-event trapped, resetting plugin [event={event}]: {e}"
                );
                Self::reset(inner, &self.rebuild, self.budget_ticks);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_requests_reads_json_array() {
        let requested = parse_event_requests(r#"["turn_start", "input"]"#).expect("valid");
        assert_eq!(requested, vec!["turn_start".to_string(), "input".to_string()]);
    }

    #[test]
    fn parse_event_requests_accepts_empty_list() {
        assert!(parse_event_requests("[]").expect("valid").is_empty());
    }

    #[test]
    fn parse_event_requests_rejects_garbage() {
        let err = parse_event_requests("not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "{err}");
    }

    #[test]
    fn parse_event_requests_caps_the_count() {
        let many: String = (0..MAX_EVENTS_PER_PLUGIN + 1)
            .map(|i| format!("\"e{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let err = parse_event_requests(&format!("[{many}]")).unwrap_err();
        assert!(err.contains("more than the"), "{err}");
    }

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
    fn parse_command_specs_reads_json_array() {
        let json = r#"[
            {"name":"todo","description":"show the list"},
            {"name":"deploy","description":"ship it"}
        ]"#;
        let cmds = parse_command_specs(json).expect("valid");
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "todo");
        assert_eq!(cmds[1].description, "ship it");
    }

    #[test]
    fn parse_command_specs_accepts_empty_list() {
        assert!(parse_command_specs("[]").expect("valid").is_empty());
    }

    #[test]
    fn parse_command_specs_rejects_garbage() {
        let err = parse_command_specs("not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "got {err}");
        // The raw payload is echoed so a plugin author can see what
        // their guest actually emitted.
        assert!(err.contains("not json"), "got {err}");
    }

    #[test]
    fn parse_command_specs_caps_the_count_and_the_description() {
        let many: String = (0..MAX_COMMANDS_PER_PLUGIN + 1)
            .map(|i| format!(r#"{{"name":"c{i}","description":"d"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let err = parse_command_specs(&format!("[{many}]")).unwrap_err();
        assert!(err.contains("more than the"), "got {err}");

        let long = "x".repeat(MAX_COMMAND_DESCRIPTION + 50);
        let one = parse_command_specs(&format!(r#"[{{"name":"c","description":"{long}"}}]"#))
            .expect("valid");
        assert_eq!(
            one[0].description.chars().count(),
            MAX_COMMAND_DESCRIPTION + 1,
            "truncated plus the ellipsis"
        );
    }

    /// The action shape is closed: exactly one known key, no more.
    #[test]
    fn wire_command_action_accepts_only_a_single_known_key() {
        for (json, want) in [
            (r#"{"print":"hi"}"#, "print"),
            (r#"{"send_user_message":"hi"}"#, "send"),
            (r#"{"error":"nope"}"#, "error"),
        ] {
            let got: WireCommandAction =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{json} -> {e}"));
            match (got, want) {
                (WireCommandAction::Print(_), "print") => {}
                (WireCommandAction::SendUserMessage(_), "send") => {}
                (WireCommandAction::Error(_), "error") => {}
                (other, _) => panic!("{json} parsed as {other:?}"),
            }
        }
        for bad in [
            "{}",
            r#"{"printt":"typo"}"#,
            r#"{"print":"a","error":"b"}"#,
            r#"{"print":1}"#,
            r#""just a string""#,
        ] {
            assert!(
                serde_json::from_str::<WireCommandAction>(bad).is_err(),
                "{bad} should not parse"
            );
        }
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

    /// A plugin that never returns must be cut off, not ridden out.
    ///
    /// Before epoch interruption this was an unbounded hang with no way
    /// out: the guest runs on a real OS thread with no yield points, so
    /// Esc cannot reach it, and `max_wasm_stack` bounds recursion
    /// rather than iteration. The fixture's `execute-tool` spins on a
    /// volatile write forever.
    ///
    /// Run with a 50ms tick and a 2-tick budget so the breaker fires in
    /// ~100ms instead of the shipped 30s. The generous 30s ceiling on
    /// the assertion is there to catch "never trapped at all", not to
    /// measure the deadline — the point is that it terminates.
    #[test]
    fn runaway_plugin_is_cut_off_by_the_epoch_deadline() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/runaway-plugin.component.wasm");

        let engine = PluginEngine::with_epoch(
            std::time::Duration::from_millis(50),
            2,
        )
        .expect("engine init");
        let (bridge, specs) = engine
            .load(&fixture, Vec::new(), std::env::temp_dir(), false, false, Vec::new())
            .expect("runaway fixture must still LOAD — only execute-tool spins");
        assert_eq!(specs.len(), 1, "fixture advertises one tool");

        let started = std::time::Instant::now();
        let err = bridge
            .execute_tool("spin", "{}")
            .expect_err("an endless loop must not return Ok");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "the deadline never fired; the call ran for {elapsed:?}"
        );
        assert!(
            err.contains("trapped"),
            "expected a trap reported as a failed tool call, got {err:?}"
        );
    }

    /// The flip side: arming the deadline is per-call, so a well-behaved
    /// plugin must stay callable no matter how many epochs have passed
    /// since it loaded. Getting this wrong is easy and quiet — with
    /// `epoch_interruption` on, a store whose deadline is never re-armed
    /// traps on its *second* call, because the first consumed it.
    #[test]
    fn epoch_deadline_is_rearmed_between_calls() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/example-plugin.component.wasm");

        let engine = PluginEngine::with_epoch(
            std::time::Duration::from_millis(10),
            2,
        )
        .expect("engine init");
        let (bridge, _) = engine
            .load(&fixture, Vec::new(), std::env::temp_dir(), false, false, Vec::new())
            .expect("example fixture loads");

        for i in 0..3 {
            // Sleep past a full budget between calls: if the deadline
            // were armed once at load, this is what would kill it.
            std::thread::sleep(std::time::Duration::from_millis(60));
            let out = bridge
                .execute_tool("rot13", r#"{"text":"abc"}"#)
                .unwrap_or_else(|e| panic!("call {i} failed: {e}"));
            assert!(!out.is_error, "call {i} errored: {}", out.content);
        }
    }

    // ── url_allowed ─────────────────────────────────────────────────
    // The allowlist is the only thing between an installed plugin and
    // the network, so the bypasses get their own tests. Expected
    // values come from the capability's spec (deny-by-default, match
    // on host), not from re-deriving what the implementation does.

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    /// `*` is the escape hatch for plugins whose host set isn't
    /// knowable in advance. It must actually work — a pattern that
    /// silently matches nothing is worse than no pattern at all,
    /// because the user believes the gate is open.
    #[test]
    fn url_allowed_star_matches_any_host() {
        let l = list(&["*"]);
        assert!(url_allowed("https://www.workbuddy.cn/", &l));
        assert!(url_allowed("http://192.168.1.1:8080/x", &l));
        assert!(url_allowed("https://anything.example/", &l));
    }

    /// ...but `*` widens hosts only. The scheme check is a separate
    /// guarantee — `file://` under `*` would turn the network
    /// capability into the filesystem one that `allow_fs` gates.
    #[test]
    fn star_does_not_widen_the_scheme_check() {
        let l = list(&["*"]);
        assert!(!url_allowed("file:///etc/passwd", &l));
        assert!(!url_allowed("ftp://example.com/x", &l));
    }

    /// `*.example.com` covers subdomains, on a dot boundary.
    #[test]
    fn url_allowed_subdomain_wildcard() {
        let l = list(&["*.workbuddy.cn"]);
        assert!(url_allowed("https://www.workbuddy.cn/", &l));
        assert!(url_allowed("https://a.b.workbuddy.cn/", &l));
        // The bypass a suffix-only check would wave through.
        assert!(!url_allowed("https://evil-workbuddy.cn/", &l));
        assert!(!url_allowed("https://workbuddy.cn.evil.com/", &l));
    }

    /// `*.example.com` deliberately excludes the apex — the bare entry
    /// is how you ask for both. If the two spellings meant the same
    /// thing there would be no way to say "subdomains only".
    #[test]
    fn subdomain_wildcard_excludes_the_apex() {
        assert!(!url_allowed("https://workbuddy.cn/", &list(&["*.workbuddy.cn"])));
        assert!(url_allowed("https://workbuddy.cn/", &list(&["workbuddy.cn"])));
    }

    /// An unsupported star position must be refused, not normalized
    /// into something broader. Dropping the `*` from `api.*.com` would
    /// leave a rule the user never wrote.
    #[test]
    fn a_star_in_the_middle_is_refused_not_widened() {
        assert_eq!(parse_allow_rule("api.*.com"), None);
        assert_eq!(parse_allow_rule("*.*"), None);
        assert_eq!(parse_allow_rule("*evil.com"), None);
        // And a refused entry grants nothing, rather than everything.
        assert!(!url_allowed("https://api.foo.com/", &list(&["api.*.com"])));
    }

    /// A wildcard entry alongside real hosts still works, and an
    /// unparseable neighbour doesn't disable the good ones.
    #[test]
    fn rules_are_independent_of_each_other() {
        let l = list(&["", "api.*.com", "*.workbuddy.cn"]);
        assert!(url_allowed("https://www.workbuddy.cn/", &l));
        assert!(!url_allowed("https://elsewhere.com/", &l));
    }

    /// The warning at load time keys off this, so it has to see `*`
    /// through the same parser the gate uses — including a `*` written
    /// with stray whitespace.
    #[test]
    fn any_host_detection_matches_the_gate() {
        assert!(allowlist_allows_any_host(&list(&["  * "])));
        assert!(allowlist_allows_any_host(&list(&["example.com", "*"])));
        assert!(!allowlist_allows_any_host(&list(&["*.example.com"])));
        assert!(!allowlist_allows_any_host(&[]));
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

    /// Regression for the bypass that retired the hand-rolled parser.
    /// WHATWG ends the authority at `\` for http/https; the old code
    /// ended it only at `/`, `?`, `#`, so it read the host from after
    /// the last `@` and matched the allowlist while reqwest connected
    /// somewhere else entirely. Verified against a live client at the
    /// time: the connection landed on the host BEFORE the backslash.
    #[test]
    fn url_allowed_refuses_backslash_authority_bypass() {
        let l = list(&["api.github.com"]);
        assert!(!url_allowed(r"https://evil.com\@api.github.com/", &l));
        // Same hole without needing userinfo at all: the old suffix
        // check saw a host ending in `.api.github.com`.
        assert!(!url_allowed(r"https://evil.com\.api.github.com/", &l));
    }

    /// The SSRF shape of the same bypass — a metadata endpoint reached
    /// through an allowlist that never mentioned it.
    #[test]
    fn url_allowed_refuses_backslash_ssrf() {
        let l = list(&["github.com"]);
        assert!(!url_allowed(
            r"https://169.254.169.254\@github.com/latest/meta-data/",
            &l
        ));
    }

    /// Sharing the client's parser also fixes a fail-closed quirk: an
    /// obfuscated IP literal is normalized, so an allowlisted address
    /// written differently now matches instead of being refused.
    #[test]
    fn url_allowed_normalizes_ip_literals() {
        let l = list(&["127.0.0.1"]);
        assert!(url_allowed("http://0177.0.0.1/x", &l));
        assert!(url_allowed("http://2130706433/x", &l));
    }

    /// A fully-qualified trailing dot names the same host.
    #[test]
    fn url_allowed_ignores_trailing_dot() {
        let l = list(&["api.github.com"]);
        assert!(url_allowed("https://api.github.com./x", &l));
    }

    /// ...but the dot must not become a way to smuggle a suffix match.
    #[test]
    fn url_allowed_trailing_dot_does_not_widen_matching() {
        let l = list(&["api.github.com"]);
        assert!(!url_allowed("https://api.github.com.evil.com./x", &l));
    }

    /// Regression: `resolve_readable` only checked containment, so a
    /// FIFO inside the working directory was accepted and the
    /// subsequent `read_to_string` blocked forever with no writer.
    /// Nothing could break it: the epoch deadline instruments guest
    /// code and cannot reach a host function parked in a syscall, so
    /// the turn never ended, the blocking thread leaked, and the
    /// plugin's bridge lock was held for the life of the process.
    ///
    /// Asserting at the guard means the test does not have to risk
    /// actually performing the hanging read.
    #[test]
    #[cfg(unix)]
    fn fs_read_refuses_a_fifo() {
        let dir = std::env::temp_dir()
            .join(format!("nanopi-fifo-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            let _ = std::fs::remove_dir_all(&dir);
            return; // no mkfifo on this box; nothing to assert
        }
        let err = resolve_readable(&dir, "pipe")
            .expect_err("a FIFO must not be accepted for reading");
        assert!(err.contains("not a regular file"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory is not readable either, and must say so rather than
    /// surfacing a confusing io error from the read.
    #[test]
    fn fs_read_refuses_a_directory() {
        let dir = std::env::temp_dir()
            .join(format!("nanopi-dir-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let err = resolve_readable(&dir, "sub").expect_err("a directory is not a file");
        assert!(err.contains("not a regular file"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Oversized files are refused at the guard, before anything is
    /// buffered host-side.
    #[test]
    fn fs_read_refuses_oversized_file() {
        let dir = std::env::temp_dir()
            .join(format!("nanopi-big-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.txt");
        std::fs::write(&big, vec![b'x'; (MAX_HOST_READ_BYTES as usize) + 1]).unwrap();
        let err = resolve_readable(&dir, "big.txt").expect_err("over the cap");
        assert!(err.contains("too large"), "got {err}");
        // A file at the limit is still fine.
        let ok = dir.join("ok.txt");
        std::fs::write(&ok, vec![b'x'; MAX_HOST_READ_BYTES as usize]).unwrap();
        assert!(resolve_readable(&dir, "ok.txt").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: a trapped component instance cannot be re-entered,
    /// so before the bridge learned to rebuild itself, the FIRST trap
    /// killed the plugin for the rest of the session — every later call
    /// returned "cannot enter component instance" in microseconds.
    ///
    /// The trigger needs no malicious plugin: a tool argument large
    /// enough to exhaust the guest's bump allocator does it, and the
    /// model has no way to know it just disabled the tool. It keeps
    /// seeing the spec in its tool list and keeps calling.
    #[test]
    fn plugin_survives_a_trap_and_stays_callable() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/example-plugin.component.wasm");
        let engine = PluginEngine::new().expect("engine init");
        let (bridge, _) = engine
            .load(&fixture, Vec::new(), std::env::temp_dir(), false, false, Vec::new())
            .expect("example fixture loads");

        let good = r#"{"text":"abc"}"#;
        let before = bridge.execute_tool("rot13", good).expect("healthy call");
        assert!(!before.is_error, "baseline call should succeed");

        // Blow the guest's 1 MiB arena.
        let huge = "x".repeat(3 * 1024 * 1024);
        let trapped = bridge
            .execute_tool("rot13", &format!(r#"{{"text":"{huge}"}}"#))
            .expect_err("an oversized argument must trap");
        assert!(trapped.contains("trapped"), "got {trapped}");

        let after = bridge
            .execute_tool("rot13", good)
            .expect("plugin must still be callable after a trap");
        assert!(!after.is_error, "post-trap call errored: {}", after.content);
        assert_eq!(
            after.content, before.content,
            "a recovered instance must compute the same answer"
        );
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
        match engine.load(&p, Vec::new(), std::env::temp_dir(), false, false, Vec::new()) {
            Ok(_) => panic!("garbage bytes must not compile as a component"),
            Err(e) => assert!(e.contains("compile"), "got {e}"),
        }
        let _ = std::fs::remove_file(&p);
    }
}

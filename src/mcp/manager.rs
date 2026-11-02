//! Owns every configured MCP server's connection: connects them
//! concurrently at startup, hands their tools to the registry, and shuts
//! them all down on exit.
//!
//! **Connections belong to the manager, not to the registry.** The
//! registry is rebuilt from scratch at `/new`, `/resume`, `/import`,
//! `/fork` and on leaving orchestrator mode, and each rebuild produces a
//! registry with no MCP tools in it. Spawning servers is far too
//! expensive to repeat there (an `npx` cold start is seconds), and a
//! server spawned per registry would be orphaned the moment its registry
//! was dropped. So connecting happens once, the manager keeps the
//! clients and the already-built [`McpTool`]s, and
//! [`McpManager::register_into`] is the cheap, repeatable step every
//! rebuild calls.
//!
//! **Startup is bounded by one outer deadline, and one server's failure
//! is that server's.** Servers connect concurrently, each under its own
//! `timeout_at` against the *same* instant, so a hung server costs
//! exactly the deadline once and never the sum, and a slow one cannot
//! discard the fast ones that finished. `McpClient::connect` is
//! cancel-safe (the transport's `ChildGuard` reaps on drop), which is
//! what makes cutting it off sound rather than a leak.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use tokio::time::Instant;

use crate::mcp::client::{ConnectionState, McpClient};
use crate::mcp::config::{self, ServerConfig, ServerSpec, SkippedServer};
use crate::mcp::credentials::{redact_arg, redact_url};
use crate::mcp::http::DELETE_TIMEOUT;
use crate::mcp::tool::{first_line, registered_tool_name, McpTool};
use crate::tool::{Tool, ToolRegistry};

/// How long startup may wait for servers, in total.
///
/// Long enough that a warm `npx` or `uvx` server (a second or two)
/// always fits, short enough that one dead or wedged server does not
/// leave the user staring at a blank terminal — the TUI cannot draw
/// until this returns. It is *not* long enough for a first-ever `npx
/// -y` download; that server is reported as not started in time, and
/// the notice says why, because a user who sees "failed" for something
/// that works on the second try needs to be told it is a cold cache.
pub const STARTUP_DEADLINE: Duration = Duration::from_secs(15);

/// How long [`McpManager::shutdown`] may take in total. Servers are
/// stopped concurrently, so this bounds the slowest one; past it the
/// remaining processes are left to the transport's `ChildGuard`, which
/// kills the group when the clients drop.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Longest status line / notice detail, in characters. A reason is
/// often one very long line (a minified stack frame, a URL), and the
/// status row and the notice both share a terminal line.
const DETAIL_CHARS: usize = 160;

/// Longest reason `/mcp` shows, in characters. Its own line (it wraps)
/// rather than a status row, so the full first line is wanted, but the
/// first line of a minified stack trace is still unbounded and the cap
/// keeps one server from filling the screen.
const REASON_FULL_CHARS: usize = 600;

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    /// Total startup budget. A field so tests need not wait out the
    /// real one.
    pub startup_deadline: Duration,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self { startup_deadline: STARTUP_DEADLINE }
    }
}

/// Which kind of nanopi process is asking, because that — not the
/// config — decides whether it may spawn servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// The interactive TUI: connects, and says so while it waits.
    Tui,
    /// A scripted `nanopi -p` run: connects, silently.
    Print,
    /// A `nanopi -p` process spawned by another nanopi as an agent
    /// (`--brief` / `NANOPI_AGENT_ID`). Never connects.
    ChildAgent,
}

/// Why this session must not connect, if it must not.
///
/// **A child agent never connects.** Children are separate processes,
/// up to `agent.max_concurrency` of them, and each one connecting its
/// own servers is `max_concurrency × servers` processes each paying an
/// `npx` cold start (or, for an HTTP server, its own session) — option B,
/// which `docs/mcp.md` rejects by name. A child reaches the parent's
/// servers through the M9 channel instead (`mcp::proxy`), and says
/// nothing about any of this: a scripted child is not a place for
/// startup chatter.
///
/// **A `--tools` allowlist excludes MCP.** An allowlist is a
/// restriction, and the only ways to honour it for MCP are to match
/// tool names the user cannot know before the servers connect, or to
/// drop MCP wholesale. Connecting and then discarding every tool would
/// spend the startup cost to produce nothing, so we do not connect at
/// all. This also keeps it from ever granting *more* than asked:
/// `register_external` filters by the plugin allowlist only in print
/// mode, and the TUI's allowlisted registry does not set one.
fn skip_reason(kind: SessionKind, tools_allow: &[String]) -> Option<&'static str> {
    if kind == SessionKind::ChildAgent {
        return Some("child agents do not connect MCP servers");
    }
    if !tools_allow.is_empty() {
        return Some("--tools restricts the tool set, which excludes MCP tools");
    }
    None
}

/// Which transport a configured server uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Http,
}

impl TransportKind {
    pub fn label(self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::Http => "http",
        }
    }
}

/// Where a configured server stands, as `/mcp` words it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Connected,
    /// The handshake is in flight. `connect` does not return until it
    /// resolves, so this is only ever seen if a client reports it; it is
    /// carried so a future reconnect needs no change here.
    Connecting,
    /// Tried and did not work (spawn failure, handshake error, timeout,
    /// died since).
    Failed,
    /// Never tried: `mcp.json` rejected the entry (see
    /// [`SkippedServer`]).
    Skipped,
}

impl ServerState {
    pub fn label(self) -> &'static str {
        match self {
            ServerState::Connected => "connected",
            ServerState::Connecting => "connecting",
            ServerState::Failed => "failed",
            ServerState::Skipped => "skipped",
        }
    }
}

/// One server as `/mcp` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    /// What is being run, with secrets masked. See [`endpoint_of`].
    /// Empty for a skipped server: the entry was rejected before it
    /// became a config, and the reason names what was wrong with it.
    pub endpoint: String,
    /// `None` for a skipped server, whose transport is not known.
    pub transport: Option<TransportKind>,
    pub state: ServerState,
    /// Same as `state == Connected`.
    pub connected: bool,
    pub tool_count: usize,
    /// The registered names (`mcp__<server>__<tool>`) of the tools this
    /// server contributed, in the order it listed them.
    pub tool_names: Vec<String>,
    /// First line of why it is not (fully) working, control characters
    /// removed and length capped at 160 — the row/notice form. Never the
    /// full reason: that can be ~12 KB of stderr, and
    /// `ConnectionState::Failed` keeps all of it for the one place that
    /// wants it.
    pub reason: Option<String>,
    /// The same first line with a much larger cap (600 characters), for
    /// `/mcp`, which gives it a line of its own. Still one line, still
    /// control characters flattened.
    pub reason_full: Option<String>,
}

struct Server {
    name: String,
    endpoint: String,
    /// `None` when the server never produced a client: it timed out
    /// before `connect` returned, and the in-flight attempt was dropped.
    client: Option<Arc<McpClient>>,
    kind: TransportKind,
    /// Registered names of the tools kept, in listing order.
    tool_names: Vec<String>,
    /// Why the server is not fully usable *as of startup*, as a first
    /// line capped for `/mcp`. Live failures after that come from the
    /// client's own state.
    startup_reason_full: Option<String>,
}

/// Every server's connection, and the tools they offered.
pub struct McpManager {
    servers: Vec<Server>,
    /// Built once, here, and shared into every registry by
    /// [`register_into`](Self::register_into).
    tools: Vec<Arc<McpTool>>,
    /// One line each, already first-line-only. Kept so the caller —
    /// which knows whether the terminal is ready — decides where they go.
    notices: Vec<String>,
    /// Set once [`shutdown`](Self::shutdown) has reaped every server.
    /// Read by [`kill_now`](Self::kill_now), which must not signal a
    /// process group whose leader has been reaped: the pid is free for
    /// reuse by then.
    stopped: AtomicBool,
    /// Entries of `mcp.json` that were rejected, in file order. They
    /// have no connection and no tools, but they are configured, and
    /// "why is my server not there" is the question `/mcp` answers.
    skipped: Vec<SkippedServer>,
    /// File-level problems: unreadable or malformed `mcp.json`, a name
    /// defined twice. Each already carries its path.
    config_notes: Vec<String>,
    /// Why this session never looked at `mcp.json`, when it did not
    /// (`--tools`, child agent). Without it `/mcp` would say "not
    /// configured" about a file it never read.
    disabled: Option<&'static str>,
}

impl McpManager {
    /// No servers. What a session that must not connect holds, so
    /// callers need no `Option`.
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
            tools: Vec::new(),
            notices: Vec::new(),
            stopped: AtomicBool::new(false),
            skipped: Vec::new(),
            config_notes: Vec::new(),
            disabled: None,
        }
    }

    /// Record what `mcp.json` held that did not become a connection.
    /// Display only: nothing here affects connecting or registration.
    pub fn with_config_problems(
        mut self,
        skipped: Vec<SkippedServer>,
        config_notes: Vec<String>,
    ) -> Self {
        self.skipped = skipped;
        self.config_notes = config_notes;
        self
    }

    /// A manager for a session that must not connect, remembering why.
    fn disabled(reason: &'static str) -> Self {
        Self { disabled: Some(reason), ..Self::empty() }
    }

    /// The one startup call: decide whether this session connects, load
    /// `mcp.json`, connect, and report problems with `note!`.
    ///
    /// Returns an `Arc` because the TUI's `App` and every later
    /// registry rebuild share it.
    pub async fn start_for_session(kind: SessionKind, tools_allow: &[String]) -> Arc<Self> {
        if let Some(reason) = skip_reason(kind, tools_allow) {
            return Arc::new(Self::disabled(reason));
        }

        let loaded = config::load_global();
        config::print_diagnostics(&loaded.diagnostics);
        let notes = loaded.file_notes();
        if loaded.servers.is_empty() {
            return Arc::new(Self::empty().with_config_problems(loaded.skipped, notes));
        }

        if kind == SessionKind::Tui {
            crate::note!(
                "nanopi: connecting to {} MCP server(s)…",
                loaded.servers.len()
            );
        }
        let manager = Self::connect(loaded.servers, ManagerOptions::default())
            .await
            .with_config_problems(loaded.skipped, notes);
        for line in manager.notices() {
            crate::note!("{line}");
        }
        Arc::new(manager)
    }

    /// Connect every server concurrently under the startup deadline,
    /// list each one's tools once, and hold the result.
    ///
    /// Infallible, like `McpClient::connect`: a server that is down is
    /// a [`ServerStatus`] row, never an error.
    pub async fn connect(configs: Vec<ServerConfig>, options: ManagerOptions) -> Self {
        let deadline = Instant::now() + options.startup_deadline;
        let attempts = configs
            .iter()
            .map(|cfg| bring_up(cfg, deadline, options.startup_deadline));
        let results = join_all(attempts).await;

        let mut manager = Self::empty();
        // Name → who registered it, so a collision can say what it lost
        // to. Shared across servers: `mcp__a__b__c` can be produced by
        // server `a` tool `b__c` and server `a__b` tool `c`.
        let mut taken: std::collections::HashMap<String, String> = Default::default();

        for (cfg, result) in configs.iter().zip(results) {
            let endpoint = endpoint_of(cfg);
            let BringUp { client, tools, reason, reason_full } = result;

            let mut skipped: Vec<String> = Vec::new();
            let mut tool_names: Vec<String> = Vec::new();
            if let Some(client) = &client {
                for def in tools {
                    // An empty name is a server bug, and would register
                    // as `mcp__<server>__`, which is not callable.
                    if def.name.is_empty() {
                        skipped.push("(unnamed)".to_string());
                        continue;
                    }
                    let name = registered_tool_name(&cfg.name, &def.name);
                    if let Some(owner) = taken.get(&name) {
                        skipped.push(format!("{:?} (same name as {owner})", def.name));
                        continue;
                    }
                    taken.insert(name.clone(), format!("{:?} of \"{}\"", def.name, cfg.name));
                    manager
                        .tools
                        .push(Arc::new(McpTool::new(client.clone(), endpoint.clone(), def)));
                    tool_names.push(name);
                }
            }

            if let Some(reason) = &reason {
                manager.notices.push(format!(
                    "nanopi: mcp: server \"{}\" unavailable: {reason}",
                    cfg.name
                ));
            }
            if !skipped.is_empty() {
                // One line per server however many collide: a server
                // with a hundred clashing tools must not flood startup.
                let shown: Vec<&str> = skipped.iter().take(3).map(String::as_str).collect();
                let more = skipped.len().saturating_sub(shown.len());
                manager.notices.push(format!(
                    "nanopi: mcp: server \"{}\": skipped {} tool(s) whose name collides after \
                     sanitising: {}{}",
                    cfg.name,
                    skipped.len(),
                    shown.join(", "),
                    if more > 0 { format!(" and {more} more") } else { String::new() }
                ));
            }

            manager.servers.push(Server {
                name: cfg.name.clone(),
                endpoint,
                client,
                kind: match cfg.spec {
                    ServerSpec::Stdio { .. } => TransportKind::Stdio,
                    ServerSpec::Http { .. } => TransportKind::Http,
                },
                tool_names,
                startup_reason_full: reason_full,
            });
        }
        manager
    }

    /// Add every held tool to `registry`. Cheap and repeatable — this
    /// is what each registry rebuild calls — and it never fails the
    /// caller: names the registry refuses (a collision with a built-in
    /// or plugin tool) are returned, not raised.
    ///
    /// Never call this on `ToolRegistry::orchestrator()`. That registry
    /// is a closed list the orchestrator's safety argument depends on;
    /// register into the standard registry *before* it is saved and
    /// swapped out, so that leaving orchestrator mode restores the MCP
    /// tools along with everything else.
    pub fn register_into(&self, registry: &mut ToolRegistry) -> Vec<String> {
        self.tools
            .iter()
            .filter_map(|tool| {
                let tool: Arc<dyn Tool> = tool.clone();
                registry.register_external(tool).err()
            })
            .collect()
    }

    /// Per-server status for `/mcp`: the connected and failed servers in
    /// config order, then the entries `mcp.json` rejected. Read from
    /// state the manager already holds (each client's state is a field
    /// read, never a request), so a server that died since startup shows
    /// as failed and a wedged one cannot make this wait.
    pub fn status(&self) -> Vec<ServerStatus> {
        let live = self.servers.iter().map(|s| {
            let (state, reason_full) = match &s.client {
                Some(client) => match client.state() {
                    ConnectionState::Ready => {
                        (ServerState::Connected, s.startup_reason_full.clone())
                    }
                    ConnectionState::Connecting => {
                        (ServerState::Connecting, Some("still connecting".to_string()))
                    }
                    ConnectionState::Failed { reason } => (
                        ServerState::Failed,
                        Some(first_line(&reason, REASON_FULL_CHARS)),
                    ),
                },
                None => (ServerState::Failed, s.startup_reason_full.clone()),
            };
            ServerStatus {
                name: s.name.clone(),
                endpoint: s.endpoint.clone(),
                transport: Some(s.kind),
                state,
                connected: state == ServerState::Connected,
                tool_count: s.tool_names.len(),
                tool_names: s.tool_names.clone(),
                reason: reason_full.as_deref().map(|r| first_line(r, DETAIL_CHARS)),
                reason_full,
            }
        });
        let skipped = self.skipped.iter().map(|s| {
            let full = first_line(&s.reason, REASON_FULL_CHARS);
            ServerStatus {
                name: s.name.clone(),
                endpoint: String::new(),
                transport: None,
                state: ServerState::Skipped,
                connected: false,
                tool_count: 0,
                tool_names: Vec::new(),
                reason: Some(first_line(&full, DETAIL_CHARS)),
                reason_full: Some(full),
            }
        });
        live.chain(skipped).collect()
    }

    /// File-level `mcp.json` problems (unreadable, malformed, a repeated
    /// name), each with its path, for `/mcp` to show.
    pub fn config_notes(&self) -> &[String] {
        &self.config_notes
    }

    /// Why this session never read `mcp.json`, if it did not.
    pub fn disabled_reason(&self) -> Option<&'static str> {
        self.disabled
    }

    /// Startup problems, one line each.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The tools held, for the parent↔child channel, which serves a
    /// child's calls from here rather than from a registry: a child's
    /// registry is never the parent's.
    pub fn tools(&self) -> &[Arc<McpTool>] {
        &self.tools
    }

    /// Total tools held, across servers.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Stop every server and reap everything it started. Idempotent,
    /// concurrent, and bounded by [`SHUTDOWN_BUDGET`].
    ///
    /// Call it on every exit path that has an executor: a server is a
    /// bare subprocess, and nothing about exiting nanopi stops one by
    /// itself. The backstops (`ChildGuard` on drop, `PDEATHSIG` on a
    /// hard kill) exist for the paths this cannot run on.
    pub async fn shutdown(&self) {
        let stops = self
            .servers
            .iter()
            .filter_map(|s| s.client.as_ref())
            .map(|c| c.shutdown());
        if tokio::time::timeout(SHUTDOWN_BUDGET, join_all(stops)).await.is_ok() {
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    /// SIGKILL every server's process group, synchronously, and end every
    /// HTTP server's session. The last resort for exits that cannot
    /// `await` [`shutdown`](Self::shutdown): a dropped future, an early
    /// `?` return, a signal handler. The HTTP half blocks for at most
    /// [`DELETE_TIMEOUT`] (plus a margin) in total, however many servers
    /// there are or however wedged: exit must not hang on a courtesy.
    ///
    /// **This exists because drop-based cleanup does not run here.** The
    /// transports carry a `ChildGuard` for exactly this, but a clone of
    /// the live registry — and so of every `McpTool` and its client — is
    /// parked in `plugin_tools`' process-wide dispatch, so the clients
    /// are never dropped before the process exits. Left to the guards, a
    /// SIGTERM'd `nanopi -p` kept its servers' backgrounded helpers
    /// (only the server itself died, to `PDEATHSIG`, which does not
    /// reach grandchildren — and with `npx`, the grandchild is the real
    /// server). Nothing signals once `shutdown` has completed; see
    /// `stopped`. The pid-reuse window before that is the one
    /// `agent_registry::kill_group` already accepts.
    pub fn kill_now(&self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        // Ready servers only. A client that failed its handshake was
        // already shut down and reaped by `connect`, so its pid may
        // belong to someone else now. An HTTP server has no pid, which
        // `filter_map` skips: there is no process of ours to kill.
        let ready = || {
            self.servers
                .iter()
                .filter_map(|s| s.client.as_ref())
                .filter(|c| c.state() == ConnectionState::Ready)
        };
        #[cfg(unix)]
        for pid in ready().filter_map(|c| c.pid()) {
            if pid > 1 {
                // SAFETY: a plain syscall; no memory is touched.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        // An HTTP server has no process to kill, but a session to end.
        // This is the only place those exits can reach `DELETE`: it
        // cannot `await`, so each one goes out on its own thread, all at
        // once, and we wait for them together under one deadline — a
        // dead server must delay an exit that is already unclean by
        // `DELETE_TIMEOUT`, not by that times the number of servers.
        let pending: Vec<_> = ready().filter_map(|c| c.abandon()).collect();
        let deadline = std::time::Instant::now() + DELETE_TIMEOUT + Duration::from_millis(250);
        for done in pending {
            let _ = done.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
        }
    }

    /// A guard that runs [`kill_now`](Self::kill_now) when dropped.
    /// Hold it for the life of the session: it covers every way the
    /// session's future can end without reaching an explicit
    /// `shutdown` — and does nothing on the paths that do.
    pub fn kill_on_drop(self: &Arc<Self>) -> KillOnDrop {
        KillOnDrop(Arc::clone(self))
    }

    /// Pids of every live server process, for tests that must prove
    /// none survive. HTTP servers have none and contribute nothing.
    pub fn pids(&self) -> Vec<u32> {
        self.servers
            .iter()
            .filter_map(|s| s.client.as_ref().and_then(|c| c.pid()))
            .collect()
    }
}

/// See [`McpManager::kill_on_drop`].
pub struct KillOnDrop(Arc<McpManager>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.kill_now();
    }
}

// ------------------------------------------------------------ bring-up

struct BringUp {
    client: Option<Arc<McpClient>>,
    tools: Vec<crate::mcp::protocol::Tool>,
    /// First line only, already cleaned. `None` = fully up.
    reason: Option<String>,
    /// The same line with the larger `/mcp` cap.
    reason_full: Option<String>,
}

/// Connect one server and list its tools, all under `deadline`.
async fn bring_up(cfg: &ServerConfig, deadline: Instant, budget: Duration) -> BringUp {
    let attempt = async {
        let client = Arc::new(McpClient::connect(cfg).await);
        match client.state() {
            ConnectionState::Ready => match client.list_tools().await {
                Ok(tools) => (client, tools, None),
                // Connected but unlistable: kept as a server (its
                // process is up and `/mcp` should say so) with no tools.
                Err(e) => (
                    client,
                    Vec::new(),
                    Some(format!(
                        "connected, but listing tools failed: {}",
                        first_line(&e.to_string(), REASON_FULL_CHARS)
                    )),
                ),
            },
            ConnectionState::Failed { reason } => {
                (client, Vec::new(), Some(first_line(&reason, REASON_FULL_CHARS)))
            }
            ConnectionState::Connecting => (
                client,
                Vec::new(),
                Some("still connecting".to_string()),
            ),
        }
    };

    match tokio::time::timeout_at(deadline, attempt).await {
        Ok((client, tools, full)) => BringUp {
            client: Some(client),
            tools,
            reason: full.as_deref().map(|r| first_line(r, DETAIL_CHARS)),
            reason_full: full,
        },
        // The attempt is dropped here, which is what reaps the child.
        // The message carries the cause people actually hit: a cold
        // `npx` cache.
        Err(_) => {
            let reason = format!(
                "did not finish starting within {}s and was skipped for this session \
                 (a first `npx`/`uvx` run downloads the package; run the command once \
                 by hand to warm its cache)",
                budget.as_secs()
            );
            BringUp {
                client: None,
                tools: Vec::new(),
                reason: Some(reason.clone()),
                reason_full: Some(reason),
            }
        }
    }
}

// ------------------------------------------------------------ endpoint

/// What `/tools` and `/mcp` show as a server's origin: the command line.
///
/// **Never the `env`.** That is where `mcp.json` convention puts API
/// keys and tokens, and the whole point of this line is to be pasted
/// into a bug report. Arguments are shown because "what am I actually
/// running" is the security question this line answers (`npx -y
/// some-package` versus a local binary), but they are not safe by
/// default either — connection strings and `-e TOKEN=...` flags are
/// common — so [`redact_arg`] masks the two shapes that carry secrets.
///
/// An HTTP server shows its URL — as written in `mcp.json`, so a `${VAR}`
/// is shown as `${VAR}` and never as what it expands to — through
/// [`redact_url`], and nothing else: `headers` carry the bearer token,
/// and even their names are left out. Masking is shared with
/// `ServerConfig`'s `Debug` and the transport's errors
/// (`mcp::credentials`), so there is one rule for what is safe to print.
pub fn endpoint_of(cfg: &ServerConfig) -> String {
    let line = match &cfg.spec {
        ServerSpec::Stdio { command, args, .. } => {
            let mut parts = vec![command.clone()];
            parts.extend(args.iter().map(|a| redact_arg(a)));
            parts.join(" ")
        }
        ServerSpec::Http { url, .. } => redact_url(url),
    };
    first_line(&line, 200)
}

/// A fake server for tests in this crate that need real connections
/// (the TUI's registry-rebuild tests, in particular).
#[cfg(all(test, unix))]
pub(crate) mod testing {
    use super::*;
    use std::collections::BTreeMap;

    /// A server that answers the handshake and `tools/list` with
    /// `tools` (bare names), after running `preamble`, and answers any
    /// `tools/call` with `called <name>`.
    pub(crate) fn fake_server(name: &str, preamble: &str, tools: &[&str]) -> ServerConfig {
        let list = tools
            .iter()
            .map(|t| format!(r#"{{"name":"{t}","inputSchema":{{"type":"object"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            r#"
            {preamble}
            while IFS= read -r line; do
                id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                case "$line" in
                    *'"method":"initialize"'*)
                        printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1"}}}}}}\n' "$id" ;;
                    *'"method":"tools/list"'*)
                        printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{list}]}}}}\n' "$id" ;;
                    *'"method":"tools/call"'*)
                        name=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
                        printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"called %s"}}]}}}}\n' "$id" "$name" ;;
                esac
            done
            "#
        );
        ServerConfig::stdio(name, "sh", vec!["-c".to_string(), script], BTreeMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn a_child_agent_never_connects_whatever_the_config_says() {
        assert!(skip_reason(SessionKind::ChildAgent, &[]).is_some());
        assert!(skip_reason(SessionKind::Print, &[]).is_none());
        assert!(skip_reason(SessionKind::Tui, &[]).is_none());
    }

    #[test]
    fn a_tools_allowlist_excludes_mcp_in_every_session_kind() {
        let allow = vec!["read".to_string(), "grep".to_string()];
        assert!(skip_reason(SessionKind::Print, &allow).is_some());
        assert!(skip_reason(SessionKind::Tui, &allow).is_some());
    }

    #[test]
    fn the_endpoint_never_contains_env_and_masks_credentials_in_arguments() {
        let cfg = ServerConfig::stdio(
            "db",
            "npx",
            vec![
                "-y".into(),
                "@scope/server-postgres".into(),
                "postgresql://admin:hunter2@db.internal:5432/app".into(),
                "-e".into(),
                "GITHUB_TOKEN=ghp_abc123".into(),
                "--root=/srv".into(),
            ],
            BTreeMap::from([("API_KEY".to_string(), "sk-live-xyz".to_string())]),
        );
        let e = endpoint_of(&cfg);
        assert!(!e.contains("hunter2"), "{e}");
        assert!(!e.contains("ghp_abc123"), "{e}");
        assert!(!e.contains("sk-live-xyz"), "{e}");
        assert!(e.contains("db.internal:5432/app"), "host survives so the line stays useful: {e}");
        assert!(e.contains("--root=/srv"), "{e}");
        assert!(e.starts_with("npx -y @scope/server-postgres"), "{e}");
    }

    #[test]
    fn an_http_endpoint_is_the_masked_url_and_never_a_header() {
        let cfg = ServerConfig::http(
            "gw",
            "https://admin:hunter2@mcp.example.com/mcp?api_key=abc123",
            BTreeMap::from([("Authorization".to_string(), "Bearer s3cret".to_string())]),
        );
        let e = endpoint_of(&cfg);
        for secret in ["hunter2", "abc123", "s3cret", "Authorization"] {
            assert!(!e.contains(secret), "{secret} leaked: {e}");
        }
        assert_eq!(e, "https://***@mcp.example.com/mcp?***");
    }

    /// The raw template is what `/tools` and `/mcp` show: a `${VAR}` is
    /// never replaced by what it expands to, and a header never appears.
    #[test]
    fn an_http_endpoint_shows_the_template_and_masks_what_it_cannot_judge() {
        let cfg = ServerConfig::http(
            "gw",
            "https://${GW_HOST}/${GW_PATH}/mcp?access_token=${GW_TOKEN}&x=1",
            BTreeMap::from([("Authorization".to_string(), "Bearer ${GW_TOKEN}".to_string())]),
        );
        let e = endpoint_of(&cfg);
        assert_eq!(e, "https://${GW_HOST}/${GW_PATH}/mcp?***");
        for leaked in ["GW_TOKEN", "Authorization", "Bearer", "access_token"] {
            assert!(!e.contains(leaked), "{leaked} leaked: {e}");
        }
    }

    /// One rule for URLs wherever they appear: the same helper masks an
    /// HTTP server's `url` and a URL passed as a stdio argument.
    #[test]
    fn a_url_argument_and_an_http_url_are_masked_the_same_way() {
        let url = "https://u:hunter2@h.example/mcp?token=abc123";
        let http = endpoint_of(&ServerConfig::http("a", url, BTreeMap::new()));
        let stdio = endpoint_of(&ServerConfig::stdio("b", "run", vec![url.into()], BTreeMap::new()));
        assert_eq!(http, "https://***@h.example/mcp?***");
        assert_eq!(stdio, format!("run {http}"));
        for e in [&http, &stdio] {
            assert!(!e.contains("hunter2") && !e.contains("abc123"), "{e}");
        }
    }

    #[test]
    fn skipped_entries_are_rows_after_the_live_ones_and_carry_their_reason() {
        let skipped = vec![
            SkippedServer { name: "gw".into(), reason: "server \"gw\": unset ${NOPE}\nsecond line".into() },
            SkippedServer { name: "ws".into(), reason: "server \"ws\": WebSocket is not supported".into() },
        ];
        let m = McpManager::empty().with_config_problems(skipped, vec!["note (/p)".into()]);
        assert!(m.is_empty(), "skipped entries hold no connection");
        let st = m.status();
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].name, "gw");
        assert_eq!(st[0].state, ServerState::Skipped);
        assert!(!st[0].connected && st[0].transport.is_none() && st[0].endpoint.is_empty());
        assert_eq!(st[0].reason_full.as_deref(), Some("server \"gw\": unset ${NOPE}"));
        assert_eq!(m.config_notes(), ["note (/p)"]);
        assert!(m.disabled_reason().is_none());
    }

    #[test]
    fn a_session_that_may_not_connect_remembers_why() {
        let m = McpManager::disabled(skip_reason(SessionKind::Tui, &["read".into()]).unwrap());
        assert!(m.disabled_reason().unwrap().contains("--tools"));
        assert!(m.status().is_empty());
        assert!(McpManager::empty().disabled_reason().is_none());
    }

    #[test]
    fn an_empty_manager_registers_nothing_and_shuts_down_instantly() {
        let m = McpManager::empty();
        let mut r = ToolRegistry::standard();
        let before = r.names();
        assert!(m.register_into(&mut r).is_empty());
        assert_eq!(r.names(), before);
        assert!(m.status().is_empty());
        assert!(m.is_empty());
    }

    #[cfg(unix)]
    mod live {
        use super::*;
        use crate::tool::ToolSource;
        use crate::mcp::manager::testing::fake_server as cfg;
        use std::time::Instant as StdInstant;

        fn quick(ms: u64) -> ManagerOptions {
            ManagerOptions { startup_deadline: Duration::from_millis(ms) }
        }

        fn tmp_file(tag: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!("nanopi-mgr-{tag}-{}", crate::util::uuid::v7()))
        }

        /// Alive means a process that is not a zombie: a killed
        /// grandchild is reparented to whatever init the container has,
        /// which may never reap it.
        fn alive(pid: u32) -> bool {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .expect("ps");
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        }

        fn read_pid(path: &std::path::Path) -> u32 {
            std::fs::read_to_string(path)
                .expect("pid file")
                .trim()
                .parse()
                .expect("pid")
        }

        #[tokio::test]
        async fn tools_are_registered_under_their_server_with_an_mcp_source() {
            let m = McpManager::connect(
                vec![cfg("alpha", "", &["one", "two"]), cfg("beta", "", &["three"])],
                ManagerOptions::default(),
            )
            .await;
            let mut r = ToolRegistry::standard();
            assert!(m.register_into(&mut r).is_empty());
            for name in ["mcp__alpha__one", "mcp__alpha__two", "mcp__beta__three"] {
                assert!(r.get(name).is_some(), "{name} missing: {:?}", r.names());
            }
            let sources: Vec<_> = r
                .entries()
                .into_iter()
                .filter_map(|(_, s)| match s {
                    ToolSource::Mcp { server, .. } => Some(server),
                    _ => None,
                })
                .collect();
            assert_eq!(sources.len(), 3);
            let st = m.status();
            assert_eq!(st.len(), 2);
            assert!(st[0].connected && st[0].tool_count == 2 && st[0].reason.is_none());
            assert!(st[1].connected && st[1].tool_count == 1);
            m.shutdown().await;
        }

        /// The `/new` regression, at the unit that fixes it: a second,
        /// freshly built registry gets the same tools without a second
        /// connection.
        #[tokio::test]
        async fn every_rebuilt_registry_gets_the_tools_from_the_one_connection() {
            let m = McpManager::connect(vec![cfg("alpha", "", &["one"])], ManagerOptions::default()).await;
            let pids = m.pids();
            for _ in 0..3 {
                let mut r = ToolRegistry::standard_with_control();
                m.register_into(&mut r);
                assert!(r.get("mcp__alpha__one").is_some());
            }
            assert_eq!(m.pids(), pids, "rebuilding must not respawn anything");
            m.shutdown().await;
        }

        #[tokio::test]
        async fn servers_connect_concurrently_not_one_after_another() {
            let slow = |n: &str| cfg(n, "sleep 1", &["t"]);
            let start = StdInstant::now();
            let m = McpManager::connect(
                vec![slow("a"), slow("b"), slow("c"), slow("d")],
                ManagerOptions::default(),
            )
            .await;
            let took = start.elapsed();
            assert!(m.status().iter().all(|s| s.connected), "{:?}", m.status());
            // Four 1s starts in series would be >= 4s.
            assert!(took < Duration::from_millis(3000), "serial startup: {took:?}");
            m.shutdown().await;
        }

        /// The acceptance case: a hung server delays startup by the
        /// deadline and no more, costs the others nothing, yields one
        /// first-line notice, and is not left running.
        #[tokio::test]
        async fn a_hung_server_costs_the_deadline_once_and_is_reaped() {
            let pidfile = tmp_file("hung");
            let hung = ServerConfig::stdio(
                "hung",
                "sh",
                vec!["-c".into(), format!("echo $$ > {}; exec sleep 60", pidfile.display())],
                BTreeMap::new(),
            );
            let start = StdInstant::now();
            let m = McpManager::connect(
                vec![hung, cfg("good", "", &["t"])],
                quick(800),
            )
            .await;
            let took = start.elapsed();
            assert!(took < Duration::from_secs(4), "{took:?}");

            let st = m.status();
            assert!(!st[0].connected && st[1].connected, "{st:?}");
            assert_eq!(st[1].tool_count, 1, "the fast server must survive the slow one");
            assert_eq!(m.notices().len(), 1, "{:?}", m.notices());
            assert!(m.notices()[0].contains("hung"), "{:?}", m.notices());
            assert!(!m.notices()[0].contains('\n'));

            // Wait on the signal, not a sleep: the process was killed
            // when its connect future was dropped.
            let pid = read_pid(&pidfile);
            let until = StdInstant::now() + Duration::from_secs(10);
            while alive(pid) {
                assert!(StdInstant::now() < until, "timed-out server {pid} is still running");
                std::thread::sleep(Duration::from_millis(20));
            }
            m.shutdown().await;
        }

        /// A failing server's ~12 KB stderr must reach nobody: the
        /// status row and the notice carry its first line only.
        #[tokio::test]
        async fn a_failed_server_reports_one_clean_first_line() {
            let script = r#"
                printf '\033[31mnpm ERR! 404 Not Found - GET https://registry/nope\033[0m\n' >&2
                i=0; while [ $i -lt 300 ]; do echo "    at stack frame $i (some/very/long/path/to/a/module.js:1:1)" >&2; i=$((i+1)); done
                exit 1
            "#;
            let dead = ServerConfig::stdio("dead", "sh", vec!["-c".into(), script.into()], BTreeMap::new());
            let m = McpManager::connect(vec![dead], ManagerOptions::default()).await;
            let st = &m.status()[0];
            assert!(!st.connected);
            let reason = st.reason.as_deref().expect("reason");
            assert!(!reason.contains('\n') && !reason.contains('\x1b'), "{reason:?}");
            assert!(reason.chars().count() <= DETAIL_CHARS + 1, "{} chars", reason.chars().count());
            assert_eq!(m.notices().len(), 1);
            assert!(!m.notices()[0].contains("stack frame"), "{}", m.notices()[0]);
            assert!(!m.notices()[0].contains('\x1b'));
            m.shutdown().await;
        }

        /// `/mcp` shows the whole first line (up to its own, larger cap)
        /// where the status row and the startup notice show 160
        /// characters: the reason is the thing the user came to read.
        #[tokio::test]
        async fn status_keeps_the_full_first_line_beside_the_capped_one() {
            let long = format!("/definitely/not/installed/{}", "x".repeat(300));
            let ghost = ServerConfig::stdio("ghost", long.clone(), Vec::new(), BTreeMap::new());
            let m = McpManager::connect(vec![ghost], ManagerOptions::default()).await;
            let st = &m.status()[0];
            assert_eq!(st.state, ServerState::Failed);
            let short = st.reason.as_deref().unwrap();
            let full = st.reason_full.as_deref().unwrap();
            assert!(short.chars().count() <= DETAIL_CHARS + 1, "{short}");
            assert!(full.contains(&long), "the whole first line, not the cap: {full}");
            assert!(!full.contains('\n'));
            assert!(full.chars().count() <= REASON_FULL_CHARS + 1);
            m.shutdown().await;
        }

        #[tokio::test]
        async fn status_lists_registered_tool_names_transport_and_state() {
            let m = McpManager::connect(
                vec![cfg("alpha", "", &["one", "two"]), cfg("beta", "", &[])],
                ManagerOptions::default(),
            )
            .await;
            let st = m.status();
            assert_eq!(st[0].state, ServerState::Connected);
            assert_eq!(st[0].transport, Some(TransportKind::Stdio));
            assert_eq!(st[0].tool_names, ["mcp__alpha__one", "mcp__alpha__two"]);
            assert_eq!(st[1].tool_count, 0);
            assert!(st[1].tool_names.is_empty());
            m.shutdown().await;
        }

        #[tokio::test]
        async fn a_server_that_timed_out_is_failed_with_the_whole_hint() {
            let hung = ServerConfig::stdio(
                "hung",
                "sh",
                vec!["-c".into(), "sleep 30".into()],
                BTreeMap::new(),
            );
            let m = McpManager::connect(vec![hung], quick(200)).await;
            let st = &m.status()[0];
            assert_eq!(st.state, ServerState::Failed);
            assert!(st.reason_full.as_deref().unwrap().contains("warm its cache"), "{st:?}");
        }

        #[tokio::test]
        async fn a_server_that_cannot_be_spawned_is_a_row_not_an_error() {
            let ghost = ServerConfig::stdio("ghost", "/definitely/not/installed", Vec::new(), BTreeMap::new());
            let m = McpManager::connect(vec![ghost, cfg("ok", "", &["t"])], ManagerOptions::default()).await;
            let st = m.status();
            assert!(!st[0].connected && st[1].connected, "{st:?}");
            assert_eq!(m.notices().len(), 1);
            m.shutdown().await;
        }

        /// An HTTP server has no pid: it is a failed row beside the
        /// working stdio one, and nothing about it (header values
        /// included) reaches the status or the notices. The address is a
        /// closed local port so the test stays off the network.
        #[tokio::test]
        async fn an_http_server_is_a_failed_row_and_leaks_nothing() {
            let remote = ServerConfig::http(
                "remote",
                "http://user:hunter2@127.0.0.1:1/mcp?api_key=abc123",
                BTreeMap::from([("Authorization".to_string(), "Bearer s3cret".to_string())]),
            );
            let m = McpManager::connect(vec![remote, cfg("ok", "", &["t"])], ManagerOptions::default()).await;
            let st = m.status();
            assert!(!st[0].connected && st[1].connected, "{st:?}");
            assert!(st[0].reason.as_deref().unwrap().contains("cannot connect"), "{st:?}");
            assert_eq!(m.pids().len(), 1, "only the stdio server has a pid");
            let shown = format!("{st:?} {:?}", m.notices());
            for secret in ["s3cret", "hunter2", "abc123"] {
                assert!(!shown.contains(secret), "{secret} leaked: {shown}");
            }
            m.kill_now();
            m.shutdown().await;
        }

        /// Sanitising merges `a.b` and `a_b`; the second must be
        /// skipped with a notice, never replace the first and never
        /// abort startup.
        #[tokio::test]
        async fn colliding_tools_keep_the_first_and_report_the_rest() {
            let m = McpManager::connect(
                vec![cfg("s", "", &["a.b", "a_b", "A_B", "ok", "ok"])],
                ManagerOptions::default(),
            )
            .await;
            let mut r = ToolRegistry::new();
            assert!(m.register_into(&mut r).is_empty());
            assert_eq!(r.names(), vec!["mcp__s__a_b", "mcp__s__ok"]);
            assert_eq!(m.status()[0].tool_count, 2);
            assert_eq!(m.notices().len(), 1, "{:?}", m.notices());
            assert!(m.notices()[0].contains("skipped 3"), "{:?}", m.notices());
            m.shutdown().await;
        }

        #[tokio::test]
        async fn collisions_across_servers_are_caught_too() {
            // `mcp__a__b__c` from server a / tool b__c and from server
            // `a__b` / tool c.
            let m = McpManager::connect(
                vec![cfg("a", "", &["b__c"]), cfg("a__b", "", &["c"])],
                ManagerOptions::default(),
            )
            .await;
            assert_eq!(m.tool_count(), 1);
            assert_eq!(m.notices().len(), 1);
            m.shutdown().await;
        }

        /// The synchronous last resort, for exits that cannot await:
        /// the whole group goes, including what the server backgrounded.
        #[tokio::test]
        async fn kill_now_reaps_the_server_and_its_helper_without_awaiting() {
            let server_pid = tmp_file("ksrv");
            let helper_pid = tmp_file("khelper");
            let preamble = format!(
                "echo $$ > {}; sleep 300 & echo $! > {}",
                server_pid.display(),
                helper_pid.display()
            );
            let m = McpManager::connect(vec![cfg("s", &preamble, &["t"])], ManagerOptions::default()).await;
            let (sp, hp) = (read_pid(&server_pid), read_pid(&helper_pid));

            // Dropping the guard is the whole call.
            drop(Arc::new(m).kill_on_drop());

            let until = StdInstant::now() + Duration::from_secs(10);
            while alive(sp) || alive(hp) {
                assert!(StdInstant::now() < until, "server {sp} / helper {hp} survived kill_now");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[tokio::test]
        async fn shutdown_kills_the_server_and_what_it_backgrounded() {
            let server_pid = tmp_file("srv");
            let helper_pid = tmp_file("helper");
            let preamble = format!(
                "echo $$ > {}; sleep 300 & echo $! > {}",
                server_pid.display(),
                helper_pid.display()
            );
            let m = McpManager::connect(vec![cfg("s", &preamble, &["t"])], ManagerOptions::default()).await;
            let (sp, hp) = (read_pid(&server_pid), read_pid(&helper_pid));
            assert!(alive(sp) && alive(hp));
            assert_eq!(m.pids(), vec![sp]);

            m.shutdown().await;
            // `shutdown` reaps before returning; no waiting needed
            // beyond the zombie being collected by init.
            let until = StdInstant::now() + Duration::from_secs(5);
            while alive(sp) || alive(hp) {
                assert!(StdInstant::now() < until, "server {sp} / helper {hp} survived shutdown");
                std::thread::sleep(Duration::from_millis(20));
            }
            // Idempotent, and status reflects it.
            m.shutdown().await;
            assert!(!m.status()[0].connected);
        }
    }
}

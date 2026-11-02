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
use crate::mcp::config::{self, redact_url, ServerConfig, ServerSpec};
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
/// `npx` cold start — option B, which `docs/mcp.md` rejects by name.
/// Until the parent↔child channel exists (M9) a child simply has no MCP
/// tools, and says so nowhere: a scripted child is not a place for
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

/// One server as `/mcp` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    /// What is being run, with secrets masked. See [`endpoint_of`].
    pub endpoint: String,
    pub connected: bool,
    pub tool_count: usize,
    /// First line of why it is not (fully) working, control characters
    /// removed and length capped. Never the full reason: that can be
    /// ~12 KB of stderr, and `ConnectionState::Failed` keeps all of it
    /// for the one place that wants it.
    pub reason: Option<String>,
}

struct Server {
    name: String,
    endpoint: String,
    /// `None` when the server never produced a client: it timed out
    /// before `connect` returned, and the in-flight attempt was dropped.
    client: Option<Arc<McpClient>>,
    tool_count: usize,
    /// Why the server is not fully usable *as of startup*. Live
    /// failures after that come from the client's own state.
    startup_reason: Option<String>,
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
        }
    }

    /// The one startup call: decide whether this session connects, load
    /// `mcp.json`, connect, and report problems with `note!`.
    ///
    /// Returns an `Arc` because the TUI's `App` and every later
    /// registry rebuild share it.
    pub async fn start_for_session(kind: SessionKind, tools_allow: &[String]) -> Arc<Self> {
        if skip_reason(kind, tools_allow).is_some() {
            return Arc::new(Self::empty());
        }

        let loaded = config::load_global();
        config::print_diagnostics(&loaded.diagnostics);
        if loaded.servers.is_empty() {
            return Arc::new(Self::empty());
        }

        if kind == SessionKind::Tui {
            crate::note!(
                "nanopi: connecting to {} MCP server(s)…",
                loaded.servers.len()
            );
        }
        let manager = Self::connect(loaded.servers, ManagerOptions::default()).await;
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
            let BringUp { client, tools, reason } = result;

            let mut skipped: Vec<String> = Vec::new();
            let mut kept = 0usize;
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
                    taken.insert(name, format!("{:?} of \"{}\"", def.name, cfg.name));
                    manager
                        .tools
                        .push(Arc::new(McpTool::new(client.clone(), endpoint.clone(), def)));
                    kept += 1;
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
                tool_count: kept,
                startup_reason: reason,
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

    /// Per-server status for `/mcp`, in config order. Read live from
    /// each client, so a server that died since startup shows as failed.
    pub fn status(&self) -> Vec<ServerStatus> {
        self.servers
            .iter()
            .map(|s| {
                let (connected, reason) = match &s.client {
                    Some(client) => match client.state() {
                        ConnectionState::Ready => (true, s.startup_reason.clone()),
                        ConnectionState::Connecting => {
                            (false, Some("still connecting".to_string()))
                        }
                        ConnectionState::Failed { reason } => {
                            (false, Some(first_line(&reason, DETAIL_CHARS)))
                        }
                    },
                    None => (false, s.startup_reason.clone()),
                };
                ServerStatus {
                    name: s.name.clone(),
                    endpoint: s.endpoint.clone(),
                    connected,
                    tool_count: s.tool_count,
                    reason,
                }
            })
            .collect()
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

    /// SIGKILL every server's process group, synchronously. The last
    /// resort for exits that cannot `await` [`shutdown`](Self::shutdown):
    /// a dropped future, an early `?` return, a signal handler.
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
        #[cfg(unix)]
        for pid in self
            .servers
            .iter()
            .filter_map(|s| s.client.as_ref())
            .filter(|c| c.state() == ConnectionState::Ready)
            .filter_map(|c| c.pid())
        {
            if pid > 1 {
                // SAFETY: a plain syscall; no memory is touched.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
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
                        first_line(&e.to_string(), DETAIL_CHARS)
                    )),
                ),
            },
            ConnectionState::Failed { reason } => {
                let reason = first_line(&reason, DETAIL_CHARS);
                (client, Vec::new(), Some(reason))
            }
            ConnectionState::Connecting => (
                client,
                Vec::new(),
                Some("still connecting".to_string()),
            ),
        }
    };

    match tokio::time::timeout_at(deadline, attempt).await {
        Ok((client, tools, reason)) => BringUp { client: Some(client), tools, reason },
        // The attempt is dropped here, which is what reaps the child.
        // The message carries the cause people actually hit: a cold
        // `npx` cache.
        Err(_) => BringUp {
            client: None,
            tools: Vec::new(),
            reason: Some(format!(
                "did not finish starting within {}s and was skipped for this session \
                 (a first `npx`/`uvx` run downloads the package; run the command once \
                 by hand to warm its cache)",
                budget.as_secs()
            )),
        },
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
/// An HTTP server shows its URL through [`redact_url`] and nothing else:
/// `headers` carry the bearer token, and even their names are left out
/// until there is a reason to show them.
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

/// Mask a URL's userinfo (`scheme://user:pass@host` → `scheme://***@host`)
/// and the value of a `KEY=value` argument whose key names a credential.
fn redact_arg(arg: &str) -> String {
    if let Some(i) = arg.find("://") {
        let (head, rest) = arg.split_at(i + 3);
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            return format!("{head}***@{}", &rest[at + 1..]);
        }
    }
    if let Some((key, _)) = arg.split_once('=') {
        let lower = key.to_ascii_lowercase();
        const SECRETISH: [&str; 7] =
            ["token", "secret", "password", "passwd", "key", "auth", "credential"];
        if SECRETISH.iter().any(|w| lower.contains(w)) {
            return format!("{key}=***");
        }
    }
    arg.to_string()
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

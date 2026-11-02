//! MCP client: the initialize handshake, `tools/list`, `tools/call`, and
//! connection state, over a [`Transport`].
//!
//! The transport knows how to move JSON-RPC and nothing about MCP, so it
//! will happily send `tools/list` to a server that was never initialised
//! — which the spec forbids and many servers answer by erroring or
//! hanging. This layer is where the protocol's *ordering* is enforced and
//! where "is this server usable" has one answer.
//!
//! **Failure is a state, not an error.** [`McpClient::connect`] is
//! infallible by signature. A server that is not installed, 404s out of
//! `npx`, hangs, or speaks a protocol we cannot is the ordinary case on
//! a user's machine, and none of them may stop nanopi starting or hide
//! the other servers. The reason is kept in [`ConnectionState::Failed`],
//! together with whatever the server wrote to stderr: an `npx` 404 only
//! ever appears there, and the process is gone by the time anyone asks.
//!
//! **State must not lie.** It is derived from the transport on every
//! read rather than cached from the last call, so a server that dies
//! between calls is reported `Failed` by the next [`McpClient::state`],
//! not `Ready` until somebody's tool call happens to trip over it. See
//! [`McpClient::state`].
//!
//! **No reconnect, on purpose.** Once the transport is closed it is
//! closed for good (`Transport::close_reason`), so a `Failed`
//! client is terminal. Recovering means building a new client from the
//! same `ServerConfig`; nothing here holds anything that would prevent
//! that, and the old client's `Drop` reaps its process group.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::mcp::config::ServerConfig;
use crate::mcp::protocol::{
    method, CallToolParams, CallToolResult, InitializeParams, InitializeResult, ListToolsParams,
    ListToolsResult, RpcError, Tool, PROTOCOL_VERSION,
};
use crate::mcp::transport::{Transport, TransportError};

/// Protocol revisions we accept in an `initialize` reply: the one we
/// ask for, plus the two before it.
///
/// The spec has the server answer with *its own* latest when it does not
/// support the version we asked for, and says the client SHOULD
/// disconnect if it cannot speak that. So the reply is a negotiation
/// outcome we must judge, not a formality. The test is "can this client
/// do its job correctly against that revision", not "is it identical":
///
/// - `2025-03-26` and `2024-11-05` are accepted because everything this
///   client sends (`initialize`, `tools/list`, `tools/call`) has the
///   same shape in all three, and `protocol.rs` parses leniently in
///   exactly the places the newer revision added fields (`title`,
///   `structuredContent`, `resource_link`, `outputSchema`) — an older
///   server simply omits them. Refusing them would lock out every
///   server whose newest revision predates ours, which is a large part
///   of the installed base, to protect against a difference that does
///   not exist for tools.
/// - Anything else is refused: an unknown *newer* revision may have
///   changed what a tool result means in ways we would silently
///   misread, and an unrecognised string is a server bug. We cannot
///   tell which, and "proceed and see" is how a tool call gets
///   misinterpreted without anyone noticing.
///
/// stdio carries no version header, so unlike Streamable HTTP nothing
/// else on the wire depends on the answer.
const SUPPORTED_VERSIONS: &[&str] = &[PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

/// Ceiling on `initialize`. Far above the ~2 s a *failing* request takes
/// to resolve (the transport waits for stderr to drain so the diagnosis
/// is not empty, see `StdioTransport::request_with_timeout`) — that wait
/// is a death being explained, not a hang, and a timeout shorter than it
/// would mask the real cause as "timed out". The headroom is for the
/// legitimate slow start: `npx -y` fetching a package on first use takes
/// tens of seconds, and a timeout that fires first would make the
/// server unusable until its cache is warm. This is also the worst-case
/// startup delay one dead server can cause.
pub const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on one `tools/list` page. Listing is a lookup, not work, so a
/// server that has not answered in this long is wedged.
pub const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default ceiling on `tools/call`; see [`ClientOptions::call_timeout`]
/// for who owns it and why this number.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Most pages `tools/list` will follow.
///
/// The cursor is opaque, so a server that mints a fresh one on every
/// page can never be caught by comparing cursors, and would otherwise
/// pin startup in a loop forever. 100 pages is thousands of tools —
/// far beyond what a model can usefully be shown — so reaching it means
/// the server is broken, not large.
const MAX_LIST_PAGES: usize = 100;

// ------------------------------------------------------------- options

/// Per-client tunables. A struct so M4 can grow a `timeout` field in
/// `mcp.json` and M6 can fill this from it without changing signatures.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Bound on the `initialize` request alone, so a hung server cannot
    /// stall startup. See [`DEFAULT_INITIALIZE_TIMEOUT`].
    pub initialize_timeout: Duration,
    /// Bound on each `tools/list` page.
    pub list_timeout: Duration,
    /// Default bound on `tools/call`.
    ///
    /// **Owned by the operator, not by the model.** Tools legitimately
    /// run long — a database query, a build, a browser step — so no
    /// single number is right for every tool on every server. It is
    /// therefore a per-server setting (this field, filled from
    /// config by M6) with a per-call override
    /// ([`McpClient::call_tool_with_timeout`]) for a caller that
    /// knows better, such as an adapter with a per-tool setting. It is
    /// deliberately *not* an argument the model can pass: the model
    /// controls tool arguments, which are untrusted input, and letting
    /// it pick its own timeout lets one call park a turn indefinitely.
    ///
    /// The default is 120 s, twice the transport's 60 s generic
    /// default. A timeout that fires on a slow-but-working tool looks to
    /// the model exactly like a broken one and prompts a retry that
    /// repeats the work, so the bias is toward waiting; it is not
    /// unbounded because a wedged server must still release the turn.
    /// A timeout does not cancel the call server-side — see the note on
    /// [`McpClient::call_tool_with_timeout`].
    pub call_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            initialize_timeout: DEFAULT_INITIALIZE_TIMEOUT,
            list_timeout: DEFAULT_LIST_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

// --------------------------------------------------------------- state

/// Where a connection is in its life.
///
/// There is no `Disconnected`: a closed transport is a failure with a
/// reason (even a deliberate shutdown, whose reason says so), and a
/// fourth state would be one more case for `/mcp` and the adapter to
/// get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// The handshake is in flight. `connect` does not return until it
    /// resolves, so this is the state calls are refused in while it
    /// runs, not one a caller of `connect` receives.
    Connecting,
    Ready,
    /// Terminal. `reason` is a one-line headline, then (when the server
    /// printed anything) a newline and its stderr tail — so the first
    /// line is suitable for a status row and the whole string for a
    /// diagnostic. Stored as a `String` because `TransportError` is
    /// not `Clone`.
    Failed { reason: String },
}

impl ConnectionState {
    /// The one line worth putting in front of a model or in a table.
    fn headline(&self) -> String {
        match self {
            ConnectionState::Connecting => "still connecting".to_string(),
            ConnectionState::Ready => "ready".to_string(),
            ConnectionState::Failed { reason } => {
                format!("failed: {}", reason.lines().next().unwrap_or(""))
            }
        }
    }
}

// -------------------------------------------------------------- errors

/// Why a client call failed.
///
/// [`ClientError::Rpc`] stays separate from [`ClientError::Transport`]
/// for the reason `TransportError::Rpc` does: the server is working and
/// refusing the request, versus the connection being unusable. And
/// *neither* is a tool that ran and failed — that is `Ok` with
/// [`CallToolResult::is_error`] set, which the model is meant to read
/// and react to.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Refused locally; nothing was sent. The headline only — the full
    /// stderr is in [`McpClient::state`], and does not belong in a
    /// message that ends up in front of a model.
    #[error("MCP server \"{server}\" is not ready ({state})")]
    NotReady { server: String, state: String },
    /// The server answered with a JSON-RPC error.
    #[error("{0}")]
    Rpc(RpcError),
    /// The request did not complete: timeout, dead pipe, I/O.
    #[error("{0}")]
    Transport(TransportError),
    /// The server answered, but not with something this protocol allows.
    #[error("{0}")]
    Protocol(String),
}

// -------------------------------------------------------------- client

/// One MCP server, handshaken and ready to list and call tools — or
/// carrying the reason it is not.
///
/// All methods take `&self`, so M5's `McpTool`s can share one client in
/// an `Arc` and call concurrently over the transport's single pipe.
#[derive(Debug)]
pub struct McpClient {
    name: String,
    /// `None` only when no connection could be made at all (the process
    /// would not spawn, or the transport is unavailable). Kept
    /// after a handshake failure too (already shut down) so `pid()` and
    /// `diagnostics()` still answer for a `Failed` server.
    transport: Option<Transport>,
    state: Mutex<ConnectionState>,
    /// Immutable after `connect`: the server's identity and capabilities
    /// do not change mid-session. `None` iff the handshake never
    /// completed.
    handshake: Option<InitializeResult>,
    /// Single-flight bookkeeping for [`McpClient::request`]'s recovery
    /// from an expired HTTP session. Never touched over stdio.
    recovery: SessionRecovery,
    options: ClientOptions,
}

/// How many times the session has been re-established, and how the last
/// attempt went.
///
/// A request notes `epoch` *before* it is sent. If it is refused because
/// the session is gone, whoever then holds `last` knows from the epoch
/// whether the session was already replaced since this request went out
/// (then only a retry is due) or not (then this request does the
/// re-`initialize`). That is what turns N concurrent `404`s into one
/// handshake: the first takes the lock and does it, the other N-1 queue on
/// the lock and find the epoch moved.
#[derive(Debug, Default)]
struct SessionRecovery {
    epoch: AtomicU64,
    /// The outcome of the attempt that produced the current `epoch`
    /// (`None` before the first). A failure is shared, not retried by each
    /// waiter: N requests queued behind a handshake that just timed out
    /// would otherwise take N timeouts to fail.
    last: tokio::sync::Mutex<Option<Result<(), String>>>,
}

impl McpClient {
    /// Spawn the server and run the handshake, with default options.
    ///
    /// Always returns a client; check [`state`](Self::state). It is not
    /// a `Result` because a server that will not start must not abort
    /// startup — see the module docs. Dropping the returned future
    /// mid-handshake (an outer startup deadline) still reaps the child,
    /// via the transport's `ChildGuard`.
    pub async fn connect(config: &ServerConfig) -> Self {
        Self::connect_with(config, ClientOptions::default()).await
    }

    pub async fn connect_with(config: &ServerConfig, options: ClientOptions) -> Self {
        let transport = match Transport::connect(config).await {
            Ok(t) => t,
            Err(e) => {
                return Self {
                    name: config.name.clone(),
                    transport: None,
                    state: Mutex::new(ConnectionState::Failed { reason: e.to_string() }),
                    handshake: None,
                    recovery: SessionRecovery::default(),
                    options,
                };
            }
        };

        match handshake(&transport, options.initialize_timeout).await {
            Ok(result) => Self {
                name: config.name.clone(),
                transport: Some(transport),
                state: Mutex::new(ConnectionState::Ready),
                handshake: Some(result),
                recovery: SessionRecovery::default(),
                options,
            },
            Err(headline) => {
                // Read before `shutdown`: the reason is captured while
                // the transport still has its buffers, and `shutdown`
                // is what stops a hung or version-refused server from
                // living on as a stray process behind a `Failed` row.
                let reason = with_diagnostics(headline, &transport);
                transport.shutdown().await;
                Self {
                    name: config.name.clone(),
                    transport: Some(transport),
                    state: Mutex::new(ConnectionState::Failed { reason }),
                    handshake: None,
                    recovery: SessionRecovery::default(),
                    options,
                }
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current state, observed rather than remembered.
    ///
    /// **A transport that died mid-session flips this to `Failed`**,
    /// whether or not anything was in flight. The alternative — leave
    /// the state alone and let the next call error — makes `/mcp` show
    /// `Ready` for a server that has been dead for an hour, and makes
    /// the adapter's "is it worth trying" check a lie. The cost is that
    /// this takes a lock and reads the transport on every call; both
    /// are trivial next to a process round-trip. A timeout is *not*
    /// death — a slow tool leaves the client `Ready`.
    pub fn state(&self) -> ConnectionState {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, ConnectionState::Ready) {
            if let Some(transport) = &self.transport {
                if let Some(reason) = transport.close_reason() {
                    *state = ConnectionState::Failed {
                        reason: with_diagnostics(reason, transport),
                    };
                }
            }
        }
        state.clone()
    }

    /// The server's `initialize` reply — identity, capabilities,
    /// `instructions`. `None` unless the handshake succeeded.
    pub fn initialize_result(&self) -> Option<&InitializeResult> {
        self.handshake.as_ref()
    }

    /// The revision the server answered with and we accepted.
    pub fn protocol_version(&self) -> Option<&str> {
        self.handshake.as_ref().map(|h| h.protocol_version.as_str())
    }

    pub fn pid(&self) -> Option<u32> {
        self.transport.as_ref().and_then(Transport::pid)
    }

    /// Everything the server printed outside the protocol, for `/mcp`.
    pub fn diagnostics(&self) -> String {
        self.transport
            .as_ref()
            .map(Transport::diagnostics)
            .unwrap_or_default()
    }

    /// Every tool the server offers, following `nextCursor` to the end.
    ///
    /// **A server that does not advertise the `tools` capability yields
    /// an empty list, without sending anything.** Not an error: a server
    /// offering only prompts or resources is healthy and conformant, and
    /// `/mcp` would otherwise show it as broken. And not a probe: the
    /// spec makes the capability the gate for the method, a server is
    /// entitled to treat an undeclared method as a protocol violation
    /// (some close the connection on one), so asking can take down a
    /// working session while not asking can at worst miss the tools of
    /// a server that forgot to declare them. `call_tool` is *not* gated
    /// the same way — a caller that has a name got it from somewhere,
    /// and the server is the authority on its own methods; if it refuses
    /// the call, that arrives as an [`ClientError::Rpc`].
    ///
    /// Any failure discards the partial list. A silently truncated tool
    /// set is worse than none: the model would not know it was missing
    /// anything. A pagination fault (a repeated cursor, or more than
    /// [`MAX_LIST_PAGES`] pages) is [`ClientError::Protocol`] and does
    /// not mark the server `Failed` — the connection is fine, only this
    /// listing is not trustworthy.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, ClientError> {
        self.ready_transport()?;
        let advertised = self
            .handshake
            .as_ref()
            .is_some_and(|h| h.capabilities.tools.is_some());
        if !advertised {
            return Ok(Vec::new());
        }

        let mut tools = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            // `{}` rather than omitted params on the first page: legal
            // either way, but some servers dereference `params`.
            let params = serde_json::to_value(ListToolsParams { cursor: cursor.clone() })
                .map_err(|e| ClientError::Transport(TransportError::Encode(e)))?;
            let page: ListToolsResult = self
                .request(method::TOOLS_LIST, params, self.options.list_timeout)
                .await?;
            tools.extend(page.tools);

            let Some(next) = page.next_cursor else {
                return Ok(tools);
            };
            // The cheap, immediate catch for the common bug (a server
            // that ignores the cursor and returns page one forever);
            // `MAX_LIST_PAGES` is the backstop for cursors that differ.
            if !seen.insert(next.clone()) {
                return Err(ClientError::Protocol(format!(
                    "server \"{}\" returned `nextCursor` {next:?} twice in `tools/list`; \
                     refusing to page forever",
                    self.name
                )));
            }
            cursor = Some(next);
        }
        Err(ClientError::Protocol(format!(
            "server \"{}\" still had more `tools/list` pages after {MAX_LIST_PAGES}; \
             refusing to page forever",
            self.name
        )))
    }

    /// Call a tool under the client's default timeout.
    ///
    /// `Err` means the *call* could not be completed or was refused at
    /// the protocol level (unknown tool, bad arguments → `-32602`).
    /// `Ok` with `is_error: true` means the tool ran and reported its
    /// own failure — kept apart because the second is content the model
    /// should read and recover from, the first is nanopi's problem.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<CallToolResult, ClientError> {
        self.call_tool_with_timeout(name, arguments, self.options.call_timeout)
            .await
    }

    /// [`call_tool`](Self::call_tool) with an explicit bound.
    ///
    /// On timeout the server is *not* told to stop: the spec's
    /// `notifications/cancelled` needs the request id, which the
    /// transport does not expose for a request that failed. So a
    /// timed-out tool may keep running, and its eventual reply is
    /// recorded by the transport as a reply to an unknown id.
    pub async fn call_tool_with_timeout(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        timeout: Duration,
    ) -> Result<CallToolResult, ClientError> {
        let params = serde_json::to_value(CallToolParams::new(name, arguments))
            .map_err(|e| ClientError::Transport(TransportError::Encode(e)))?;
        self.request(method::TOOLS_CALL, params, timeout).await
    }

    /// Stop the server and reap it, including anything it backgrounded.
    /// Idempotent. Afterwards [`state`](Self::state) is `Failed` with
    /// the transport's "was shut down" reason, which is accurate.
    pub async fn shutdown(&self) {
        if let Some(transport) = &self.transport {
            transport.shutdown().await;
        }
    }

    /// The enforcement point for "no `tools/*` before `initialize`
    /// completes". The transport has no notion of a handshake, so
    /// without this a caller could send a request a server has not yet
    /// agreed to serve.
    fn ready_transport(&self) -> Result<&Transport, ClientError> {
        let state = self.state();
        match (&state, &self.transport) {
            (ConnectionState::Ready, Some(transport)) => Ok(transport),
            _ => Err(ClientError::NotReady {
                server: self.name.clone(),
                state: state.headline(),
            }),
        }
    }

    /// Shut down from a context that cannot `await`; see
    /// [`McpManager::kill_now`](crate::mcp::manager::McpManager::kill_now).
    /// The receiver yields when the best-effort HTTP `DELETE` is over.
    pub fn abandon(&self) -> Option<std::sync::mpsc::Receiver<()>> {
        self.transport.as_ref().and_then(Transport::abandon)
    }

    /// Send one request, recovering once from an expired HTTP session.
    ///
    /// **A `404` on a request that carried a session id means the server
    /// dropped the session and did not run the request** (the transport
    /// only reports [`TransportError::SessionExpired`] for exactly that).
    /// The spec's remedy is a fresh `initialize`, after which the original
    /// request is sent again — once. This is safe for that one status and
    /// for no other: a timeout, a `5xx` or a cut stream may follow a
    /// `tools/call` that *did* run, and repeating it would run it twice. A
    /// second `SessionExpired` is returned, not looped on: a server that
    /// refuses the session it just issued is broken, not forgetful.
    ///
    /// **The tool table is deliberately not rebuilt.** The `McpTool`s the
    /// model was shown were fixed at startup and a re-`initialize` does
    /// not change them; if the restarted server now offers different
    /// tools, a call to a vanished one comes back as its own JSON-RPC
    /// error and a new one stays invisible until the next nanopi session.
    /// Likewise [`initialize_result`](Self::initialize_result) keeps the
    /// first handshake's capabilities. Reconciling would mean tools
    /// appearing and disappearing under a running turn.
    async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<T, ClientError> {
        let transport = self.ready_transport()?;
        // Before sending: see `SessionRecovery`.
        let epoch = self.recovery.epoch.load(Ordering::Acquire);
        // Only HTTP has a session to lose; do not copy arguments that may
        // be megabytes for a transport that can never use the copy.
        let spare = matches!(transport, Transport::Http(_)).then(|| params.clone());
        let sent = transport
            .request_with_timeout(method, Some(params), timeout)
            .await;
        let raw = match (sent, spare) {
            (Err(TransportError::SessionExpired), Some(params)) => {
                self.recover_session(transport, epoch).await?;
                transport
                    .request_with_timeout(method, Some(params), timeout)
                    .await
                    .map_err(|e| self.classify(e))?
            }
            (other, _) => other.map_err(|e| self.classify(e))?,
        };
        // Strict here, unlike the lenient content blocks inside: a
        // result that is not even the right envelope cannot be mapped
        // to anything, and pretending otherwise would hand the model an
        // empty success.
        serde_json::from_value(raw)
            .map_err(|e| ClientError::Protocol(format!("malformed `{method}` result: {e}")))
    }

    /// Make sure the session is newer than `seen_epoch`, re-running the
    /// handshake only if nobody else has. `Ok` means "send again".
    ///
    /// The stale session id is left in place until `initialize` replaces
    /// it (which sends no id of its own), rather than cleared first:
    /// requests that start during the handshake then fail with a `404` we
    /// know how to recover from, instead of an id-less request the server
    /// refuses with a `400` we do not.
    async fn recover_session(
        &self,
        transport: &Transport,
        seen_epoch: u64,
    ) -> Result<(), ClientError> {
        let mut last = self.recovery.last.lock().await;
        if self.recovery.epoch.load(Ordering::Acquire) == seen_epoch {
            let outcome = handshake(transport, self.options.initialize_timeout).await;
            *last = Some(outcome.as_ref().map(|_| ()).map_err(String::clone));
            self.recovery.epoch.fetch_add(1, Ordering::Release);
            if let Err(headline) = outcome {
                return Err(self.reinitialize_failed(transport, headline));
            }
        }
        match last.as_ref() {
            Some(Err(headline)) => Err(self.reinitialize_failed(transport, headline.clone())),
            _ => Ok(()),
        }
    }

    fn reinitialize_failed(&self, transport: &Transport, headline: String) -> ClientError {
        // Shut down underneath us: that is the real answer, not "failed to
        // re-initialize".
        if let Some(reason) = transport.close_reason() {
            return self.classify(TransportError::Closed(reason));
        }
        // `Disconnected`, not `Closed`: the transport is intact, and the
        // next call may find the server back and recover.
        ClientError::Transport(TransportError::Disconnected(format!(
            "the server dropped the session and re-initializing failed: {headline}"
        )))
    }

    fn classify(&self, error: TransportError) -> ClientError {
        match error {
            TransportError::Rpc(e) => ClientError::Rpc(e),
            TransportError::Closed(reason) => {
                // `state()` normally learns of a death from the
                // transport's own close flag, but a failed *write* can
                // report `Closed` while that flag is still unset (the
                // reader had not yet observed the exit). Returning the
                // error without recording it would leave state `Ready`
                // beside a call that just said the server is gone.
                let mut state = self.state.lock().unwrap();
                if matches!(*state, ConnectionState::Ready) {
                    *state = ConnectionState::Failed {
                        reason: match &self.transport {
                            Some(t) => with_diagnostics(reason.clone(), t),
                            None => reason.clone(),
                        },
                    };
                }
                ClientError::Transport(TransportError::Closed(reason))
            }
            other => ClientError::Transport(other),
        }
    }
}

// ----------------------------------------------------------- handshake

/// `initialize`, judge the answer, then `notifications/initialized`.
/// `Err` is the failure headline; the caller attaches diagnostics.
async fn handshake(
    transport: &Transport,
    timeout: Duration,
) -> Result<InitializeResult, String> {
    let params = serde_json::to_value(InitializeParams::new("nanopi", env!("CARGO_PKG_VERSION")))
        .map_err(|e| format!("encoding `initialize`: {e}"))?;
    let raw = transport
        .request_with_timeout(method::INITIALIZE, Some(params), timeout)
        .await
        .map_err(|e| format!("`initialize` failed: {e}"))?;
    let result: InitializeResult = serde_json::from_value(raw)
        .map_err(|e| format!("server sent a malformed `initialize` result: {e}"))?;

    // Checked before `initialized` is sent: that notification tells the
    // server we are proceeding, and we are not.
    if !SUPPORTED_VERSIONS.contains(&result.protocol_version.as_str()) {
        return Err(format!(
            "server speaks MCP protocol version {:?}; nanopi supports {}",
            result.protocol_version,
            SUPPORTED_VERSIONS.join(", ")
        ));
    }

    // Before `initialized`: that notification is already a request the
    // server may check the version header on.
    transport.set_protocol_version(&result.protocol_version);
    transport
        .notify(method::INITIALIZED, None)
        .await
        .map_err(|e| format!("sending `notifications/initialized` failed: {e}"))?;
    Ok(result)
}

/// Append the server's stderr tail to a one-line headline.
///
/// This is the whole reason a failed `npx` server is debuggable: the
/// transport's own error says "exited with code 1", and the 404 that
/// caused it exists only in what the process printed.
fn with_diagnostics(headline: String, transport: &Transport) -> String {
    let diagnostics = transport.diagnostics();
    let diagnostics = diagnostics.trim_end();
    if diagnostics.is_empty() {
        headline
    } else {
        format!("{headline}\n{diagnostics}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn config(script: &str) -> ServerConfig {
        ServerConfig::stdio("fake", "sh", vec!["-c".to_string(), script.to_string()], BTreeMap::new())
    }

    /// Short timeouts so a hung server costs a test a fraction of a
    /// second; every non-hanging test is bounded by the transport's own
    /// death detection instead.
    fn quick() -> ClientOptions {
        ClientOptions {
            initialize_timeout: Duration::from_secs(10),
            list_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(10),
        }
    }

    /// A shell fragment that answers `initialize` with `@VERSION@`
    /// substituted and then consumes `notifications/initialized`.
    /// Rejects the request if we did not ask for `2025-06-18`, so a
    /// regression in the version we send fails the handshake test.
    fn after_handshake(version: &str, body: &str) -> String {
        format!(
            r#"
            IFS= read -r line
            id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            case "$line" in
                *'"protocolVersion":"2025-06-18"'*) ;;
                *) printf '{{"jsonrpc":"2.0","id":%s,"error":{{"code":-32602,"message":"wrong version"}}}}\n' "$id"; exit 9 ;;
            esac
            printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1.0"}}}}}}\n' "$id"
            IFS= read -r line
            {body}
            "#
        )
    }

    /// A full fake: answers every method (`@VERSION@` is substituted), pages `tools/list` over three
    /// cursors, and reports in `tools/call` whether it had seen
    /// `notifications/initialized` — proving the ordering end to end.
    const SERVER_LOOP: &str = r#"
        inited=0
        while IFS= read -r line; do
            id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            case "$line" in
                *'"method":"initialize"'*)
                    case "$line" in
                        *'"protocolVersion":"2025-06-18"'*) ;;
                        *) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"wrong version"}}\n' "$id"; exit 9 ;;
                    esac
                    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"@VERSION@","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1.0"}}}\n' "$id" ;;
                *'"method":"notifications/initialized"'*) inited=1 ;;
                *'"method":"tools/list"'*)
                    cursor=$(printf '%s' "$line" | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
                    case "$cursor" in
                        "") printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"a","inputSchema":{"type":"object"}},{"name":"b","inputSchema":{"type":"object"}}],"nextCursor":"p2"}}\n' "$id" ;;
                        p2) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"c","inputSchema":{"type":"object"}},{"name":"d","inputSchema":{"type":"object"}}],"nextCursor":"p3"}}\n' "$id" ;;
                        *) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"e","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
                    esac ;;
                *'"method":"tools/call"'*)
                    case "$line" in
                        *'"name":"boom"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"kaput"}],"isError":true}}\n' "$id" ;;
                        *'"name":"nope"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"Unknown tool: nope"}}\n' "$id" ;;
                        *) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"inited=%s"}]}}\n' "$id" "$inited" ;;
                    esac ;;
            esac
        done
    "#;

    fn full_server(version: &str) -> String {
        SERVER_LOOP.replace("@VERSION@", version)
    }

    #[cfg(unix)]
    fn group_alive(pgid: u32) -> bool {
        unsafe { libc::killpg(pgid as libc::pid_t, 0) == 0 }
    }

    /// Every test that spawned a process ends here: shut down, then
    /// require the whole group — server and anything it backgrounded —
    /// to be gone.
    #[cfg(unix)]
    async fn assert_no_stray_process(client: McpClient) {
        let pid = client.pid().expect("a spawned server has a pid");
        client.shutdown().await;
        wait_for_group_exit(pid).await;
    }

    #[cfg(unix)]
    async fn wait_for_group_exit(pid: u32) {
        for _ in 0..200 {
            if !group_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("process group {pid} survived");
    }

    fn failed_reason(client: &McpClient) -> String {
        match client.state() {
            ConnectionState::Failed { reason } => reason,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_handshake_completes_end_to_end() {
        let client = McpClient::connect_with(&config(&full_server("2025-06-18")), quick()).await;

        assert_eq!(client.state(), ConnectionState::Ready);
        assert_eq!(client.protocol_version(), Some("2025-06-18"));
        let info = &client.initialize_result().unwrap().server_info;
        assert_eq!(info.name, "fake");

        // The server only says `inited=1` if `initialized` reached it
        // before this call — the ordering the spec requires.
        let result = client.call_tool("echo", Map::new()).await.unwrap();
        assert!(!result.is_error);
        match &result.content[..] {
            [crate::mcp::protocol::ContentBlock::Text { text, .. }] => {
                assert_eq!(text, "inited=1")
            }
            other => panic!("unexpected content {other:?}"),
        }

        assert_no_stray_process(client).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_older_supported_version_is_accepted() {
        let client = McpClient::connect_with(&config(&full_server("2025-03-26")), quick()).await;
        assert_eq!(client.state(), ConnectionState::Ready);
        assert_eq!(client.protocol_version(), Some("2025-03-26"));
        assert_no_stray_process(client).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unsupported_version_is_refused_and_the_server_stopped() {
        // `sleep` backgrounded so a lingering group would be visible.
        let script = format!("sleep 120 & {}", after_handshake("2099-01-01", "sleep 120"));
        let client = McpClient::connect_with(&config(&script), quick()).await;

        let reason = failed_reason(&client);
        assert!(reason.contains("2099-01-01"), "{reason}");
        assert!(reason.contains("2025-06-18"), "should say what we do speak: {reason}");
        assert!(matches!(
            client.list_tools().await,
            Err(ClientError::NotReady { .. })
        ));
        // `connect` itself must have stopped it — a refused server is
        // not left running behind a `Failed` row.
        wait_for_group_exit(client.pid().unwrap()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_paginated_list_returns_every_tool() {
        let client = McpClient::connect_with(&config(&full_server("2025-06-18")), quick()).await;

        let tools = client.list_tools().await.unwrap();
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d", "e"]);

        assert_no_stray_process(client).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_error_and_a_protocol_error_stay_distinct() {
        let client = McpClient::connect_with(&config(&full_server("2025-06-18")), quick()).await;

        // The tool ran and failed: `Ok`, flagged, readable by the model.
        let failed = client.call_tool("boom", Map::new()).await.unwrap();
        assert!(failed.is_error);

        // The protocol refused the call: `Err(Rpc)`, with the code kept.
        match client.call_tool("nope", Map::new()).await {
            Err(ClientError::Rpc(e)) => {
                assert_eq!(e.code, crate::mcp::protocol::error_code::INVALID_PARAMS)
            }
            other => panic!("expected an RPC error, got {other:?}"),
        }
        // Neither is a dead connection.
        assert_eq!(client.state(), ConnectionState::Ready);

        assert_no_stray_process(client).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_dies_in_initialize_ends_failed_with_stderr() {
        let client = McpClient::connect_with(
            &config("echo 'npm ERR! 404 Not Found: @scope/server' >&2; exit 1"),
            quick(),
        )
        .await;

        let reason = failed_reason(&client);
        assert!(reason.contains("exit code 1"), "lost the exit status: {reason}");
        assert!(reason.contains("npm ERR! 404"), "lost the stderr: {reason}");
        // Reachable as a state without anything aborting.
        assert!(matches!(
            client.call_tool("x", Map::new()).await,
            Err(ClientError::NotReady { .. })
        ));

        assert_no_stray_process(client).await;
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_spawned_is_failed_not_fatal() {
        let cfg = ServerConfig::stdio("fake", "definitely-not-a-program-xyz", Vec::new(), BTreeMap::new());
        let client = McpClient::connect_with(&cfg, quick()).await;

        let reason = failed_reason(&client);
        assert!(reason.contains("definitely-not-a-program-xyz"), "{reason}");
        assert!(client.pid().is_none());
        // Idempotent and harmless with no process behind it.
        client.shutdown().await;
    }

    /// A dead `http` server is an ordinary `Failed` row: no panic, no
    /// hang, a reason that says what could not be reached.
    #[tokio::test]
    async fn an_unreachable_http_server_ends_failed() {
        let cfg = ServerConfig::http("remote", "http://127.0.0.1:1/mcp", BTreeMap::new());
        let client = tokio::time::timeout(Duration::from_secs(5), McpClient::connect_with(&cfg, quick()))
            .await
            .expect("connect must not hang");

        let reason = failed_reason(&client);
        assert!(reason.contains("cannot connect to 127.0.0.1:1"), "{reason}");
        assert!(client.pid().is_none());
        assert!(matches!(
            client.list_tools().await,
            Err(ClientError::NotReady { .. })
        ));
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_answers_initialize_with_an_error_ends_failed() {
        let script = r#"
            IFS= read -r line
            id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"license expired"}}\n' "$id"
            echo 'checking license' >&2
            while IFS= read -r line; do :; done
        "#;
        let client = McpClient::connect_with(&config(script), quick()).await;

        let reason = failed_reason(&client);
        assert!(reason.contains("license expired"), "{reason}");
        assert_no_stray_process(client).await;
    }

    /// A hung server must cost startup its timeout and no more, and
    /// must not outlive the failure that the timeout declared.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_initialize_times_out_and_leaves_no_process() {
        let started = Instant::now();
        let client = McpClient::connect_with(
            &config("sleep 120 & while IFS= read -r line; do :; done"),
            ClientOptions {
                initialize_timeout: Duration::from_millis(300),
                ..quick()
            },
        )
        .await;

        let reason = failed_reason(&client);
        assert!(reason.contains("timed out"), "{reason}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        wait_for_group_exit(client.pid().unwrap()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_repeats_a_cursor_is_stopped_loudly() {
        let script = after_handshake(
            "2025-06-18",
            r#"while IFS= read -r line; do
                id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"t","inputSchema":{"type":"object"}}],"nextCursor":"same"}}\n' "$id"
            done"#,
        );
        let client = McpClient::connect_with(&config(&script), quick()).await;
        assert_eq!(client.state(), ConnectionState::Ready);

        match client.list_tools().await {
            Err(ClientError::Protocol(msg)) => assert!(msg.contains("same"), "{msg}"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
        // The listing was untrustworthy, the connection was not.
        assert_eq!(client.state(), ConnectionState::Ready);

        assert_no_stray_process(client).await;
    }

    /// Distinct cursors defeat the repeat check, so the page cap is
    /// what stands between a hostile server and an endless startup.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_minting_endless_cursors_hits_the_page_cap() {
        let script = after_handshake(
            "2025-06-18",
            r#"n=0
            while IFS= read -r line; do
                id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                n=$((n+1))
                printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[],"nextCursor":"c%s"}}\n' "$id" "$n"
            done"#,
        );
        let client = McpClient::connect_with(&config(&script), quick()).await;

        match client.list_tools().await {
            Err(ClientError::Protocol(msg)) => {
                assert!(msg.contains(&MAX_LIST_PAGES.to_string()), "{msg}")
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }

        assert_no_stray_process(client).await;
    }

    /// `tools/*` before `initialize` has completed is refused locally.
    /// Asserted on the wire, not just on the return value: the server
    /// logs every line it receives and must have received none.
    #[cfg(unix)]
    #[tokio::test]
    async fn calls_before_ready_are_refused_without_sending_anything() {
        let log = std::env::temp_dir().join(format!("nanopi-mcp-client-{}-notready", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let script = format!(
            "while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
            log.display()
        );
        let transport = Transport::connect(&config(&script)).await.unwrap();
        // Built directly: `connect` never hands out a `Connecting` client.
        let client = McpClient {
            name: "fake".to_string(),
            transport: Some(transport),
            state: Mutex::new(ConnectionState::Connecting),
            handshake: None,
            recovery: SessionRecovery::default(),
            options: quick(),
        };

        match client.list_tools().await {
            Err(ClientError::NotReady { state, .. }) => assert!(state.contains("connecting"), "{state}"),
            other => panic!("expected NotReady, got {other:?}"),
        }
        assert!(matches!(
            client.call_tool("x", Map::new()).await,
            Err(ClientError::NotReady { .. })
        ));

        // Generous: a request that *was* sent would have been logged in
        // microseconds.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        assert!(logged.is_empty(), "a request reached the server: {logged}");

        assert_no_stray_process(client).await;
    }

    /// Dies while idle: nobody is mid-call to notice, so the state has
    /// to be learned from the transport.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_dies_while_idle_is_reported_failed() {
        let script = after_handshake("2025-06-18", "echo 'segfault in plugin' >&2; exit 4");
        let client = McpClient::connect_with(&config(&script), quick()).await;
        assert!(matches!(
            client.state(),
            ConnectionState::Ready | ConnectionState::Failed { .. }
        ));

        let mut reason = None;
        for _ in 0..200 {
            if let ConnectionState::Failed { reason: r } = client.state() {
                reason = Some(r);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let reason = reason.expect("the death was never reflected in state");
        assert!(reason.contains("exit code 4"), "{reason}");
        assert!(reason.contains("segfault in plugin"), "{reason}");

        assert_no_stray_process(client).await;
    }

    /// Dies during a call: the call errors, and state agrees with it
    /// immediately rather than after some later observation.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_dies_mid_call_fails_the_call_and_the_state() {
        let script = after_handshake("2025-06-18", "IFS= read -r line; echo 'oom' >&2; exit 5");
        let client = McpClient::connect_with(&config(&script), quick()).await;
        assert_eq!(client.state(), ConnectionState::Ready);

        match client.call_tool("anything", Map::new()).await {
            Err(ClientError::Transport(TransportError::Closed(_))) => {}
            other => panic!("expected a closed transport, got {other:?}"),
        }
        let reason = failed_reason(&client);
        assert!(reason.contains("exit code 5"), "{reason}");
        assert!(reason.contains("oom"), "{reason}");
        // And the next call is refused locally, naming the cause.
        match client.call_tool("anything", Map::new()).await {
            Err(ClientError::NotReady { state, .. }) => assert!(state.contains("exit code 5"), "{state}"),
            other => panic!("expected NotReady, got {other:?}"),
        }

        assert_no_stray_process(client).await;
    }

    /// A slow tool is not a dead server.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_call_timeout_leaves_the_client_ready() {
        let script = after_handshake("2025-06-18", "while IFS= read -r line; do :; done");
        let client = McpClient::connect_with(&config(&script), quick()).await;

        let err = client
            .call_tool_with_timeout("slow", Map::new(), Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Transport(TransportError::Timeout { .. })), "{err:?}");
        assert_eq!(client.state(), ConnectionState::Ready);

        assert_no_stray_process(client).await;
    }

    /// Declared-nothing servers (prompts/resources only) are healthy.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_without_the_tools_capability_lists_nothing() {
        let script = r#"
            IFS= read -r line
            id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"prompts":{}},"serverInfo":{"name":"p","version":"1"}}}\n' "$id"
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"tools/'*) echo "unexpected: $line" >&2 ;;
                esac
            done
        "#;
        let client = McpClient::connect_with(&config(script), quick()).await;
        assert_eq!(client.state(), ConnectionState::Ready);

        assert!(client.list_tools().await.unwrap().is_empty());
        // Nothing was sent: the server complains on stderr if it got a line.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!client.diagnostics().contains("unexpected"), "{}", client.diagnostics());

        assert_no_stray_process(client).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_reads_back_as_failed_and_is_idempotent() {
        let client = McpClient::connect_with(&config(&full_server("2025-06-18")), quick()).await;
        client.shutdown().await;
        client.shutdown().await;

        let reason = failed_reason(&client);
        assert!(reason.contains("shut down"), "{reason}");
        assert!(matches!(
            client.list_tools().await,
            Err(ClientError::NotReady { .. })
        ));
        assert_no_stray_process(client).await;
    }
}

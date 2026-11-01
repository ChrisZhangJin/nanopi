//! Parent↔child channel: lets a child agent ask the main process to make
//! an MCP call on its behalf. Design: `docs/mcp-ipc.md`.
//!
//! A child is a separate `nanopi -p` process that must not connect to MCP
//! servers itself (`docs/mcp.md`, option B), so the main process holds
//! every connection and a child reaches them over a Unix socket. The wire
//! is newline-delimited JSON-RPC and reuses [`protocol`] unchanged — the
//! parent is a small MCP proxy, which is why there is no second codec.
//!
//! **What actually protects this channel.** The agent id a child sends in
//! its handshake is *not a secret*: ids are short and sequential, every
//! sibling agent has the same environment layout, and the id is on the
//! child's command line. Two things carry the weight instead:
//!
//! - the socket lives in a directory created mode `0700`, so another
//!   *user* cannot connect at all. That is the whole of the connect-time
//!   access control, and it does nothing against a process running as the
//!   same user — which includes every sibling agent and any tool a child
//!   runs.
//! - authorisation is the parent's, per agent id, from an allowlist
//!   recorded when the agent was dispatched ([`IpcServer::grant`]). The
//!   child's own claims about what it may call carry no weight, and the
//!   allowlist cannot be changed over the wire.
//!
//! What does *not* protect it: the id. A sibling that guesses `a2` and
//! says so in a handshake would inherit `a2`'s allowlist. Two things cut
//! that down, neither of them a guarantee. A second connection claiming an
//! id that is already connected is refused. And a grant can be bound to
//! the pid the parent spawned ([`IpcServer::bind_process`]), after which
//! the kernel-reported peer pid of the connection must match; an unbound
//! grant is id-only. The residue is a same-user process claiming an id
//! whose agent has not connected yet and whose grant is unbound — it gets
//! only that agent's MCP tools, never the parent's whole catalogue, and
//! those are tools the user already chose to give an agent.
//!
//! **A slow call must not delay anyone else.** One task serves each
//! connection and each request on it runs as its own task, so neither
//! another child nor the same child's next call waits behind it. No lock
//! is held across an `await`: the allowlist is read under a `std` mutex
//! and released before the proxied call starts, and replies leave through
//! a per-connection queue rather than a shared writer lock.
//!
//! **A child that goes away takes its calls with it.** When a connection
//! closes (the child exited, or `stop_agent` killed it) the tasks serving
//! it are dropped, which drops the in-flight `McpClient` call, which
//! makes the stdio transport send `notifications/cancelled` for it (see
//! `stdio::InFlight`). Without that a stopped agent's browser step or
//! database query would run to completion for nobody.
//!
//! **Parent death.** On Linux a child is killed by `PDEATHSIG` when the
//! parent dies; on macOS it relies on the once-a-second `getppid` poll in
//! `main.rs`, so it can outlive the parent by about a second. Neither is
//! what makes an in-flight call fail: the kernel closes the parent's end
//! of the socket when the parent exits for any reason, including
//! `SIGKILL`, so the child's reader sees EOF and every pending call fails
//! at once with a clear error. That holds on every unix because the
//! socket fds are close-on-exec and never leak into children.
//!
//! Unix only. `tokio` has no Unix sockets on Windows; the whole module is
//! `cfg(unix)` and a Windows child gets an in-band "not available"
//! (M10).

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::mcp::manager::McpManager;
use crate::mcp::protocol::method::{TOOLS_CALL, TOOLS_LIST};
use crate::mcp::protocol::{
    error_code, CallToolParams, CallToolResult, Incoming, ListToolsResult, Request, RequestId,
    Response, ResponsePayload, RpcError, Tool,
};
use crate::mcp::tool::{first_line, McpTool};
use crate::mcp::client::ClientError;

/// Environment variable carrying the socket path to a child. Set by
/// `tool::agent::build_child_env`, next to `NANOPI_AGENT_ID`.
pub const SOCKET_ENV: &str = "NANOPI_MCP_SOCKET";

/// The agent id, as `tool::agent` sets it. Read here so the child side
/// has one entry point ([`IpcClient::from_env`]).
const AGENT_ID_ENV: &str = "NANOPI_AGENT_ID";

/// First message on a connection. Not an MCP method: it is ours, and the
/// `nanopi/` prefix keeps it out of MCP's namespace.
const HELLO: &str = "nanopi/hello";

/// Bumped if a message changes shape incompatibly. A child and a parent
/// are the same binary today, but a long-lived parent can outlive an
/// upgraded `nanopi` on `$PATH`, and a mismatch is better named than
/// misparsed.
const WIRE_VERSION: u32 = 1;

/// Server-defined JSON-RPC error: the parent refused. Kept apart from
/// `INVALID_PARAMS` and friends so a child can tell "you may not" from
/// "you asked wrongly" without parsing prose.
pub const DENIED: i32 = -32001;

/// Longest request line the parent accepts. A request carries a tool
/// name and a model-written argument object; nothing legitimate is near
/// this, and the bound is what stops a hostile child growing the parent's
/// buffer without limit by never sending a newline.
const MAX_REQUEST_LINE: usize = 8 * 1024 * 1024;

/// Longest reply line a child accepts. Higher than the request bound
/// because a reply can carry an `image` block, which is base64 and
/// routinely megabytes; it matches the stdio transport's own ceiling plus
/// headroom for the envelope.
const MAX_REPLY_LINE: usize = 32 * 1024 * 1024;

/// How long a new connection has to say hello. An idle connection holds
/// a task and a file descriptor, and nothing legitimate waits.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a child waits to *connect*. A missing or stale socket fails
/// immediately (`ENOENT`, `ECONNREFUSED`); this only bounds the odd case
/// of a listener whose backlog is full, so the failure is never "hung
/// until the call timeout".
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Default bound on `tools/list`.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default bound on a proxied `tools/call`, as seen by the child.
///
/// Every call is already bounded *in the parent* by the server's own
/// `call_timeout` (120 s by default), and that is the error a child
/// should see, since it names the cause. This ceiling is only for a
/// parent that is alive but not answering, so it must sit above any
/// operator-configured `call_timeout`; a parent that dies is reported by
/// EOF, not by this.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Concurrent in-flight requests per connection. A model emits a handful
/// of tool calls per turn; the bound exists so a hostile child cannot
/// make the parent spawn unbounded tasks, each holding a server request.
const MAX_IN_FLIGHT: usize = 16;

/// Concurrent connections. `agent.max_live` is far below this; it only
/// stops a runaway local process using up file descriptors.
const MAX_CONNECTIONS: usize = 64;

/// Replies queued per connection before a handler waits. Bounded so a
/// child that stops reading its socket stalls only its own handlers.
const OUTBOX: usize = 64;

/// What the longest acceptable socket path may be. Linux's `sun_path` is
/// 108 bytes and macOS's 104, both including the NUL; 100 leaves slack
/// and is the same on both.
const SUN_PATH_BUDGET: usize = 100;

const SOCKET_NAME: &str = "mcp.sock";

// -------------------------------------------------------------- errors

/// Why a child's call over the channel failed.
///
/// Mirrors `ClientError` on purpose, so M10's proxy tool can reuse the
/// same in-band-failure mapping `McpTool` has: a refusal and a dead
/// parent call for different reactions from the model, and neither is a
/// tool that ran and failed (that is `Ok` with `is_error` set).
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// Nothing to talk to: no socket path, no listener, or no answer to
    /// connect. Raised before anything is sent.
    #[error("{0}")]
    Unreachable(String),
    /// The parent answered the handshake with a refusal.
    #[error("the parent refused this agent's MCP channel: {0}")]
    Refused(String),
    /// The parent will not make this call for this agent.
    #[error("the parent denied the call: {0}")]
    Denied(String),
    /// The parent, or the MCP server behind it, answered with a
    /// JSON-RPC error.
    #[error("{0}")]
    Rpc(RpcError),
    /// The connection is gone: the parent exited or shut the channel.
    #[error("{0}")]
    Closed(String),
    #[error("`{method}` timed out after {:.1}s", timeout.as_secs_f32())]
    Timeout { method: String, timeout: Duration },
    /// The other side said something this protocol does not allow.
    #[error("{0}")]
    Protocol(String),
}

/// Map the parent's refusal and failures onto [`IpcError`].
fn error_from_wire(e: RpcError) -> IpcError {
    if e.code == DENIED {
        IpcError::Denied(e.message)
    } else {
        IpcError::Rpc(e)
    }
}

// -------------------------------------------------------------- server

/// One agent's authority, recorded when it was dispatched.
struct Grant {
    /// Registered MCP tool names (`mcp__server__tool`) the agent may call.
    mcp_tools: HashSet<String>,
    /// The pid the parent spawned for this agent, once known.
    pid: Option<u32>,
}

#[derive(Default)]
struct State {
    grants: HashMap<String, Grant>,
    /// Agent ids with a live connection.
    connected: HashSet<String>,
}

struct Inner {
    /// Registered name → tool. Built once from the manager: its tool set
    /// is fixed after startup.
    tools: HashMap<String, Arc<McpTool>>,
    state: Mutex<State>,
}

/// The parent's end of the channel: one listener, one connection per
/// child.
///
/// Created by [`IpcServer::start`] or, for a real session,
/// [`start_for_session`]. The allowlist API — [`grant`](Self::grant),
/// [`bind_process`](Self::bind_process), [`revoke`](Self::revoke) — is
/// what `tool::agent`'s dispatch path calls; nothing a child sends can
/// reach it.
pub struct IpcServer {
    inner: Arc<Inner>,
    socket_path: PathBuf,
    dir: PathBuf,
    accept: Mutex<Option<JoinHandle<()>>>,
}

impl IpcServer {
    /// Listen under `$TMPDIR`, serving `manager`'s tools.
    pub fn start(manager: &McpManager) -> io::Result<Arc<Self>> {
        Self::start_in(&std::env::temp_dir(), manager)
    }

    /// [`start`](Self::start) with the base directory chosen, which is
    /// how tests exercise the path-length handling without touching the
    /// process environment.
    ///
    /// **A short path, in a directory made `0700`.** The socket is not
    /// under the project tree: `sun_path` is 104–108 bytes and a
    /// realistic checkout already costs 80 before the file name, so a
    /// slightly deeper one would make `bind` fail. If `base` is too long
    /// (macOS's per-user `$TMPDIR` is ~50 bytes, a user's own can be
    /// anything) `/tmp` is tried, and failing that the error says what to
    /// change.
    pub fn start_in(base: &Path, manager: &McpManager) -> io::Result<Arc<Self>> {
        let tools = manager
            .tools()
            .iter()
            .map(|t| (t.definition().name, Arc::clone(t)))
            .collect();
        let inner = Arc::new(Inner { tools, state: Mutex::new(State::default()) });

        let (dir, socket_path) = make_socket_dir(base)?;
        let listener = match bind_private(&socket_path) {
            Ok(l) => l,
            Err(e) => {
                let _ = std::fs::remove_dir(&dir);
                return Err(e);
            }
        };
        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&inner)));
        Ok(Arc::new(Self {
            inner,
            socket_path,
            dir,
            accept: Mutex::new(Some(accept)),
        }))
    }

    /// Where children connect. Goes to them as [`SOCKET_ENV`].
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Record which MCP tools `agent_id` may call, **replacing** any
    /// earlier grant. Call it when the agent is dispatched, before the
    /// child is spawned: the handshake looks the id up here, and an id
    /// with no grant is refused.
    ///
    /// `tools` are registered names (`mcp__server__tool`). The set is
    /// whatever the dispatching model asked for in the `agent` tool's
    /// `tools` field *after* the parent validated it; this layer enforces
    /// it and does not second-guess it. Names the manager does not hold
    /// are harmless: they can never be listed or called.
    ///
    /// Only the parent can reach this. There is deliberately no wire
    /// message that changes a grant, so a child can never widen its own.
    pub fn grant<I, S>(&self, agent_id: &str, tools: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mcp_tools = tools.into_iter().map(Into::into).collect();
        let mut state = self.inner.state.lock().unwrap();
        let pid = state.grants.get(agent_id).and_then(|g| g.pid);
        state.grants.insert(agent_id.to_string(), Grant { mcp_tools, pid });
    }

    /// Tie `agent_id`'s grant to the process the parent spawned for it.
    /// Afterwards a handshake claiming the id is accepted only from a
    /// connection whose kernel-reported peer pid is `pid`, so a sibling
    /// that learned the id cannot use it. Call it right after the spawn
    /// returns, before any `await`: the child cannot have connected yet.
    ///
    /// Optional, and only as strong as the pid: it is the *direct* child.
    /// A wrapper that execs a different process would not match, so the
    /// call is for spawns whose program is nanopi itself.
    pub fn bind_process(&self, agent_id: &str, pid: u32) {
        if let Some(grant) = self.inner.state.lock().unwrap().grants.get_mut(agent_id) {
            grant.pid = Some(pid);
        }
    }

    /// Drop `agent_id`'s grant, when the agent has finished. Calls
    /// already on the wire for it still complete; anything new is
    /// refused, and the id can be granted afresh.
    pub fn revoke(&self, agent_id: &str) {
        self.inner.state.lock().unwrap().grants.remove(agent_id);
    }

    /// Ids with a live connection. For tests and diagnostics.
    pub fn connected_agents(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.inner.state.lock().unwrap().connected.iter().cloned().collect();
        ids.sort();
        ids
    }

    /// Stop listening, drop every connection (and with them every
    /// in-flight proxied call), and remove the socket and its directory.
    /// Idempotent, synchronous, and safe from a signal path or `Drop`.
    ///
    /// Run it *before* the manager's shutdown, so a child cannot start a
    /// call into servers that are being stopped.
    pub fn shutdown(&self) {
        if let Some(accept) = self.accept.lock().unwrap().take() {
            // Dropping the accept task drops its connection set, which
            // drops each connection's request set, which drops each
            // proxied call: that is what cancels them.
            accept.abort();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        // Not `remove_dir_all`: the directory is ours and holds only the
        // socket, and a stray file there is worth finding rather than
        // deleting.
        let _ = std::fs::remove_dir(&self.dir);
        uninstall(self);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Create a fresh `0700` directory under `base` (or `/tmp`) and return it
/// with the socket path inside.
///
/// Only a directory *this call created* is used: `mkdir` fails on an
/// existing name, so a stale `nanopi-<pid>` from a crashed run, or one an
/// attacker planted in a shared `/tmp`, is stepped over rather than
/// trusted. The mode is given to `mkdir` itself, so there is no window in
/// which it exists with a looser one.
fn make_socket_dir(base: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let pid = std::process::id();
    let mut too_long = None;
    for base in [base, Path::new("/tmp")] {
        let sample = base.join(format!("nanopi-{pid}-99")).join(SOCKET_NAME);
        if sample.as_os_str().len() > SUN_PATH_BUDGET {
            too_long.get_or_insert_with(|| base.to_path_buf());
            continue;
        }
        for attempt in 0..100 {
            let name = if attempt == 0 {
                format!("nanopi-{pid}")
            } else {
                format!("nanopi-{pid}-{attempt}")
            };
            let dir = base.join(name);
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    let socket = dir.join(SOCKET_NAME);
                    return Ok((dir, socket));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                // Unusable base (read-only, missing): try the next one.
                Err(_) => break,
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        match too_long {
            Some(base) => format!(
                "cannot create a Unix socket under {}: the path would exceed the {SUN_PATH_BUDGET}-byte \
                 limit (sun_path); point TMPDIR at a shorter directory",
                base.display()
            ),
            None => "cannot create a private socket directory under $TMPDIR or /tmp".to_string(),
        },
    ))
}

/// Bind and make the socket file owner-only. The directory already
/// excludes other users; this keeps the file itself private too if it is
/// ever moved.
fn bind_private(path: &Path) -> io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn accept_loop(listener: UnixListener, inner: Arc<Inner>) {
    let mut connections: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    // Transient (`EMFILE`, `ECONNABORTED`): spinning on it
                    // would peg a core, and giving up would silently end
                    // the channel for every child.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                if connections.len() >= MAX_CONNECTIONS {
                    continue; // dropped: the peer sees EOF
                }
                connections.spawn(serve_connection(Arc::clone(&inner), stream));
            }
            // Reap finished connections so the set tracks live ones.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

// ----------------------------------------------------- one connection

/// What a hello is allowed to carry. Unknown fields are ignored on
/// purpose and are *not* a way to ask for more: the allowlist is never
/// read from the wire.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hello {
    agent_id: String,
}

/// Removes the agent from `connected` however the connection ends.
struct Registration<'a> {
    inner: &'a Inner,
    agent_id: String,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().connected.remove(&self.agent_id);
    }
}

async fn serve_connection(inner: Arc<Inner>, stream: UnixStream) {
    // The kernel's word for who is on the other end; unlike anything in
    // the handshake it cannot be chosen by the peer. `None` where the
    // platform will not say, which fails closed for a bound grant.
    let peer_pid = stream
        .peer_cred()
        .ok()
        .and_then(|c| c.pid())
        .and_then(|p| u32::try_from(p).ok());
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();

    let hello = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        read_request(&mut reader, &mut buf, MAX_REQUEST_LINE),
    )
    .await
    {
        Ok(Some(req)) => req,
        // EOF, garbage, an oversized line, or silence: nothing to answer.
        _ => return,
    };

    // Written directly rather than through the connection's outbox: the
    // refusal must reach the peer before this task ends and drops the
    // socket.
    let agent_id = match authenticate(&inner, &hello, peer_pid) {
        Ok(id) => id,
        Err(refusal) => {
            let _ = write_response(&mut write, reply_err(hello.id, refusal)).await;
            return;
        }
    };
    let _registration = Registration { inner: &inner, agent_id: agent_id.clone() };
    let ack = reply_ok(hello.id, json!({ "version": WIRE_VERSION, "capabilities": ["mcp"] }));
    if write_response(&mut write, ack).await.is_err() {
        return;
    }

    let (outbox, queue) = mpsc::channel::<String>(OUTBOX);
    // One future, so that whichever side ends first takes the other down
    // with it. A writer spawned on its own would outlive an aborted
    // connection, stuck on a peer that has stopped reading.
    tokio::select! {
        _ = serve_requests(Arc::clone(&inner), agent_id, reader, buf, outbox) => {}
        _ = write_replies(write, queue) => {}
    }
}

/// Check the handshake. Every refusal reads the same, so a probing child
/// cannot tell an unknown id from one that is taken or bound elsewhere —
/// that would let it enumerate its siblings.
fn authenticate(inner: &Inner, hello: &Request, peer_pid: Option<u32>) -> Result<String, RpcError> {
    let refused = || RpcError {
        code: DENIED,
        message: "this agent is not registered for MCP access".to_string(),
        data: None,
    };
    if hello.method != HELLO {
        return Err(RpcError {
            code: error_code::INVALID_REQUEST,
            message: format!("expected `{HELLO}` first, got `{}`", hello.method),
            data: None,
        });
    }
    let hello: Hello = hello
        .params
        .clone()
        .and_then(|p| serde_json::from_value(p).ok())
        .ok_or_else(|| RpcError {
            code: error_code::INVALID_PARAMS,
            message: "`agentId` is required".to_string(),
            data: None,
        })?;

    let mut state = inner.state.lock().unwrap();
    let Some(grant) = state.grants.get(&hello.agent_id) else {
        return Err(refused());
    };
    if let Some(bound) = grant.pid {
        if peer_pid != Some(bound) {
            return Err(refused());
        }
    }
    if !state.connected.insert(hello.agent_id.clone()) {
        return Err(refused());
    }
    Ok(hello.agent_id)
}

async fn serve_requests(
    inner: Arc<Inner>,
    agent_id: String,
    mut reader: BufReader<OwnedReadHalf>,
    mut buf: Vec<u8>,
    outbox: mpsc::Sender<String>,
) {
    // Dropped when this function returns, which aborts every request
    // still running. That is the cancellation path: see the module docs.
    let mut requests: JoinSet<()> = JoinSet::new();
    loop {
        while requests.try_join_next().is_some() {}
        // A line that is not a message ends the connection: the framing
        // is not trustworthy past that point, and the only client is our
        // own code.
        let Some(request) = read_request(&mut reader, &mut buf, MAX_REQUEST_LINE).await else {
            return;
        };
        if requests.len() >= MAX_IN_FLIGHT {
            let busy = reply_err(
                request.id,
                RpcError {
                    code: error_code::INTERNAL_ERROR,
                    message: format!("too many calls in flight (limit {MAX_IN_FLIGHT})"),
                    data: None,
                },
            );
            if send(&outbox, busy).await.is_err() {
                return;
            }
            continue;
        }
        let inner = Arc::clone(&inner);
        let agent_id = agent_id.clone();
        let outbox = outbox.clone();
        requests.spawn(async move {
            let Request { id, method, params, .. } = request;
            let reply = match dispatch(&inner, &agent_id, &method, params).await {
                Ok(value) => reply_ok(id, value),
                Err(e) => reply_err(id, e),
            };
            let _ = send(&outbox, reply).await;
        });
    }
}

/// Where a method name becomes a handler. A second capability is another
/// arm here (and whatever it needs in [`Grant`]); the connection,
/// handshake and allowlist lookup around it are shared.
async fn dispatch(
    inner: &Inner,
    agent_id: &str,
    method: &str,
    params: Option<Value>,
) -> Result<Value, RpcError> {
    match method {
        TOOLS_LIST => mcp_list(inner, agent_id),
        TOOLS_CALL => mcp_call(inner, agent_id, params).await,
        other => Err(RpcError {
            code: error_code::METHOD_NOT_FOUND,
            message: format!("`{other}` is not served over this channel"),
            data: None,
        }),
    }
}

/// The agent's allowlist, intersected with what the manager holds. Never
/// "everything the parent offers": a child's view of the tool world is
/// exactly what it was granted.
fn mcp_list(inner: &Inner, agent_id: &str) -> Result<Value, RpcError> {
    let granted: HashSet<String> = {
        let state = inner.state.lock().unwrap();
        match state.grants.get(agent_id) {
            Some(g) => g.mcp_tools.clone(),
            None => return Err(not_registered()),
        }
    };
    let mut tools: Vec<Tool> = granted
        .iter()
        .filter_map(|name| inner.tools.get(name))
        .map(|t| t.definition())
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    to_value(&ListToolsResult { tools, next_cursor: None })
}

async fn mcp_call(inner: &Inner, agent_id: &str, params: Option<Value>) -> Result<Value, RpcError> {
    let params: CallToolParams = params
        .and_then(|p| serde_json::from_value(p).ok())
        .ok_or_else(|| RpcError {
            code: error_code::INVALID_PARAMS,
            message: "`tools/call` needs a `name` and an `arguments` object".to_string(),
            data: None,
        })?;

    // The authorisation check, and the whole of it. It reads the
    // parent's own record, keyed by the id the connection was
    // authenticated as — never anything in this request — and the lock
    // is gone before the call starts.
    let allowed = {
        let state = inner.state.lock().unwrap();
        match state.grants.get(agent_id) {
            Some(g) => g.mcp_tools.contains(&params.name),
            None => false,
        }
    };
    // One message for "not granted" and "does not exist", so a child
    // cannot use the difference to map out tools it was not given.
    if !allowed {
        return Err(RpcError {
            code: DENIED,
            message: format!("tool \"{}\" was not granted to this agent", params.name),
            data: None,
        });
    }
    let Some(tool) = inner.tools.get(&params.name) else {
        return Err(RpcError {
            code: error_code::INTERNAL_ERROR,
            message: format!("tool \"{}\" is no longer available", params.name),
            data: None,
        });
    };

    match tool.call_raw(params.arguments).await {
        Ok(result) => to_value(&result),
        // The server's own refusal keeps its code: it is what the model
        // should see, and it is the same text the parent's own model
        // would have got in solo mode.
        Err(ClientError::Rpc(e)) => Err(RpcError { code: e.code, message: first_line(&e.message, 1000), data: None }),
        // Headlines only, as `McpTool` does: these can embed a stderr
        // tail, which belongs in `/mcp`, not in a child's context.
        Err(e) => Err(RpcError {
            code: error_code::INTERNAL_ERROR,
            message: first_line(&e.to_string(), 1000),
            data: None,
        }),
    }
}

fn reply_ok(id: RequestId, value: Value) -> Response {
    Response { id: Some(id), payload: ResponsePayload::Result(value) }
}

fn reply_err(id: RequestId, error: RpcError) -> Response {
    Response { id: Some(id), payload: ResponsePayload::Error(error) }
}

fn not_registered() -> RpcError {
    RpcError {
        code: DENIED,
        message: "this agent is not registered for MCP access".to_string(),
        data: None,
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError {
        code: error_code::INTERNAL_ERROR,
        message: format!("encoding the reply: {e}"),
        data: None,
    })
}

/// Queue a reply. `Err` means the connection is already closing.
async fn send(outbox: &mpsc::Sender<String>, response: Response) -> Result<(), ()> {
    let line = serde_json::to_string(&response).map_err(|_| ())?;
    outbox.send(line).await.map_err(|_| ())
}

async fn write_replies(mut write: OwnedWriteHalf, mut queue: mpsc::Receiver<String>) {
    while let Some(line) = queue.recv().await {
        if write_line(&mut write, &line).await.is_err() {
            return;
        }
    }
}

async fn write_response(write: &mut OwnedWriteHalf, response: Response) -> io::Result<()> {
    let line = serde_json::to_string(&response).map_err(io::Error::other)?;
    write_line(write, &line).await
}

/// `serde_json` escapes newlines inside strings, so the line is one line.
async fn write_line(write: &mut OwnedWriteHalf, line: &str) -> io::Result<()> {
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await
}

// ------------------------------------------------------------- framing

enum Line {
    Eof,
    TooLong,
    Text(String),
}

/// Read one non-empty line, holding at most `max` bytes of it.
///
/// `take` is what makes the bound real: `read_until` alone buffers until
/// it sees a newline, so checking the length afterwards (as a simple
/// reader would) lets a peer that never sends one grow the buffer without
/// limit. A final line with no newline is discarded as EOF — a message
/// cut off by the peer dying is not a message.
async fn read_line<R>(reader: &mut R, buf: &mut Vec<u8>, max: usize) -> io::Result<Line>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        buf.clear();
        let n = (&mut *reader)
            .take(max as u64 + 1)
            .read_until(b'\n', buf)
            .await?;
        if n == 0 {
            return Ok(Line::Eof);
        }
        if buf.len() > max {
            return Ok(Line::TooLong);
        }
        if buf.last() != Some(&b'\n') {
            return Ok(Line::Eof);
        }
        let text = String::from_utf8_lossy(buf);
        let text = text.trim();
        if !text.is_empty() {
            return Ok(Line::Text(text.to_string()));
        }
    }
}

/// The next *request*, or `None` for anything that should end the
/// connection. Notifications and responses are not part of what a child
/// may send and are skipped.
async fn read_request<R>(reader: &mut R, buf: &mut Vec<u8>, max: usize) -> Option<Request>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        match read_line(reader, buf, max).await {
            Ok(Line::Text(text)) => match serde_json::from_str::<Incoming>(&text) {
                Ok(Incoming::Request(request)) => return Some(request),
                Ok(_) => continue,
                Err(_) => return None,
            },
            Ok(Line::Eof | Line::TooLong) | Err(_) => return None,
        }
    }
}

// ------------------------------------------------------ session wiring

/// The installed server, if any. A process holds at most one; what reads
/// it is `tool::agent`, which has no handle on the session's startup.
static GLOBAL: Mutex<Option<Arc<IpcServer>>> = Mutex::new(None);

/// The server for this process, if one is running.
pub fn global() -> Option<Arc<IpcServer>> {
    GLOBAL.lock().unwrap().clone()
}

fn uninstall(server: &IpcServer) {
    let mut global = GLOBAL.lock().unwrap();
    if global.as_ref().is_some_and(|g| std::ptr::eq(Arc::as_ptr(g), server)) {
        *global = None;
    }
}

/// Stop the installed server, if any. Idempotent; the call for Ctrl-C and
/// normal-exit paths that have no handle on it.
pub fn shutdown_global() {
    // Taken out first so the lock is not held while `shutdown` re-locks
    // it to uninstall.
    let server = GLOBAL.lock().unwrap().take();
    if let Some(server) = server {
        server.shutdown();
    }
}

/// Dropping this stops the server. **Hold it for the life of the
/// session**: the global holds its own `Arc`, so dropping every other
/// handle never runs the server's `Drop`, and a SIGTERM that drops the
/// whole session future would otherwise leave the socket on disk — the
/// same trap `McpManager::kill_on_drop` documents.
pub struct IpcGuard(Arc<IpcServer>);

impl IpcGuard {
    pub fn server(&self) -> &Arc<IpcServer> {
        &self.0
    }
}

impl Drop for IpcGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// Start the channel for this session and install it for `tool::agent`.
///
/// `None` when there is nothing to proxy — no MCP tools, which is also
/// what a child agent's empty manager looks like — so a session without
/// MCP creates no socket and no directory. A failure to start is reported
/// and is not fatal: agents simply run without MCP tools.
pub fn start_for_session(manager: &McpManager) -> Option<IpcGuard> {
    if manager.tool_count() == 0 {
        return None;
    }
    match IpcServer::start(manager) {
        Ok(server) => {
            *GLOBAL.lock().unwrap() = Some(Arc::clone(&server));
            Some(IpcGuard(server))
        }
        Err(e) => {
            crate::note!("nanopi: mcp: agents will not be able to call MCP tools: {e}");
            None
        }
    }
}

// -------------------------------------------------------------- client

/// Tunables for [`IpcClient`]; fields so tests need not wait out the
/// real bounds.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    pub list_timeout: Duration,
    pub call_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            list_timeout: LIST_TIMEOUT,
            call_timeout: CALL_TIMEOUT,
        }
    }
}

type Reply = Result<Value, IpcError>;

/// As in `stdio::Slots`: the pending map and the closed flag are one
/// lock, so a request registered after the reader noticed EOF is refused
/// instead of parked on a channel nobody will ever answer.
enum Slots {
    Open(HashMap<RequestId, oneshot::Sender<Reply>>),
    Closed(String),
}

struct ClientShared {
    slots: Mutex<Slots>,
}

impl ClientShared {
    fn register(&self, id: RequestId, tx: oneshot::Sender<Reply>) -> Result<(), IpcError> {
        match &mut *self.slots.lock().unwrap() {
            Slots::Open(map) => {
                map.insert(id, tx);
                Ok(())
            }
            Slots::Closed(reason) => Err(IpcError::Closed(reason.clone())),
        }
    }

    fn forget(&self, id: &RequestId) {
        if let Slots::Open(map) = &mut *self.slots.lock().unwrap() {
            map.remove(id);
        }
    }

    fn deliver(&self, id: &RequestId, reply: Reply) {
        let waiter = match &mut *self.slots.lock().unwrap() {
            Slots::Open(map) => map.remove(id),
            Slots::Closed(_) => None,
        };
        if let Some(tx) = waiter {
            let _ = tx.send(reply);
        }
    }

    /// First reason wins: every later observation is a consequence of it.
    fn close(&self, reason: String) {
        let mut slots = self.slots.lock().unwrap();
        let drained = match std::mem::replace(&mut *slots, Slots::Closed(reason.clone())) {
            Slots::Open(map) => map,
            Slots::Closed(first) => {
                *slots = Slots::Closed(first);
                return;
            }
        };
        drop(slots);
        for (_, tx) in drained {
            let _ = tx.send(Err(IpcError::Closed(reason.clone())));
        }
    }

    fn close_reason(&self) -> Option<String> {
        match &*self.slots.lock().unwrap() {
            Slots::Open(_) => None,
            Slots::Closed(r) => Some(r.clone()),
        }
    }
}

/// Removes a request's slot however its future ends — including being
/// dropped mid-wait, which would otherwise leak the entry for the life of
/// the connection.
struct SlotGuard<'a> {
    shared: &'a ClientShared,
    id: RequestId,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.shared.forget(&self.id);
    }
}

/// The child's end: connect, handshake, then list and call tools through
/// the parent. `Send + Sync`, and every method takes `&self`, so M10's
/// proxy tools can share one in an `Arc` and call concurrently.
///
/// Returns the same [`Tool`] and [`CallToolResult`] shapes as
/// `McpClient`, so a proxy tool maps results exactly as `McpTool` does.
pub struct IpcClient {
    agent_id: String,
    shared: Arc<ClientShared>,
    /// Lines to write. Requests go through a queue and a single writer
    /// task rather than a shared write lock, so a future dropped
    /// mid-send cannot leave half a line in the socket: queueing a whole
    /// line is atomic, and only the writer task ever touches the half.
    outbox: mpsc::Sender<String>,
    next_id: AtomicI64,
    options: ClientOptions,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl IpcClient {
    /// Connect using the environment [`build_child_env`] set up.
    ///
    /// A missing variable is the ordinary way a child ends up here — an
    /// older parent, a parent without MCP servers, a platform without
    /// the channel — so it is a clear error, not a hang and not a panic.
    ///
    /// [`build_child_env`]: crate::tool::agent::build_child_env
    pub async fn from_env() -> Result<Self, IpcError> {
        let path = std::env::var_os(SOCKET_ENV).ok_or_else(|| {
            IpcError::Unreachable(format!(
                "this agent has no channel to its parent's MCP servers: {SOCKET_ENV} is not set \
                 (the parent has no MCP servers, or predates this feature)"
            ))
        })?;
        let agent_id = std::env::var(AGENT_ID_ENV).map_err(|_| {
            IpcError::Unreachable(format!(
                "this agent cannot identify itself to its parent: {AGENT_ID_ENV} is not set"
            ))
        })?;
        Self::connect(Path::new(&path), &agent_id).await
    }

    pub async fn connect(path: &Path, agent_id: &str) -> Result<Self, IpcError> {
        Self::connect_with(path, agent_id, ClientOptions::default()).await
    }

    /// Connect and handshake, failing fast with the cause.
    pub async fn connect_with(
        path: &Path,
        agent_id: &str,
        options: ClientOptions,
    ) -> Result<Self, IpcError> {
        let stream = match tokio::time::timeout(options.connect_timeout, UnixStream::connect(path)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                return Err(IpcError::Unreachable(format!(
                    "cannot reach the parent's MCP channel at {}: {e}",
                    path.display()
                )))
            }
            Err(_) => {
                return Err(IpcError::Unreachable(format!(
                    "the parent's MCP channel at {} did not accept a connection within {:.1}s",
                    path.display(),
                    options.connect_timeout.as_secs_f32()
                )))
            }
        };
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);

        let hello = Request::new(
            0,
            HELLO,
            Some(json!({ "agentId": agent_id, "version": WIRE_VERSION })),
        );
        let line = serde_json::to_string(&hello).map_err(|e| IpcError::Protocol(e.to_string()))?;
        let reply = tokio::time::timeout(options.handshake_timeout, async {
            write_line(&mut write, &line).await?;
            let mut buf = Vec::new();
            read_line(&mut reader, &mut buf, MAX_REPLY_LINE).await
        })
        .await;
        let text = match reply {
            Ok(Ok(Line::Text(text))) => text,
            Ok(Ok(_)) | Ok(Err(_)) => {
                return Err(IpcError::Closed(
                    "the parent closed the MCP channel during the handshake".to_string(),
                ))
            }
            Err(_) => {
                return Err(IpcError::Unreachable(format!(
                    "the parent did not answer the MCP handshake within {:.1}s",
                    options.handshake_timeout.as_secs_f32()
                )))
            }
        };
        match serde_json::from_str::<Incoming>(&text) {
            Ok(Incoming::Response(Response { payload: ResponsePayload::Result(v), .. })) => {
                let version = v.get("version").and_then(Value::as_u64);
                if version != Some(u64::from(WIRE_VERSION)) {
                    return Err(IpcError::Protocol(format!(
                        "the parent speaks MCP channel version {version:?}, this agent speaks {WIRE_VERSION}"
                    )));
                }
            }
            Ok(Incoming::Response(Response { payload: ResponsePayload::Error(e), .. })) => {
                return Err(IpcError::Refused(e.message));
            }
            _ => return Err(IpcError::Protocol("the parent's handshake reply was not understood".to_string())),
        }

        let shared = Arc::new(ClientShared { slots: Mutex::new(Slots::Open(HashMap::new())) });
        let (outbox, queue) = mpsc::channel::<String>(OUTBOX);
        let reader = tokio::spawn(client_read_loop(reader, Arc::clone(&shared)));
        let writer = tokio::spawn(client_write_loop(write, queue, Arc::clone(&shared)));
        Ok(Self {
            agent_id: agent_id.to_string(),
            shared,
            outbox,
            next_id: AtomicI64::new(1),
            options,
            reader,
            writer,
        })
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// `Some(reason)` once the channel is unusable.
    pub fn close_reason(&self) -> Option<String> {
        self.shared.close_reason()
    }

    /// The tools the parent granted this agent — and only those.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, IpcError> {
        let value = self
            .request(TOOLS_LIST, json!({}), self.options.list_timeout)
            .await?;
        let page: ListToolsResult = serde_json::from_value(value)
            .map_err(|e| IpcError::Protocol(format!("unusable tool list from the parent: {e}")))?;
        Ok(page.tools)
    }

    /// Call a tool by its registered name (`mcp__server__tool`). `Err`
    /// means the call could not be made or was refused; `Ok` with
    /// `is_error` set means the tool ran and reported its own failure.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<CallToolResult, IpcError> {
        let params = serde_json::to_value(CallToolParams::new(name, arguments))
            .map_err(|e| IpcError::Protocol(e.to_string()))?;
        let value = self.request(TOOLS_CALL, params, self.options.call_timeout).await?;
        serde_json::from_value(value)
            .map_err(|e| IpcError::Protocol(format!("unusable tool result from the parent: {e}")))
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let line = serde_json::to_string(&Request::new(id.clone(), method, Some(params)))
            .map_err(|e| IpcError::Protocol(e.to_string()))?;

        let (tx, rx) = oneshot::channel();
        // Before the send, so a reply racing the queue has somewhere to go.
        self.shared.register(id.clone(), tx)?;
        let _slot = SlotGuard { shared: &self.shared, id };

        if self.outbox.send(line).await.is_err() {
            return Err(IpcError::Closed(
                self.shared
                    .close_reason()
                    .unwrap_or_else(|| "the MCP channel to the parent is closed".to_string()),
            ));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => reply,
            // `close` always sends, so a dropped sender means the reader
            // task died outright.
            Ok(Err(_)) => Err(IpcError::Closed(
                self.shared
                    .close_reason()
                    .unwrap_or_else(|| "the MCP channel to the parent stopped reading".to_string()),
            )),
            Err(_) => Err(IpcError::Timeout { method: method.to_string(), timeout }),
        }
    }
}

impl Drop for IpcClient {
    /// Closing the socket is how the parent learns this child is done
    /// with its calls; aborting both tasks is what closes it.
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

async fn client_read_loop(mut reader: BufReader<OwnedReadHalf>, shared: Arc<ClientShared>) {
    let mut buf = Vec::new();
    let reason = loop {
        match read_line(&mut reader, &mut buf, MAX_REPLY_LINE).await {
            Ok(Line::Text(text)) => match serde_json::from_str::<Incoming>(&text) {
                Ok(Incoming::Response(Response { id: Some(id), payload })) => {
                    let reply = match payload {
                        ResponsePayload::Result(v) => Ok(v),
                        ResponsePayload::Error(e) => Err(error_from_wire(e)),
                    };
                    shared.deliver(&id, reply);
                }
                // The parent sends this child nothing else.
                Ok(_) => {}
                Err(_) => break "the parent sent something that is not JSON-RPC".to_string(),
            },
            Ok(Line::Eof) => {
                break "the parent closed the MCP channel (it has exited or shut down)".to_string()
            }
            Ok(Line::TooLong) => {
                break format!("the parent sent a reply over {} MiB", MAX_REPLY_LINE / (1024 * 1024))
            }
            Err(e) => break format!("reading from the parent: {e}"),
        }
    };
    shared.close(reason);
}

async fn client_write_loop(
    mut write: OwnedWriteHalf,
    mut queue: mpsc::Receiver<String>,
    shared: Arc<ClientShared>,
) {
    while let Some(line) = queue.recv().await {
        if let Err(e) = write_line(&mut write, &line).await {
            shared.close(format!("writing to the parent: {e}"));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::ServerConfig;
    use crate::mcp::manager::ManagerOptions;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    /// Unique and short: the base directory is part of a `sun_path`.
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nipc-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    const LAUNCH_MARKER: &str = "launch-marker-7f3a91";
    const ENV_SECRET: &str = "s3cr3t-do-not-print-4c1d";

    /// A server that answers the handshake and lists three tools, logs
    /// every `tools/call` and every cancellation, and answers a call with
    /// the name it was asked for. `slow` does not answer until the
    /// release file exists, and each call runs in the background, so the
    /// server itself can hold several open — the property the
    /// concurrency tests need from it.
    const SERVER_SH: &str = r#"
        handle_call() {
            name=$(printf '%s' "$1" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
            case "$name" in
                slow) while [ ! -f "$RELEASE" ]; do sleep 0.01; done ;;
            esac
            printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ran %s"}]}}\n' "$2" "$name"
        }
        while IFS= read -r line; do
            id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            case "$line" in
                *'"method":"initialize"'*)
                    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
                *'"method":"tools/list"'*)
                    printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echoes","inputSchema":{"type":"object"}},{"name":"slow","description":"waits","inputSchema":{"type":"object"}},{"name":"other","description":"not granted","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
                *'"method":"tools/call"'*)
                    printf '%s\n' "$line" >> "$LOG"
                    handle_call "$line" "$id" & ;;
                *'"method":"notifications/cancelled"'*)
                    printf '%s\n' "$line" >> "$LOG" ;;
            esac
        done
    "#;

    struct Harness {
        manager: Arc<McpManager>,
        server: Arc<IpcServer>,
        dir: PathBuf,
    }

    impl Harness {
        async fn new(tag: &str) -> Self {
            let dir = scratch(tag);
            let config = ServerConfig {
                name: "fake".into(),
                command: "sh".into(),
                args: vec!["-c".into(), SERVER_SH.into(), "sh".into(), LAUNCH_MARKER.into()],
                env: BTreeMap::from([
                    ("LOG".to_string(), dir.join("calls.log").display().to_string()),
                    ("RELEASE".to_string(), dir.join("release").display().to_string()),
                    ("API_TOKEN".to_string(), ENV_SECRET.to_string()),
                ]),
            };
            let manager = Arc::new(McpManager::connect(vec![config], ManagerOptions::default()).await);
            assert_eq!(manager.tool_count(), 3, "{:?}", manager.status());
            let server = IpcServer::start_in(&dir, &manager).expect("start");
            Self { manager, server, dir }
        }

        fn path(&self) -> &Path {
            self.server.socket_path()
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.join("calls.log")).unwrap_or_default()
        }

        /// Wait on the fact, not on time. The deadline only ever fires
        /// when something is genuinely broken.
        async fn wait_log(&self, needle: &str) -> String {
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                let log = self.log();
                if log.contains(needle) {
                    return log;
                }
                assert!(Instant::now() < until, "never saw {needle:?} in {log:?}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        fn release(&self) {
            std::fs::write(self.dir.join("release"), "").unwrap();
        }

        /// Every test ends here: nothing of ours may be left running.
        async fn finish(self) {
            let pids = self.manager.pids();
            self.server.shutdown();
            self.manager.shutdown().await;
            let until = Instant::now() + Duration::from_secs(10);
            for pid in pids {
                while alive(pid) {
                    assert!(Instant::now() < until, "server {pid} survived shutdown");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A zombie answers signal 0 exactly like a live process, so ask
    /// `ps` for the state.
    fn alive(pid: u32) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    fn text(result: &CallToolResult) -> String {
        match &result.content[0] {
            crate::mcp::protocol::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("not text: {other:?}"),
        }
    }

    fn args() -> Map<String, Value> {
        Map::new()
    }

    /// A client that does not use [`IpcClient`], so it can say what the
    /// real one never would. Everything the parent sends is kept, for
    /// the tests that assert on what crossed the wire.
    struct Raw {
        reader: BufReader<OwnedReadHalf>,
        write: OwnedWriteHalf,
        seen: String,
    }

    impl Raw {
        async fn connect(path: &Path) -> Self {
            let (read, write) = UnixStream::connect(path).await.expect("connect").into_split();
            Self { reader: BufReader::new(read), write, seen: String::new() }
        }

        async fn send(&mut self, v: Value) {
            let _ = write_line(&mut self.write, &v.to_string()).await;
        }

        /// The next message, or `None` once the parent closed.
        async fn recv(&mut self) -> Option<Value> {
            let mut buf = Vec::new();
            match tokio::time::timeout(
                Duration::from_secs(10),
                read_line(&mut self.reader, &mut buf, MAX_REPLY_LINE),
            )
            .await
            .expect("the parent neither answered nor closed")
            {
                Ok(Line::Text(t)) => {
                    self.seen.push_str(&t);
                    self.seen.push('\n');
                    Some(serde_json::from_str(&t).expect("json"))
                }
                _ => None,
            }
        }

        async fn rpc(&mut self, id: i64, method: &str, params: Value) -> Value {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
            self.recv().await.expect("a reply")
        }

        async fn hello(&mut self, agent: &str) -> Value {
            self.rpc(0, HELLO, json!({ "agentId": agent })).await
        }
    }

    fn code(reply: &Value) -> i64 {
        reply["error"]["code"].as_i64().unwrap_or_else(|| panic!("not an error: {reply}"))
    }

    #[tokio::test]
    async fn a_child_lists_and_calls_a_granted_tool() {
        let h = Harness::new("happy").await;
        h.server.grant("a1", ["mcp__fake__echo"]);

        let client = IpcClient::connect(h.path(), "a1").await.expect("connect");
        let tools = client.list_tools().await.expect("list");
        assert_eq!(tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["mcp__fake__echo"]);
        assert_eq!(tools[0].description.as_deref(), Some("echoes"));

        let result = client.call_tool("mcp__fake__echo", args()).await.expect("call");
        // The server saw its own name for the tool, not the registered one.
        assert_eq!(text(&result), "ran echo");
        assert!(!result.is_error);

        drop(client);
        h.finish().await;
    }

    /// The acceptance case: refused by the *parent*, and the MCP server
    /// is never contacted — shown by ordering, since the refusal is
    /// received before the next granted call is made and the server logs
    /// calls as it receives them.
    #[tokio::test]
    async fn a_call_outside_the_allowlist_is_refused_and_never_reaches_the_server() {
        let h = Harness::new("deny").await;
        h.server.grant("a1", ["mcp__fake__echo"]);

        let mut raw = Raw::connect(h.path()).await;
        assert!(raw.hello("a1").await.get("result").is_some());

        for (i, name) in ["mcp__fake__slow", "mcp__fake__other", "slow", "mcp__fake__missing"]
            .into_iter()
            .enumerate()
        {
            let reply = raw
                .rpc(10 + i as i64, TOOLS_CALL, json!({"name": name, "arguments": {}}))
                .await;
            assert_eq!(code(&reply), i64::from(DENIED), "{name}: {reply}");
        }
        // The connection survives a refusal, and a granted call still works.
        let ok = raw.rpc(20, TOOLS_CALL, json!({"name":"mcp__fake__echo","arguments":{}})).await;
        assert!(ok.get("result").is_some(), "{ok}");

        let log = h.log();
        assert_eq!(log.matches("tools/call").count(), 1, "the server saw {log}");
        assert!(log.contains("\"name\":\"echo\""), "{log}");

        // The typed client reports the same refusal as a distinct error.
        let client = IpcClient::connect(h.path(), "a2-unregistered").await;
        assert!(matches!(client, Err(IpcError::Refused(_))), "{:?}", client.err());

        drop(raw);
        h.finish().await;
    }

    #[tokio::test]
    async fn a_client_cannot_widen_its_own_allowlist() {
        let h = Harness::new("widen").await;
        h.server.grant("a1", ["mcp__fake__echo"]);

        // Every field a hostile client might hope the parent reads.
        let mut raw = Raw::connect(h.path()).await;
        let reply = raw
            .rpc(
                0,
                HELLO,
                json!({
                    "agentId": "a1",
                    "tools": ["mcp__fake__slow"],
                    "allowed": ["mcp__fake__slow"],
                    "mcp_tools": ["mcp__fake__slow"],
                }),
            )
            .await;
        assert!(reply.get("result").is_some(), "{reply}");

        let listed = raw.rpc(1, TOOLS_LIST, json!({})).await;
        let names: Vec<_> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["mcp__fake__echo"]);

        // Nor does the call itself carry any weight about authority.
        let reply = raw
            .rpc(
                2,
                TOOLS_CALL,
                json!({"name":"mcp__fake__slow","arguments":{},"agentId":"a1","tools":["mcp__fake__slow"]}),
            )
            .await;
        assert_eq!(code(&reply), i64::from(DENIED), "{reply}");

        // There is no method that changes a grant.
        let reply = raw.rpc(3, "nanopi/grant", json!({"agentId":"a1","tools":["mcp__fake__slow"]})).await;
        assert_eq!(code(&reply), i64::from(error_code::METHOD_NOT_FOUND), "{reply}");
        let reply = raw.rpc(4, TOOLS_CALL, json!({"name":"mcp__fake__slow","arguments":{}})).await;
        assert_eq!(code(&reply), i64::from(DENIED), "{reply}");

        assert_eq!(h.log().matches("tools/call").count(), 0, "{}", h.log());
        drop(raw);
        h.finish().await;
    }

    #[tokio::test]
    async fn a_handshake_claiming_another_agents_id_is_refused() {
        let h = Harness::new("claim").await;
        h.server.grant("a1", ["mcp__fake__echo"]);
        h.server.grant("a2", ["mcp__fake__slow"]);

        // An id nobody was dispatched under.
        let mut raw = Raw::connect(h.path()).await;
        assert_eq!(code(&raw.hello("a99").await), i64::from(DENIED));
        assert!(raw.recv().await.is_none(), "a refused connection is closed");

        // Nothing but a hello is accepted first.
        let mut raw = Raw::connect(h.path()).await;
        let reply = raw.rpc(1, TOOLS_CALL, json!({"name":"mcp__fake__echo","arguments":{}})).await;
        assert_eq!(code(&reply), i64::from(error_code::INVALID_REQUEST), "{reply}");
        assert!(raw.recv().await.is_none());

        // a1 is live; a second connection saying it is a1 is refused,
        // with the same words as an unknown id.
        let first = IpcClient::connect(h.path(), "a1").await.expect("a1");
        let mut raw = Raw::connect(h.path()).await;
        let dup = raw.hello("a1").await;
        let mut unknown = Raw::connect(h.path()).await;
        assert_eq!(dup["error"], unknown.hello("a99").await["error"], "the refusals differ");
        assert_eq!(h.server.connected_agents(), ["a1"]);

        // And the id is free again once its holder is gone.
        drop(first);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            match IpcClient::connect(h.path(), "a1").await {
                Ok(_) => break,
                Err(IpcError::Refused(_)) => {
                    assert!(Instant::now() < until, "a1 was never released");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("{e}"),
            }
        }
        h.finish().await;
    }

    /// The grant is bound to the pid the parent spawned, so a sibling
    /// that learned the id cannot use it — the kernel, not the peer,
    /// says who is connecting.
    #[tokio::test]
    async fn a_grant_bound_to_a_process_refuses_everyone_else() {
        let h = Harness::new("bind").await;
        let mut other = std::process::Command::new("sleep").arg("60").spawn().expect("sleep");

        h.server.grant("a1", ["mcp__fake__echo"]);
        h.server.bind_process("a1", other.id());
        let refused = IpcClient::connect(h.path(), "a1").await;
        assert!(matches!(refused, Err(IpcError::Refused(_))), "{:?}", refused.err());

        // Bound to this process instead, the same connection is accepted.
        h.server.bind_process("a1", std::process::id());
        let client = IpcClient::connect(h.path(), "a1").await.expect("bound to us");
        assert!(client.call_tool("mcp__fake__echo", args()).await.is_ok());
        drop(client);

        // A re-grant (the dispatch path adjusting the list) keeps the
        // binding rather than quietly dropping it.
        h.server.bind_process("a1", other.id());
        h.server.grant("a1", ["mcp__fake__echo", "mcp__fake__slow"]);
        let refused = IpcClient::connect(h.path(), "a1").await;
        assert!(matches!(refused, Err(IpcError::Refused(_))), "{:?}", refused.err());

        let _ = other.kill();
        let _ = other.wait();
        h.finish().await;
    }

    #[tokio::test]
    async fn a_revoked_agent_is_refused_from_then_on() {
        let h = Harness::new("revoke").await;
        h.server.grant("a1", ["mcp__fake__echo"]);
        let client = IpcClient::connect(h.path(), "a1").await.expect("connect");
        assert!(client.call_tool("mcp__fake__echo", args()).await.is_ok());

        h.server.revoke("a1");
        let err = client.call_tool("mcp__fake__echo", args()).await.unwrap_err();
        assert!(matches!(err, IpcError::Denied(_)), "{err:?}");
        assert!(matches!(client.list_tools().await, Err(IpcError::Denied(_))));
        drop(client);
        h.finish().await;
    }

    /// A child must not learn how a server was launched or what is in its
    /// `env` — not from a reply, and not from an error.
    #[tokio::test]
    async fn nothing_a_child_can_see_names_the_servers_env_or_command() {
        let h = Harness::new("leak").await;
        h.server.grant("a1", ["mcp__fake__echo"]);

        let mut raw = Raw::connect(h.path()).await;
        raw.hello("a1").await;
        raw.rpc(1, TOOLS_LIST, json!({})).await;
        raw.rpc(2, TOOLS_CALL, json!({"name":"mcp__fake__echo","arguments":{}})).await;
        raw.rpc(3, TOOLS_CALL, json!({"name":"mcp__fake__other","arguments":{}})).await;
        raw.rpc(4, "tools/nope", json!({})).await;
        raw.rpc(5, TOOLS_CALL, json!("not an object")).await;

        assert!(raw.seen.lines().count() >= 6, "{}", raw.seen);
        for forbidden in [ENV_SECRET, LAUNCH_MARKER, "API_TOKEN", "sed -n", "handle_call"] {
            assert!(!raw.seen.contains(forbidden), "{forbidden:?} leaked: {}", raw.seen);
        }
        drop(raw);
        h.finish().await;
    }

    #[tokio::test]
    async fn an_unreachable_socket_fails_fast_and_says_why() {
        let dir = scratch("unreach");

        let started = Instant::now();
        let err = IpcClient::connect(&dir.join("nothing.sock"), "a1").await.err().expect("must fail");
        assert!(matches!(err, IpcError::Unreachable(_)), "{err:?}");
        assert!(err.to_string().contains("nothing.sock"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());

        // A stale socket file — the parent died without cleaning up — is
        // refused by the kernel at once, not hung on.
        let stale = dir.join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).expect("bind"));
        assert!(stale.exists());
        let started = Instant::now();
        let err = IpcClient::connect(&stale, "a1").await.err().expect("must fail");
        // `Closed` is accepted as well: a sibling test forking in the
        // instant before `drop` holds the listener fd until its `exec`,
        // so the connect can succeed and then see EOF. Either way it is
        // immediate, which is what is being pinned.
        assert!(matches!(err, IpcError::Unreachable(_) | IpcError::Closed(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());

        // No variable at all is the commonest case, and names itself.
        std::env::remove_var(SOCKET_ENV);
        let err = IpcClient::from_env().await.err().expect("must fail");
        assert!(matches!(err, IpcError::Unreachable(_)), "{err:?}");
        assert!(err.to_string().contains(SOCKET_ENV), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A parent that accepts and then says nothing is bounded by the
    /// handshake timeout, not by the (ten-minute) call timeout.
    #[tokio::test]
    async fn a_parent_that_never_answers_the_handshake_is_given_up_on() {
        let dir = scratch("silent");
        let path = dir.join("silent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");

        let started = Instant::now();
        let err = IpcClient::connect_with(
            &path,
            "a1",
            ClientOptions { handshake_timeout: Duration::from_millis(300), ..Default::default() },
        )
        .await
        .err()
        .expect("must fail");
        assert!(matches!(err, IpcError::Unreachable(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The acceptance case for hazard 2: one call parked inside the MCP
    /// server delays neither another child nor the same child's next
    /// call.
    #[tokio::test]
    async fn one_slow_call_does_not_delay_another_child_or_the_same_one() {
        let h = Harness::new("slow").await;
        h.server.grant("a1", ["mcp__fake__slow", "mcp__fake__echo"]);
        h.server.grant("a2", ["mcp__fake__echo"]);
        let a1 = Arc::new(IpcClient::connect(h.path(), "a1").await.expect("a1"));
        let a2 = IpcClient::connect(h.path(), "a2").await.expect("a2");

        let parked = {
            let a1 = Arc::clone(&a1);
            tokio::spawn(async move { a1.call_tool("mcp__fake__slow", args()).await })
        };
        // The call is inside the server now: from here on it is blocking
        // something, not merely queued.
        h.wait_log("\"name\":\"slow\"").await;

        let other_child = tokio::time::timeout(
            Duration::from_secs(5),
            a2.call_tool("mcp__fake__echo", args()),
        )
        .await
        .expect("another child waited behind a slow call")
        .expect("call");
        assert_eq!(text(&other_child), "ran echo");

        let same_child = tokio::time::timeout(
            Duration::from_secs(5),
            a1.call_tool("mcp__fake__echo", args()),
        )
        .await
        .expect("the same child's next call waited behind its slow one")
        .expect("call");
        assert_eq!(text(&same_child), "ran echo");
        assert!(!parked.is_finished(), "the slow call finished before it was released");

        h.release();
        let slow = parked.await.expect("join").expect("slow call");
        assert_eq!(text(&slow), "ran slow");
        drop(a1);
        drop(a2);
        h.finish().await;
    }

    /// Hazard 1: a child that goes away mid-call (`stop_agent` killed it)
    /// must not leave the call running in the server.
    #[tokio::test]
    async fn a_connection_dropping_mid_call_cancels_the_call_at_the_server() {
        let h = Harness::new("drop").await;
        h.server.grant("a1", ["mcp__fake__slow"]);
        let client = Arc::new(IpcClient::connect(h.path(), "a1").await.expect("connect"));

        let call = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.call_tool("mcp__fake__slow", args()).await })
        };
        let log = h.wait_log("\"name\":\"slow\"").await;
        let sent: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();

        // What a killed child looks like from here: the socket closes.
        call.abort();
        let _ = call.await;
        drop(client);

        let log = h.wait_log("notifications/cancelled").await;
        let cancelled: Value =
            serde_json::from_str(log.lines().find(|l| l.contains("cancelled")).unwrap()).unwrap();
        assert_eq!(cancelled["params"]["requestId"], sent["id"], "cancelled the wrong request");

        // The parent noticed, freed the id, and still serves others.
        let until = Instant::now() + Duration::from_secs(10);
        while !h.server.connected_agents().is_empty() {
            assert!(Instant::now() < until, "the dead connection was never reaped");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        h.server.grant("a2", ["mcp__fake__echo"]);
        let a2 = IpcClient::connect(h.path(), "a2").await.expect("a2");
        assert_eq!(text(&a2.call_tool("mcp__fake__echo", args()).await.expect("call")), "ran echo");
        drop(a2);
        h.release();
        h.finish().await;
    }

    /// A client that vanishes without a word, or says something that is
    /// not a message, costs the parent that connection and nothing else.
    #[tokio::test]
    async fn broken_clients_cost_only_their_own_connection() {
        let h = Harness::new("broken").await;
        h.server.grant("a1", ["mcp__fake__slow", "mcp__fake__echo"]);

        // Hangs up with a request in flight, never reading the reply.
        let mut raw = Raw::connect(h.path()).await;
        raw.hello("a1").await;
        raw.send(json!({"jsonrpc":"2.0","id":1,"method":TOOLS_CALL,"params":{"name":"mcp__fake__slow","arguments":{}}})).await;
        h.wait_log("\"name\":\"slow\"").await;
        drop(raw);
        h.wait_log("notifications/cancelled").await;

        // Hangs up halfway through a line.
        let mut raw = Raw::connect(h.path()).await;
        raw.hello("a1").await;
        raw.write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"meth").await.unwrap();
        drop(raw);

        // Sends something that is not JSON-RPC at all.
        let mut raw = Raw::connect(h.path()).await;
        raw.hello("a1").await;
        raw.write.write_all(b"this is not json\n").await.unwrap();
        assert!(raw.recv().await.is_none(), "the connection should be closed");

        // Sends a line that never ends: the parent must stop reading it.
        let mut raw = Raw::connect(h.path()).await;
        raw.hello("a1").await;
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..(MAX_REQUEST_LINE / chunk.len() + 2) {
            if raw.write.write_all(&chunk).await.is_err() {
                break;
            }
        }
        assert!(raw.recv().await.is_none(), "an unbounded line was not cut off");

        // More calls than the connection may have in flight.
        let mut raw = Raw::connect(h.path()).await;
        raw.hello("a1").await;
        for id in 0..(MAX_IN_FLIGHT as i64 + 4) {
            raw.send(json!({"jsonrpc":"2.0","id":100+id,"method":TOOLS_CALL,"params":{"name":"mcp__fake__slow","arguments":{}}})).await;
        }
        let busy = raw.recv().await.expect("a refusal");
        assert!(busy["error"]["message"].as_str().unwrap().contains("too many"), "{busy}");
        drop(raw);

        // Through all of it the parent still serves a well-behaved child.
        let until = Instant::now() + Duration::from_secs(10);
        let client = loop {
            match IpcClient::connect(h.path(), "a1").await {
                Ok(c) => break c,
                Err(IpcError::Refused(_)) => {
                    assert!(Instant::now() < until, "the id was never released");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("{e}"),
            }
        };
        assert_eq!(text(&client.call_tool("mcp__fake__echo", args()).await.expect("call")), "ran echo");
        drop(client);
        h.release();
        h.finish().await;
    }

    /// Parent death, as a child sees it: the socket closes, and the call
    /// that was waiting fails at once and says so, instead of waiting
    /// out its timeout.
    #[tokio::test]
    async fn a_call_in_flight_fails_cleanly_when_the_parent_goes_away() {
        let h = Harness::new("parentgone").await;
        h.server.grant("a1", ["mcp__fake__slow"]);
        let client = Arc::new(IpcClient::connect(h.path(), "a1").await.expect("connect"));

        let call = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.call_tool("mcp__fake__slow", args()).await })
        };
        h.wait_log("\"name\":\"slow\"").await;

        h.server.shutdown();
        let started = Instant::now();
        let err = call.await.expect("join").unwrap_err();
        assert!(matches!(err, IpcError::Closed(_)), "{err:?}");
        assert!(err.to_string().contains("parent"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());

        // And the channel stays closed, rather than parking the next call.
        assert!(client.close_reason().is_some());
        assert!(matches!(
            client.call_tool("mcp__fake__slow", args()).await,
            Err(IpcError::Closed(_))
        ));
        // The in-flight call was cancelled at the server as well.
        h.wait_log("notifications/cancelled").await;
        drop(client);
        h.release();
        h.finish().await;
    }

    #[tokio::test]
    async fn shutdown_removes_the_socket_and_its_directory() {
        let h = Harness::new("cleanup").await;
        let socket = h.path().to_path_buf();
        let dir = socket.parent().unwrap().to_path_buf();
        assert!(socket.exists());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "other users must not be able to enter it");
        assert_eq!(mode(&socket), 0o600);
        assert!(
            socket.as_os_str().len() < SUN_PATH_BUDGET,
            "{} bytes: {}",
            socket.as_os_str().len(),
            socket.display()
        );

        h.server.shutdown();
        assert!(!socket.exists(), "the socket file survived shutdown");
        assert!(!dir.exists(), "the socket directory survived shutdown");
        // Idempotent, and nothing is listening any more.
        h.server.shutdown();
        assert!(IpcClient::connect(&socket, "a1").await.is_err());
        h.finish().await;
    }

    /// The SIGTERM path: `main.rs` drops the whole session future, so
    /// only a destructor runs.
    #[tokio::test]
    async fn dropping_the_server_cleans_up_too() {
        let h = Harness::new("dropclean").await;
        let dir = h.path().parent().unwrap().to_path_buf();
        let Harness { manager, server, dir: scratch_dir } = h;
        drop(server);
        assert!(!dir.exists(), "the socket directory survived a drop");
        manager.shutdown().await;
        let _ = std::fs::remove_dir_all(scratch_dir);
    }

    #[tokio::test]
    async fn the_socket_is_a_short_path_even_under_a_long_tmpdir() {
        let manager = McpManager::empty();
        // Far past any `sun_path`. Falls back to `/tmp`, and the server
        // is reachable there.
        let root = scratch("long");
        let mut long = root.clone();
        for _ in 0..6 {
            long = long.join("a-rather-long-directory-name");
        }
        std::fs::create_dir_all(&long).unwrap();
        assert!(long.as_os_str().len() > 150);
        let server = IpcServer::start_in(&long, &manager).expect("falls back to /tmp");
        assert!(server.socket_path().starts_with("/tmp"), "{}", server.socket_path().display());
        server.grant("a1", Vec::<String>::new());
        assert!(IpcClient::connect(server.socket_path(), "a1").await.is_ok());
        let dir = server.socket_path().parent().unwrap().to_path_buf();
        drop(server);
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn two_servers_in_one_process_do_not_collide() {
        let base = scratch("two");
        let manager = McpManager::empty();
        let a = IpcServer::start_in(&base, &manager).unwrap();
        let b = IpcServer::start_in(&base, &manager).unwrap();
        assert_ne!(a.socket_path(), b.socket_path());
        drop((a, b));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A directory planted under the name we would have chosen is
    /// stepped over, never reused: `mkdir` fails on it, so it can never
    /// be one this call made.
    #[tokio::test]
    async fn a_pre_existing_directory_is_never_reused() {
        let base = scratch("squat");
        let squatter = base.join(format!("nanopi-{}", std::process::id()));
        std::fs::create_dir(&squatter).unwrap();
        let server = IpcServer::start_in(&base, &McpManager::empty()).unwrap();
        assert!(!server.socket_path().starts_with(&squatter));
        drop(server);
        assert!(squatter.exists(), "someone else's directory must be left alone");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The only test that installs the process-wide server, so nothing
    /// else can observe the global mid-way.
    #[tokio::test]
    async fn the_session_wiring_hands_children_the_socket_and_takes_it_away() {
        // Nothing to proxy: no socket, no directory, no global.
        assert!(start_for_session(&McpManager::empty()).is_none());
        assert!(global().is_none());

        let h = Harness::new("session").await;
        let guard = start_for_session(&h.manager).expect("a manager with tools starts a channel");
        let server = guard.server().clone();
        assert!(global().is_some());

        let env = crate::tool::agent::build_child_env(&Default::default(), "a1");
        let socket = env
            .iter()
            .find(|(k, _)| k == SOCKET_ENV)
            .map(|(_, v)| v.clone())
            .expect("the child is told where the channel is");
        assert_eq!(Path::new(&socket), server.socket_path());
        assert!(env.iter().any(|(k, v)| k == "NANOPI_AGENT_ID" && v == "a1"));

        // The explicit stop used by Ctrl-C and normal exit.
        shutdown_global();
        assert!(global().is_none());
        assert!(!Path::new(&socket).exists());
        let env = crate::tool::agent::build_child_env(&Default::default(), "a1");
        assert!(!env.iter().any(|(k, _)| k == SOCKET_ENV), "a stopped channel must not be advertised");

        drop(guard);
        drop(server);
        h.finish().await;
    }
}

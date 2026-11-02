//! stdio transport: the MCP server as a child process, newline-delimited
//! JSON-RPC over its stdin/stdout.
//!
//! The transport's whole job is to turn one pipe into many independent
//! request futures. A server multiplexes replies, its own notifications
//! and its own requests onto a single stdout in whatever order it
//! likes, and the spec explicitly allows it to answer out of order — so
//! a reader task owns stdout alone and routes by `id` through a pending
//! map. Nothing else reads the pipe; two readers would race for lines.
//!
//! **Three things here exist because of how this fails in the field.**
//!
//! *A server that dies must fail its callers, not hang them.* An `npx`
//! server that cannot resolve its package exits in under a second, and
//! everything already in flight has to learn that immediately. Every
//! path that notices the pipe is gone — EOF, a read error, explicit
//! shutdown — ends at [`Shared::close`], which drains the pending map
//! and errors every waiter. A request that arrives after that is
//! rejected at registration rather than parked on a channel nobody will
//! ever send to.
//!
//! *stderr is the only diagnostic.* Servers log there by design (the
//! spec reserves stdout for protocol traffic), and when one dies during
//! startup its stderr is the entire explanation. It is drained
//! continuously — a server that fills the pipe buffer and blocks on
//! `write` would otherwise deadlock — and kept as a bounded tail.
//!
//! *A server is a bare subprocess with the user's full privileges.*
//! Unlike a WASM plugin it is not sandboxed (`docs/mcp.md`), so the
//! only hygiene available is making sure it cannot outlive us:
//! `process_group(0)` so it can be killed as a group, `PDEATHSIG` so a
//! hard crash still reaps it, and [`ChildGuard`] so an early return or
//! a cancelled future does too.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::agent_registry::ChildGuard;
use crate::mcp::config::{ServerConfig, ServerSpec};
use crate::mcp::protocol::method::{CANCELLED, INITIALIZE};
// Re-exported so `stdio::TransportError` keeps working for existing
// importers; the type itself is shared with the HTTP transport.
pub use crate::mcp::transport::TransportError;
use crate::mcp::protocol::{
    error_code, Incoming, Notification, Request, RequestId, Response, ResponsePayload, RpcError,
};

/// Default ceiling on a single request. Generous because `tools/call`
/// reaches real networks — a GitHub search or a browser automation step
/// legitimately takes tens of seconds, and a timeout that fires first
/// looks to the model exactly like a broken tool. Callers that need a
/// tighter bound (M3's `initialize`, which must not stall startup) pass
/// their own via [`StdioTransport::request_with_timeout`].
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Bytes of stderr retained. A tail, not a head: when a server dies the
/// last thing it printed is the cause, while the first thing is usually
/// a banner.
const STDERR_CAP: usize = 8 * 1024;

/// Bytes of non-protocol stdout retained, under the same cap and for
/// the same reason — see [`Shared::diagnose`].
const STDOUT_NOISE_CAP: usize = 4 * 1024;

/// A stdout line longer than this is treated as a runaway rather than a
/// message. The limit is high because a legitimate `image` content
/// block is base64 and routinely megabytes; it exists only so a server
/// emitting an endless line cannot grow the buffer without bound.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// How long the server gets to exit after its stdin is closed before we
/// start signalling. The spec names stdin EOF as the graceful stop, and
/// a well-behaved server takes milliseconds; this is slack for one that
/// flushes state on the way out.
const STDIN_CLOSE_GRACE: Duration = Duration::from_millis(1000);

/// How long SIGTERM gets before SIGKILL.
const SIGTERM_GRACE: Duration = Duration::from_millis(500);

/// After stdout hits EOF the process is almost always already gone;
/// this is just enough slack to read its exit status and put it in the
/// failure message, rather than reporting the vaguer "closed stdout".
const EXIT_AFTER_EOF_GRACE: Duration = Duration::from_millis(200);

/// How long a failing request waits for stderr to finish arriving —
/// see [`close_with_stderr`].
const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// How long a failed write waits for the reader's better explanation.
/// Must exceed what the reader itself spends establishing one
/// ([`EXIT_AFTER_EOF_GRACE`] plus [`STDERR_DRAIN_GRACE`]), with slack
/// for a loaded machine.
const WRITE_FAILURE_GRACE: Duration = Duration::from_secs(2);

const EXIT_POLL: Duration = Duration::from_millis(10);

// ------------------------------------------------------- shared state

type Reply = Result<Value, TransportError>;

/// The pending map and the closed flag are one lock, not two, because
/// the dangerous interleaving is between them: a reader task that
/// observes EOF and drains the map, then a `request` that inserts into
/// it, leaves a caller waiting on a channel whose sender nobody holds —
/// it would hang until its timeout with the answer ("the server is
/// dead") already known. Making "closed" a state of the map itself
/// makes that unrepresentable.
enum Slots {
    Open(HashMap<RequestId, oneshot::Sender<Reply>>),
    Closed(String),
}

struct Shared {
    slots: Mutex<Slots>,
    /// stderr tail plus anything stdout produced that was not a
    /// message. Rendered as one view, because the user's question is
    /// "what did this server say", not "which pipe did it say it on".
    diagnostics: Mutex<Diagnostics>,
}

impl Shared {
    fn new() -> Self {
        Self {
            slots: Mutex::new(Slots::Open(HashMap::new())),
            diagnostics: Mutex::new(Diagnostics::new()),
        }
    }

    fn register(&self, id: RequestId, tx: oneshot::Sender<Reply>) -> Result<(), TransportError> {
        match &mut *self.slots.lock().unwrap() {
            Slots::Open(map) => {
                map.insert(id, tx);
                Ok(())
            }
            Slots::Closed(reason) => Err(TransportError::Closed(reason.clone())),
        }
    }

    fn forget(&self, id: &RequestId) {
        if let Slots::Open(map) = &mut *self.slots.lock().unwrap() {
            map.remove(id);
        }
    }

    /// Hand a reply to whoever is waiting for it. An id we do not
    /// recognise is not dropped silently: it is either a late answer to
    /// a request that already timed out or a server echoing an id it
    /// invented, and both are worth seeing when debugging why a server
    /// "does nothing".
    fn deliver(&self, id: RequestId, reply: Reply) {
        let waiter = match &mut *self.slots.lock().unwrap() {
            Slots::Open(map) => map.remove(&id),
            Slots::Closed(_) => None,
        };
        match waiter {
            Some(tx) => {
                let _ = tx.send(reply);
            }
            None => self.diagnose(Source::Stdout, format!("reply to unknown request id {id}")),
        }
    }

    /// Fail every pending request and refuse further ones.
    ///
    /// The *first* reason wins. Once the pipe breaks every later
    /// observation ("stdin closed", "shut down") is a consequence of
    /// the same event, and the first one is the only one that says what
    /// actually happened.
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
            let _ = tx.send(Err(TransportError::Closed(reason.clone())));
        }
    }

    fn close_reason(&self) -> Option<String> {
        match &*self.slots.lock().unwrap() {
            Slots::Open(_) => None,
            Slots::Closed(r) => Some(r.clone()),
        }
    }

    fn diagnose(&self, source: Source, line: String) {
        self.diagnostics.lock().unwrap().push(source, line);
    }
}

/// Which pipe a diagnostic line came off. Explicit rather than sniffed
/// from the text, so a server that legitimately prints `[stdout]` on
/// stderr cannot be filed under the wrong budget.
#[derive(Clone, Copy)]
enum Source {
    Stderr,
    Stdout,
}

/// A bounded tail of lines, with a separate budget per pipe: a server
/// chattering on the wrong one must not evict the stderr line that
/// explains why it died.
struct Diagnostics {
    stderr: Tail,
    stdout: Tail,
}

impl Diagnostics {
    fn new() -> Self {
        Self {
            stderr: Tail::new(STDERR_CAP),
            stdout: Tail::new(STDOUT_NOISE_CAP),
        }
    }

    fn push(&mut self, source: Source, line: String) {
        match source {
            Source::Stderr => self.stderr.push(line),
            Source::Stdout => self.stdout.push(format!("[stdout] {line}")),
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for line in self.stderr.lines.iter().chain(self.stdout.lines.iter()) {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

struct Tail {
    lines: VecDeque<String>,
    bytes: usize,
    cap: usize,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            cap,
        }
    }

    fn push(&mut self, line: String) {
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.bytes > self.cap && self.lines.len() > 1 {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len() + 1;
            }
        }
    }
}

// ----------------------------------------------------------- transport

/// Tunables a caller may want to differ per server.
#[derive(Debug, Clone)]
pub struct TransportOptions {
    pub request_timeout: Duration,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// A live stdio connection to one MCP server.
///
/// Every method takes `&self` so M3 can hold this in an `Arc` and let
/// concurrent tool calls share the one pipe; the writer is serialised
/// internally so two calls cannot interleave halves of a line.
pub struct StdioTransport {
    name: String,
    /// `Some` until [`shutdown`](Self::shutdown) reaps the child.
    /// Shared with the reader task, which needs it to turn an EOF into
    /// an exit status — see [`exit_reason`].
    child: Arc<AsyncMutex<Option<Child>>>,
    /// Also the process-group id, since we spawn with `process_group(0)`.
    pid: Option<u32>,
    /// `None` once closed. Dropping the handle is how the server is
    /// told to exit, so this must be takeable, not just writable.
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    shared: Arc<Shared>,
    /// Disarmed by a successful shutdown; otherwise it SIGKILLs the
    /// whole group on drop, which covers panics, `?` early returns and
    /// a cancelled startup future.
    guard: Mutex<ChildGuard>,
    reader: JoinHandle<()>,
    stderr: JoinHandle<()>,
    next_id: AtomicI64,
    options: TransportOptions,
}

impl StdioTransport {
    /// Spawn `config`'s server and start reading it.
    ///
    /// Returns `Err` only if the process could not be started at all. A
    /// server that starts and then immediately dies is *not* an error
    /// here — it is reported by the first request, with its stderr
    /// attached, because that is the only place the reason exists.
    pub async fn spawn(config: &ServerConfig) -> Result<Self, TransportError> {
        Self::spawn_with(config, TransportOptions::default()).await
    }

    pub async fn spawn_with(
        config: &ServerConfig,
        options: TransportOptions,
    ) -> Result<Self, TransportError> {
        let ServerSpec::Stdio { command: program, args, env } = &config.spec else {
            // Unreachable through `Transport::connect`, which dispatches on
            // the spec; here so a direct caller gets an error, not a spawn
            // of nothing.
            return Err(TransportError::Unsupported(format!(
                "server \"{}\" is not a stdio server",
                config.name
            )));
        };
        let mut command = build_command(program, args, env);
        let mut child = command.spawn().map_err(|source| TransportError::Spawn {
            command: program.clone(),
            source,
        })?;
        let pid = child.id();
        let guard = ChildGuard::new(pid);

        // `take` rather than `expect`: all three are `piped()` above, so
        // `None` is impossible, but an unwrap here would panic the whole
        // agent over a third-party process.
        let (Some(stdout), Some(stderr_pipe), Some(stdin)) =
            (child.stdout.take(), child.stderr.take(), child.stdin.take())
        else {
            return Err(TransportError::Closed(format!(
                "server \"{}\" did not expose stdio",
                config.name
            )));
        };

        let shared = Arc::new(Shared::new());
        let stdin = Arc::new(AsyncMutex::new(Some(stdin)));
        let child = Arc::new(AsyncMutex::new(Some(child)));

        // Lets the reader wait for stderr to reach EOF before it fails
        // pending requests — see `close_with_stderr`.
        let (done, stderr_drained) = oneshot::channel();
        let stderr_task = tokio::spawn(drain_stderr(stderr_pipe, Arc::clone(&shared), done));
        let reader = tokio::spawn(read_stdout(
            stdout,
            Arc::clone(&shared),
            Arc::clone(&stdin),
            Arc::clone(&child),
            stderr_drained,
        ));

        Ok(Self {
            name: config.name.clone(),
            child,
            pid,
            stdin,
            shared,
            guard: Mutex::new(guard),
            reader,
            stderr: stderr_task,
            // JSON-RPC forbids reusing an id while a request is in
            // flight; a monotonic counter is the cheapest guarantee and,
            // unlike a random id, makes a transcript readable.
            next_id: AtomicI64::new(1),
            options,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The child's pid, which is also its process-group id.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// `Some(reason)` once the connection is unusable. M3 maps this
    /// onto `Failed { reason }`.
    pub fn close_reason(&self) -> Option<String> {
        self.shared.close_reason()
    }

    /// Tail of everything the server printed outside the protocol —
    /// stderr, plus any stdout line that was not a message. Empty is
    /// normal and means the server has been quiet.
    pub fn diagnostics(&self) -> String {
        self.shared.diagnostics.lock().unwrap().render()
    }

    /// Send a request and wait for its reply, under the default timeout.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, TransportError> {
        self.request_with_timeout(method, params, self.options.request_timeout)
            .await
    }

    /// The result is the raw `result` member. It stays a `Value`
    /// because correlation has to happen before anyone knows which
    /// request this id belonged to, and therefore before the shape is
    /// known; M3 deserialises it.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, TransportError> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        // Encoded before anything is registered, so the two failures that
        // are our fault (`Encode`, `Framing`) leave nothing to clean up
        // and the connection stays usable.
        let line = encode_line(&Request::new(id.clone(), method, params))?;

        let (tx, rx) = oneshot::channel();
        // Registered before the write, so a reply that arrives while we
        // are still inside `write_all` has somewhere to go.
        self.shared.register(id.clone(), tx)?;

        // From here the request is in flight, and this future being
        // dropped rather than finishing means the caller no longer wants
        // the answer. See [`InFlight`].
        let mut in_flight = InFlight {
            id: id.clone(),
            shared: Arc::clone(&self.shared),
            stdin: Arc::clone(&self.stdin),
            write: None,
            // The spec forbids cancelling `initialize`; the only way it
            // is ever abandoned is startup giving up on the server,
            // which kills the process anyway.
            cancellable: method != INITIALIZE,
            armed: true,
        };

        // The write runs detached and is awaited through its handle. A
        // `write_all` dropped half way leaves a partial line in the pipe
        // and the next message is glued onto it — silent
        // desynchronisation of every later request, for every other
        // caller sharing this pipe. Abandoning a request is now an
        // ordinary event (the parent↔child channel aborts proxied calls
        // when a child goes away), so it must not be able to do that.
        let stdin = Arc::clone(&self.stdin);
        // The handle lives in `in_flight` while it is awaited, so a drop
        // during the write still lets the cancellation queue behind it.
        let write = in_flight
            .write
            .insert(tokio::spawn(async move { write_line(&stdin, &line).await }));
        let written = match write.await {
            Ok(result) => result,
            Err(join) => Err(TransportError::Closed(format!("the write task failed: {join}"))),
        };
        in_flight.write = None;
        if let Err(e) = written {
            // The pipe is gone. `EPIPE` is simply how a server that
            // has already exited presents itself on the write side,
            // and the reader task is a moment away from reporting
            // the same event with the exit status and the stderr
            // attached. Waiting for that rather than returning the
            // raw `Io` keeps "the server is dead" a single variant
            // — M3 would otherwise have to treat two as one, and
            // whichever it forgot would be the one users hit.
            in_flight.disarm();
            return match tokio::time::timeout(WRITE_FAILURE_GRACE, rx).await {
                Ok(Ok(reply)) => reply,
                _ => {
                    self.shared.forget(&id);
                    Err(TransportError::Closed(e.to_string()))
                }
            };
        }

        let outcome = tokio::time::timeout(timeout, rx).await;
        // Whatever happened, the request is finished: nothing to cancel.
        in_flight.disarm();
        match outcome {
            Ok(Ok(reply)) => reply,
            // The sender was dropped without sending. `close` always
            // sends, so this is only reachable if the reader task died
            // outright — report it as a dead connection rather than
            // silently succeeding.
            Ok(Err(_)) => Err(TransportError::Closed(
                self.shared
                    .close_reason()
                    .unwrap_or_else(|| "the transport stopped reading".to_string()),
            )),
            Err(_) => {
                // Drop the slot: the server may still answer, and
                // leaving the entry would leak one map slot per timeout
                // for the life of the connection.
                self.shared.forget(&id);
                Err(TransportError::Timeout {
                    method: method.to_string(),
                    timeout,
                })
            }
        }
    }

    /// Fire-and-forget. `Ok` means it reached the pipe, nothing more —
    /// the spec requires a server to stay silent about notifications it
    /// does not recognise, so there is no acknowledgement to wait for.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), TransportError> {
        send_line(&self.stdin, &Notification::new(method, params)).await
    }

    /// Stop the server and reap it. Idempotent.
    ///
    /// Escalates rather than killing outright: a server may be holding
    /// a lock, a temp file or a remote session, and stdin EOF is the
    /// stop the spec defines, so it gets that first. The group SIGKILL
    /// at the end is unconditional even when the server exited politely
    /// — a server that backgrounded a helper (`npx` spawning node, a
    /// headless browser) leaves it running otherwise, and that orphan
    /// is exactly what `process_group(0)` was for.
    pub async fn shutdown(&self) {
        {
            let mut stdin = self.stdin.lock().await;
            *stdin = None;
        }

        let mut slot = self.child.lock().await;
        let Some(child) = slot.as_mut() else {
            return;
        };

        if !wait_for_exit(child, STDIN_CLOSE_GRACE).await {
            terminate_group(self.pid);
            if !wait_for_exit(child, SIGTERM_GRACE).await {
                let _ = child.start_kill();
            }
        }
        // Accepts the same pid-reuse window as `agent_registry::kill_group`:
        // the child may already have been reaped above, freeing its pgid.
        // Anything still in the group keeps the pgid allocated, so the
        // only way to lose the race is for an unrelated process to become
        // a group leader with this exact id in between.
        kill_group(self.pid);
        let _ = child.wait().await;
        *slot = None;

        self.guard.lock().unwrap().disarm();
        self.shared
            .close(format!("server \"{}\" was shut down", self.name));
    }
}

/// Not derivable — `Shared` holds `oneshot::Sender`s and a live
/// `Child` — and these three fields are the ones anyone debugging a
/// server actually wants anyway.
impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioTransport")
            .field("name", &self.name)
            .field("pid", &self.pid)
            .field("closed", &self.close_reason())
            .finish()
    }
}

impl Drop for StdioTransport {
    /// The tasks hold clones of the shared state and would otherwise
    /// outlive the transport, blocked on a pipe whose owner is gone.
    /// `ChildGuard`'s own `Drop` then kills the process group.
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr.abort();
    }
}

// ------------------------------------------------------------- spawning

/// Build the child command.
///
/// **The child inherits nanopi's environment**, with the config's `env`
/// overlaid on top. This is the decision M4 deferred here, and it goes
/// this way because an MCP server is almost never a self-contained
/// binary: the ecosystem's standard invocations are `npx`, `uvx`,
/// `node`, `python`, `docker`. Every one of those needs `PATH` to be
/// found at all, and then `HOME`, the Node/Python install roots, the
/// proxy variables and the platform's CA bundle path to work. A
/// cleared environment turns `"command": "npx"` into "No such file or
/// directory" for every user, which is a support burden with no payoff:
/// an stdio server is an unsandboxed subprocess with the user's full
/// privileges (`docs/mcp.md`), so it can read `~/.aws/credentials`
/// whether or not we hand it `AWS_PROFILE`. Withholding environment
/// variables from something that can already read the whole home
/// directory is theatre. It also matches every other MCP client, which
/// is what makes a pasted-in config work unchanged — the point of
/// adopting the ecosystem file shape in the first place.
///
/// The two variables that are *not* inherited are nanopi's own
/// child-agent protocol. `NANOPI_PARENT_PID` makes a nanopi process
/// exit immediately when `getppid()` does not match it (`main.rs`), so
/// a server that shells out to `nanopi` — or is one — would die on
/// startup for no visible reason; `NANOPI_RUN_ID` would make it join
/// this run's agent registry and write into its archive. Both are
/// present in the environment whenever nanopi is itself a child agent,
/// which M6 makes a normal case.
fn build_command(program: &str, args: &[String], env: &BTreeMap<String, String>) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env)
        .env_remove("NANOPI_PARENT_PID")
        .env_remove("NANOPI_RUN_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Own process group, so the server can be killed together with
    // anything it backgrounds, and so a terminal SIGINT aimed at
    // nanopi's foreground group does not reach it mid-call — we decide
    // when it stops. Same reasoning as `tool::agent`'s spawn (RT-03).
    #[cfg(unix)]
    command.process_group(0);

    // Backstop for the case `ChildGuard` cannot cover: nanopi being
    // SIGKILLed or panicking the process, where no destructor runs.
    //
    // Linux-only — `prctl`/`PR_SET_PDEATHSIG` do not exist elsewhere,
    // and `libc` itself is declared under
    // `[target.'cfg(unix)'.dependencies]`, so touching it without a
    // guard breaks the Windows build (this repo has shipped that bug
    // once already, with `getppid`).
    //
    // Known limitation: the kernel ties PDEATHSIG to the *thread* that
    // forked, not the process, so a tokio worker thread retiring would
    // fire it early. Worker threads live as long as the runtime, so in
    // practice this only triggers on real process death — and the
    // normal path is explicit `shutdown`, not this.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs between fork and exec, where only
        // async-signal-safe calls are allowed. `prctl` is one, and
        // nothing else here allocates, locks or touches memory.
        unsafe {
            command.as_std_mut().pre_exec(|| {
                libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                );
                Ok(())
            });
        }
    }

    command
}

// --------------------------------------------------------------- tasks

/// Own stdout and route every line.
async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Shared>,
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    child: Arc<AsyncMutex<Option<Child>>>,
    stderr_drained: oneshot::Receiver<()>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // `read_until` + lossy decode rather than `lines()`, which
        // errors out on the first invalid UTF-8 byte and would take the
        // whole connection down. A server that writes one bad byte to
        // stdout should cost us that line, not the session.
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                close_with_stderr(&shared, stderr_drained, format!("reading from the server: {e}")).await;
                return;
            }
        }
        if buf.len() > MAX_LINE {
            let size = buf.len() / (1024 * 1024);
            let reason = format!("server sent a {size} MiB line, which is not a JSON-RPC message");
            close_with_stderr(&shared, stderr_drained, reason).await;
            return;
        }

        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches('\n').trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Incoming>(line) {
            Ok(Incoming::Response(response)) => deliver_response(&shared, response),
            Ok(Incoming::Request(request)) => {
                answer_unsupported(&stdin, &shared, request).await;
            }
            // `notifications/tools/list_changed` and the logging
            // notifications land here. Dropped on purpose: nothing in
            // M2 subscribes, and TODO.md defers re-listing to a turn
            // boundary. Not recorded either, because a server that logs
            // via notifications would flood the diagnostics buffer and
            // push out its own stderr.
            Ok(Incoming::Notification(_)) => {}
            Err(e) => {
                // `Incoming` is `untagged`, so serde's message here is
                // the useless "data did not match any variant". The raw
                // line is the only thing that identifies the real cause
                // — usually a wrapper script printing a banner, or a
                // server that logged to stdout instead of stderr.
                shared.diagnose(Source::Stdout, format!("not a JSON-RPC message ({e}): {line}"));
            }
        }
    }

    // EOF. Either the process is gone or it closed stdout while alive;
    // both mean no reply is ever coming, so no caller may keep waiting.
    let reason = exit_reason(&child).await;
    close_with_stderr(&shared, stderr_drained, reason).await;
}

/// Fail the pending requests, but not before the stderr that explains
/// why has made it into the buffer.
///
/// Without this the two pipes race: a server that dies on startup
/// closes stdout and stderr at the same instant, and whichever task is
/// polled first wins. Lose the race and the caller gets "the server
/// process exited (exit code 1)" with empty diagnostics — the one
/// moment the stderr was worth keeping is the one moment it is missing.
/// Bounded, because a server that closed stdout while still holding
/// stderr open must not stall the failure indefinitely.
async fn close_with_stderr(shared: &Shared, drained: oneshot::Receiver<()>, reason: String) {
    // The sender is never used; it resolves by being dropped when
    // `drain_stderr` returns, which is exactly "stderr hit EOF".
    let _ = tokio::time::timeout(STDERR_DRAIN_GRACE, drained).await;
    shared.close(reason);
}

fn deliver_response(shared: &Shared, response: Response) {
    let Some(id) = response.id else {
        // JSON-RPC mandates a null id when the request could not be
        // parsed far enough to echo one back, so this is uncorrelatable
        // by construction — and it is the one error that says *we* sent
        // something malformed, which is worth keeping.
        let body = match response.payload {
            ResponsePayload::Error(e) => e.to_string(),
            ResponsePayload::Result(v) => v.to_string(),
        };
        shared.diagnose(Source::Stdout, format!("response with no id: {body}"));
        return;
    };
    let reply = match response.payload {
        ResponsePayload::Result(v) => Ok(v),
        ResponsePayload::Error(e) => Err(TransportError::Rpc(e)),
    };
    shared.deliver(id, reply);
}

/// Refuse a server-initiated request.
///
/// nanopi advertises no client capabilities, so a correct server never
/// sends one — but `sampling/createMessage` and `elicitation/create`
/// are requests, meaning the server *blocks* until answered. Ignoring
/// it would hang that server forever, and with it every tool call
/// behind it. `-32601` is the spec's answer for a method we do not
/// implement and lets the server take its own fallback path.
async fn answer_unsupported(
    stdin: &AsyncMutex<Option<ChildStdin>>,
    shared: &Shared,
    request: Request,
) {
    let Request { id, method, .. } = request;
    let response = Response {
        id: Some(id),
        payload: ResponsePayload::Error(RpcError {
            code: error_code::METHOD_NOT_FOUND,
            message: format!("nanopi does not implement `{method}`"),
            data: None,
        }),
    };
    if let Err(e) = send_line(stdin, &response).await {
        // Recorded rather than escalated: the connection may well still
        // work for our own calls, and the visible symptom — a server
        // that goes quiet — is otherwise unexplainable.
        shared.diagnose(
            Source::Stdout,
            format!("could not refuse the server's `{method}` request: {e}"),
        );
    }
}

/// Keep stderr drained for the life of the process.
///
/// Draining is not optional even if nobody reads the result: a server
/// that writes more than the pipe buffer holds blocks in `write` and
/// then never answers a request, which presents as a hang with no
/// explanation.
async fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    shared: Arc<Shared>,
    _done: oneshot::Sender<()>,
) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        if !line.is_empty() {
            shared.diagnose(Source::Stderr, line.to_string());
        }
    }
}

// ------------------------------------------------------------- plumbing

/// Serialise one message and write it as a single line.
async fn send_line<T: Serialize>(
    stdin: &AsyncMutex<Option<ChildStdin>>,
    message: &T,
) -> Result<(), TransportError> {
    write_line(stdin, &encode_line(message)?).await
}

fn encode_line<T: Serialize>(message: &T) -> Result<String, TransportError> {
    let line = serde_json::to_string(message)?;
    // The spec forbids an embedded newline because the newline *is* the
    // frame. `serde_json` escapes `\n` inside strings, so this holds
    // today — checked rather than assumed because if it ever stopped
    // holding the failure would not be a parse error on this message,
    // it would be silent desynchronisation of every message after it.
    if line.contains('\n') || line.contains('\r') {
        return Err(TransportError::Framing);
    }
    Ok(line)
}

/// The lock spans the payload *and* its newline: two concurrent
/// `tools/call`s that interleaved would produce two unparseable lines
/// and desynchronise the stream permanently.
async fn write_line(
    stdin: &AsyncMutex<Option<ChildStdin>>,
    line: &str,
) -> Result<(), TransportError> {
    let mut guard = stdin.lock().await;
    let Some(writer) = guard.as_mut() else {
        return Err(TransportError::Closed(
            "the server's stdin is closed".to_string(),
        ));
    };
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// A request that has been (or is being) written and has not resolved.
/// Dropping it while still armed means the future that owned the request
/// was dropped before the reply — the caller gave up — and the server is
/// told so with `notifications/cancelled`.
///
/// **This exists because the server otherwise keeps working.** The id of
/// a request lives only inside
/// [`StdioTransport::request_with_timeout`], so nobody outside it could
/// ever name it in a cancellation. The consumer is the parent↔child
/// channel (`mcp::ipc`): when a child agent is stopped, the proxied tool
/// call it had in flight is dropped here, and without this a browser step
/// or a long query runs to completion for nobody. It also removes the
/// pending slot, which a dropped future used to leak.
///
/// Not armed for a timeout: the caller is still here and is told. A
/// timed-out call is therefore still not cancelled server-side, which is
/// the existing documented behaviour and is left alone.
struct InFlight {
    id: RequestId,
    shared: Arc<Shared>,
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    /// The detached write of the request itself while it may still be
    /// pending. The cancellation queues behind it so it cannot overtake
    /// the request it refers to — a `cancelled` for an id the server has
    /// not seen yet is ignored by spec, and the call would run anyway.
    write: Option<JoinHandle<Result<(), TransportError>>>,
    cancellable: bool,
    armed: bool,
}

impl InFlight {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.shared.forget(&self.id);
        if !self.cancellable {
            return;
        }
        // No runtime means the process is tearing down: nobody is left to
        // tell, and spawning would panic.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let write = self.write.take();
        let stdin = Arc::clone(&self.stdin);
        let params = serde_json::json!({
            "requestId": self.id,
            "reason": "the caller abandoned the request",
        });
        runtime.spawn(async move {
            if let Some(write) = write {
                let _ = write.await;
            }
            // Best effort: a dead pipe means the server is gone, which
            // cancels the work just as well.
            let _ = send_line(&stdin, &Notification::new(CANCELLED, Some(params))).await;
        });
    }
}

/// Poll for the child's exit for up to `grace`. `true` if it exited.
///
/// `try_wait` rather than `Child::wait` because tokio's `wait` drops
/// the child's stdin as a deadlock guard. That is the wrong side effect
/// for the caller that is still deciding whether to escalate, and for
/// the reader task, which must not close a pipe it does not own.
async fn wait_for_exit(child: &mut Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            // An error here means the child cannot be waited for at
            // all; treating it as "exited" avoids spinning to the
            // deadline on every shutdown.
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(EXIT_POLL).await;
    }
}

/// The message pending requests get when stdout closes.
///
/// Worth the extra moment it costs: "exited with code 1" plus the
/// stderr tail is a diagnosis, while "the connection closed" sends the
/// user looking in the wrong place.
///
/// It has to go through the `Child` rather than `kill(pid, 0)`, which
/// would be simpler and wrong — an exited child we have not reaped is a
/// zombie, and a zombie answers signal 0 exactly like a live process.
async fn exit_reason(child: &AsyncMutex<Option<Child>>) -> String {
    let mut slot = child.lock().await;
    let Some(child) = slot.as_mut() else {
        // `shutdown` took it, so this EOF is the one it caused.
        return "the server was shut down".to_string();
    };
    if !wait_for_exit(child, EXIT_AFTER_EOF_GRACE).await {
        return "the server closed its stdout while still running".to_string();
    }
    match child.try_wait() {
        Ok(Some(status)) => format!("the server process exited ({})", describe_exit(status)),
        _ => "the server process exited".to_string(),
    }
}

fn describe_exit(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // Not representable as a code, and the usual reason an `npx`
        // server vanishes without printing anything is the OOM killer.
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "no exit status".to_string(),
    }
}

/// SIGTERM the server's process group. No-op off unix.
fn terminate_group(pgid: Option<u32>) {
    signal_group(pgid, SignalKind::Term);
}

/// SIGKILL the server's process group, which is the only thing that
/// reaches a helper the server backgrounded. No-op off unix.
fn kill_group(pgid: Option<u32>) {
    signal_group(pgid, SignalKind::Kill);
}

enum SignalKind {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pgid: Option<u32>, kind: SignalKind) {
    // `pgid <= 1` would mean "every process we may signal" — never
    // reachable for a real child, and catastrophic if it were.
    if let Some(pgid) = pgid.filter(|p| *p > 1) {
        let sig = match kind {
            SignalKind::Term => libc::SIGTERM,
            SignalKind::Kill => libc::SIGKILL,
        };
        // SAFETY: plain syscall; no memory is touched. ESRCH (the group
        // is already gone) is the expected case and ignored.
        unsafe {
            libc::killpg(pgid as libc::pid_t, sig);
        }
    }
}

/// Windows has no process groups in this sense and no `libc` in the
/// dependency graph; `Child::start_kill` is the only lever, and the
/// shutdown path already pulls it.
#[cfg(not(unix))]
fn signal_group(_pgid: Option<u32>, _kind: SignalKind) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config(script: &str) -> ServerConfig {
        ServerConfig::stdio(
            "fake",
            "sh",
            vec!["-c".to_string(), script.to_string()],
            BTreeMap::new(),
        )
    }

    /// Echo every request back as a success whose `result` carries the
    /// id we were given, so the test proves correlation rather than
    /// "something came back".
    const ECHO: &str = r#"
        while IFS= read -r line; do
            id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            m=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"result":{"method":"%s"}}\n' "$id" "$m"
        done
    "#;

    /// `kill(pid, 0)` is true for a zombie too, so this is only a valid
    /// "no stray process" assertion after the transport has reaped.
    #[cfg(unix)]
    fn group_alive(pgid: u32) -> bool {
        unsafe { libc::killpg(pgid as libc::pid_t, 0) == 0 }
    }

    /// Every test ends here. A transport that shuts down cleanly must
    /// leave nothing behind — not the server, and not anything the
    /// server backgrounded.
    #[cfg(unix)]
    async fn assert_no_stray_process(transport: StdioTransport) {
        let pid = transport.pid().expect("spawned child has a pid");
        transport.shutdown().await;
        for _ in 0..200 {
            if !group_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("process group {pid} survived shutdown");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_request_is_answered_by_its_own_id() {
        let t = StdioTransport::spawn(&config(ECHO)).await.unwrap();

        let first = t.request("tools/list", None).await.unwrap();
        assert_eq!(first["method"], json!("tools/list"));
        // A second request proves the id counter advances and the
        // reader keeps routing after the first reply.
        let second = t.request("tools/call", Some(json!({"name": "x"}))).await.unwrap();
        assert_eq!(second["method"], json!("tools/call"));

        assert_no_stray_process(t).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_protocol_error_is_an_error_not_a_result() {
        let script = r#"
            while IFS= read -r line; do
                id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"Unknown tool"}}\n' "$id"
            done
        "#;
        let t = StdioTransport::spawn(&config(script)).await.unwrap();

        match t.request("tools/call", None).await {
            Err(TransportError::Rpc(e)) => {
                assert_eq!(e.code, error_code::INVALID_PARAMS);
                assert_eq!(e.message, "Unknown tool");
            }
            other => panic!("expected an RPC error, got {other:?}"),
        }

        assert_no_stray_process(t).await;
    }

    /// The failure mode that matters most: the request must resolve,
    /// and it must resolve long before its timeout would have fired.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_exits_mid_request_fails_it_instead_of_hanging() {
        let t = StdioTransport::spawn(&config("IFS= read -r line; exit 3"))
            .await
            .unwrap();

        let started = Instant::now();
        let err = t
            .request_with_timeout("tools/list", None, Duration::from_secs(30))
            .await
            .unwrap_err();

        assert!(
            matches!(err, TransportError::Closed(_)),
            "expected a closed connection, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?} — it waited for the timeout rather than noticing the death",
            started.elapsed()
        );
        // And the connection stays closed, rather than letting the next
        // caller park on a dead pipe.
        assert!(t.close_reason().is_some());
        assert!(matches!(
            t.request("tools/list", None).await,
            Err(TransportError::Closed(_))
        ));

        assert_no_stray_process(t).await;
    }

    /// A server that dies on startup says why on stderr and nowhere
    /// else, so that text has to survive to the failed request.
    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_survives_the_death_that_produced_it() {
        let t = StdioTransport::spawn(&config(
            "echo 'npm ERR! 404 Not Found: @scope/server' >&2; exit 1",
        ))
        .await
        .unwrap();

        let err = t
            .request_with_timeout("initialize", None, Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Closed(_)), "got {err:?}");
        assert!(
            t.diagnostics().contains("npm ERR! 404"),
            "diagnostics lost the cause: {:?}",
            t.diagnostics()
        );

        assert_no_stray_process(t).await;
    }

    /// The write side has its own way of noticing a dead server —
    /// `EPIPE` — and it must not leak out as a second spelling of the
    /// same condition.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_already_dead_before_the_write_still_reports_closed() {
        let t = StdioTransport::spawn(&config("echo 'boom' >&2; exit 7"))
            .await
            .unwrap();
        // Long enough that the write is guaranteed to hit a closed pipe
        // rather than a buffer nobody will ever read.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let err = t
            .request_with_timeout("initialize", None, Duration::from_secs(10))
            .await
            .unwrap_err();
        match err {
            TransportError::Closed(reason) => {
                assert!(reason.contains("exit code 7"), "lost the status: {reason}");
            }
            other => panic!("expected Closed, got {other:?}"),
        }
        assert!(t.diagnostics().contains("boom"), "{:?}", t.diagnostics());

        assert_no_stray_process(t).await;
    }

    /// Wrapper scripts print banners and some servers log to stdout by
    /// mistake. Neither may cost the connection — and the raw line has
    /// to be recoverable, because serde's untagged error alone ("data
    /// did not match any variant") identifies nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn garbage_on_stdout_is_recorded_and_survived() {
        let script = format!(
            "printf 'Debugger listening on ws://127.0.0.1:9229\\n'; \
             printf '{{\"broken\\n'; \
             printf '\\n'; \
             {ECHO}"
        );
        let t = StdioTransport::spawn(&config(&script)).await.unwrap();

        let result = t
            .request_with_timeout("tools/list", None, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(result["method"], json!("tools/list"));

        let diagnostics = t.diagnostics();
        assert!(
            diagnostics.contains("Debugger listening on ws://127.0.0.1:9229"),
            "the raw line was not kept: {diagnostics:?}"
        );
        assert!(diagnostics.contains("{\"broken"), "got {diagnostics:?}");

        assert_no_stray_process(t).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_request_the_server_never_answers_times_out() {
        // Consumes stdin so the write succeeds, then says nothing.
        let t = StdioTransport::spawn(&config("while IFS= read -r line; do :; done"))
            .await
            .unwrap();

        let err = t
            .request_with_timeout("tools/call", None, Duration::from_millis(150))
            .await
            .unwrap_err();
        match err {
            TransportError::Timeout { method, .. } => assert_eq!(method, "tools/call"),
            other => panic!("expected a timeout, got {other:?}"),
        }
        // A timeout is not a dead connection: the next request must
        // still be allowed through.
        assert!(t.close_reason().is_none());

        assert_no_stray_process(t).await;
    }

    /// A server that asks us something blocks until answered, so the
    /// reply has to carry both `-32601` and the server's own id.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_initiated_request_gets_method_not_found() {
        let script = r#"
            IFS= read -r req
            id=$(printf '%s' "$req" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{"jsonrpc":"2.0","id":"srv-1","method":"sampling/createMessage","params":{}}\n'
            IFS= read -r reply
            code=$(printf '%s' "$reply" | sed -n 's/.*"code":\(-*[0-9]*\).*/\1/p')
            rid=$(printf '%s' "$reply" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"result":{"code":%s,"rid":"%s"}}\n' "$id" "$code" "$rid"
        "#;
        let t = StdioTransport::spawn(&config(script)).await.unwrap();

        let seen = t
            .request_with_timeout("tools/list", None, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(seen["code"], json!(error_code::METHOD_NOT_FOUND));
        assert_eq!(seen["rid"], json!("srv-1"));

        assert_no_stray_process(t).await;
    }

    /// `process_group(0)` earns its keep here: closing stdin stops the
    /// server itself, but the helper it backgrounded — `npx` spawning
    /// node, a server spawning a headless browser — only dies if the
    /// whole group is signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_what_the_server_backgrounded() {
        let script = format!("sleep 120 & {ECHO}");
        let t = StdioTransport::spawn(&config(&script)).await.unwrap();

        // Round-trip first, so the shell has definitely reached the
        // read loop and the `sleep` is definitely running.
        t.request("tools/list", None).await.unwrap();

        assert_no_stray_process(t).await;
    }

    /// A server that logs every line it receives, and never answers.
    #[cfg(unix)]
    fn logging_server(log: &std::path::Path) -> ServerConfig {
        config(&format!(
            "while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
            log.display()
        ))
    }

    /// Wait for `log` to contain `needle`; the deadline only fires on a
    /// genuine failure.
    #[cfg(unix)]
    async fn wait_for_log(log: &std::path::Path, needle: &str) -> String {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            if text.contains(needle) {
                return text;
            }
            assert!(Instant::now() < until, "never saw {needle:?} in {text:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The reason the transport exposes a request's id at all: a caller
    /// that gives up must be able to stop the server's work, and only
    /// the transport knows which id that is.
    #[cfg(unix)]
    #[tokio::test]
    async fn abandoning_a_request_tells_the_server_to_cancel_it() {
        let log = std::env::temp_dir().join(format!("nanopi-stdio-cancel-{}", crate::util::uuid::v7()));
        let t = Arc::new(StdioTransport::spawn(&logging_server(&log)).await.unwrap());

        let call = {
            let t = Arc::clone(&t);
            tokio::spawn(async move { t.request("tools/call", Some(json!({"name": "slow"}))).await })
        };
        // The server has the request: the cancellation has something to
        // refer to, and the abort below lands mid-wait rather than
        // before the write.
        let text = wait_for_log(&log, "\"tools/call\"").await;
        let sent: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        call.abort();
        let _ = call.await;

        let text = wait_for_log(&log, "notifications/cancelled").await;
        let cancelled: serde_json::Value =
            serde_json::from_str(text.lines().find(|l| l.contains("cancelled")).unwrap()).unwrap();
        assert_eq!(cancelled["params"]["requestId"], sent["id"], "named the wrong request");
        assert!(cancelled.get("id").is_none(), "a notification carries no id");

        // The abandoned slot is gone, and the connection still works.
        assert!(t.close_reason().is_none());
        let _ = std::fs::remove_file(&log);
        let t = Arc::try_unwrap(t).expect("the aborted task released its handle");
        assert_no_stray_process(t).await;
    }

    /// A timeout is not an abandonment: the caller is told, and the
    /// documented behaviour (no server-side cancel) is unchanged.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_request_is_not_cancelled() {
        let log = std::env::temp_dir().join(format!("nanopi-stdio-nocancel-{}", crate::util::uuid::v7()));
        let t = StdioTransport::spawn(&logging_server(&log)).await.unwrap();

        let err = t
            .request_with_timeout("tools/call", None, Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Timeout { .. }), "{err:?}");
        // A later request round-trips through the same pipe, so by the
        // time its line is logged anything the timeout had queued would
        // be too.
        let probe = t.notify("probe", None).await;
        assert!(probe.is_ok());
        let text = wait_for_log(&log, "\"probe\"").await;
        assert!(!text.contains("cancelled"), "{text}");

        let _ = std::fs::remove_file(&log);
        assert_no_stray_process(t).await;
    }

    /// The spec forbids cancelling `initialize`; the only way it is
    /// abandoned is startup giving up, and the server dies anyway.
    #[cfg(unix)]
    #[tokio::test]
    async fn initialize_is_never_cancelled() {
        let log = std::env::temp_dir().join(format!("nanopi-stdio-init-{}", crate::util::uuid::v7()));
        let t = Arc::new(StdioTransport::spawn(&logging_server(&log)).await.unwrap());

        let init = {
            let t = Arc::clone(&t);
            tokio::spawn(async move { t.request("initialize", None).await })
        };
        wait_for_log(&log, "\"initialize\"").await;
        init.abort();
        let _ = init.await;
        let probe = t.notify("probe", None).await;
        assert!(probe.is_ok());
        let text = wait_for_log(&log, "\"probe\"").await;
        assert!(!text.contains("cancelled"), "{text}");

        let _ = std::fs::remove_file(&log);
        let t = Arc::try_unwrap(t).expect("the aborted task released its handle");
        assert_no_stray_process(t).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let t = StdioTransport::spawn(&config(ECHO)).await.unwrap();
        t.shutdown().await;
        t.shutdown().await;
        assert!(t.close_reason().is_some());
        assert_no_stray_process(t).await;
    }

    #[tokio::test]
    async fn spawning_a_missing_command_is_an_error_not_a_panic() {
        let err = StdioTransport::spawn(&config_for("definitely-not-a-program-xyz"))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Spawn { .. }), "got {err:?}");
    }

    fn config_for(command: &str) -> ServerConfig {
        ServerConfig::stdio("missing", command, Vec::new(), BTreeMap::new())
    }

    /// The framing invariant, asserted rather than assumed: a newline
    /// anywhere in a serialised message would split it into two frames
    /// and desynchronise every message after it.
    #[test]
    fn a_serialised_message_never_contains_a_newline() {
        let nasty = json!({
            "text": "line one\nline two\r\nline three",
            "tab": "\t",
            "nested": { "deep": ["a\nb", "c\u{2028}d"] },
        });
        let request = Request::new(7, "tools/call", Some(nasty));
        let line = serde_json::to_string(&request).unwrap();
        assert!(!line.contains('\n'), "{line}");
        assert!(!line.contains('\r'), "{line}");
        // And it still round-trips, i.e. the escaping is lossless.
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn the_diagnostics_tail_is_bounded_and_keeps_the_end() {
        let mut d = Diagnostics::new();
        for i in 0..100_000 {
            d.push(Source::Stderr, format!("line {i}"));
        }
        let rendered = d.render();
        assert!(rendered.len() <= STDERR_CAP + 64, "{}", rendered.len());
        assert!(rendered.contains("line 99999"), "the tail was dropped");
        assert!(!rendered.contains("line 0\n"), "the head was kept");
    }

    /// stdout noise has its own budget so a server that chatters on the
    /// wrong pipe cannot evict the stderr line that explains its death.
    #[test]
    fn stdout_noise_cannot_evict_stderr() {
        let mut d = Diagnostics::new();
        d.push(Source::Stderr, "npm ERR! 404 Not Found".to_string());
        for i in 0..10_000 {
            d.push(Source::Stdout, format!("noise {i}"));
        }
        assert!(d.render().contains("npm ERR! 404 Not Found"));
    }
}

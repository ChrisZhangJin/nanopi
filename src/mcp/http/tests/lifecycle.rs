//! Session lifecycle (H3): recovery from a dropped session, `DELETE` on
//! shutdown, and what a dead host costs.
//!
//! Recovery lives in `McpClient`, so those tests go through it; the
//! `DELETE` tests that are about the transport alone use `HttpTransport`
//! directly. Same fake server and same rule as the parent module: nothing
//! waits on a sleep.

use super::*;
use crate::mcp::client::ClientError;
use crate::mcp::manager::{ManagerOptions, McpManager};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering as AtomicOrdering};

/// What a server that issues real sessions needs to remember: which one
/// is live, and how it should misbehave on request.
struct Sessions {
    /// `None` after the server "restarts" and forgets everyone.
    live: std::sync::Mutex<Option<String>>,
    inits: AtomicUsize,
    /// Taken by the next `initialize`, whose reply then waits for it.
    hold_next_init: std::sync::Mutex<Option<Gate>>,
    /// While set, `initialize` is refused with a `503`.
    fail_inits: AtomicBool,
    /// What `DELETE` is answered with.
    delete_status: AtomicU16,
}

impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            live: std::sync::Mutex::new(None),
            inits: AtomicUsize::new(0),
            hold_next_init: std::sync::Mutex::new(None),
            fail_inits: AtomicBool::new(false),
            delete_status: AtomicU16::new(200),
        })
    }

    /// The server forgets every session, as after a restart.
    fn forget(&self) {
        *self.live.lock().unwrap() = None;
    }
}

/// Like [`mcp_server`], but sessions are real: each `initialize` mints
/// `sess-N`, and a request carrying any other id is `404`.
fn session_server(
    state: &Arc<Sessions>,
    other: impl Fn(&Req, &Value) -> Reply + Send + Sync + 'static,
) -> impl Fn(&Req) -> Reply + Send + Sync + 'static {
    let state = Arc::clone(state);
    move |req| {
        if req.method == "DELETE" {
            *state.live.lock().unwrap() = None;
            return Reply::status(state.delete_status.load(AtomicOrdering::SeqCst));
        }
        let body = req.json();
        let method = req.rpc_method();
        if method.as_deref() == Some("initialize") {
            if state.fail_inits.load(AtomicOrdering::SeqCst) {
                return Reply::text(503, "restarting");
            }
            let n = state.inits.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let id = format!("sess-{n}");
            *state.live.lock().unwrap() = Some(id.clone());
            let message = result_of(
                &body["id"],
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "1"},
                }),
            );
            let held = state.hold_next_init.lock().unwrap().take();
            return match held {
                Some(gate) => {
                    Reply::sse_parts(vec![Part::Wait(gate), Part::Bytes(frame(&message))])
                }
                None => Reply::sse_message(&message),
            }
            .header("mcp-session-id", &id);
        }
        if req.header("mcp-session-id") != state.live.lock().unwrap().as_deref() {
            return Reply::text(404, "Session not found");
        }
        if method.as_deref().is_some_and(|m| m.starts_with("notifications/")) {
            return Reply::status(202);
        }
        other(req, &body)
    }
}

/// `tools/list` offers `a`; `tools/call` succeeds.
fn standard(req: &Req, body: &Value) -> Reply {
    match req.rpc_method().as_deref() {
        Some("tools/list") => Reply::sse_message(&result_of(
            &body["id"],
            json!({"tools": [{"name": "a", "inputSchema": {"type": "object"}}]}),
        )),
        _ => Reply::sse_message(&result_of(
            &body["id"],
            json!({"content": [{"type": "text", "text": "ran"}]}),
        )),
    }
}

/// Every request the server saw, as `initialize`, `tools/call`, … or
/// `DELETE`, in arrival order.
fn seen_methods(fake: &Fake) -> Vec<String> {
    fake.requests()
        .iter()
        .map(|r| {
            if r.method == "DELETE" {
                "DELETE".to_string()
            } else {
                r.rpc_method().unwrap_or_default()
            }
        })
        .collect()
}

fn count(fake: &Fake, method: &str) -> usize {
    seen_methods(fake).iter().filter(|m| *m == method).count()
}

fn deletes(fake: &Fake) -> Vec<Req> {
    fake.requests().into_iter().filter(|r| r.method == "DELETE").collect()
}

async fn ready_client(fake: &Fake) -> McpClient {
    let client = McpClient::connect(&fake.config(&[])).await;
    assert_eq!(client.state(), ConnectionState::Ready, "{:?}", client.state());
    client
}

fn call_args() -> serde_json::Map<String, Value> {
    json!({"q": "needle"}).as_object().unwrap().clone()
}

// ------------------------------------------------------------- recovery

#[tokio::test]
async fn a_dropped_session_is_recovered_transparently_and_the_call_runs_once() {
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let client = ready_client(&fake).await;
    client.list_tools().await.unwrap();

    state.forget();
    let result = client.call_tool("a", call_args()).await.unwrap();
    assert_eq!(result.content.len(), 1);

    assert_eq!(
        seen_methods(&fake),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            // refused: the server no longer knows the session
            "tools/call",
            // the recovery
            "initialize",
            "notifications/initialized",
            // the original call, again
            "tools/call",
        ]
    );
    let seen = fake.requests();
    assert_eq!(seen[3].header("mcp-session-id"), Some("sess-1"));
    assert_eq!(seen[6].header("mcp-session-id"), Some("sess-2"));
    assert_eq!(seen[6].header("mcp-protocol-version"), Some("2025-06-18"));
    // The retry is the same request, not a reconstruction of it.
    assert_eq!(seen[3].json()["params"], seen[6].json()["params"]);
    assert_eq!(seen[6].json()["params"]["arguments"], json!({"q": "needle"}));
    // The recovery's `initialize` offered no session, as any other.
    assert_eq!(seen[4].header("mcp-session-id"), None);
    assert_eq!(client.state(), ConnectionState::Ready);

    // And the new session is simply the session now.
    client.call_tool("a", call_args()).await.unwrap();
    assert_eq!(count(&fake, "initialize"), 2);
}

#[tokio::test]
async fn a_dropped_session_is_recovered_for_listing_too() {
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let client = ready_client(&fake).await;
    state.forget();
    assert_eq!(client.list_tools().await.unwrap().len(), 1);
    assert_eq!(count(&fake, "initialize"), 2);
}

#[tokio::test]
async fn the_retry_is_once_a_second_session_expired_is_returned() {
    let state = Sessions::new();
    // Refuses with 404 even on the session it just issued.
    let fake = Fake::start(session_server(&state, |req, body| match req.rpc_method().as_deref() {
        Some("tools/call") => Reply::text(404, "Session not found"),
        _ => standard(req, body),
    }))
    .await;
    let client = ready_client(&fake).await;

    let e = client.call_tool("a", call_args()).await.unwrap_err();
    assert!(matches!(e, ClientError::Transport(TransportError::SessionExpired)), "{e:?}");
    assert_eq!(count(&fake, "tools/call"), 2, "one try, one retry, no more");
    assert_eq!(count(&fake, "initialize"), 2, "one recovery, no loop");
    // Not a death: the next call may find the server sane again.
    assert_eq!(client.state(), ConnectionState::Ready);
}

#[tokio::test]
async fn only_session_expired_is_ever_retried() {
    // Each of these may follow a `tools/call` that ran. One attempt, no
    // re-initialize, and the server was asked exactly once.
    async fn once(reply: Reply, timeout: Duration) -> ClientError {
        let state = Sessions::new();
        let fake = Fake::start(session_server(&state, move |req, body| {
            match req.rpc_method().as_deref() {
                Some("tools/call") => reply.clone(),
                _ => standard(req, body),
            }
        }))
        .await;
        let client = ready_client(&fake).await;
        let e = client.call_tool_with_timeout("a", call_args(), timeout).await.unwrap_err();
        assert_eq!(count(&fake, "tools/call"), 1, "{e:?} was retried");
        assert_eq!(count(&fake, "initialize"), 1, "{e:?} caused a re-initialize");
        assert_eq!(client.state(), ConnectionState::Ready);
        e
    }
    let long = Duration::from_secs(10);

    assert!(matches!(
        once(Reply::text(503, "overloaded"), long).await,
        ClientError::Transport(TransportError::Http { status: 503, .. })
    ));
    assert!(matches!(
        once(Reply::text(500, "boom"), long).await,
        ClientError::Transport(TransportError::Http { status: 500, .. })
    ));
    assert!(matches!(
        once(Reply::text(400, "bad"), long).await,
        ClientError::Transport(TransportError::Http { status: 400, .. })
    ));
    // The stream is cut before the reply.
    assert!(matches!(
        once(Reply::sse_parts(vec![]), long).await,
        ClientError::Transport(TransportError::Disconnected(_))
    ));
    // The reply never comes.
    assert!(matches!(
        once(Reply::sse_parts(vec![]).stalled(), Duration::from_millis(300)).await,
        ClientError::Transport(TransportError::Timeout { .. })
    ));
    // 401 is about credentials, which a new session does not change.
    assert!(matches!(
        once(Reply::text(401, "no"), long).await,
        ClientError::Transport(TransportError::Unauthorized { .. })
    ));
    // Nor is 403: the same credentials on a new session fare the same.
    assert!(matches!(
        once(Reply::text(403, "no"), long).await,
        ClientError::Transport(TransportError::Forbidden { .. })
    ));
}

#[tokio::test]
async fn a_404_without_a_session_is_a_wrong_url_not_an_expiry() {
    // A server that never issues ids: there is nothing to re-establish.
    let fake = Fake::start(|req| match req.rpc_method().as_deref() {
        Some("initialize") => Reply::sse_message(&result_of(
            &req.json()["id"],
            json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                   "serverInfo": {"name": "fake", "version": "1"}}),
        )),
        Some(m) if m.starts_with("notifications/") => Reply::status(202),
        _ => Reply::text(404, "Not Found"),
    })
    .await;
    let client = ready_client(&fake).await;
    let e = client.call_tool("a", call_args()).await.unwrap_err();
    assert!(
        matches!(e, ClientError::Transport(TransportError::Http { status: 404, .. })),
        "{e:?}"
    );
    assert_eq!(count(&fake, "initialize"), 1);
    assert_eq!(count(&fake, "tools/call"), 1);
}

#[tokio::test]
async fn concurrent_404s_reinitialize_once_and_every_call_completes() {
    const CALLS: usize = 8;
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let client = Arc::new(ready_client(&fake).await);
    client.list_tools().await.unwrap();
    let before = fake.requests().len();

    // The recovery's reply is held back until every call has been
    // refused, so none of them can be late enough to find a new session
    // already in place: all of them must wait on the one re-initialize.
    let gate: Gate = Arc::new(Notify::new());
    *state.hold_next_init.lock().unwrap() = Some(Arc::clone(&gate));
    state.forget();

    let calls: Vec<_> = (0..CALLS)
        .map(|_| {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.call_tool("a", call_args()).await })
        })
        .collect();
    // CALLS refusals, plus the one `initialize` that is being held.
    fake.wait_for_requests(before + CALLS + 1).await;
    gate.notify_one();

    for call in calls {
        call.await.unwrap().expect("a call that hit the 404 was not recovered");
    }
    assert_eq!(count(&fake, "initialize"), 2, "{CALLS} 404s must cost one re-initialize");
    assert_eq!(count(&fake, "notifications/initialized"), 2);
    assert_eq!(count(&fake, "tools/call"), CALLS * 2, "each retried exactly once");
}

#[tokio::test]
async fn a_failed_recovery_is_reported_and_the_next_call_tries_again() {
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let client = ready_client(&fake).await;

    // The server restarts and is not back yet.
    state.forget();
    state.fail_inits.store(true, AtomicOrdering::SeqCst);
    let e = client.call_tool("a", call_args()).await.unwrap_err();
    match &e {
        ClientError::Transport(TransportError::Disconnected(m)) => {
            assert!(m.contains("re-initializing failed"), "{m}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&fake, "tools/call"), 1, "nothing to retry against");
    // A server that is down is not a server that is gone.
    assert_eq!(client.state(), ConnectionState::Ready);

    // It comes back: the very next call recovers.
    state.fail_inits.store(false, AtomicOrdering::SeqCst);
    client.call_tool("a", call_args()).await.unwrap();
}

#[tokio::test]
async fn shutdown_during_recovery_reports_closed_not_a_failed_reinitialize() {
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let client = Arc::new(ready_client(&fake).await);
    let before = fake.requests().len();

    // The recovery is under way (its reply is held) when we shut down.
    let gate: Gate = Arc::new(Notify::new());
    *state.hold_next_init.lock().unwrap() = Some(Arc::clone(&gate));
    state.forget();
    let call = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("a", call_args()).await })
    };
    fake.wait_for_requests(before + 2).await;
    client.shutdown().await;

    match call.await.unwrap() {
        Err(ClientError::Transport(TransportError::Closed(m))) => {
            assert!(m.contains("was shut down"), "{m}")
        }
        other => panic!("{other:?}"),
    }
}

// --------------------------------------------------------------- DELETE

#[tokio::test]
async fn shutdown_ends_the_session_with_a_delete_exactly_once() {
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let config = fake.config(&[("Authorization", "Bearer s3cret")]);
    let client = McpClient::connect(&config).await;
    client.list_tools().await.unwrap();

    client.shutdown().await;
    client.shutdown().await;

    let sent = deletes(&fake);
    assert_eq!(sent.len(), 1, "{:?}", seen_methods(&fake));
    let delete = &sent[0];
    assert_eq!(delete.path, "/mcp");
    assert_eq!(delete.header("mcp-session-id"), Some("sess-1"));
    assert_eq!(delete.header("mcp-protocol-version"), Some("2025-06-18"));
    assert_eq!(delete.header("authorization"), Some("Bearer s3cret"));
    assert!(delete.body.is_empty());
    // The server really did end it.
    assert_eq!(*state.live.lock().unwrap(), None);
    // After it, nothing else is sent: shutdown is the last word.
    assert_eq!(seen_methods(&fake).last().unwrap(), "DELETE");
    assert!(matches!(client.state(), ConnectionState::Failed { .. }));
}

#[tokio::test]
async fn concurrent_shutdowns_send_one_delete_and_all_wait_for_it() {
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let t = Arc::new(session_transport(&fake).await);

    tokio::join!(t.shutdown(), t.shutdown(), t.shutdown());
    // Every caller has returned, so the one `DELETE` has been answered:
    // none of them may have raced ahead of it and let a process exit.
    assert_eq!(deletes(&fake).len(), 1);
    assert_eq!(*state.live.lock().unwrap(), None);
}

#[tokio::test]
async fn no_session_means_no_delete() {
    // Never initialized.
    let fake = Fake::start(|_| Reply::status(200)).await;
    let t = fake.transport().await;
    t.shutdown().await;
    assert!(fake.requests().is_empty(), "{:?}", seen_methods(&fake));

    // Initialized, but the server issued no session id.
    let fake = Fake::start(|req| Reply::json(200, &result_of(&req.json()["id"], json!({})))).await;
    let t = fake.transport().await;
    t.request("initialize", Some(json!({}))).await.unwrap();
    t.shutdown().await;
    assert_eq!(seen_methods(&fake), ["initialize"]);
}

#[tokio::test]
async fn a_delete_the_server_refuses_is_ignored() {
    // 405 is the spec's own "this server does not let clients end
    // sessions"; the rest are other ways a server can be unhelpful.
    for status in [405u16, 404, 400, 500, 501] {
        let state = Sessions::new();
        state.delete_status.store(status, AtomicOrdering::SeqCst);
        let fake = Fake::start(session_server(&state, standard)).await;
        let client = ready_client(&fake).await;
        client.shutdown().await;
        assert_eq!(deletes(&fake).len(), 1, "{status}");
        assert!(matches!(client.state(), ConnectionState::Failed { .. }), "{status}");
    }
}

#[tokio::test]
async fn a_server_that_is_gone_does_not_delay_or_break_shutdown() {
    let state = Sessions::new();
    let mut fake = Fake::start(session_server(&state, standard)).await;
    let t = session_transport(&fake).await;
    // Refused from here on: the accept loop is gone and its socket closed.
    fake.stop().await;

    let started = Instant::now();
    t.shutdown().await;
    assert!(started.elapsed() < DELETE_TIMEOUT, "{:?}", started.elapsed());
    assert!(t.close_reason().is_some());
}

#[tokio::test]
async fn a_hanging_delete_is_bounded_and_does_not_hold_up_what_was_in_flight() {
    // The server answers `initialize` normally, then never answers
    // anything else: a call that is mid-flight, and later the `DELETE`.
    let never: Gate = Arc::new(Notify::new());
    let fake = {
        let never = Arc::clone(&never);
        Fake::start(mcp_server(move |_, _| Reply::status(200).held_by(&never)))
        .await
    };
    let t = Arc::new(session_transport(&fake).await);
    let in_flight = {
        let t = Arc::clone(&t);
        tokio::spawn(async move { t.request("tools/call", None).await })
    };
    fake.wait_for_requests(2).await;

    let started = Instant::now();
    let shutdown = {
        let t = Arc::clone(&t);
        tokio::spawn(async move { t.shutdown().await })
    };
    // The DELETE has reached the server, which is sitting on it...
    fake.wait_for_requests(3).await;
    assert_eq!(deletes(&fake).len(), 1);
    // ...and the call that was in flight is already failed. Cancelling
    // after the DELETE would leave it waiting on this server too, for the
    // whole `DELETE_TIMEOUT`. The bound is a failure path only.
    match tokio::time::timeout(DELETE_TIMEOUT / 2, in_flight).await {
        Ok(joined) => match joined.unwrap() {
            Err(TransportError::Closed(m)) => assert!(m.contains("was shut down"), "{m}"),
            other => panic!("{other:?}"),
        },
        Err(_) => panic!("an in-flight call was not cancelled before the DELETE finished"),
    }
    assert!(!shutdown.is_finished(), "shutdown returned before its DELETE timed out");

    // Shutdown itself gives up on the DELETE at the deadline, not later.
    tokio::time::timeout(DELETE_TIMEOUT + Duration::from_secs(3), shutdown)
        .await
        .expect("shutdown outlived its DELETE timeout")
        .unwrap();
    let took = started.elapsed();
    assert!(took >= DELETE_TIMEOUT - Duration::from_millis(100), "gave up early: {took:?}");
    assert_eq!(deletes(&fake).len(), 1);
}

// ----------------------------------------------- exits that cannot await

/// A multi-threaded runtime: the drop path blocks its thread while the
/// fake, on another, must keep answering.
fn multi() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

fn manager_config(fake: &Fake) -> Vec<ServerConfig> {
    vec![fake.config(&[])]
}

#[test]
fn abandon_sends_the_delete_without_awaiting() {
    multi().block_on(async {
        let state = Sessions::new();
        let fake = Fake::start(session_server(&state, standard)).await;
        let t = session_transport(&fake).await;

        let done = t.abandon().expect("there is a session to end");
        tokio::task::spawn_blocking(move || done.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the DELETE attempt never finished");
        let sent = deletes(&fake);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].header("mcp-session-id"), Some("sess-1"));
        assert!(t.close_reason().is_some());

        // However shutdown starts, it happens once.
        assert!(t.abandon().is_none());
        t.shutdown().await;
        assert_eq!(deletes(&fake).len(), 1);
    });
}

#[test]
fn the_kill_guard_ends_http_sessions_when_shutdown_never_ran() {
    multi().block_on(async {
        let state = Sessions::new();
        let fake = Fake::start(session_server(&state, standard)).await;
        let manager =
            Arc::new(McpManager::connect(manager_config(&fake), ManagerOptions::default()).await);
        assert_eq!(manager.tool_count(), 1);
        // An HTTP server has no pid: that is normal, not a missing server.
        assert!(manager.pids().is_empty());

        let guard = manager.kill_on_drop();
        assert!(deletes(&fake).is_empty());
        drop(guard);
        assert_eq!(deletes(&fake).len(), 1, "the drop path never sent a DELETE");
        assert_eq!(deletes(&fake)[0].header("mcp-session-id"), Some("sess-1"));
        assert!(manager.pids().is_empty());

        // And a later, orderly shutdown does not send a second.
        manager.shutdown().await;
        assert_eq!(deletes(&fake).len(), 1);
    });
}

#[test]
fn the_kill_guard_does_nothing_after_an_orderly_shutdown() {
    multi().block_on(async {
        let state = Sessions::new();
        let fake = Fake::start(session_server(&state, standard)).await;
        let manager =
            Arc::new(McpManager::connect(manager_config(&fake), ManagerOptions::default()).await);
        let guard = manager.kill_on_drop();
        manager.shutdown().await;
        assert_eq!(deletes(&fake).len(), 1);
        drop(guard);
        assert_eq!(deletes(&fake).len(), 1, "the guard repeated the DELETE");
    });
}

#[test]
fn the_kill_guard_does_not_hang_exit_on_a_wedged_server() {
    multi().block_on(async {
        let never: Gate = Arc::new(Notify::new());
        let fake = {
            let never = Arc::clone(&never);
            // `mcp_server` routes the DELETE (which has no JSON-RPC
            // method) to this closure.
            Fake::start(mcp_server(move |req, body| {
                if req.method == "DELETE" {
                    return Reply::status(200).held_by(&never);
                }
                standard(req, body)
            }))
            .await
        };
        let manager =
            Arc::new(McpManager::connect(manager_config(&fake), ManagerOptions::default()).await);
        let guard = manager.kill_on_drop();
        let started = Instant::now();
        drop(guard);
        assert!(
            started.elapsed() < DELETE_TIMEOUT + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(deletes(&fake).len(), 1);
    });
}

#[tokio::test]
async fn http_servers_have_no_pid_and_that_is_normal() {
    let state = Sessions::new();
    let fake = Fake::start(session_server(&state, standard)).await;
    let client = ready_client(&fake).await;
    assert_eq!(client.pid(), None);
    client.shutdown().await;

    let manager = McpManager::connect(manager_config(&fake), ManagerOptions::default()).await;
    assert!(manager.status()[0].connected, "{:?}", manager.status());
    assert!(manager.pids().is_empty());
    manager.shutdown().await;
    // With nothing to signal and nothing to end, `kill_now` is a no-op.
    McpManager::empty().kill_now();
}

// ------------------------------------------------------------ dead host

#[tokio::test]
async fn a_dead_host_fails_the_handshake_in_milliseconds_not_the_call_timeout() {
    // A loopback port that was open a moment ago and is not now.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let config =
        ServerConfig::http("dead", format!("http://127.0.0.1:{port}/mcp"), BTreeMap::new());

    let started = Instant::now();
    let client = McpClient::connect(&config).await;
    let elapsed = started.elapsed();

    match client.state() {
        ConnectionState::Failed { reason } => {
            assert!(reason.contains(&format!("cannot connect to 127.0.0.1:{port}")), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    // Far below the 30 s `initialize` timeout and the 60 s request one.
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    // And shutting down a client that never connected is quiet and quick.
    let started = Instant::now();
    client.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(1));
}

//! Tests against a hand-rolled HTTP server on a loopback socket, in the
//! manner of `tests/print_mode_e2e.rs`: no HTTP crate, so what the
//! transport does is judged against bytes written by hand, not against
//! another library's idea of a response.
//!
//! Nothing waits on a fixed sleep. A test that needs the server to have
//! seen something waits on [`Fake::wait_for_requests`], and one that
//! needs the server to hold a reply back releases it with a [`Gate`] when
//! the thing it is waiting for has been observed.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use super::*;
use crate::mcp::client::{ConnectionState, McpClient};

mod lifecycle;

// ------------------------------------------------------------ fake server

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    /// Names lower-cased.
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// The JSON-RPC method, or `None` for a response we were sent.
    fn rpc_method(&self) -> Option<String> {
        self.json()["method"].as_str().map(str::to_string)
    }
}

/// Released by the test (or by the handler) when the server may carry on
/// with a reply it is holding back. `notify_one` leaves a permit, so a
/// release that happens before the wait is not lost.
type Gate = Arc<Notify>;

#[derive(Clone)]
enum Part {
    Bytes(Vec<u8>),
    /// Hold the rest of the reply until the gate opens.
    Wait(Gate),
}

#[derive(Clone, Copy, PartialEq)]
enum Framing {
    /// `Content-Length`, then close.
    Length,
    /// No length: the body ends when the server closes, as an SSE
    /// stream's does.
    UntilClose,
    /// As `UntilClose`, but never close: wait for the client to hang up.
    Stall,
}

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    parts: Vec<Part>,
    framing: Framing,
    /// Say nothing at all, not even the status line, until this opens.
    /// (`Part::Wait` holds back the *body*; a client's `send()` has
    /// already returned by then.)
    hold: Option<Gate>,
}

impl Reply {
    fn raw(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            parts: vec![Part::Bytes(body)],
            framing: Framing::Length,
            hold: None,
        }
    }

    fn json(status: u16, body: &Value) -> Self {
        Self::raw(status, "application/json", body.to_string().into_bytes())
    }

    fn status(status: u16) -> Self {
        Self { status, headers: vec![], parts: vec![], framing: Framing::Length, hold: None }
    }

    fn text(status: u16, body: &str) -> Self {
        Self::raw(status, "text/plain", body.as_bytes().to_vec())
    }

    /// `200 text/event-stream` carrying `message` as one `event: message`
    /// frame, the way the real server answers every POST.
    fn sse_message(message: &Value) -> Self {
        Self::sse_parts(vec![Part::Bytes(frame(message))])
    }

    fn sse_parts(parts: Vec<Part>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            parts,
            framing: Framing::UntilClose,
            hold: None,
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Hold the whole reply, head included, until `gate` opens.
    fn held_by(mut self, gate: &Gate) -> Self {
        self.hold = Some(Arc::clone(gate));
        self
    }

    fn stalled(mut self) -> Self {
        self.framing = Framing::Stall;
        self
    }
}

fn frame(message: &Value) -> Vec<u8> {
    format!("event: message\ndata: {message}\n\n").into_bytes()
}

fn result_of(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

type Handler = Arc<dyn Fn(&Req) -> Reply + Send + Sync>;

struct Fake {
    addr: SocketAddr,
    requests: Arc<std::sync::Mutex<Vec<Req>>>,
    arrived: Arc<Notify>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Fake {
    /// Stop accepting and close the listening socket, so the next
    /// connection is refused. Awaits the task's end: `abort` alone only
    /// asks, and the socket stays open until the task is actually gone.
    async fn stop(&mut self) {
        self.accept.abort();
        let _ = (&mut self.accept).await;
    }

    async fn start(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let handler: Handler = Arc::new(handler);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let arrived = Arc::new(Notify::new());
        let accept = {
            let (requests, arrived) = (Arc::clone(&requests), Arc::clone(&arrived));
            tokio::spawn(async move {
                loop {
                    let Ok((conn, _)) = listener.accept().await else { return };
                    tokio::spawn(serve(
                        conn,
                        Arc::clone(&handler),
                        Arc::clone(&requests),
                        Arc::clone(&arrived),
                    ));
                }
            })
        };
        Self { addr, requests, arrived, accept }
    }

    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    fn config(&self, headers: &[(&str, &str)]) -> ServerConfig {
        let headers: BTreeMap<String, String> =
            headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ServerConfig::http("fake", self.url(), headers)
    }

    async fn transport(&self) -> HttpTransport {
        HttpTransport::connect(&self.config(&[])).await.unwrap()
    }

    fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }

    /// Wait until the server has parsed `n` requests. The bound is a
    /// failure path only: it turns a hang into a test failure.
    async fn wait_for_requests(&self, n: usize) {
        let wait = async {
            loop {
                let notified = self.arrived.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.requests.lock().unwrap().len() >= n {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("the server never saw the expected requests");
    }
}

async fn read_request(conn: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = conn.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = conn.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Req { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

async fn serve(
    mut conn: TcpStream,
    handler: Handler,
    requests: Arc<std::sync::Mutex<Vec<Req>>>,
    arrived: Arc<Notify>,
) {
    // Separate writes should be separate segments, so a frame really does
    // reach the client in pieces. (Best effort: TCP may still coalesce,
    // which weakens a test but cannot make it fail; the splits that must
    // be exact are tested without a socket, on `EventReader`.)
    let _ = conn.set_nodelay(true);
    let Some(req) = read_request(&mut conn).await else { return };
    let reply = handler(&req);
    requests.lock().unwrap().push(req);
    arrived.notify_waiters();

    if let Some(gate) = &reply.hold {
        gate.notified().await;
    }
    let mut head = format!("HTTP/1.1 {} X\r\nconnection: close\r\n", reply.status);
    for (k, v) in &reply.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if reply.framing == Framing::Length {
        let len: usize = reply
            .parts
            .iter()
            .map(|p| if let Part::Bytes(b) = p { b.len() } else { 0 })
            .sum();
        head.push_str(&format!("content-length: {len}\r\n"));
    }
    head.push_str("\r\n");
    if conn.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let _ = conn.flush().await;
    for part in &reply.parts {
        match part {
            Part::Bytes(bytes) => {
                if conn.write_all(bytes).await.is_err() {
                    return;
                }
                let _ = conn.flush().await;
                tokio::task::yield_now().await;
            }
            Part::Wait(gate) => gate.notified().await,
        }
    }
    if reply.framing == Framing::Stall {
        let mut sink = [0u8; 64];
        while matches!(conn.read(&mut sink).await, Ok(n) if n > 0) {}
    }
}

// ---------------------------------------------------------------- helpers

fn init_reply(request: &Value) -> Reply {
    Reply::sse_message(&result_of(
        &request["id"],
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "1"},
        }),
    ))
    .header("mcp-session-id", "sess-abc123")
}

/// A server that answers like the real one: `initialize` with a session
/// id, `notifications/*` with `202`, everything else through `other`.
fn mcp_server(
    other: impl Fn(&Req, &Value) -> Reply + Send + Sync + 'static,
) -> impl Fn(&Req) -> Reply + Send + Sync + 'static {
    move |req| {
        let body = req.json();
        match req.rpc_method().as_deref() {
            Some("initialize") => init_reply(&body),
            Some(m) if m.starts_with("notifications/") => Reply::status(202),
            _ => other(req, &body),
        }
    }
}

fn quick(timeout: Duration) -> HttpOptions {
    HttpOptions { connect_timeout: Duration::from_secs(5), request_timeout: timeout }
}

async fn session_transport(fake: &Fake) -> HttpTransport {
    let t = fake.transport().await;
    t.request("initialize", Some(json!({}))).await.unwrap();
    t
}

// ------------------------------------------------------- response shapes

#[tokio::test]
async fn json_response_is_one_object() {
    let fake = Fake::start(|req| {
        Reply::json(200, &result_of(&req.json()["id"], json!({"tools": []})))
    })
    .await;
    let t = fake.transport().await;

    let result = t.request("tools/list", Some(json!({"cursor": "c"}))).await.unwrap();
    assert_eq!(result, json!({"tools": []}));

    let seen = &fake.requests()[0];
    assert_eq!((seen.method.as_str(), seen.path.as_str()), ("POST", "/mcp"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("accept"), Some("application/json, text/event-stream"));
    assert_eq!(seen.json()["method"], json!("tools/list"));
    assert_eq!(seen.json()["params"], json!({"cursor": "c"}));
}

#[tokio::test]
async fn sse_single_frame_is_the_reply() {
    // The shape the real server uses for every POST.
    let fake = Fake::start(|req| {
        Reply::sse_message(&result_of(&req.json()["id"], json!({"ok": true})))
    })
    .await;
    let t = fake.transport().await;
    assert_eq!(t.request("tools/list", None).await.unwrap(), json!({"ok": true}));
}

#[tokio::test]
async fn sse_reply_pretty_printed_across_several_data_lines() {
    // Spec-legal: one event, one JSON value written over many `data:`
    // lines, which the client joins with `\n` before parsing. Sent one
    // byte per part so line and `\r\n` boundaries all get split too.
    let fake = Fake::start(|req| {
        let pretty = serde_json::to_string_pretty(&result_of(&req.json()["id"], json!({"ok": [1, 2]})))
            .unwrap();
        assert!(pretty.lines().count() > 3);
        let mut wire = String::from("event: message\r\nid: 1\r\n");
        for line in pretty.lines() {
            wire.push_str(&format!("data: {line}\r\n"));
        }
        wire.push_str("\r\n");
        Reply::sse_parts(wire.bytes().map(|b| Part::Bytes(vec![b])).collect())
    })
    .await;
    let t = fake.transport().await;
    assert_eq!(t.request("tools/list", None).await.unwrap(), json!({"ok": [1, 2]}));
}

#[tokio::test]
async fn sse_reply_with_data_field_lacking_the_space() {
    let fake = Fake::start(|req| {
        let message = result_of(&req.json()["id"], json!({"nospace": true}));
        Reply::sse_parts(vec![Part::Bytes(format!("data:{message}\n\n").into_bytes())])
    })
    .await;
    let t = fake.transport().await;
    assert_eq!(t.request("tools/list", None).await.unwrap(), json!({"nospace": true}));
}

#[tokio::test]
async fn large_sse_frame_split_across_reads_and_mid_line() {
    // 600 KB on one `data:` line, in 997-byte pieces: not a multiple of
    // anything, so pieces end mid-line, mid-JSON and mid-`data: `.
    let payload = "x".repeat(600 * 1024);
    let fake = {
        let payload = payload.clone();
        Fake::start(move |req| {
            let bytes = frame(&result_of(&req.json()["id"], json!({"blob": payload})));
            Reply::sse_parts(bytes.chunks(997).map(|c| Part::Bytes(c.to_vec())).collect())
        })
        .await
    };
    let t = fake.transport().await;
    let result = t.request("tools/call", None).await.unwrap();
    assert_eq!(result["blob"].as_str().unwrap().len(), payload.len());
}

#[tokio::test]
async fn unrelated_messages_before_the_match_are_ignored() {
    let fake = Fake::start(|req| {
        let id = req.json()["id"].clone();
        Reply::sse_parts(vec![
            Part::Bytes(frame(&json!({
                "jsonrpc": "2.0", "method": "notifications/progress",
                "params": {"progressToken": 1, "progress": 0.5}
            }))),
            Part::Bytes(b": keep-alive\n\n".to_vec()),
            // A reply to some other request, and one that is not
            // JSON-RPC at all.
            Part::Bytes(frame(&result_of(&json!(999), json!({"wrong": true})))),
            Part::Bytes(b"event: message\ndata: not json\n\n".to_vec()),
            Part::Bytes(frame(&result_of(&id, json!({"right": true})))),
        ])
    })
    .await;
    let t = fake.transport().await;
    assert_eq!(t.request("tools/call", None).await.unwrap(), json!({"right": true}));
}

#[tokio::test]
async fn server_request_on_the_stream_is_refused_with_method_not_found() {
    // The reply is held back until the server has been *answered*, so a
    // transport that ignores the request hits its timeout instead of
    // passing by luck.
    let gate: Gate = Arc::new(Notify::new());
    let fake = {
        let gate = Arc::clone(&gate);
        Fake::start(mcp_server(move |_, body| {
            if body["error"]["code"] == json!(error_code::METHOD_NOT_FOUND) {
                gate.notify_one();
                return Reply::status(202);
            }
            Reply::sse_parts(vec![
                Part::Bytes(frame(&json!({
                    "jsonrpc": "2.0", "id": "srv-1",
                    "method": "sampling/createMessage", "params": {}
                }))),
                Part::Wait(Arc::clone(&gate)),
                Part::Bytes(frame(&result_of(&body["id"], json!({"done": true})))),
            ])
        }))
        .await
    };
    let t = HttpTransport::connect_with(&fake.config(&[]), quick(Duration::from_secs(10)))
        .await
        .unwrap();
    t.request("initialize", Some(json!({}))).await.unwrap();

    assert_eq!(t.request("tools/call", None).await.unwrap(), json!({"done": true}));

    let refusal = fake
        .requests()
        .into_iter()
        .find(|r| r.json()["error"]["code"] == json!(error_code::METHOD_NOT_FOUND))
        .expect("the server's request was never answered");
    assert_eq!(refusal.json()["id"], json!("srv-1"));
    assert_eq!(refusal.header("mcp-session-id"), Some("sess-abc123"));
}

#[tokio::test]
async fn jsonrpc_error_inside_a_200_stream_is_rpc() {
    let fake = Fake::start(|req| {
        Reply::sse_message(&json!({
            "jsonrpc": "2.0", "id": req.json()["id"],
            "error": {"code": -32602, "message": "Unknown tool: nope"}
        }))
    })
    .await;
    let t = fake.transport().await;
    match t.request("tools/call", None).await {
        Err(TransportError::Rpc(e)) => {
            assert_eq!(e.code, -32602);
            assert_eq!(e.message, "Unknown tool: nope");
        }
        other => panic!("expected Rpc, got {other:?}"),
    }
}

#[tokio::test]
async fn null_id_error_on_a_stream_is_the_reply_to_our_request() {
    let fake = Fake::start(|_| {
        Reply::sse_message(&json!({
            "jsonrpc": "2.0", "id": null,
            "error": {"code": -32700, "message": "Parse error"}
        }))
    })
    .await;
    let t = fake.transport().await;
    assert!(matches!(
        t.request("tools/list", None).await,
        Err(TransportError::Rpc(e)) if e.code == -32700
    ));
}

#[tokio::test]
async fn stream_that_ends_before_the_matching_id_is_an_error() {
    let fake = Fake::start(|_| {
        Reply::sse_parts(vec![
            Part::Bytes(frame(&json!({"jsonrpc": "2.0", "method": "notifications/message"}))),
            Part::Bytes(b"data: garbage\n\n".to_vec()),
        ])
    })
    .await;
    let t = fake.transport().await;
    match t.request("tools/list", None).await {
        Err(TransportError::Disconnected(m)) => {
            assert!(m.contains("without replying to `tools/list`"), "{m}");
            assert!(m.contains("1 event(s) were not JSON-RPC"), "{m}");
        }
        other => panic!("expected Disconnected, got {other:?}"),
    }
    // An empty stream is the same failure, not an empty success.
    let fake = Fake::start(|_| Reply::sse_parts(vec![])).await;
    let t = fake.transport().await;
    assert!(matches!(
        t.request("tools/list", None).await,
        Err(TransportError::Disconnected(_))
    ));
}

#[tokio::test]
async fn bodies_that_are_not_a_reply_are_protocol_errors() {
    // Wrong content type.
    let fake = Fake::start(|_| Reply::text(200, "hello")).await;
    let t = fake.transport().await;
    assert!(matches!(t.request("tools/list", None).await, Err(TransportError::Protocol(_))));

    // `202` is the answer to a notification, never to a request.
    let fake = Fake::start(|_| Reply::status(202)).await;
    let t = fake.transport().await;
    assert!(matches!(t.request("tools/list", None).await, Err(TransportError::Protocol(_))));

    // A JSON body for somebody else's id.
    let fake = Fake::start(|_| Reply::json(200, &result_of(&json!(424242), json!({})))).await;
    let t = fake.transport().await;
    assert!(matches!(t.request("tools/list", None).await, Err(TransportError::Protocol(_))));

    // Not JSON-RPC at all.
    let fake = Fake::start(|_| Reply::json(200, &json!({"hello": "world"}))).await;
    let t = fake.transport().await;
    assert!(matches!(t.request("tools/list", None).await, Err(TransportError::Protocol(_))));
}

// ---------------------------------------------------------- notifications

#[tokio::test]
async fn notification_expects_202() {
    let fake = Fake::start(|_| Reply::status(202)).await;
    let t = fake.transport().await;
    t.notify("notifications/initialized", None).await.unwrap();
    let seen = &fake.requests()[0];
    assert_eq!(seen.json()["method"], json!("notifications/initialized"));
    assert!(seen.json().get("id").is_none(), "a notification has no id");

    // Any other success is wrong, not "close enough".
    let fake = Fake::start(|_| Reply::status(200)).await;
    let t = fake.transport().await;
    assert!(matches!(
        t.notify("notifications/initialized", None).await,
        Err(TransportError::Protocol(m)) if m.contains("expected 202")
    ));

    let fake = Fake::start(|_| Reply::text(500, "boom")).await;
    let t = fake.transport().await;
    assert!(matches!(
        t.notify("notifications/initialized", None).await,
        Err(TransportError::Http { status: 500, .. })
    ));
}

// ----------------------------------------------------------- status codes

/// A server that issues a session on `initialize` and answers every other
/// request with `reply`.
async fn failing_after_session(reply: Reply) -> (Fake, HttpTransport) {
    // Not `mcp_server`: that would answer the notification with `202`.
    let fake = Fake::start(move |req| match req.rpc_method().as_deref() {
        Some("initialize") => init_reply(&req.json()),
        _ => reply.clone(),
    })
    .await;
    let t = session_transport(&fake).await;
    (fake, t)
}

async fn failing_without_session(reply: Reply) -> (Fake, HttpTransport) {
    let fake = Fake::start(move |_| reply.clone()).await;
    let t = fake.transport().await;
    (fake, t)
}

#[tokio::test]
async fn http_400_without_a_jsonrpc_body() {
    let (_f, t) = failing_without_session(Reply::text(400, "Bad Request: missing header")).await;
    match t.request("tools/list", None).await {
        Err(TransportError::Http { status: 400, detail }) => {
            assert!(detail.contains("missing header"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn http_400_with_a_null_id_jsonrpc_body_is_http_not_rpc() {
    // The real server's answer to a missing session header: about the
    // request's framing, not a refusal of the method.
    let body = json!({"jsonrpc": "2.0", "id": null,
        "error": {"code": -32000, "message": "Bad Request: No valid session ID provided"}});
    let (_f, t) = failing_without_session(Reply::json(400, &body)).await;
    match t.request("tools/list", None).await {
        Err(TransportError::Http { status: 400, detail }) => {
            assert!(detail.contains("No valid session ID provided"), "{detail}");
            assert!(detail.contains("-32000"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn http_400_echoing_our_id_is_the_server_declining_the_call() {
    let fake = Fake::start(|req| {
        Reply::json(
            400,
            &json!({"jsonrpc": "2.0", "id": req.json()["id"],
                "error": {"code": -32602, "message": "Unknown tool"}}),
        )
    })
    .await;
    let t = fake.transport().await;
    assert!(matches!(
        t.request("tools/call", None).await,
        Err(TransportError::Rpc(e)) if e.code == -32602
    ));
}

#[tokio::test]
async fn http_401_and_403_have_their_own_honest_messages() {
    let (_f, t) = failing_without_session(Reply::text(401, "nope")).await;
    let e = t.request("tools/list", None).await.unwrap_err();
    assert!(matches!(e, TransportError::Unauthorized { .. }), "{e:?}");
    assert_eq!(
        e.to_string(),
        "server requires authentication (HTTP 401): server \"fake\" has no `headers`; add one, \
         such as `Authorization`"
    );

    let (_f, t) = failing_without_session(Reply::json(
        403,
        &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -1, "message": "no"}}),
    ))
    .await;
    let e = t.request("tools/list", None).await.unwrap_err();
    assert!(matches!(e, TransportError::Forbidden { .. }), "{e:?}");
    assert!(e.to_string().contains("(HTTP 403)"), "{e}");
}

#[tokio::test]
async fn http_404_with_a_session_is_session_expired_and_without_is_not() {
    let (fake, t) = failing_after_session(Reply::text(404, "Session not found")).await;
    let e = t.request("tools/list", None).await.unwrap_err();
    assert!(matches!(e, TransportError::SessionExpired), "{e:?}");
    // It carried the session, which is what made the 404 mean that.
    assert_eq!(fake.requests().last().unwrap().header("mcp-session-id"), Some("sess-abc123"));
    // And a notification is no different.
    assert!(matches!(
        t.notify("notifications/x", None).await,
        Err(TransportError::SessionExpired)
    ));

    // No session: a 404 is a wrong URL, and must not trigger a
    // re-initialize that cannot help.
    let (_f, t) = failing_without_session(Reply::text(404, "Not Found")).await;
    match t.request("tools/list", None).await {
        Err(TransportError::Http { status: 404, .. }) => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn http_429_and_5xx_are_http_errors_with_their_detail() {
    let (_f, t) = failing_without_session(
        Reply::text(429, "slow down").header("retry-after", "7"),
    )
    .await;
    match t.request("tools/list", None).await {
        Err(TransportError::Http { status: 429, detail }) => {
            assert!(detail.contains("slow down"), "{detail}");
            assert!(detail.contains("retry after 7"), "{detail}");
        }
        other => panic!("{other:?}"),
    }

    // A 5xx is the server's state even when it says it in JSON-RPC, and
    // even when the id matches: not something to hand the model as an
    // in-band tool failure.
    let fake = Fake::start(|req| {
        Reply::json(
            503,
            &json!({"jsonrpc": "2.0", "id": req.json()["id"],
                "error": {"code": -32603, "message": "overloaded"}}),
        )
    })
    .await;
    let t = fake.transport().await;
    match t.request("tools/list", None).await {
        Err(TransportError::Http { status: 503, detail }) => {
            assert!(detail.contains("overloaded"), "{detail}")
        }
        other => panic!("{other:?}"),
    }

    let (_f, t) = failing_without_session(Reply::status(500)).await;
    match t.request("tools/list", None).await {
        Err(TransportError::Http { status: 500, detail }) => {
            assert_eq!(detail, "Internal Server Error", "no body: the status text alone")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn redirect_is_refused_and_the_target_never_contacted() {
    let target = Fake::start(|_| Reply::status(200)).await;
    let location = target.url();
    let origin = Fake::start(move |_| Reply::status(307).header("location", &location)).await;

    let t = HttpTransport::connect(&origin.config(&[("Authorization", "Bearer s3cret")]))
        .await
        .unwrap();
    match t.request("tools/list", None).await {
        Err(TransportError::Http { status: 307, detail }) => {
            assert!(detail.contains("not followed"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
    assert!(target.requests().is_empty(), "the redirect was followed");
}

#[tokio::test]
async fn error_body_diagnostics_are_capped_and_free_of_control_characters() {
    let hostile = format!("\x1b]0;pwned\x07\x1b[31mred\x1b[0m\r\n{}", "A".repeat(100_000));
    let (_f, t) = failing_without_session(Reply::text(500, &hostile)).await;
    assert!(t.diagnostics().is_empty(), "nothing is said before something fails");

    let e = t.request("tools/list", None).await.unwrap_err();
    let diagnostics = t.diagnostics();
    assert!(!diagnostics.chars().any(char::is_control), "{diagnostics:?}");
    assert!(diagnostics.chars().count() <= DIAGNOSTIC_CAP + 1, "{}", diagnostics.len());
    assert!(diagnostics.contains("red"));
    // The error message quotes far less than the diagnostics keep.
    assert!(e.to_string().chars().count() < DETAIL_CAP + 40, "{e}");
}

#[tokio::test]
async fn neither_credentials_nor_session_id_are_ever_shown() {
    let fake = Fake::start(mcp_server(|_, _| Reply::text(500, "boom"))).await;
    let t = HttpTransport::connect(&fake.config(&[("Authorization", "Bearer s3cret-token")]))
        .await
        .unwrap();
    t.request("initialize", Some(json!({}))).await.unwrap();
    let e = t.request("tools/list", None).await.unwrap_err();

    for shown in [format!("{t:?}"), t.diagnostics(), e.to_string()] {
        assert!(!shown.contains("s3cret-token"), "{shown}");
        assert!(!shown.contains("sess-abc123"), "{shown}");
        assert!(!shown.to_lowercase().contains("authorization"), "{shown}");
    }
    assert!(format!("{t:?}").contains("has_session: true"));
}

// ---------------------------------------------------- session & headers

#[tokio::test]
async fn session_and_version_headers_follow_the_handshake() {
    // Through `McpClient`, so the handshake hook is what is under test.
    let fake = Fake::start(mcp_server(|_, body| {
        Reply::sse_message(&result_of(&body["id"], json!({"tools": []})))
    }))
    .await;
    let config = fake.config(&[("Authorization", "Bearer s3cret")]);
    let client = McpClient::connect(&config).await;
    assert_eq!(client.state(), ConnectionState::Ready, "{:?}", client.state());
    assert_eq!(client.pid(), None);
    client.list_tools().await.unwrap();

    let seen = fake.requests();
    let methods: Vec<_> = seen.iter().map(|r| r.rpc_method().unwrap()).collect();
    assert_eq!(methods, ["initialize", "notifications/initialized", "tools/list"]);

    // `initialize` proposes a version and starts a session: neither header.
    assert_eq!(seen[0].header("mcp-session-id"), None);
    assert_eq!(seen[0].header("mcp-protocol-version"), None);
    // Everything after carries both, including the notification that
    // completes the handshake.
    for later in &seen[1..] {
        assert_eq!(later.header("mcp-session-id"), Some("sess-abc123"), "{:?}", later.rpc_method());
        assert_eq!(later.header("mcp-protocol-version"), Some("2025-06-18"));
    }
    for r in &seen {
        assert_eq!(r.header("authorization"), Some("Bearer s3cret"));
    }
}

#[tokio::test]
async fn a_second_initialize_starts_a_fresh_session() {
    let fake = Fake::start(mcp_server(|_, _| Reply::status(500))).await;
    let t = fake.transport().await;
    t.request("initialize", Some(json!({}))).await.unwrap();
    t.set_protocol_version("2025-06-18");
    t.request("initialize", Some(json!({}))).await.unwrap();

    let second = &fake.requests()[1];
    assert_eq!(second.header("mcp-session-id"), None);
    assert_eq!(second.header("mcp-protocol-version"), None);
}

#[tokio::test]
async fn a_server_without_sessions_gets_no_session_header() {
    let fake = Fake::start(|req| {
        Reply::json(200, &result_of(&req.json()["id"], json!({})))
    })
    .await;
    let t = fake.transport().await;
    t.request("initialize", Some(json!({}))).await.unwrap();
    t.request("tools/list", None).await.unwrap();
    assert_eq!(fake.requests()[1].header("mcp-session-id"), None);
}

// --------------------------------------------------------- the network

#[tokio::test]
async fn connection_refused_fails_fast_and_clearly() {
    // A port that was open a moment ago and is not now.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let config = ServerConfig::http("dead", format!("http://127.0.0.1:{port}/mcp"), BTreeMap::new());
    let t = HttpTransport::connect_with(
        &config,
        HttpOptions { connect_timeout: Duration::from_secs(30), request_timeout: Duration::from_secs(30) },
    )
    .await
    .unwrap();

    let started = Instant::now();
    let e = t.request("initialize", None).await.unwrap_err();
    match &e {
        TransportError::Disconnected(m) => {
            assert!(m.starts_with(&format!("cannot connect to 127.0.0.1:{port}")), "{m}");
        }
        other => panic!("{other:?}"),
    }
    // Neither the connect nor the request timeout (30 s each) was waited out.
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    // A refusal is about one request: the transport is not closed, so a
    // server that comes back is reachable again.
    assert_eq!(t.close_reason(), None);
}

#[tokio::test]
async fn per_request_timeout_fires_on_a_stalled_stream() {
    let fake = Fake::start(|_| {
        // Headers, then half a frame, then silence.
        Reply::sse_parts(vec![Part::Bytes(b"event: message\ndata: {\"jsonrpc\"".to_vec())])
            .stalled()
    })
    .await;
    let t = fake.transport().await;

    let started = Instant::now();
    let e = t
        .request_with_timeout("tools/call", None, Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, TransportError::Timeout { method, .. } if method == "tools/call"),
        "{e:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(t.close_reason(), None, "a timeout is not a death");
}

#[tokio::test]
async fn a_slow_but_alive_stream_is_not_cut_by_a_body_timeout() {
    // The reason there is no reqwest `.timeout()`: the reply here arrives
    // in two halves with the connection quiet between them, longer than
    // any timeout that would have been applied to the body.
    let gate: Gate = Arc::new(Notify::new());
    let fake = {
        let gate = Arc::clone(&gate);
        Fake::start(move |req| {
            let id = req.json()["id"].clone();
            Reply::sse_parts(vec![
                Part::Bytes(frame(&json!({"jsonrpc": "2.0", "method": "notifications/progress"}))),
                Part::Wait(Arc::clone(&gate)),
                Part::Bytes(frame(&result_of(&id, json!({"late": true})))),
            ])
        })
        .await
    };
    let t = fake.transport().await;
    let call = t.request("tools/call", None);
    tokio::pin!(call);
    // Release only after the server has the request, so the reply is
    // really in flight and the transport is really reading it.
    tokio::select! {
        r = &mut call => panic!("answered before the gate opened: {r:?}"),
        () = fake.wait_for_requests(1) => {}
    }
    gate.notify_one();
    assert_eq!(call.await.unwrap(), json!({"late": true}));
}

#[tokio::test]
async fn shutdown_is_idempotent_and_fails_what_is_in_flight() {
    let fake = Fake::start(|_| Reply::sse_parts(vec![]).stalled()).await;
    let t = Arc::new(fake.transport().await);

    let in_flight = {
        let t = Arc::clone(&t);
        tokio::spawn(async move { t.request("tools/call", None).await })
    };
    fake.wait_for_requests(1).await;

    t.shutdown().await;
    t.shutdown().await;

    match in_flight.await.unwrap() {
        Err(TransportError::Closed(m)) => assert!(m.contains("was shut down"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(t.close_reason().unwrap().contains("was shut down"));
    // Refused locally: nothing more reaches the server.
    assert!(matches!(t.request("tools/list", None).await, Err(TransportError::Closed(_))));
    assert!(matches!(t.notify("notifications/x", None).await, Err(TransportError::Closed(_))));
    assert_eq!(fake.requests().len(), 1);
}

// ------------------------------------------------------------- loopback

#[tokio::test]
async fn loopback_server_bypasses_a_configured_proxy() {
    let proxy = Fake::start(|_| Reply::text(502, "proxy cannot reach loopback")).await;
    let target = Fake::start(|req| {
        Reply::json(200, &result_of(&req.json()["id"], json!({"direct": true})))
    })
    .await;

    // Control: this reqwest, given the same proxy, does send loopback
    // traffic through it. Without this the assertion below could pass
    // only because reqwest never proxied loopback in the first place.
    let plain = reqwest::Client::builder()
        .proxy(Proxy::all(format!("http://{}", proxy.addr)).unwrap())
        .build()
        .unwrap();
    let via = plain.post(target.url()).body("{}").send().await.unwrap();
    assert_eq!(via.status().as_u16(), 502);
    assert_eq!(proxy.requests().len(), 1);
    assert!(target.requests().is_empty());

    let t = HttpTransport::build(
        &target.config(&[]),
        HttpOptions::default(),
        Some(Proxy::all(format!("http://{}", proxy.addr)).unwrap()),
        &|_| None,
    )
    .unwrap();
    assert_eq!(t.request("tools/list", None).await.unwrap(), json!({"direct": true}));
    assert_eq!(proxy.requests().len(), 1, "the transport used the proxy for a loopback host");
    assert_eq!(target.requests().len(), 1);
}

#[test]
fn which_hosts_count_as_loopback() {
    let loopback = |u: &str| is_loopback(&Url::parse(u).unwrap());
    for yes in [
        "http://localhost:8931/mcp",
        "http://LOCALHOST/mcp",
        "http://localhost./mcp",
        "http://playwright.localhost/mcp",
        "http://127.0.0.1:1/",
        "http://127.8.9.10/",
        "http://[::1]:8080/",
        "http://[::ffff:127.0.0.1]/",
    ] {
        assert!(loopback(yes), "{yes}");
    }
    for no in [
        "https://mcp.example.com/mcp",
        "http://10.0.0.5/",
        "http://192.168.1.2:8931/",
        "http://0.0.0.0/",
        "http://128.0.0.1/",
        "http://localhost.example.com/",
        "http://notlocalhost/",
        "http://[::2]/",
        "http://[2001:db8::1]/",
    ] {
        assert!(!loopback(no), "{no}");
    }
}

// --------------------------------------------------------------- config

#[tokio::test]
async fn unusable_configs_are_refused_without_echoing_secrets() {
    let http = |url: &str, headers: &[(&str, &str)]| {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ServerConfig::http("s", url, headers)
    };

    let e = HttpTransport::connect(&http("ftp://example.com/mcp", &[])).await.unwrap_err();
    assert!(matches!(&e, TransportError::Unsupported(m) if m.contains("ftp")), "{e:?}");

    let e = HttpTransport::connect(&http("not a url?token=hunter2", &[])).await.unwrap_err();
    assert!(matches!(e, TransportError::Unsupported(ref m) if !m.contains("hunter2")), "{e:?}");

    let e = HttpTransport::connect(&http("http://localhost/", &[("Authorization", "Bearer a\nb")]))
        .await
        .unwrap_err();
    match e {
        TransportError::Unsupported(m) => {
            assert!(m.contains("Authorization"), "{m}");
            assert!(!m.contains("Bearer"), "{m}");
        }
        other => panic!("{other:?}"),
    }

    let stdio = ServerConfig::stdio("s", "true", vec![], BTreeMap::new());
    assert!(matches!(
        HttpTransport::connect(&stdio).await,
        Err(TransportError::Unsupported(_))
    ));
}

#[tokio::test]
async fn a_configured_header_cannot_replace_the_content_type() {
    let fake = Fake::start(|req| Reply::json(200, &result_of(&req.json()["id"], json!({})))).await;
    let t = HttpTransport::connect(&fake.config(&[("Content-Type", "text/plain")])).await.unwrap();
    t.request("tools/list", None).await.unwrap();
    assert_eq!(fake.requests()[0].header("content-type"), Some("application/json"));
}

// ----------------------------------------------------------- credentials

const SECRET: &str = "s3cr3t-value-7f3a";

/// Build a transport against `${VAR}` values from `vars`, not from the
/// process environment, which a test must not touch.
fn build_with(
    config: &ServerConfig,
    vars: &'static [(&'static str, &'static str)],
) -> Result<HttpTransport, TransportError> {
    HttpTransport::build(config, HttpOptions::default(), None, &|name: &str| {
        vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    })
}

#[tokio::test]
async fn headers_are_expanded_on_the_wire_and_the_template_never_is() {
    let fake = Fake::start(|req| Reply::json(200, &result_of(&req.json()["id"], json!({})))).await;
    let config = fake.config(&[
        ("Authorization", "Bearer ${GW_TOKEN}"),
        ("X-Plain", "literal"),
        ("X-Escaped", "$${NOT_EXPANDED}"),
        ("X-Dollar", "$HOME"),
    ]);
    let t = build_with(&config, &[("GW_TOKEN", SECRET)]).unwrap();
    t.request("tools/list", None).await.unwrap();

    let req = &fake.requests()[0];
    assert_eq!(req.header("authorization"), Some(format!("Bearer {SECRET}").as_str()));
    assert_eq!(req.header("x-plain"), Some("literal"));
    assert_eq!(req.header("x-escaped"), Some("${NOT_EXPANDED}"));
    assert_eq!(req.header("x-dollar"), Some("$HOME"));
    // The one literal `${` on the wire is the escaped one the user wrote.
    for (name, value) in &req.headers {
        assert!(!value.contains("${GW_TOKEN}"), "{name}: {value}");
    }
}

#[tokio::test]
async fn an_unset_variable_stops_the_connect_and_nothing_is_sent() {
    let fake = Fake::start(|req| Reply::json(200, &result_of(&req.json()["id"], json!({})))).await;
    let config = fake.config(&[("Authorization", "Bearer ${GW_TOKEN}")]);
    let e = build_with(&config, &[("UNRELATED", SECRET)]).unwrap_err();
    match &e {
        TransportError::Unsupported(m) => {
            assert!(m.contains("GW_TOKEN") && m.contains("\"fake\""), "{m}");
            assert!(!m.contains("Bearer") && !m.contains(SECRET), "{m}");
        }
        other => panic!("{other:?}"),
    }
    assert!(fake.requests().is_empty(), "the literal template must never be sent");

    // Through the public entry point, which reads the real environment.
    let config = fake.config(&[("Authorization", "Bearer ${NANOPI_TEST_H4_SURELY_UNSET_9f2c}")]);
    let e = HttpTransport::connect(&config).await.unwrap_err();
    assert!(matches!(&e, TransportError::Unsupported(m) if m.contains("NANOPI_TEST_H4_SURELY_UNSET_9f2c")), "{e:?}");
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn a_templated_host_is_shown_as_written_never_as_expanded() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let config = ServerConfig::http("dead", format!("http://${{HOST}}:{port}/mcp"), BTreeMap::new());
    let t = build_with(&config, &[("HOST", "127.0.0.1")]).unwrap();
    let e = t.request("initialize", None).await.unwrap_err();
    match &e {
        TransportError::Disconnected(m) => {
            assert!(m.starts_with(&format!("cannot connect to ${{HOST}}:{port}")), "{m}");
            assert!(!m.contains("127.0.0.1"), "{m}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!format!("{t:?}").contains("127.0.0.1"));
}

#[tokio::test]
async fn cleartext_credentials_are_refused_at_connect_before_any_io() {
    let headers = BTreeMap::from([("Authorization".to_string(), format!("Bearer {SECRET}"))]);
    for url in [
        "http://mcp.example.com/mcp",
        "http://localhost.evil.com/mcp",
        "http://127.0.0.1.nip.io/mcp",
        "http://127.0.0.1@evil.com/mcp",
        "http://[::ffff:10.0.0.1]/mcp",
    ] {
        let e = HttpTransport::connect(&ServerConfig::http("gw", url, headers.clone()))
            .await
            .unwrap_err();
        match &e {
            TransportError::Unsupported(m) => {
                assert!(m.contains("cleartext") && m.contains("https://"), "{url}: {m}");
                assert!(!m.contains(SECRET), "{m}");
            }
            other => panic!("{url}: {other:?}"),
        }
    }
    // Loopback is the motivating case, and https is always the user's call.
    for url in ["http://127.0.0.1:1/mcp", "http://localhost:1/mcp", "http://[::1]:1/mcp", "https://mcp.example.com/mcp"] {
        assert!(HttpTransport::connect(&ServerConfig::http("gw", url, headers.clone())).await.is_ok(), "{url}");
    }
}

/// The reply a server that echoes the credential it just refused sends.
fn echoing(status: u16) -> Reply {
    Reply::text(status, &format!("invalid credentials: Bearer {SECRET} (token {SECRET})"))
        .header("www-authenticate", "Bearer realm=\"internal-realm\", error=\"invalid_token\"")
}

async fn refused(status: u16, headers: &[(&str, &str)]) -> (Fake, HttpTransport, TransportError) {
    let fake = Fake::start(move |_| echoing(status)).await;
    let t = HttpTransport::connect(&fake.config(headers)).await.unwrap();
    let e = t.request("tools/list", None).await.unwrap_err();
    (fake, t, e)
}

#[tokio::test]
async fn a_401_asks_for_the_headers_names_the_scheme_and_repeats_no_secret() {
    let auth = format!("Bearer {SECRET}");
    let (fake, t, e) = refused(401, &[("Authorization", auth.as_str())]).await;
    assert!(matches!(e, TransportError::Unauthorized { .. }), "{e:?}");
    let m = e.to_string();
    assert_eq!(
        m,
        "server requires authentication (HTTP 401, Bearer): check the `headers` for server \"fake\""
    );
    // The scheme only: not the realm, not the error description, not the body.
    for leaked in [SECRET, "internal-realm", "invalid_token", "invalid credentials"] {
        assert!(!m.contains(leaked), "{leaked} in {m}");
    }
    // What the server said is kept for `/mcp`, with our credential scrubbed out.
    let d = t.diagnostics();
    assert!(d.contains("invalid credentials"), "{d}");
    assert!(!d.contains(SECRET), "{d}");
    assert!(!format!("{t:?}").contains(SECRET));
    // It sent the header; the scrub is not an accident of it being absent.
    assert_eq!(fake.requests()[0].header("authorization"), Some(auth.as_str()));
}

#[tokio::test]
async fn a_401_without_headers_says_to_add_one() {
    let (_f, _t, e) = refused(401, &[]).await;
    let m = e.to_string();
    assert!(m.contains("HTTP 401") && m.contains("has no `headers`") && m.contains("Authorization"), "{m}");
    assert!(!m.contains(SECRET), "{m}");
}

#[tokio::test]
async fn a_403_says_the_credentials_were_rejected_and_repeats_no_secret() {
    let auth = format!("Bearer {SECRET}");
    let (_f, t, e) = refused(403, &[("Authorization", auth.as_str())]).await;
    assert!(matches!(e, TransportError::Forbidden { .. }), "{e:?}");
    let m = e.to_string();
    assert_eq!(
        m,
        "credentials were rejected or access is forbidden (HTTP 403): check the `headers` for \
         server \"fake\""
    );
    assert!(!m.contains(SECRET) && !m.contains("invalid credentials"), "{m}");
    assert!(!t.diagnostics().contains(SECRET), "{}", t.diagnostics());
    // 401 and 403 are different answers, and read differently.
    assert_ne!(m, refused(401, &[("Authorization", auth.as_str())]).await.2.to_string());
}

#[tokio::test]
async fn the_www_authenticate_scheme_is_capped_and_deduplicated() {
    let long = "A".repeat(100);
    let fake = Fake::start(move |_| {
        Reply::text(401, "")
            .header("www-authenticate", &format!("{long} realm=\"x\""))
            .header("www-authenticate", "Basic realm=\"x\", Bearer")
            .header("www-authenticate", "Basic")
            .header("www-authenticate", "Digest nonce=\"zzz\"")
            .header("www-authenticate", "Negotiate")
    })
    .await;
    let t = fake.transport().await;
    let m = t.request("tools/list", None).await.unwrap_err().to_string();
    assert!(m.contains(&format!("HTTP 401, {}/Basic/Digest)", "A".repeat(20))), "{m}");
    assert!(!m.contains(&"A".repeat(21)) && !m.contains("nonce") && !m.contains("Negotiate"), "{m}");
}

#[tokio::test]
async fn a_server_that_quotes_the_credential_in_a_json_error_is_scrubbed_too() {
    let fake = Fake::start(|_| {
        Reply::json(
            500,
            &json!({"jsonrpc": "2.0", "id": null,
                "error": {"code": -32000, "message": format!("could not verify {SECRET} for you")}}),
        )
    })
    .await;
    let auth = format!("Bearer {SECRET}");
    let t = HttpTransport::connect(&fake.config(&[("Authorization", auth.as_str())])).await.unwrap();
    let e = t.request("tools/list", None).await.unwrap_err();
    match &e {
        TransportError::Http { status: 500, detail } => {
            assert!(detail.contains("could not verify"), "{detail}");
            assert!(!detail.contains(SECRET), "{detail}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!t.diagnostics().contains(SECRET));
}

#[tokio::test]
async fn a_credential_in_the_url_is_scrubbed_from_what_the_server_says_back() {
    let fake = Fake::start(|req| {
        Reply::text(500, &format!("bad request for {}", req.path))
    })
    .await;
    let config = ServerConfig::http("fake", format!("{}?api_key={SECRET}", fake.url()), BTreeMap::new());
    let t = HttpTransport::connect(&config).await.unwrap();
    let e = t.request("tools/list", None).await.unwrap_err();
    assert!(!e.to_string().contains(SECRET), "{e}");
    assert!(!t.diagnostics().contains(SECRET), "{}", t.diagnostics());
}

#[tokio::test]
async fn a_401_or_403_on_a_live_session_is_not_a_session_expiry() {
    for (status, what) in [(401u16, "Unauthorized"), (403, "Forbidden")] {
        let (fake, t) = failing_after_session(Reply::text(status, "no")).await;
        let e = t.request("tools/list", None).await.unwrap_err();
        assert!(
            matches!(e, TransportError::Unauthorized { .. } | TransportError::Forbidden { .. }),
            "{status}: {e:?}"
        );
        assert!(!matches!(e, TransportError::SessionExpired), "{what}");
        // It carried the session id, and still is not `SessionExpired`.
        assert_eq!(fake.requests().last().unwrap().header("mcp-session-id"), Some("sess-abc123"));
    }
}

#[tokio::test]
async fn a_401_server_is_a_status_row_with_the_advice_and_no_secret() {
    use crate::mcp::manager::{ManagerOptions, McpManager};

    let fake = Fake::start(|_| echoing(401)).await;
    let auth = format!("Bearer {SECRET}");
    let config = ServerConfig::http(
        "gw",
        format!("{}?api_key={SECRET}", fake.url()),
        BTreeMap::from([("Authorization".to_string(), auth.clone())]),
    );
    let manager = McpManager::connect(vec![config], ManagerOptions::default()).await;

    let status = manager.status();
    assert_eq!(status.len(), 1);
    let row = &status[0];
    assert!(!row.connected && row.tool_count == 0, "{row:?}");
    assert_eq!(row.endpoint, format!("{}?***", fake.url()));
    let reason = row.reason.as_deref().unwrap();
    assert!(reason.contains("requires authentication"), "{reason}");
    assert!(reason.contains("check the `headers` for server \"gw\""), "{reason}");

    // Everything the manager would show, in one place.
    let everything = format!("{row:?} {:?} {:?}", manager.notices(), manager.status());
    for leaked in [SECRET, "Authorization", "internal-realm", "invalid credentials"] {
        assert!(!everything.contains(leaked), "{leaked} leaked: {everything}");
    }
    assert!(fake.requests().iter().all(|r| r.rpc_method().as_deref() != Some("tools/list")));
}

#[tokio::test]
async fn an_unset_variable_at_connect_is_a_status_row_naming_the_variable() {
    use crate::mcp::manager::{ManagerOptions, McpManager};

    let fake = Fake::start(|req| Reply::json(200, &result_of(&req.json()["id"], json!({})))).await;
    let config = ServerConfig::http(
        "gw",
        format!("{}?api_key=${{NANOPI_TEST_H4_SURELY_UNSET_9f2c}}", fake.url()),
        BTreeMap::from([("Authorization".to_string(), format!("Bearer {SECRET}"))]),
    );
    let manager = McpManager::connect(vec![config], ManagerOptions::default()).await;
    let row = &manager.status()[0];
    assert!(!row.connected);
    let reason = row.reason.as_deref().unwrap();
    assert!(reason.contains("NANOPI_TEST_H4_SURELY_UNSET_9f2c"), "{reason}");
    assert!(!reason.contains(SECRET), "{reason}");
    // The row shows the template's masked form: the variable's name is
    // inside the masked query, so not even that.
    assert_eq!(row.endpoint, format!("{}?***", fake.url()));
    assert!(fake.requests().is_empty(), "nothing was sent");
}

// ----------------------------------------------------- hostile-ish input

fn chunks(pieces: Vec<Vec<u8>>) -> impl Stream<Item = Result<Bytes, String>> + Send + 'static {
    futures_util::stream::iter(pieces.into_iter().map(|p| Ok(Bytes::from(p))))
}

#[tokio::test]
async fn events_survive_being_split_at_every_byte() {
    let wire = [
        frame(&json!({"jsonrpc": "2.0", "method": "notifications/progress"})),
        b": comment\r\n\r\n".to_vec(),
        frame(&result_of(&json!(7), json!({"n": 1}))),
    ]
    .concat();
    let mut reader = EventReader::new(chunks(wire.iter().map(|b| vec![*b]).collect()), MAX_EVENT);
    assert!(matches!(reader.next().await, Ok(Some(Incoming::Notification(_)))));
    assert!(matches!(reader.next().await, Ok(Some(Incoming::Response(_)))));
    assert!(matches!(reader.next().await, Ok(None)));
}

#[tokio::test]
async fn a_line_that_never_ends_is_refused_after_the_cap() {
    // An endless `data:` line, 4 KiB at a time, against a 256 KiB cap.
    let pulled = Arc::new(AtomicUsize::new(0));
    let endless = {
        let pulled = Arc::clone(&pulled);
        futures_util::stream::repeat_with(move || {
            pulled.fetch_add(1, Ordering::Relaxed);
            Ok(Bytes::from(vec![b'a'; 4096]))
        })
    };
    let cap = 256 * 1024;
    let mut reader = EventReader::new(endless, cap);
    let e = reader.next().await.unwrap_err();
    assert!(matches!(&e, TransportError::Protocol(m) if m.contains("refusing to buffer")), "{e:?}");
    // It stopped reading: the buffer never held more than the cap plus
    // the one chunk that tripped it.
    let pulled = pulled.load(Ordering::Relaxed);
    assert_eq!(pulled, cap / 4096 + 1, "kept reading past the cap");
}

#[tokio::test]
async fn a_long_stream_of_small_events_is_not_a_big_event() {
    // 20 000 events of ~60 bytes: over a 64 KiB cap in total, far under
    // it per event. The cap is on what is buffered, so this must pass.
    let one = frame(&json!({"jsonrpc": "2.0", "method": "notifications/progress"}));
    let mut reader = EventReader::new(
        chunks(std::iter::repeat(one).take(20_000).collect()),
        64 * 1024,
    );
    let mut seen = 0;
    while let Some(m) = reader.next().await.unwrap() {
        assert!(matches!(m, Incoming::Notification(_)));
        seen += 1;
    }
    assert_eq!(seen, 20_000);
}

#[tokio::test]
async fn the_real_cap_admits_a_multi_megabyte_event_and_stops_at_the_limit() {
    // A 3 MiB screenshot-sized event goes through.
    let big = frame(&result_of(&json!(1), json!({"image": "A".repeat(3 * 1024 * 1024)})));
    let mut reader = EventReader::new(
        chunks(big.chunks(16 * 1024).map(<[u8]>::to_vec).collect()),
        MAX_EVENT,
    );
    assert!(matches!(reader.next().await, Ok(Some(Incoming::Response(_)))));

    // An endless line in 16 KiB chunks is refused at the real cap. This
    // is also the CPU worst case: before `SseStream` resumed its newline
    // scan it rescanned the whole partial line per chunk, which took
    // half a minute here.
    let over = futures_util::stream::repeat_with(|| Ok(Bytes::from(vec![b'a'; 16 * 1024])));
    let mut reader = EventReader::new(over, MAX_EVENT);
    assert!(matches!(reader.next().await, Err(TransportError::Protocol(_))));
}

#[tokio::test]
async fn an_oversized_json_body_is_refused() {
    let padding = "a".repeat(MAX_EVENT + 1024);
    let fake = Fake::start(move |req| {
        Reply::json(200, &result_of(&req.json()["id"], json!({"pad": padding})))
    })
    .await;
    let t = fake.transport().await;
    assert!(matches!(
        t.request("tools/call", None).await,
        Err(TransportError::Protocol(m)) if m.contains("larger than 16 MiB")
    ));
}

#[test]
fn sanitize_drops_controls_collapses_space_and_caps() {
    assert_eq!(sanitize("a\x1b[1mb\r\n\t c\x07", 100), "a[1mb c");
    assert_eq!(sanitize("  \n ", 10), "");
    assert_eq!(sanitize("abcdef", 3), "abc…");
    assert_eq!(sanitize("日本語のテキスト", 3), "日本語…");
}

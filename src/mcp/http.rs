//! Streamable HTTP transport (spec 2025-06-18). Design: `docs/mcp-http.md`.
//!
//! Where stdio turns one pipe into many request futures, HTTP needs no
//! multiplexer: every request is its own POST on its own (pooled)
//! connection, so a request future owns its whole exchange and there is
//! no reader task, no pending map, and nothing a dead peer can leave
//! half-drained. What stays hard is that the *reply* is not shaped like
//! the request.
//!
//! **A POST answers in one of two content types, and a real server picks
//! the less obvious one.** The Playwright server answers *every* request,
//! a 17 KB `tools/list` included, as `text/event-stream` carrying one
//! `message` event. A client that handled only `application/json` would
//! fail its first call. So the body is dispatched on `Content-Type`, and
//! on a stream the reply is the event whose `id` is ours: the same
//! stream may carry progress notifications first, or a server request
//! (`sampling/createMessage`) that blocks the server until answered.
//!
//! **Errors arrive on two channels** — an HTTP status, and a JSON-RPC
//! `error` inside a `200` stream — and must stay distinguishable from
//! each other and from a transport that merely failed to deliver. See the
//! variants of [`TransportError`] this module produces.
//!
//! **Nothing here bounds time except us.** reqwest's whole-request
//! `.timeout()` also covers reading the body, so it would kill a tool
//! call that legitimately streams for minutes. The client therefore only
//! has a connect timeout (a dead host must fail in seconds), and each
//! request is wrapped in its own deadline covering the whole exchange up
//! to the matching reply.
//!
//! **A redirect is never followed.** A `3xx` to a POST that carries
//! `Authorization` would send the credential to a host the user never
//! configured.

use std::error::Error as StdError;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Client, Proxy, StatusCode, Url};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::mcp::config::{ServerConfig, ServerSpec};
use crate::mcp::protocol::method::INITIALIZE;
use crate::mcp::protocol::{
    error_code, Incoming, Notification, Request, RequestId, Response, ResponsePayload, RpcError,
};
use crate::mcp::stdio::DEFAULT_REQUEST_TIMEOUT;
use crate::mcp::transport::TransportError;
use crate::provider::sse::{SseError, SseStream};

/// How long a TCP connect (and DNS lookup, and TLS handshake) may take.
/// Short because it is the only thing standing between a dead host and a
/// caller waiting out the whole call timeout; a reachable server accepts
/// in milliseconds, and "refused" returns at once regardless.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest single SSE event, or JSON body, accepted. Matches
/// `stdio::MAX_LINE`, for the same reason: an `image` content block is
/// base64 and routinely megabytes, so the cap must sit well above any
/// honest reply while still stopping a server that streams one endless
/// event. It bounds what is *buffered*, not the stream: a long stream of
/// small events (progress, keep-alives) is unbounded in length and
/// constant in memory.
const MAX_EVENT: usize = 16 * 1024 * 1024;

/// How much of a non-2xx body is read at all. An error page is for
/// diagnosis; reading past the first few KB only lets a hostile server
/// spend our time.
const ERROR_BODY_READ: usize = 4 * 1024;

/// Characters of the last error body kept for [`HttpTransport::diagnostics`].
const DIAGNOSTIC_CAP: usize = 1024;

/// Characters of an error body quoted in the error message itself, which
/// can end up in front of a model — far less than the diagnostics keep.
const DETAIL_CAP: usize = 200;

const SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");
const PROTOCOL_VERSION: HeaderName = HeaderName::from_static("mcp-protocol-version");

/// Tunables a caller may want to differ per server.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// The headers that tie requests to the negotiated session. Held as
/// `HeaderValue`s, not strings: they are only ever replayed, and the
/// session id is marked sensitive so nothing that prints a request can
/// show it.
#[derive(Default)]
struct Session {
    id: Option<HeaderValue>,
    protocol_version: Option<HeaderValue>,
}

/// A live Streamable HTTP connection to one MCP server.
///
/// "Live" is loose: nothing is held open between requests, and
/// [`connect`](Self::connect) does no network I/O. The first request
/// (`initialize`) is the first contact, so a dead host surfaces there.
pub struct HttpTransport {
    name: String,
    client: Client,
    url: Url,
    /// `host:port`, the only part of the URL that is ever shown. The path
    /// and query may carry a token and the userinfo is a credential.
    endpoint: String,
    /// Configured headers plus the fixed ones. Secrets: never printed.
    base_headers: HeaderMap,
    session: Mutex<Session>,
    next_id: AtomicI64,
    options: HttpOptions,
    /// The last thing the server said that was not a protocol message —
    /// HTTP's counterpart of stdio's stderr tail. One slot, not a log:
    /// there is no stream to drain, only the most recent failure to
    /// explain.
    last_error_body: Mutex<String>,
    /// `Some` only after [`shutdown`](Self::shutdown). See
    /// [`close_reason`](Self::close_reason).
    closed: Mutex<Option<String>>,
    /// Fired by `shutdown` to fail requests that are mid-flight, as
    /// stdio's `Shared::close` fails its pending map.
    cancel: CancellationToken,
}

impl HttpTransport {
    pub async fn connect(config: &ServerConfig) -> Result<Self, TransportError> {
        Self::connect_with(config, HttpOptions::default()).await
    }

    pub async fn connect_with(
        config: &ServerConfig,
        options: HttpOptions,
    ) -> Result<Self, TransportError> {
        Self::build(config, options, None)
    }

    /// `proxy` stands in for the process-wide environment proxy, which
    /// reqwest reads once per process and so cannot be varied per test.
    /// Production always passes `None`.
    fn build(
        config: &ServerConfig,
        options: HttpOptions,
        proxy: Option<Proxy>,
    ) -> Result<Self, TransportError> {
        let ServerSpec::Http { url, headers } = &config.spec else {
            return Err(TransportError::Unsupported(format!(
                "server \"{}\" is not an HTTP server",
                config.name
            )));
        };
        // The parse error is not echoed with the input: it would be the
        // one place a malformed URL carrying a token could leak.
        let url = Url::parse(url).map_err(|e| {
            TransportError::Unsupported(format!(
                "server \"{}\" has an invalid url ({e})",
                config.name
            ))
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(TransportError::Unsupported(format!(
                "server \"{}\": url scheme `{}` is not http or https",
                config.name,
                url.scheme()
            )));
        }
        let Some(host) = url.host_str() else {
            return Err(TransportError::Unsupported(format!(
                "server \"{}\": url has no host",
                config.name
            )));
        };
        let endpoint = match url.port_or_known_default() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };

        let mut client = crate::net::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(options.connect_timeout);
        if let Some(proxy) = proxy {
            client = client.proxy(proxy);
        }
        // A proxy cannot reach the user's own 127.0.0.1: it would dial
        // its own loopback, or refuse. Honouring `http_proxy` here turns
        // a working local server into an unexplained failure, and the
        // motivating case (a container published on a loopback port) is
        // exactly this one. Decided per server, not globally, so a remote
        // server in the same config still goes through the proxy.
        if is_loopback(&url) {
            client = client.no_proxy();
        }
        let client = client.build().map_err(|e| {
            TransportError::Unsupported(format!(
                "server \"{}\": cannot build an HTTP client: {}",
                config.name,
                error_chain(&e.without_url())
            ))
        })?;

        Ok(Self {
            name: config.name.clone(),
            client,
            url,
            endpoint,
            base_headers: base_headers(&config.name, headers)?,
            session: Mutex::new(Session::default()),
            next_id: AtomicI64::new(1),
            options,
            last_error_body: Mutex::new(String::new()),
            closed: Mutex::new(None),
            cancel: CancellationToken::new(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Record the revision `initialize` settled on; every later request
    /// carries it in `MCP-Protocol-Version`. Not sent on `initialize`
    /// itself, which is what proposes a version.
    pub fn set_protocol_version(&self, version: &str) {
        // Only fails on control characters, which no version string the
        // client accepted can contain; skipping the header is then the
        // right degradation (the spec has a server assume a default).
        if let Ok(v) = HeaderValue::from_str(version) {
            self.session.lock().unwrap().protocol_version = Some(v);
        }
    }

    /// `Some(reason)` only once [`shutdown`](Self::shutdown) has run.
    ///
    /// HTTP has no process and no pipe, so there is no event that makes
    /// the transport unusable by itself: a refused connection or a `5xx`
    /// is a property of one request, and the same server may answer the
    /// next (a container restarting is the common case). Reporting those
    /// as "closed" would make `McpClient` mark the server `Failed`
    /// permanently over a blip, so they are [`TransportError::Disconnected`]
    /// and [`TransportError::Http`] instead. "Closed" means what it
    /// means for stdio after `shutdown`: we ended it, on purpose.
    pub fn close_reason(&self) -> Option<String> {
        self.closed.lock().unwrap().clone()
    }

    /// A capped, control-character-free snippet of the last error body
    /// the server sent, for `/mcp`. Never anything we sent: request
    /// headers carry the credentials.
    pub fn diagnostics(&self) -> String {
        self.last_error_body.lock().unwrap().clone()
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, TransportError> {
        self.request_with_timeout(method, params, self.options.request_timeout)
            .await
    }

    /// The result is the raw `result` member, as in stdio.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, TransportError> {
        self.check_open()?;
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let body = encode(&Request::new(id.clone(), method, params))?;
        self.bounded(method, timeout, self.exchange(method, &id, body))
            .await
    }

    /// Expects `202 Accepted` and nothing else: any other success status
    /// means the server did something other than what the spec says it
    /// does with a notification, and "it probably worked" is how a
    /// missed `initialized` goes unnoticed.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), TransportError> {
        self.check_open()?;
        let body = encode(&Notification::new(method, params))?;
        self.bounded(method, self.options.request_timeout, async {
            let (headers, had_session) = self.headers_for(method);
            let response = self.post(body, headers).await?;
            let status = response.status();
            if status == StatusCode::ACCEPTED {
                return Ok(());
            }
            if status.is_success() {
                return Err(TransportError::Protocol(format!(
                    "server answered notification `{method}` with HTTP {}, expected 202",
                    status.as_u16()
                )));
            }
            Err(self.status_error(response, had_session, None).await)
        })
        .await
    }

    /// End the transport. Idempotent: the first call wins and later ones
    /// return at once.
    ///
    /// H3 sends `DELETE` here — after the flag is set and in-flight
    /// requests are cancelled, while `session` still holds the id, and
    /// bypassing `check_open`, which now refuses — with its own short
    /// timeout, because shutdown must not wait on a server that is gone.
    pub async fn shutdown(&self) {
        {
            let mut closed = self.closed.lock().unwrap();
            if closed.is_some() {
                return;
            }
            *closed = Some(format!("server \"{}\" was shut down", self.name));
        }
        self.cancel.cancel();
    }

    // --------------------------------------------------------- internals

    fn check_open(&self) -> Result<(), TransportError> {
        match self.close_reason() {
            Some(reason) => Err(TransportError::Closed(reason)),
            None => Ok(()),
        }
    }

    /// Run one exchange under the caller's deadline and `shutdown`.
    ///
    /// The deadline wraps the *whole* exchange, body included, which is
    /// the point: it is the replacement for the reqwest timeout we
    /// deliberately do not set.
    async fn bounded<T>(
        &self,
        method: &str,
        timeout: Duration,
        work: impl std::future::Future<Output = Result<T, TransportError>>,
    ) -> Result<T, TransportError> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(TransportError::Closed(
                self.close_reason().unwrap_or_else(|| "the transport was shut down".to_string()),
            )),
            outcome = tokio::time::timeout(timeout, work) => match outcome {
                Ok(result) => result,
                Err(_) => Err(TransportError::Timeout { method: method.to_string(), timeout }),
            },
        }
    }

    /// Headers for one request, and whether it carried a session id —
    /// which is what makes a later `404` mean "session expired" rather
    /// than "wrong URL".
    ///
    /// `initialize` gets neither session nor version: it *starts* a
    /// session, so replaying an old id would be asking to resume one the
    /// server may have dropped, and the version header is the thing it is
    /// still negotiating.
    fn headers_for(&self, method: &str) -> (HeaderMap, bool) {
        let mut headers = self.base_headers.clone();
        if method == INITIALIZE {
            return (headers, false);
        }
        let session = self.session.lock().unwrap();
        if let Some(v) = &session.protocol_version {
            headers.insert(PROTOCOL_VERSION, v.clone());
        }
        match &session.id {
            Some(id) => {
                headers.insert(SESSION_ID, id.clone());
                (headers, true)
            }
            None => (headers, false),
        }
    }

    async fn post(
        &self,
        body: Bytes,
        headers: HeaderMap,
    ) -> Result<reqwest::Response, TransportError> {
        self.client
            .post(self.url.clone())
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|e| self.send_error(e))
    }

    /// Every failure here is "did not get through", so none is `Closed`
    /// (see [`close_reason`](Self::close_reason)). The URL is stripped
    /// from reqwest's message, which would otherwise print the path and
    /// query, and only `host:port` is named.
    fn send_error(&self, e: reqwest::Error) -> TransportError {
        let e = e.without_url();
        let what = if e.is_timeout() {
            "timed out connecting to"
        } else if e.is_connect() {
            "cannot connect to"
        } else {
            "request to"
        };
        TransportError::Disconnected(format!("{what} {}: {}", self.endpoint, error_chain(&e)))
    }

    async fn exchange(
        &self,
        method: &str,
        id: &RequestId,
        body: Bytes,
    ) -> Result<Value, TransportError> {
        let (headers, had_session) = self.headers_for(method);
        let response = self.post(body, headers).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.status_error(response, had_session, Some(id)).await);
        }
        if method == INITIALIZE {
            self.capture_session(response.headers());
        }
        if status == StatusCode::ACCEPTED || status == StatusCode::NO_CONTENT {
            return Err(TransportError::Protocol(format!(
                "server accepted `{method}` (HTTP {}) but sent no reply",
                status.as_u16()
            )));
        }

        match ContentType::of(response.headers()) {
            ContentType::EventStream => self.read_stream(response, method, id).await,
            ContentType::Json => read_json(response, method, id).await,
            ContentType::Other(ct) => Err(TransportError::Protocol(format!(
                "server answered `{method}` with content-type {ct:?}, \
                 expected application/json or text/event-stream"
            ))),
        }
    }

    fn capture_session(&self, headers: &HeaderMap) {
        // Overwritten even when absent: a server that stopped issuing
        // ids on re-initialize must not leave the old one being replayed.
        let id = headers.get(SESSION_ID).filter(|v| !v.is_empty()).cloned().map(|mut v| {
            v.set_sensitive(true);
            v
        });
        self.session.lock().unwrap().id = id;
    }

    /// Read the stream until the event that answers `id`.
    async fn read_stream(
        &self,
        response: reqwest::Response,
        method: &str,
        id: &RequestId,
    ) -> Result<Value, TransportError> {
        let mut events = EventReader::new(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|e| error_chain(&e.without_url()))),
            MAX_EVENT,
        );
        loop {
            match events.next().await {
                Ok(Some(Incoming::Response(reply))) => {
                    // A reply with a null id is the server saying it could
                    // not parse our request. The stream was opened by that
                    // request and nothing else is in flight on it, so it
                    // can only be ours.
                    if reply.id.as_ref().is_none_or(|rid| rid == id) {
                        return reply.into_result().map_err(TransportError::Rpc);
                    }
                }
                Ok(Some(Incoming::Request(request))) => self.refuse(request).await,
                // Progress and logging. Dropped, as in stdio.
                Ok(Some(Incoming::Notification(_))) => {}
                Ok(None) => {
                    return Err(TransportError::Disconnected(format!(
                        "the server ended the response stream without replying to `{method}`{}",
                        events.skipped_note()
                    )));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Refuse a server-initiated request, as `stdio::answer_unsupported`
    /// does, and for the same reason: nanopi advertises no client
    /// capabilities, but a server that asks anyway *blocks* until
    /// answered, taking the reply we are waiting for with it. Over HTTP
    /// the answer is its own POST.
    async fn refuse(&self, request: Request) {
        let Request { id, method, .. } = request;
        let reply = Response {
            id: Some(id),
            payload: ResponsePayload::Error(RpcError {
                code: error_code::METHOD_NOT_FOUND,
                message: format!("nanopi does not implement `{method}`"),
                data: None,
            }),
        };
        let outcome = async {
            let body = encode(&reply).map_err(|e| e.to_string())?;
            // Not `INITIALIZE`, so this carries the session headers.
            let (headers, _) = self.headers_for("");
            let response = self.post(body, headers).await.map_err(|e| e.to_string())?;
            match response.status() {
                s if s.is_success() => Ok(()),
                s => Err(format!("HTTP {}", s.as_u16())),
            }
        }
        .await;
        if let Err(e) = outcome {
            // Recorded, not raised: our own request may still be answered,
            // and a server that goes quiet is otherwise unexplainable.
            self.remember(&format!("could not refuse the server's `{method}` request: {e}"));
        }
    }

    /// Turn a non-2xx response into the error that says what it means.
    async fn status_error(
        &self,
        response: reqwest::Response,
        had_session: bool,
        id: Option<&RequestId>,
    ) -> TransportError {
        let status = response.status();
        if status.is_redirection() {
            return TransportError::Http {
                status: status.as_u16(),
                detail: "redirects are not followed".to_string(),
            };
        }
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(|v| sanitize(v, 40));
        // Best effort: a body that cannot be read must not replace the
        // status, which is the part that is always reliable.
        let (body, _) = read_capped(response, ERROR_BODY_READ).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        self.remember(&text);

        match status.as_u16() {
            401 => return TransportError::Unauthorized,
            403 => return TransportError::Forbidden,
            404 if had_session => return TransportError::SessionExpired,
            _ => {}
        }

        let rpc = serde_json::from_str::<Response>(&text).ok();
        // Only a client-class refusal that echoes *our* id is the server
        // declining this call (`-32602` returned as a `400`); that is an
        // `Rpc`, as it would be inside a `200`. `429`/`5xx` are about the
        // server's state, and a null id is about the request's framing.
        if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
            if let (Some(want), Some(Response { id: Some(got), payload: ResponsePayload::Error(e) })) =
                (id, &rpc)
            {
                if got == want {
                    return TransportError::Rpc(e.clone());
                }
            }
        }

        let quoted = match &rpc {
            Some(Response { payload: ResponsePayload::Error(e), .. }) => {
                format!("{} (code {})", sanitize(&e.message, DETAIL_CAP), e.code)
            }
            _ => sanitize(&text, DETAIL_CAP),
        };
        let mut detail = status.canonical_reason().unwrap_or("error").to_string();
        if !quoted.is_empty() {
            detail = format!("{detail}: {quoted}");
        }
        if let Some(after) = retry_after {
            detail = format!("{detail} (retry after {after})");
        }
        TransportError::Http { status: status.as_u16(), detail }
    }

    fn remember(&self, text: &str) {
        *self.last_error_body.lock().unwrap() = sanitize(text, DIAGNOSTIC_CAP);
    }
}

/// Header values and the session id are secrets, and a derived `Debug`
/// would print them (`HeaderValue` only hides itself if marked, and the
/// URL's path and query can carry a token regardless).
impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("has_session", &self.session.lock().unwrap().id.is_some())
            .field("closed", &self.close_reason())
            .finish()
    }
}

// ------------------------------------------------------------- helpers

fn encode<T: Serialize>(message: &T) -> Result<Bytes, TransportError> {
    Ok(Bytes::from(serde_json::to_vec(message)?))
}

/// The configured headers, then the fixed ones on top: a user header
/// must not be able to replace `Content-Type` and make the server read
/// our JSON as something else.
fn base_headers(
    server: &str,
    configured: &std::collections::BTreeMap<String, String>,
) -> Result<HeaderMap, TransportError> {
    let mut map = HeaderMap::new();
    for (name, value) in configured {
        // The value is never put in the message, only the name: an
        // invalid one is most likely a pasted secret with a stray newline.
        let n = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            TransportError::Unsupported(format!(
                "server \"{server}\": {name:?} is not a valid header name"
            ))
        })?;
        let mut v = HeaderValue::from_str(value).map_err(|_| {
            TransportError::Unsupported(format!(
                "server \"{server}\": the value of header {name:?} is not valid"
            ))
        })?;
        v.set_sensitive(true);
        map.insert(n, v);
    }
    map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    map.insert(ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));
    Ok(map)
}

/// Whether `url` points at this machine.
fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback(),
        // `::ffff:127.0.0.1` reaches the same socket as `127.0.0.1`.
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

/// `a: b: c` for an error and its sources. reqwest's own message is the
/// useless "error sending request"; the cause (refused, no such host) is
/// two levels down.
fn error_chain(e: &(dyn StdError + 'static)) -> String {
    let mut parts = Vec::new();
    let mut source = e.source();
    while let Some(s) = source {
        parts.push(s.to_string());
        source = s.source();
    }
    if parts.is_empty() {
        e.to_string()
    } else {
        parts.join(": ")
    }
}

/// Server-controlled text made safe to log and to show: control
/// characters (an ANSI escape or a terminal-title sequence in an error
/// page) are dropped, whitespace runs collapse to one space, and the
/// length is capped.
fn sanitize(text: &str, cap: usize) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    if joined.chars().count() <= cap {
        return joined;
    }
    let mut cut: String = joined.chars().take(cap).collect();
    cut.push('…');
    cut
}

enum ContentType {
    EventStream,
    Json,
    Other(String),
}

impl ContentType {
    fn of(headers: &HeaderMap) -> Self {
        let raw = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        let essence = raw.split(';').next().unwrap_or("").trim();
        if essence.eq_ignore_ascii_case("text/event-stream") {
            Self::EventStream
        } else if essence.eq_ignore_ascii_case("application/json") {
            Self::Json
        } else {
            Self::Other(sanitize(raw, 80))
        }
    }
}

/// Read at most `cap` bytes of a body. The flag says whether more was
/// left behind.
async fn read_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<(Vec<u8>, bool), TransportError> {
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            TransportError::Disconnected(format!(
                "reading the response body: {}",
                error_chain(&e.without_url())
            ))
        })?;
        let room = cap - out.len();
        if chunk.len() > room {
            out.extend_from_slice(&chunk[..room]);
            return Ok((out, true));
        }
        out.extend_from_slice(&chunk);
    }
    Ok((out, false))
}

async fn read_json(
    response: reqwest::Response,
    method: &str,
    id: &RequestId,
) -> Result<Value, TransportError> {
    let (body, truncated) = read_capped(response, MAX_EVENT).await?;
    if truncated {
        return Err(TransportError::Protocol(format!(
            "reply to `{method}` is larger than {} MiB",
            MAX_EVENT / (1024 * 1024)
        )));
    }
    let reply: Response = serde_json::from_slice(&body).map_err(|e| {
        TransportError::Protocol(format!("reply to `{method}` is not a JSON-RPC response: {e}"))
    })?;
    // No stream to skip past: a body for someone else's id is not "an
    // unrelated message to ignore", it is the wrong answer.
    if reply.id.as_ref().is_some_and(|rid| rid != id) {
        return Err(TransportError::Protocol(format!(
            "reply to `{method}` answers a different request (id {})",
            reply.id.as_ref().map(ToString::to_string).unwrap_or_default()
        )));
    }
    reply.into_result().map_err(TransportError::Rpc)
}

// ------------------------------------------------------------ SSE reader

/// What the byte stream under an [`EventReader`] has done to the budget.
struct Budget {
    /// Bytes pulled off the wire since the last event was handed out.
    /// An upper bound on what [`SseStream`] is buffering, which is what
    /// the cap protects.
    fed: AtomicUsize,
    tripped: AtomicBool,
}

/// Messages off a `text/event-stream` body, one at a time.
///
/// [`SseStream`] buffers until it has a full line and has no limit of its
/// own, so a server that never sends a newline would grow that buffer
/// until the process dies. The cap is enforced one layer down, on the
/// bytes fed to it: an event that needs more than `cap` bytes before it
/// completes is refused. The counter resets whenever an event comes out,
/// so a stream's total length is unbounded; only the buffer is capped,
/// and the overshoot is at most one network chunk.
struct EventReader {
    events: SseStream<Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>, String>,
    budget: Arc<Budget>,
    /// `data:` payloads that were not JSON-RPC, counted so the "stream
    /// ended" error can say the server was sending *something*.
    skipped: usize,
}

impl EventReader {
    fn new<S>(bytes: S, cap: usize) -> Self
    where
        S: Stream<Item = Result<Bytes, String>> + Send + 'static,
    {
        let budget = Arc::new(Budget { fed: AtomicUsize::new(0), tripped: AtomicBool::new(false) });
        let counted = {
            let budget = Arc::clone(&budget);
            bytes.map(move |chunk| {
                let chunk = chunk?;
                let total = budget.fed.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len();
                if total > cap {
                    budget.tripped.store(true, Ordering::Relaxed);
                    return Err(format!("an event is larger than {} bytes", cap));
                }
                Ok(chunk)
            })
        };
        Self {
            events: SseStream::new(Box::pin(counted)),
            budget,
            skipped: 0,
        }
    }

    /// The next JSON-RPC message, or `None` when the stream ends.
    async fn next(&mut self) -> Result<Option<Incoming>, TransportError> {
        loop {
            match self.events.next().await {
                None => return Ok(None),
                Some(Err(SseError::Stream(msg))) => {
                    return Err(if self.budget.tripped.load(Ordering::Relaxed) {
                        TransportError::Protocol(format!("server sent {msg}; refusing to buffer it"))
                    } else {
                        TransportError::Disconnected(format!("the response stream broke: {msg}"))
                    });
                }
                // Skipped inside `SseStream` today; matched so a change
                // there cannot make this loop spin.
                Some(Err(SseError::Utf8)) => continue,
                Some(Ok(event)) => {
                    self.budget.fed.store(0, Ordering::Relaxed);
                    let data = event.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Incoming>(data) {
                        Ok(message) => return Ok(Some(message)),
                        Err(_) => self.skipped += 1,
                    }
                }
            }
        }
    }

    fn skipped_note(&self) -> String {
        match self.skipped {
            0 => String::new(),
            n => format!(" ({n} event(s) were not JSON-RPC and were ignored)"),
        }
    }
}

#[cfg(test)]
mod tests;

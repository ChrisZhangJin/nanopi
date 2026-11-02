# MCP over Streamable HTTP — implementation plan

Status: **in progress** — H1 (transport seam) and H2 (HTTP core) merged. Task list is in `TODO.md` under "Streamable HTTP".

## Why

stdio covers servers nanopi launches itself. A server that is already running —
a shared browser, a team gateway, anything behind a URL — can only be reached
over HTTP. The concrete motivating case is the `playwright-mcp` container already
running on this machine (`http://127.0.0.1:8931/mcp`), which nanopi cannot use
today: `config.rs` reports any `url` entry as "not supported yet".

Only **Streamable HTTP** (spec 2025-06-18). The older HTTP+SSE transport is
deprecated and is out of scope; see "Not in this plan".

## What a real server does

Probed against the live `playwright-mcp` server (one `initialize`, one
`tools/list`, bad-request cases, then `DELETE` of the session). Everything matched
the spec, but two behaviours shape the design:

| Probe | Observed |
|---|---|
| `POST initialize` | `200`, **`content-type: text/event-stream`**, one `event: message` / `data: {…}` frame, `mcp-session-id` header |
| `POST notifications/initialized` | `202`, no body |
| `POST tools/list` | `200`, SSE again — a single 17 KB `data:` frame |
| `POST` with no session header | `400`, `application/json`, JSON-RPC error with `"id": null` |
| `POST` with an unknown / deleted session | `404` |
| `GET` with the session | `200`, a standing `text/event-stream` (server-initiated messages) |
| `DELETE` session | `200`; the session is then `404` |

1. **A POST response is not necessarily JSON.** This server answers every request
   as a one-event SSE stream. A client that only parses `application/json` would
   fail on the first call. Both content types are mandatory (spec: the client
   MUST support both), and the response must be matched to the request by `id`
   rather than assumed to be the only message.
2. **Errors arrive in two shapes** — an HTTP status with a JSON body (`400`,
   `404`) and a JSON-RPC `error` inside a `200` stream — and both must reach the
   caller as the same typed error.

## Design

### A transport seam, not a trait object

`McpClient` is built directly on `StdioTransport`, but its use of the transport is
narrow: `request_with_timeout`, `notify`, `shutdown`, `close_reason`,
`diagnostics`, `pid`. Introduce `enum Transport { Stdio(StdioTransport),
Http(HttpTransport) }` exposing exactly that surface. An enum rather than
`dyn Trait` because there are two variants, no third is planned, and it keeps
`async` methods free of `async_trait` boxing. `TransportError` becomes shared
(it already has `Rpc`, `Timeout`, `Closed`, `Io`, `Encode`, `Framing`).

This is a pure refactor with **no behaviour change**, done first and on its own,
so every existing stdio test is the proof it changed nothing.

### Config

```json
{ "mcpServers": {
    "playwright": { "type": "http", "url": "http://127.0.0.1:8931/mcp" },
    "gateway":    { "type": "http", "url": "https://mcp.example.com/mcp",
                    "headers": { "Authorization": "Bearer …" } }
} }
```

`ServerConfig` becomes `{ name, spec }` with `ServerSpec::Stdio { command, args,
env }` / `ServerSpec::Http { url, headers }`. A new enum, not extra optional
fields on the existing struct: `command: String` empty-means-HTTP would let an
invalid state compile. The cost is mechanical — 18 struct-literal sites across
`client`, `config`, `ipc`, `manager`, `stdio`, `tool`.

`type` is optional when `url` is present and means `http`. `type: "sse"` and
`"ws"` keep producing a clear "not supported" diagnostic, now naming the
transport rather than a generic "remote".

### The HTTP transport

- **Client:** `net::client_builder()` — the shared builder that carries the DNS
  policy (hickory + `NANOPI_DNS`). Do not build a bare `reqwest::Client`; that
  bypasses the Android/musl resolver fix.
- **Request:** `POST` the JSON-RPC message with `Accept: application/json,
  text/event-stream`. After `initialize`, send `MCP-Protocol-Version` and, if the
  server issued one, `Mcp-Session-Id` on every request.
- **Response:** `application/json` → one object. `text/event-stream` → parse with
  the existing generic `provider::sse::SseStream`, take the frame whose `id`
  matches, ignore unrelated notifications. Other frames on that stream that are
  *requests* from the server get a `-32601` reply, as in stdio, or a server
  awaiting e.g. `sampling/createMessage` hangs.
- **Notifications:** expect `202`; anything else is an error.
- **Timeouts:** our own per-request timeout (as in stdio). **No** reqwest
  whole-request `.timeout()`: it covers the body, and a legitimately long tool
  call streams. A separate short connect timeout so a dead host fails fast.
- **Redirects: disabled.** A `3xx` on a `POST` is an error, not a follow. A
  redirect could re-send credentials somewhere the user never configured.
- **Proxy:** loopback targets bypass any configured proxy. A proxy cannot reach
  the user's `127.0.0.1`, so honouring `http_proxy` there turns a working local
  server into a confusing failure.
- **No `pid`.** `pid()` returns `None`; `manager.pids()` and the `kill_on_drop`
  guard must treat that as normal, not as a bug.

### Session lifecycle

- `404` on a request that carried a session id means the server dropped the
  session. The spec requires a fresh `initialize`. The client does that once and
  retries the failed request. Retrying is safe here specifically because `404`
  means the server did not execute it; it must not be generalised to timeouts or
  `5xx`, where a `tools/call` may have run.
- `DELETE` on shutdown, best effort with a short timeout. There is no process to
  kill, so unlike stdio a SIGKILLed nanopi leaves its session on the server until
  the server expires it. That is a server-side leak, not a local one, and it is
  documented rather than engineered around.
- No standing `GET` stream in the first version. It carries only
  server-initiated messages, which stdio already drops (`list_changed` included);
  same scope, same deferral.

### Credentials

- `headers` values are secrets. They never appear in `/tools`, `/mcp`, notices or
  errors. `endpoint_of` must mask URL userinfo and credential-looking query
  parameters, and show header *names* at most.
- The existing owner rule applies: **a child agent must never learn a server's
  credentials.** Children reach HTTP servers only through the M9 channel, so
  `ipc.rs` already prevents this; add a test that no header value is ever
  serialised toward a child.
- Sending credentials over cleartext `http://` to a **non-loopback** host is
  refused with a diagnostic (loopback `http://` is fine — that is the motivating
  case). Open question below.
- `401`/`403` map to a distinct error: "server requires authentication" /
  "credentials rejected", not a generic transport failure.
- OAuth is out of scope. Static headers only.

## Decisions (owner, resolved)

1. **`${VAR}` expansion: yes, in `headers` and `url` only.** Otherwise a bearer
   token sits in `mcp.json` in plaintext. An unset variable is an error
   diagnostic for that server and the server is skipped — the literal `${VAR}`
   is **never** sent on the wire. `env` (stdio) stays unexpanded, as M4 decided.
2. **Credentials over cleartext `http://` to a non-loopback host: refused** with
   a diagnostic. Loopback `http://` is allowed (the motivating case).
3. **Legacy HTTP+SSE fallback: deferred to the next phase** (task H6 in
   `TODO.md`), not dropped.

## Verification

- Unit/integration tests against a hand-rolled local HTTP server (as
  `tests/print_mode_e2e.rs` does — no new dependency), covering JSON responses,
  SSE responses, a notification `202`, `400`/`404`/`401`, a redirect, a stream
  that closes before the matching id, and a server request on the stream.
- Windows: the transport is plain `reqwest`, so it must compile and run on
  Windows with no `cfg(unix)` — `RUSTFLAGS="-D warnings" cargo check --target
  x86_64-pc-windows-gnu` stays clean. Unlike M9, HTTP MCP is available there.
- **Live:** connect to the real `playwright-mcp` at `127.0.0.1:8931`, list its 23
  tools, call `browser_navigate` and `browser_snapshot`, and confirm the session
  is `DELETE`d afterwards. Real servers behave in ways fakes do not (the SSE-only
  POST response above was not in the spec reading).

## Not in this plan

Legacy HTTP+SSE transport (next phase, H6); the standing `GET` stream and resumability
(`Last-Event-ID`); `notifications/tools/list_changed`; OAuth; WebSocket.

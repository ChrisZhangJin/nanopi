# MCP over Streamable HTTP — implementation plan

Status: **in progress** — H1 (transport seam), H2 (HTTP core), H3 (session lifecycle) and H4 (credentials) done; H5 (wiring, live check) is next. Task list is in `TODO.md` under "Streamable HTTP".

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
                    "headers": { "Authorization": "Bearer ${GATEWAY_TOKEN}" } }
} }
```

A token belongs in the environment, not in `mcp.json`: see "Credentials".

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
  Implemented in `McpClient::request` (it owns the handshake), single-flight:
  concurrent `404`s trigger one `initialize`, the rest wait and retry on the new
  session; a second `404` is returned, never looped on. The tool table is *not*
  rebuilt after a re-initialize, so a restarted server's changed tool list is
  not picked up until the next nanopi session. The stale session id stays in
  place until `initialize` replaces it, so a failed recovery is retried by the
  next call rather than leaving id-less requests the server answers with `400`.
- `DELETE` on shutdown, best effort with a short timeout
  (`http::DELETE_TIMEOUT`, 2 s). Sent after the transport is marked closed and
  in-flight requests are cancelled; any failure (`405` included) is ignored.
  Exits that cannot `await` (SIGTERM, early return) reach it through
  `McpManager::kill_now`, which sends it from its own thread and waits at most
  the same 2 s in total. There is no process to
  kill, so unlike stdio a SIGKILLed nanopi leaves its session on the server until
  the server expires it. That is a server-side leak, not a local one, and it is
  documented rather than engineered around.
- No standing `GET` stream in the first version. It carries only
  server-initiated messages, which stdio already drops (`list_changed` included);
  same scope, same deferral.

### Credentials

Implemented in `src/mcp/credentials.rs`, used by `config`, `http`, `manager`.

**A secret lives in the environment and in the outgoing request, nowhere
else.** `ServerConfig` keeps the *template* (`Bearer ${GATEWAY_TOKEN}`), not
its expansion. Expansion happens in `credentials::resolve_http`, which both
`config::load` (so a bad server is skipped at startup, with a diagnostic, and
the others still load) and `HttpTransport::connect` (the only thing that sends
anything, so it does not trust that a config came from a file) call. What it
returns exists only to build the request's `HeaderMap`, whose values are marked
sensitive.

**`${VAR}` expansion — `headers` values and `url` only.** Not header names, not
`env`, not `command`/`args`.

| Input | Result |
|---|---|
| `${NAME}`, `NAME` = `[A-Za-z_][A-Za-z0-9_]*` | the variable's value |
| `${NAME}`, variable **unset** | **error**: that one server is skipped; the diagnostic names the server, the header (or `url`) and `NAME`, never a value |
| `${NAME}`, variable set but **empty** | allowed; expands to nothing (`Bearer ${EMPTY}` → `Bearer `) |
| `$NAME`, `$`, `$$`, `pa$$word` | not expanded; literal text |
| `$${` | the one escape: a literal `${` (`$${HOME}` → `${HOME}`) |
| `${` with no `}`; `${}`; `${A-B}`; `${A:-d}`; nested `${A${B}}` | **error**, server skipped, template text not quoted |
| a variable whose value contains `${OTHER}` | copied as text; never expanded twice |

Unset is an error and empty is not because an empty value is a statement ("no
token here") while an unset one is nearly always a typo or a forgotten
`export`. `$$${X}` reads left to right (a `$`, then the escape, giving `$${X}`),
so a literal `$` immediately before a real reference cannot be written; that is
accepted. A variable that is not valid Unicode counts as unset. The literal
`${VAR}` is never sent: it is either expanded or the server does not connect.

After expansion the header name must be a valid HTTP name and the value a valid
header value (no control characters — typically a trailing newline from `$(cat
file)`); the diagnostic names the header, never the value.

**Cleartext refusal.** A server whose URL is `http://` to a host that is not
loopback is skipped (at load, and again at connect) if it carries credentials:
*any* configured header, URL userinfo, or a credential-looking query parameter.
The diagnostic names the host (not the URL) and suggests `https://`.

- Loopback is `127.0.0.0/8`, `::1` (and IPv4-mapped `::ffff:127.x`), and the
  exact name `localhost`. Decided on `reqwest::Url`'s parsed host after
  expansion, so `localhost.evil.com`, `127.0.0.1.nip.io`, `[::ffff:10.0.0.1]` and
  `http://127.0.0.1@evil.com/` (host `evil.com`, userinfo `127.0.0.1`) are
  remote. `*.localhost` is **not** loopback here (a resolver that ignores RFC
  6761 would send it to DNS); the proxy bypass in `http.rs` still treats it as
  local, which only affects routing.
- `http://` to a remote host with *no* credentials is allowed; `https://` is
  always allowed; loopback `http://` with headers is allowed.
- "Credential-looking" is `credentials::is_credential_name`: contains `token`,
  `secret`, `password`, `passwd`, `pwd`, `key`, `auth`, `credential`,
  `signature`, `bearer`, `cookie` or `jwt`, or has `sig` as a whole word
  (case-insensitive).

**Masking — one helper.** `credentials::redact_url` and `redact_arg` are the
only masking code; `ServerConfig`'s `Debug`, `manager::endpoint_of` (which feeds
`/mcp`, `/tools` and `ToolSource::Mcp.endpoint`) and the transport's address
in errors use them. Userinfo becomes `***@`; **everything** after `?`/`#`
becomes `?***` — stricter than masking only credential-looking parameters,
because a parameter named `s` or `sid` is as secret as `token`. Header names are
not shown at all. The path is shown as written, so put a secret there as a
`${VAR}`: the template is what is printed. A stdio `KEY=value` argument whose key
looks like a credential has its value masked; a URL argument gets `redact_url`.
A host that comes from `${VAR}` is shown as `${VAR}` in transport errors.

**`401`/`403`** are `TransportError::Unauthorized` and `Forbidden`, neither
carrying a header value or the response body:
`server requires authentication (HTTP 401[, Bearer]): check the `headers` for
server "X"` (with no `headers` configured: add one, such as `Authorization`) and
`credentials were rejected or access is forbidden (HTTP 403): check the
`headers` for server "X"`. A `WWW-Authenticate` contributes only scheme names
(at most three, 20 characters each), never its parameters. Only `404` with a
session re-initializes; `401`/`403` never do (asserted). Whatever the server
says that is kept for `/mcp` (the error-body diagnostics, a quoted detail) has
this server's header values and URL credentials scrubbed out first, because
error pages sometimes quote the credential they just rejected.

**Nothing toward a child.** Children reach HTTP servers only through the M9
channel, which carries tool names, schemas and results, never how a server is
reached. `ipc::tests::no_http_credential_is_ever_sent_toward_a_child` drives a
server holding three distinctive secrets (an expanded `${VAR}`, a literal
header, a URL query value) through the real channel and greps every message,
the child environment and its argv. One thing nanopi does not do: a variable
named in a `${VAR}` stays in the parent's environment, which a child process
inherits like any other.

OAuth is out of scope. Static headers only.

## Decisions (owner, resolved)

1. **`${VAR}` expansion: yes, in `headers` and `url` only.** Otherwise a bearer
   token sits in `mcp.json` in plaintext. An unset variable is an error
   diagnostic for that server and the server is skipped — the literal `${VAR}`
   is **never** sent on the wire. `env` (stdio) stays unexpanded, as M4 decided.
2. **Credentials over cleartext `http://` to a non-loopback host: refused** with
   a diagnostic. Loopback `http://` is allowed (the motivating case). The exact
   rules are in "Credentials".
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

# MCP — task breakdown

Branch: `feat/mcp`. Design and rationale: [`docs/mcp.md`](docs/mcp.md).

Ordered so the tree compiles and the suite stays green at every step, in the
style of `docs/PLAN.md`. Each task ends with **ACCEPTANCE** — what must pass
before moving on.

## Sequencing note (revised)

An earlier version of this note put the parent↔child channel last, on the
reasoning that solo mode needs no IPC. That was the wrong priority: orchestrator
mode is the **default**, so M1–M8 alone leave a default user with MCP servers
that start but whose tools nobody can call. The owner's requirement is that
using MCP must not require turning the orchestrator off.

So **M9 and M10 are the mainline**, designed in
[`docs/mcp-ipc.md`](docs/mcp-ipc.md). M1–M8 are the foundation they build on,
and the manager already connecting in orchestrator mode is exactly what M9 needs
to proxy. M7 (`/mcp`) is secondary.

---

## ✅ M0 — Fix `canonical_name()` before anything registers

`ToolRegistry::canonical_name()` (`src/tool/mod.rs`) lowercases and strips a
`_tool` suffix. A server tool named `search_tool` would register as
`mcp__github__search_tool` and resolve as `mcp__github__search` — a silent
collision or mis-dispatch.

Make the normalisation not apply to names that already carry the `mcp__`
prefix, or drop the suffix-stripping for external tools generally.

**ACCEPTANCE** `canonical_name("mcp__x__search_tool")` round-trips unchanged;
existing built-in canonicalisation tests still pass.

## ✅ M1 — Protocol types and JSON-RPC codec

New `src/mcp/protocol.rs`. Pure serde, no I/O.

- JSON-RPC 2.0 request / response / notification; `id` correlation
- `InitializeResult`, `ServerCapabilities`, `Tool` (`name`, `title`,
  `description`, `inputSchema`), `CallToolResult`
- Content blocks: `text`, `image`, `audio`, `resource_link`, `resource`
- Distinguish a protocol `error` from a result carrying `isError: true`

**ACCEPTANCE** Unit tests deserialise a real `tools/list` and `tools/call`
payload from the spec, including an unknown content-block type (must not fail
the whole response) and a protocol error.

## ✅ M2 — stdio transport

New `src/mcp/stdio.rs`.

- Spawn the server, newline-delimited JSON over stdin/stdout — messages must
  not contain embedded newlines
- Reader task + `HashMap<RequestId, oneshot::Sender>` correlation
- Capture stderr for diagnostics (servers log there legitimately)
- Per-request timeout; process death resolves every pending request as an error
  rather than hanging
- Reuse the child-process hygiene in `src/tool/agent.rs`: `process_group(0)`
  plus `PDEATHSIG`, so a crashed nanopi does not orphan servers

**ACCEPTANCE** Integration test against a fake server (a shell script echoing
canned JSON) covers: normal round-trip, server exits mid-request, server writes
garbage to stdout, request times out. No test leaves a stray process.

## ✅ M3 — Client and lifecycle

New `src/mcp/client.rs`.

- `initialize` → `notifications/initialized` handshake, protocol version
  `2025-06-18`
- `tools/list` with `nextCursor` pagination
- `tools/call`
- Connection state: `Connecting` / `Ready` / `Failed { reason }`

**ACCEPTANCE** Handshake is exercised end to end against the fake server; a
paginated `tools/list` returns every tool; a server that fails `initialize`
ends `Failed` with the reason preserved, and does not abort startup.

## ✅ M4 — Configuration

New `src/mcp/config.rs` + `paths::global_mcp_path()`.

`<nanopi_home>/mcp.json`, **global only** — see `docs/mcp.md` for why a project
file would flip every adopting repo's trust posture. Reuse the
load / diagnostics / never-clobber-a-malformed-file skeleton from
`src/model_catalogue.rs`.

Use the de-facto ecosystem shape so existing configs paste in unchanged:

```json
{ "mcpServers": {
    "github": { "command": "npx", "args": ["-y", "@x/server"], "env": {"TOKEN": "..."} }
} }
```

**ACCEPTANCE** Absent file is a silent no-op (not an error). A malformed file
is left untouched, reports the line, and startup continues. Tests cover an
empty `mcpServers`, an entry missing `command`, and a duplicate name.

## ✅ M5 — Tool adapter

New `src/mcp/tool.rs`: `impl Tool for McpTool`, modelled on
`impl Tool for WasmTool` (`src/wasm/host.rs`).

- Add `ToolSource::Mcp { server, endpoint }`
- Name as `mcp__<server>__<tool>`; non-`[A-Za-z0-9_-]` → `_`
- Result mapping: `text` → `content`; `image` → `images`;
  `resource_link`/`resource` → text summary; `structuredContent` → `metadata`;
  `isError` → `is_error`
- A transport or protocol failure returns an in-band `is_error` result — never
  aborts the turn, matching `src/tool/agent.rs`'s soft-error convention

**ACCEPTANCE** Unit tests for each content-block mapping, including a mixed
`content[]` array and an empty one. A dead server surfaces as a failed tool
result, not a panic.

## ✅ M6 — Startup wiring (solo mode)

Connect in `src/mode/tui.rs` and `src/mode/print.rs` beside the existing
`model_catalogue::init`, register via `ToolRegistry::register_external` so
collisions and the `--tools` allowlist are already handled.

Orchestrator mode is untouched: `ToolRegistry::orchestrator()` stays the same
closed four-tool list.

**ACCEPTANCE** With a configured server and `orchestrator = false`, `/tools`
lists the MCP tools under their server; with orchestrator on (the default), the
registry is still exactly the four dispatch tools — asserted by the existing
closed-list test. A server that is down delays startup by no more than its
timeout.

## M7 — `/mcp` command, read-only

Add `SlashCmd::Mcp`. Lists each server: status, tool count, and the failure
reason when `Failed`. Mirrors `/tools` and `/skills` formatting.

**ACCEPTANCE** Renders connected, failed and not-configured states. Credentials
from `env` never appear in the output.

## ✅ M8 — Shutdown hygiene

Terminate every server on TUI exit and on print-mode completion: close stdin,
then SIGTERM, then SIGKILL.

**ACCEPTANCE** A test asserts no server process survives the parent, including
on the Ctrl-C path. This is the same failure mode
`print_mode_ctrl_c_stops_all_background_agents` exists to catch for agents.

> **Milestone: MCP works in solo mode.** Worth shipping and using before
> starting M9.

## ✅ M9 — Parent↔child channel (design: docs/mcp-ipc.md)

The prerequisite for option C, and the largest single piece. Today parent and
child communicate only through `brief.md` / `report.md` at turn boundaries —
there is no RPC.

Scope to decide at the time: unix socket vs an extra pipe pair on the existing
spawn; framing; what happens when the parent dies mid-call; whether it is
MCP-specific or a general child→parent capability channel (it probably should
be general, but MCP should be its only caller at first).

**ACCEPTANCE** A child can invoke a parent-held capability and receive the
result; parent death fails the call cleanly rather than hanging the child;
no new orphan processes. Design note: `docs/mcp-ipc.md` (unix socket under `$TMPDIR`, mode 0700; reuses
`mcp::protocol`; per-agent allowlist enforced by the parent; unix-only first).

## ✅ M10 — Complete option C (design: docs/mcp-ipc.md)

- Orchestrator gets the MCP tool **list** (names + descriptions) so it can name
  them in a brief, without being able to call them
- Child agents call MCP through the parent over M9's channel
- Decide how a child declares which MCP tools it needs (likely an extension of
  the `agent` tool's existing `tools` field)

**ACCEPTANCE** An orchestrator dispatches an agent that successfully calls an
MCP tool; process count is one parent plus one per server regardless of
`agent.max_concurrency`.

As built (`tests/mcp_agent_e2e.rs` drives it through a print-mode parent,
since orchestrator mode is TUI-only): the `agent` tool's `tools` field takes
`mcp__…` names (`validate_tools`); `tool::agent::run_body` grants before the
spawn, binds the pid right after and revokes on exit or stop — both dispatch
paths and `send_message` continuation go through it; the child builds proxy
tools from `--tools` (`mcp::proxy`); the orchestrator's prompt carries a
capped catalogue (`mcp::catalogue`, applied by the TUI's one recompose
helper). Known gap: a `send_message` continuation of an agent adopted from an
*earlier process* continues without its MCP tools (the grants lived in that
process's channel) and says so in its result.

## M11 — `/mcp` interactive

Reconnect, enable/disable a server for this session.

---

## Streamable HTTP (design: [`docs/mcp-http.md`](docs/mcp-http.md))

Motivating case: the `playwright-mcp` container already running at
`http://127.0.0.1:8931/mcp`, which nanopi cannot use today. Order matters: H1 is
a behaviour-preserving refactor and must land alone.

### H1 — Transport seam (pure refactor)

`enum Transport { Stdio, Http }` with the surface `McpClient` already uses
(`request_with_timeout`, `notify`, `shutdown`, `close_reason`, `diagnostics`,
`pid`); `ServerConfig` becomes `{ name, spec: ServerSpec }`
(`Stdio { command, args, env }` / `Http { url, headers }`); `config.rs` parses
`url` / `type` / `headers` and reports `sse`/`ws` as unsupported by name. Declare
`src/mcp/transport.rs` and `src/mcp/http.rs` (placeholder) up front.

**ACCEPTANCE** Every existing test passes with no assertion changed. stdio
behaviour is byte-identical. An `http` entry now parses into `ServerSpec::Http`
(and is skipped with a "not implemented" diagnostic until H2).

### H2 — HTTP transport core

`src/mcp/http.rs`: POST over `net::client_builder()`; parse **both**
`application/json` and `text/event-stream` responses (the real server answers
every POST as SSE) via `provider::sse::SseStream`, matching on `id`; `Mcp-Session-Id`
and `MCP-Protocol-Version` replay; `202` for notifications; `-32601` reply to
server-initiated requests; own per-request timeout and **no** whole-request
reqwest timeout; redirects disabled; loopback bypasses any proxy.

**ACCEPTANCE** Tests against a hand-rolled local HTTP server: JSON response, SSE
response, `202`, `400`/`404`/`401`/`5xx` mapped to typed errors, a redirect
refused, a stream closing before the matching id, a server request on the stream.
No new dependency. Windows check clean.

### H3 — Session lifecycle

`404` with a session id → re-`initialize` once and retry (only `404`; never
timeouts or `5xx`, where a `tools/call` may have run); `DELETE` on shutdown with a
short timeout; `pid()` is `None` and `manager.pids()` / `kill_on_drop` treat that
as normal.

**ACCEPTANCE** A server that drops its session mid-run is recovered from
transparently; shutdown sends `DELETE` (asserted on the fake server); a dead host
fails fast rather than waiting out the call timeout.

### H4 — Credentials

`headers` support; `endpoint_of` masks URL userinfo and credential-looking query
parameters and shows header names at most; `401`/`403` get distinct errors;
cleartext `http://` with credentials to a non-loopback host is refused with a
diagnostic. `${VAR}` expansion in `headers` and `url` only; an unset variable
skips that server with a diagnostic and is never sent literally (decided).

**ACCEPTANCE** No header value appears in `/tools`, notices, errors or anything
sent toward a child (test it); the refusal and the masking each have a test.

### H5 — Wiring and live check

Manager/status/`/tools` show the masked URL; the IPC path is unchanged because
children never see how a server is reached. Docs (`README`, `config.toml.example`).

**ACCEPTANCE** Against the real `playwright-mcp` at `127.0.0.1:8931`: connect,
list the 23 tools, call `browser_navigate` and `browser_snapshot`, and confirm
the session is `DELETE`d on exit. Full suite, Windows and wasm checks clean.

### H6 — Legacy HTTP+SSE fallback (next phase)

Decided to defer, not drop. Per the spec's backwards-compatibility note: POST an
`initialize` to the configured URL; if it fails with a `4xx` (`405`/`404`), `GET`
the URL expecting an SSE stream whose first event is `endpoint`, then use the old
transport (`GET` stream for responses, `POST` to the announced endpoint). Pick it
up when a server the owner actually uses needs it. Reuses H2's SSE parsing and
H4's credential handling.

**ACCEPTANCE** A fake legacy-only server is reachable with `type` omitted; a
Streamable HTTP server is never routed through the legacy path.

## Later, explicitly out of scope for now

- **Standing `GET` stream** and resumability for Streamable HTTP.
- **`prompts/*`** — maps naturally onto skills / slash commands
  (`/mcp:<server>:<prompt>`).
- **`resources/*`**.
- **OAuth.** PKCE, token storage and a local callback listener are a
  significant size cost on a 1.8 MB binary. Start with a static `Authorization`
  header only.
- **`notifications/tools/list_changed`** — re-list on the next turn boundary.

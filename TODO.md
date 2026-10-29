# MCP — task breakdown

Branch: `feat/mcp`. Design and rationale: [`docs/mcp.md`](docs/mcp.md).

Ordered so the tree compiles and the suite stays green at every step, in the
style of `docs/PLAN.md`. Each task ends with **ACCEPTANCE** — what must pass
before moving on.

## Sequencing note (revises the "IPC first" framing in docs/mcp.md)

`docs/mcp.md` says the parent↔child IPC channel is a prerequisite. That is true
**for orchestrator mode**, and it made the whole feature look like it had to
start with a large, risky, user-invisible project.

It doesn't. In **solo mode** (`[experimental] orchestrator = false`) the main
agent legitimately does the work itself and there are no children to proxy for,
so MCP tools belong in its registry — that is not "option A", it is just the
case where the ownership question does not arise.

So M1–M8 deliver working MCP for solo users with no IPC at all. Orchestrator
mode is unchanged throughout (it keeps its four dispatch tools and simply has
no MCP yet — no regression, just not finished). M9–M10 then build the IPC
channel and complete option C, with a working, tested MCP client to integrate
against rather than a speculative one.

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

## M3 — Client and lifecycle

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

## 🔶 M5 — Tool adapter (mapping done, `impl Tool` waits on M3)

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

## M6 — Startup wiring (solo mode)

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

## M8 — Shutdown hygiene

Terminate every server on TUI exit and on print-mode completion: close stdin,
then SIGTERM, then SIGKILL.

**ACCEPTANCE** A test asserts no server process survives the parent, including
on the Ctrl-C path. This is the same failure mode
`print_mode_ctrl_c_stops_all_background_agents` exists to catch for agents.

> **Milestone: MCP works in solo mode.** Worth shipping and using before
> starting M9.

## M9 — Parent↔child IPC channel

The prerequisite for option C, and the largest single piece. Today parent and
child communicate only through `brief.md` / `report.md` at turn boundaries —
there is no RPC.

Scope to decide at the time: unix socket vs an extra pipe pair on the existing
spawn; framing; what happens when the parent dies mid-call; whether it is
MCP-specific or a general child→parent capability channel (it probably should
be general, but MCP should be its only caller at first).

**ACCEPTANCE** A child can invoke a parent-held capability and receive the
result; parent death fails the call cleanly rather than hanging the child;
no new orphan processes. Needs its own design note before coding.

## M10 — Complete option C

- Orchestrator gets the MCP tool **list** (names + descriptions) so it can name
  them in a brief, without being able to call them
- Child agents call MCP through the parent over M9's channel
- Decide how a child declares which MCP tools it needs (likely an extension of
  the `agent` tool's existing `tools` field)

**ACCEPTANCE** An orchestrator dispatches an agent that successfully calls an
MCP tool; process count is one parent plus one per server regardless of
`agent.max_concurrency`.

## M11 — `/mcp` interactive

Reconnect, enable/disable a server for this session.

---

## Later, explicitly out of scope for now

- **Streamable HTTP transport.** Reuse `SseStream` (`src/provider/sse.rs`),
  `MCP-Protocol-Version: 2025-06-18` header, `Mcp-Session-Id` session. The
  older HTTP+SSE transport is deprecated — do not implement it.
- **`prompts/*`** — maps naturally onto skills / slash commands
  (`/mcp:<server>:<prompt>`).
- **`resources/*`**.
- **OAuth.** PKCE, token storage and a local callback listener are a
  significant size cost on a 1.8 MB binary. Start with a static `Authorization`
  header only.
- **`notifications/tools/list_changed`** — re-list on the next turn boundary.

# MCP (Model Context Protocol)

Status: **researched, architecture decided, not implemented.** This file
exists so the work can be picked up without re-deriving the research.

Target: let nanopi act as an MCP *client*, so the model can call tools served
by external MCP servers, with a `/mcp` command to inspect and manage them.

## Decision: the main process owns every connection and proxies for children

MCP connections are stateful and long-lived — stdio is a resident subprocess,
Streamable HTTP is a session keyed by `Mcp-Session-Id`. So the first question
is not "how do we speak JSON-RPC" but **who holds the connection**, because
that decides which agent can call MCP tools.

nanopi's process shape makes this sharp:

```
main agent (TUI process)   orchestrator mode: 4 dispatch tools, no file/shell tools
  └─ child agent           separate `nanopi -p` process, full standard() registry
  └─ child agent           isolated; communicates only via brief.md / report.md
  └─ ...                   there is no realtime parent↔child channel
```

That last line is the constraint. `send_message` appends text to `brief.md` and
the child picks it up at its next turn boundary (`src/agent/brief.rs`) — it is
not RPC.

Three options were considered:

| | Who connects | Why rejected / chosen |
|---|---|---|
| **A** | Main agent calls MCP directly | Simplest, constant process count. Conflicts with the orchestrator rule that the main agent does no concrete work itself. |
| **B** | Each child connects independently | Matches the orchestrator philosophy and is trivial to build — but the main agent then **cannot see the MCP tool list**, so it cannot name those tools when writing a brief. An orchestrator that cannot see the tools cannot dispatch against them. Also explodes process count: `agent.max_concurrency` × servers, each paying an `npx` cold start. |
| **C** | Main process connects; children call through it | **Chosen.** Main agent sees the tool list for dispatch without calling tools itself; process count stays constant. |

### Consequence: the IPC channel comes first

Option C needs a realtime parent↔child channel, which does not exist today.
That is its own piece of work and must land before any MCP code. MCP cannot be
a "phase 1".

### Rejected variant

MCP's `annotations.readOnlyHint` looks like it could split read-only tools
(safe for the orchestrator) from writing tools (delegate). It cannot be used as
a security boundary — the spec says clients *MUST* treat annotations as
untrusted unless the server is trusted.

## What the research established

### No new dependencies

Everything MCP needs is already in the tree, so this does **not** need a
wasm-style feature gate:

| MCP needs | Already present |
|---|---|
| spawn subprocess, pipe I/O | `tokio` with `process` + `io-util` (`Cargo.toml`) — the agent children already use it |
| HTTP + SSE streaming | `reqwest` with `stream`; and `SseStream<S, E>` in `src/provider/sse.rs` is generic over the inner stream, so it is reusable for Streamable HTTP |
| JSON-RPC codec | `serde_json`, a base dependency |

Estimated cost is code size only — roughly +30–60 KB at `opt-level = "z"` + LTO.
OAuth (PKCE, token storage, a local callback listener) would change that
materially; it should stay out of the first implementation.

### The registration seam already exists

External tools are an established concept — WASM plugins use it. An MCP tool is
the second implementor:

```rust
// src/tool/mod.rs
pub enum ToolSource {
    Builtin,
    Plugin { name: String, path: String },
    Mcp { server: String, endpoint: String },   // to add
}
```

`Tool` has two required methods (`spec()` / `execute()`),
`ToolRegistry::register_external` already rejects name collisions, and `/tools`
groups by source automatically. `impl Tool for WasmTool` (`src/wasm/host.rs`)
is the template.

### Result shape must be adapted

MCP returns a multimodal array; `ToolOutput` is flat:

| MCP `tools/call` result | → `ToolOutput` |
|---|---|
| `content[].type = "text"` | concatenate into `content: String` |
| `content[].type = "image"` | `images: Vec<ImageAttachment>` (already supported) |
| `content[].type = "resource_link"` / `"resource"` | degrade to a text summary |
| `structuredContent` | `metadata: Option<Value>` |
| `isError: true` | `is_error` |

JSON-RPC protocol errors (`-32602` and friends) should also map to an in-band
`is_error` result rather than aborting the turn — consistent with how
`src/tool/agent.rs` handles soft errors.

### Two gotchas

**Tool names get normalised.** `ToolRegistry::canonical_name()`
(`src/tool/mod.rs`) lowercases and strips a `_tool` suffix. With Claude Code's
`mcp__<server>__<tool>` convention, a server tool named `search_tool` becomes
`mcp__github__search` — a possible collision or mis-resolution. **Fix this
function before wiring MCP tools in.**

**A project-local config file changes the repo's trust posture.**
`trust::check_trust_status()` returns `NoProjectResources` only while
`<cwd>/.nanopi` does not exist — it is the directory's existence, not any
particular file, that triggers the trust question. So a project-scoped
`.nanopi/mcp.json` would silently flip every repo that adopts it to
`NeedsPrompt`, which resolves as *untrusted* and disables project skills,
`SYSTEM.md` overrides and project agents. Prefer a global
`<nanopi_home>/mcp.json`; if project scope is genuinely needed, gate it on
trust the way project skills and agents are.

Note the security asymmetry too: an MCP **stdio server is a bare subprocess
with the user's full privileges**, unlike a WASM plugin, which is sandboxed and
has capabilities granted one at a time (`allow_network`, `allow_fs`, …).

## Suggested phasing

0. **Parent↔child IPC channel** — prerequisite for option C.
1. **stdio + tools only.** Config in `<nanopi_home>/mcp.json`, reusing the
   load/merge/diagnostics skeleton from `src/model_catalogue.rs`. Connect →
   `initialize` → `tools/list` → register. `/mcp` lists status and tool counts.
2. **Streamable HTTP.** Reuse `SseStream`. Lazy connect, timeouts, reconnect.
   `MCP-Protocol-Version: 2025-06-18` header, `Mcp-Session-Id` session. The
   older HTTP+SSE transport is deprecated — do not implement it.
3. **Optional.** `prompts/*` maps naturally onto nanopi's skills / slash
   commands (e.g. `/mcp:<server>:<prompt>`). OAuth last, or start with a static
   `Authorization` header only.

## References

- Transports — https://modelcontextprotocol.io/specification/2025-06-18/basic/transports
- Tools — https://modelcontextprotocol.io/specification/2025-06-18/server/tools
- Claude Code's `/mcp` for prior art — https://code.claude.com/docs/en/mcp

# Parent↔child channel (M9) and MCP for agents (M10)

Why this exists: in orchestrator mode — the default — the main agent has four
dispatch tools and no way to do work, and a child agent is a separate
`nanopi -p` process that must **not** connect to MCP servers itself (that is
option B in `docs/mcp.md`, rejected: `max_concurrency × servers` processes, each
paying an `npx` cold start). So the only party that can call an MCP tool is the
main process, and the only party that wants to is a child. A child therefore
needs a way to ask the parent to make the call.

Hard requirement from the owner: **using MCP must not require turning the
orchestrator off.** M1–M8 delivered MCP for solo mode only; M9/M10 are what make
it work in the default mode.

## What exists today

Only two parent↔child paths, both files, neither realtime:

- `brief.md` amendment section (`<!-- nanopi:amendments -->`): parent → child,
  read at a turn boundary.
- `report.md`: child → parent, read after the child exits.

A child's stdin is `Stdio::null()` and its stdout carries the final JSON
envelope, so neither pipe can be reused. No socket code exists anywhere in
`src/`.

## Decisions

### Transport: Unix domain socket, unix-only in the first version

One listener per parent process, one connection per child. Newline-delimited
JSON-RPC 2.0, **reusing `mcp::protocol`** (`Request`, `Response`, `Incoming`,
`RequestId`). The parent is effectively a small MCP proxy server, so there is no
new codec.

Windows: `tokio` has no unix sockets there. First version degrades — a child
asking for an MCP tool gets a clear in-band error ("MCP is not available to
agents on this platform yet"), never a silent failure. Windows already runs
without `PDEATHSIG`, so the child path was degraded there anyway. `cargo check
--target x86_64-pc-windows-gnu` must stay clean; the whole module is
`cfg(unix)`.

### Socket location: a short path under `$TMPDIR`, mode 0700

**Not** under the project's `.nanopi/agents/<run>/<agent>/`. `sun_path` is 108
bytes on Linux and 104 on macOS, and a realistic project path already costs 80
before the filename; a slightly deeper checkout overflows and `bind` fails.
Use a per-process directory such as `$TMPDIR/nanopi-<pid>/` created `0700`,
socket inside, removed on shutdown. The 0700 directory is the access control
for connecting; it is not the authorisation (below).

The child learns the path through an environment variable set in
`build_child_env` (`src/tool/agent.rs`), next to `NANOPI_AGENT_ID`.

### Authorisation is the parent's job, per agent id

A child is model-driven and not trusted. The parent records, at dispatch time,
which MCP tools that agent id was allowed, and rejects anything else — a child's
own claim about what it may call carries no weight. The agent id is taken from
the connection handshake and checked against the registry; it is not a secret,
so the socket directory permission and the dispatch-time allowlist together are
what matter.

### Declaring tools: reuse the `agent` tool's `tools` field

`validate_tools` currently rejects anything not in `ToolRegistry::standard()`.
It must accept `mcp__…` names that the manager actually has, and still apply the
deny-list. An unknown MCP name is an error naming the available ones.

### The orchestrator sees the catalogue but cannot call

Inject the MCP tool list (name + description) into the orchestrator's system
prompt so it can name tools in a brief. `compose_system_prompt_mode` already
lists skills for an agent that has `agent` but not `read`, with delegating
wording; do the same for MCP, and cap each description's length (server-supplied
text reaching the model is a prompt-injection and context-size surface).

### Parent death does not hang a child — on Linux

Children already carry `PDEATHSIG`, so a dead parent kills the child. macOS
relies on the `getppid` poll in `main.rs`. State this in the code; do not claim
a guarantee that is only Linux-true.

## Known hazards to design for

1. **`stop_agent` leaves the server working.** Killing the child does not cancel
   an in-flight MCP call; `StdioTransport` does not expose the request id, so no
   `notifications/cancelled` can be sent (flagged during M3). The parent should
   cancel proxied calls when the child's connection closes. That needs the
   transport to expose the id of an in-flight request.
2. **One slow call must not block other children.** The listener serves each
   connection on its own task; the proxy must not serialise on a lock held
   across an `await`.
3. **A child that never connects** (older binary, platform without support) must
   get a fast, clear failure, not a hang until the call timeout.
4. **Do not widen the child's registry beyond what was allowed.** The child's
   tools are built from the dispatch-time allowlist, never from "everything the
   parent offers".
5. **Credentials.** A child must never receive a server's `env`. It talks to the
   parent; it does not learn how the server was launched.

## Split

**M9 — the channel.** Listener in the parent, client in the child, a small
capability dispatch (`tools/list`, `tools/call` for now), handshake carrying the
agent id, per-agent allowlist enforcement, clean shutdown, socket cleanup. Make
it general enough that a second capability could be added later, but MCP is the
only caller.

**M10 — wire it up.** `validate_tools` accepts MCP names; the child registers
proxy tools from its allowlist; orchestrator prompt carries the catalogue;
default-mode end-to-end test.

## Acceptance (end to end)

With the default orchestrator on and a configured fake server, an orchestrator
dispatches an agent with `tools: ["mcp__demo__echo"]`, that agent calls the tool
and the result reaches its report; the orchestrator's own registry is still
exactly the four dispatch tools; process count is one parent plus one per server
regardless of `agent.max_concurrency`; a child asking for an MCP tool it was not
granted is refused by the parent; no socket file or server process survives exit.

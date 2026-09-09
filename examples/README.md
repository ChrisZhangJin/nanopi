# Examples

Two kinds of extension live here: **WASM plugins** (sandboxed
components, `--features wasm`) and **shell hooks** (ordinary scripts,
every build).

## WASM plugins

Five, in the order you would read them. The first three demonstrate the
mechanism; the last two are meant to be installed and used.

| Plugin | What it is for | Tools | Commands | Events |
|---|---|---|---|---|
| [`wasm-plugin-minimal/`](wasm-plugin-minimal/) | **The skeleton to copy.** Smallest plugin that still exercises a capability gate — one pure tool and one gated one, split into "boilerplate" and "your part". | `greet`, `fetch_head` | — | — |
| [`wasm-plugin/`](wasm-plugin/) | **The reference.** The fuller version: four tools, slash commands, and all three command actions (`print` / `send_user_message` / `error`). Longest README of the set. | `wordcount`, `readfile`, `fetch`, `rot13` | `/todo`, `/explain` | — |
| [`wasm-plugin-events/`](wasm-plugin-events/) | **Lifecycle event subscription**, and the two-list grant: a plugin's own `list-events` *and* the config's `events` must agree. Also holds the trap/spin fixtures the host's hang-breaker tests use. | `events_seen`, `busy`, `greet` | *(none — `[]` stub, the ladder's cost)* | `tool_execution_start`, `turn_start`, `input` |
| [`wasm-plugin-memory/`](wasm-plugin-memory/) | **Durable project memory.** Facts become markdown files in `.nanopi/memory/` that you can diff, edit and commit. Only the compact index enters the model's context; it calls `recall` for the full text of whatever looks relevant. | `remember`, `recall`, `forget` | `/memory` | — |
| [`wasm-plugin-report/`](wasm-plugin-report/) | **Where the time went.** Per-tool call counts, duration and failures, kept across sessions. Exists because `tool_execution_end` carries a `duration_ms` that nothing else persists — so "why was that slow" and "what keeps failing" have no other answer. | `tool_stats` | `/tools-report` | `tool_execution_end`, `session_start` |

### Grants each one needs

A plugin reaches nothing it was not granted. These are the
`[[extensions]]` keys each example wants; every README spells out the
exact block.

| Plugin | Grants | Host imports used |
|---|---|---|
| `wasm-plugin-minimal` | `allow_network` + `url_allowlist` (only `fetch_head` needs anything; `greet` needs nothing) | `host-log`, `host-http-get` — it *declares* `host-fs-read` but never calls it, and LTO drops unused imports, so it is absent from the built world |
| `wasm-plugin` | `allow_fs`, `allow_network` + `url_allowlist` | `host-log`, `host-fs-read`, `host-http-get` |
| `wasm-plugin-events` | `events = [...]` | `host-log` |
| `wasm-plugin-memory` | `allow_fs`, `allow_tools = ["write"]`, `allow_context` | `host-log`, `host-fs-read`, `host-notify`, `host-set-context`, `host-call-tool` |
| `wasm-plugin-report` | `allow_store`, `events = [...]` | `host-log`, `host-notify`, `host-store-get`, `host-store-set` |

The last two are deliberately complementary — between them they cover
the whole outbound surface (`docs/plugin-capabilities.md`), so if you
want to see a capability actually used rather than demonstrated, one of
those two uses it.

### Worlds and builds

WIT worlds are a linear ladder, `extension ⊂ extension-commands ⊂
extension-events`, so a plugin targets the lowest one that covers what
it exports. See the header comment in `wit/nanopi-extension.wit` for why
it is a ladder and not a lattice.

| Plugin | World | Build |
|---|---|---|
| `wasm-plugin-minimal` | `extension` | *no make target* — by hand, see its README |
| `wasm-plugin` | `extension-commands` | `make plugin` |
| `wasm-plugin-events` | `extension-events` | `make plugin-events` |
| `wasm-plugin-memory` | `extension-commands` | `make plugin-memory` |
| `wasm-plugin-report` | `extension-events` | `make plugin-report` |

Always build **from the repo root** — `wasm-tools component embed wit/`
resolves `wit/` relative to the working directory, and building from
inside an example silently embeds a stale module. Output lands in
`dist/`. The host must be built with `--features wasm`; the stock
release binary has no WASM runtime.

## Shell hooks

[`hooks/`](hooks/) — scripts for `[[hooks.*]]`, mirroring PI's safety
catalog: `check-rm-rf.sh`, `protected-paths.sh`, `redact-secrets.sh`,
`dirty-repo-guard.sh`, `audit-log.sh`, `session-log.sh`.

**Hooks are not a worse plugin — they are the other half.** Two things
only a hook can do:

- **Veto.** A hook can block or rewrite a tool call. Plugins observe
  events and cannot refuse them; that is a deliberate design decision,
  not a gap.
- **Never miss an event.** Hooks are synchronous. Plugin event delivery
  is drop-on-busy and does not queue, so anything that has to be
  *complete* — a real audit log — belongs in a hook. `wasm-plugin-report`
  says so in its own output for exactly this reason.

## Not here on purpose

`tests/fixtures/runaway-plugin/` is a plugin whose `execute-tool` never
returns, used to prove the epoch hang-breaker fires. Its own header
says it plainly: *"Deliberately NOT in `examples/` — nothing here is
worth copying."* A directory whose other READMEs say "copy this as a
starting point" is the wrong home for a component that hangs.

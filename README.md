<div align="center">

# nanopi

**No Node. No Python. No `node_modules`.**

A coding-agent CLI you can `scp` onto a box that has no runtime —
a single ~4 MB static Rust binary, ported from [Pi](https://github.com/earendil-works/pi).
Runs on Alpine, on CentOS 6, and anywhere `npm install` isn't an option.

[![Release](https://img.shields.io/github/v/release/ChrisZhangJin/nanopi?style=flat-square&color=blue)](https://github.com/ChrisZhangJin/nanopi/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)
![Binary](https://img.shields.io/badge/binary-~4%20MB-brightgreen?style=flat-square)
![Static musl](https://img.shields.io/badge/static-musl-informational?style=flat-square)
![Rust](https://img.shields.io/badge/rust-stable-orange?style=flat-square&logo=rust&logoColor=white)
[![CI](https://img.shields.io/github/actions/workflow/status/ChrisZhangJin/nanopi/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/ChrisZhangJin/nanopi/actions/workflows/ci.yml)

**English** · [简体中文](README_zh.md)

<br>

<img src="https://raw.githubusercontent.com/ChrisZhangJin/nanopi/main/img/tui.png" alt="nanopi TUI screenshot" width="760">

<p><em>TUI on Linux (macOS / Linux terminal)</em></p>

<img src="https://raw.githubusercontent.com/ChrisZhangJin/nanopi/main/img/tui_win.png" alt="nanopi TUI screenshot (Windows)" width="760">

<p><em>TUI on Windows — captured from <code>nanopi.exe</code> on Windows 10/11</em></p>

</div>

---

## Why nanopi?

- 🚫 **Zero runtime dependencies** — no Node, no Python, no package manager.
  Download one file, `chmod +x`, run.
- 🖥 **Runs on ancient boxes** — glibc 2.12+ (CentOS 6), or the fully static
  musl build on Alpine and anything else
- 🪶 **~4 MB static binary** — musl + LTO + strip (the download is 1.6 MB,
  UPX-packed)
- 🧬 **PI-parity** — mirrors [Pi](https://github.com/earendil-works/pi)'s surface: JSONL sessions, hooks, skills, `-p`, `/fork`, `/resume`
- 🔌 **Multi-provider** — any OpenAI-compatible endpoint (DeepSeek, ollama, vLLM, …) plus native Anthropic; `provider` / `api_kind` in `config.toml` pick the vendor and wire protocol explicitly when the base_url sniff isn't enough. A **leading** `<think>…</think>` block in the reply text (R1-lineage models — R1, its distills, QwQ, GLM, etc. — served through any OpenAI-compatible endpoint) is split out and rendered as thinking; a `<think>` appearing after other text is left literal. `inline_think_tags = true | false` overrides the default (on).
- 🛠 **Streaming tool calls** — `read` / `write` / `edit` / `bash`, rendered live in a ratatui TUI
- 🪝 **Claude Code-protocol hooks** — JSON-on-stdin, exit-2-to-block shell hooks, using PI's event names (`tool_execution_start` / `tool_execution_end` / `input` / …)
- 🧠 **Agent Skills** — [spec-compliant](https://agentskills.io/specification) `SKILL.md` discovery + `/skill:name` expansion

## Background — why nanopi exists

Pi is a great coding agent, but its upstream chose not to support
certain environments that real users need:

| Upstream issue | User request | Upstream status |
|---|---|---|
| [pi#8591](https://github.com/earendil-works/pi/issues/8591) | musl-linked builds for Alpine | not planned |
| [pi#6546](https://github.com/earendil-works/pi/issues/6546) | Avoid glibc version mismatch on older Linux | not planned |
| [pi#6075](https://github.com/earendil-works/pi/issues/6075) | Startup time is too slow | not planned |

Three separate people asked for musl builds, old-glibc compatibility and
a lighter startup; upstream closed all three as *not planned*. That is a
reasonable call for them — Pi targets modern machines — but it leaves the
old-hardware case unserved. **nanopi is a Rust rewrite for exactly that
case:**

- **Static musl build** — zero runtime deps, runs in Alpine containers
  (see [`release.yml`](https://github.com/ChrisZhangJin/nanopi/blob/main/.github/workflows/release.yml) for the CI matrix)
- **glibc 2.12+ (CentOS 6)** — the dynamic build covers old servers;
  the musl build covers everything else
- **~4 MB** — Rust + LTO + `opt-level = "z"` + `panic = abort` + strip;
  the published binary is UPX-packed down to 1.6 MB
- **Prebuilt for** `linux-x86_64`, `linux-x86_64-musl`,
  `linux-aarch64-musl`, `android-aarch64`, `macos-aarch64` and
  `windows-x86_64`. Each Linux and Android target also ships a `-wasm`
  variant with the plugin runtime compiled in.

## Install

### Prebuilt binaries

Grab a build from [Releases](https://github.com/ChrisZhangJin/nanopi/releases/latest):

```bash
# Adjust VERSION to the tag you want (e.g. v0.9.1)
VERSION=v0.9.1
curl -L -o nanopi \
  "https://github.com/ChrisZhangJin/nanopi/releases/download/${VERSION}/nanopi-${VERSION}-linux-x86_64-musl"
chmod +x nanopi
./nanopi --version
```

Per release, prebuilt binaries ship for:
- `nanopi-<ver>-linux-x86_64-musl` — fully static Linux, works on anything (recommended)
- `nanopi-<ver>-linux-x86_64` — dynamic glibc Linux, slightly smaller
- `nanopi-<ver>-linux-aarch64-musl` — static arm64: Raspberry Pi and
  arm64 servers
- `nanopi-<ver>-android-aarch64` — Android (Termux or `adb shell`).
  Prefer this over the musl build on a phone: it links bionic, so DNS
  resolves through the system resolver with no extra setup
- `nanopi-<ver>-macos-aarch64` — Apple Silicon (M1+)
- `nanopi-<ver>-windows-x86_64.exe` — Windows 10/11

Every Linux and Android asset above also has a `-wasm` twin (e.g.
`nanopi-<ver>-linux-x86_64-musl-wasm`) built with `--features wasm`. Take it
only if you use `[[extensions]]` WASM plugins — it carries the wasmtime
runtime and is ~7.2 MiB against the stock build's ~4 MB.

macOS and Windows are stock-only; for plugin support there, build from source with `cargo build --release --features wasm`.

### DNS on hosts without `/etc/resolv.conf`

nanopi resolves names with the bundled hickory resolver, which reads
`/etc/resolv.conf` and nothing else. Android has no such file — DNS is
done by netd — so a **static musl** build on a phone fails every request
with `error reading DNS system conf for hickory-dns: io error: os error 2`.

The `android-aarch64` asset handles this for you: on Android nanopi
switches off hickory and resolves through libc `getaddrinfo`, which on a
bionic-linked binary reaches netd. (Linking bionic is not sufficient on
its own — reqwest picks the resolver from a compile-time feature flag,
so the switch has to be made at runtime. See `src/net.rs`.) The static
musl build cannot do this: musl's own resolver reads the same missing
file and then defaults to `127.0.0.1:53`.

So on a phone, prefer `android-aarch64`. If you are on the musl build
instead, give it nameservers explicitly:

```bash
export NANOPI_DNS=223.5.5.5,119.29.29.29     # or any IP[:port] list
# or, persistently:
printf 'nameserver 223.5.5.5\nnameserver 119.29.29.29\n' > ~/.nanopi/resolv.conf
```

`NANOPI_DNS=system` forces the default behaviour, for a host that has a
working `/etc/resolv.conf` but a stale `~/.nanopi/resolv.conf`. Note that
explicit nameservers bypass any VPN's resolver, which can matter for
split-horizon DNS.

macOS Intel isn't prebuilt (GitHub runner supply is scarce); build from source with `cargo build --target x86_64-apple-darwin`.

On macOS the download is unsigned, so Gatekeeper blocks it. Clear the
quarantine attribute and ad-hoc sign it:

```bash
xattr -d com.apple.quarantine ./nanopi-<ver>-macos-aarch64 2>/dev/null || true
codesign --force --sign - ./nanopi-<ver>-macos-aarch64
```

### Build from source

```bash
# One-time host setup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
source "$HOME/.cargo/env"
rustup target add x86_64-unknown-linux-musl
sudo apt install -y musl-tools build-essential   # Debian/Ubuntu

# Build
cargo build --release --target x86_64-unknown-linux-musl
./target/x86_64-unknown-linux-musl/release/nanopi --version
```

## Quick start

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_BASE_URL=https://api.deepseek.com/v1
export OPENAI_MODEL=deepseek-v4-flash

# Interactive TUI (default)
nanopi

# One-shot -p mode (Claude Code semantics)
nanopi -p "read /etc/hostname and tell me what you see"

# JSON output for scripting
nanopi -p --output json "say hi"

# Prompt piped on stdin
echo "explain this error" | nanopi -p

# Resume: last session / by id / fork
nanopi --continue
nanopi --session <id>
nanopi --fork <id>
```

## CLI

| Flag | Default | Purpose |
|---|---|---|
| `--base-url` | `https://api.openai.com/v1` | OpenAI-compatible API root |
| `--model` | (required) | Model id |
| `--api-key` | `$OPENAI_API_KEY` | Bearer token |
| `-m`, `--message` | (piped stdin) | User message; first positional arg also accepted. In `-p` mode, falls back to piped stdin |
| `-p`, `--print` | false | Non-interactive mode |
| `--output` | `text` | `-p` output: `text` \| `json` |
| `--continue` | false | Resume the most recent session |
| `--session <id>` | — | Resume by session id |
| `--fork <id>` | — | Fork an existing session |
| `--no-hooks` | false | Disable all hooks |
| `-a`, `--approve` | false | Trust project resources for this run |
| `-N`, `--distrust` | false | Distrust project resources |
| `--skill <path>` | — | Load a skill file/dir (repeatable) |
| `-S`, `--no-skills` | false | Disable skill discovery |
| `-C`, `--no-context-files` | false | Disable AGENTS.md / CLAUDE.md discovery |
| `--system-prompt <text\|path>` | — | Replace the built-in system prompt |
| `--append-system-prompt <text\|path>` | — | Append to the system prompt (repeatable) |

## Skills

Nanopi implements the [Agent Skills spec](https://agentskills.io/specification). Drop a `SKILL.md` into `~/.nanopi/skills/<name>/`:

```markdown
---
name: greet
description: Greet the user warmly. Use for hellos.
---
Say "hi, friend" — nothing else.
```

Invoke explicitly, or let the model discover it via the auto-appended `<available_skills>` block in the system prompt:

```bash
/skill:greet             # expands SKILL.md into the message
/skill:greet in french   # extra args are appended
```

**Locations** (earlier wins on name collisions):
- User: `~/.nanopi/skills/`
- Project: `<cwd>/.nanopi/skills/` (only when trusted via `-a` or persisted decision)
- CLI: `--skill <path>` (files or dirs; loads even with `--no-skills`)

## Custom system prompt

`--system-prompt <text|path>` replaces the built-in identity/guidelines prompt; `--append-system-prompt <text|path>` (repeatable, values joined by a blank line) adds text after it. Both accept literal text OR a path to an existing file. Either flag suppresses the matching file discovery below entirely — no merge.

Without a flag, nanopi discovers:
- `<cwd>/.nanopi/SYSTEM.md` (only when the project is trusted via `-a` or a persisted decision), then `~/.nanopi/SYSTEM.md` — for `--system-prompt`.
- `<cwd>/.nanopi/APPEND_SYSTEM.md` (same trust rule), then `~/.nanopi/APPEND_SYSTEM.md` — for `--append-system-prompt`.

Project beats global; the global file needs no trust gate (it's your own machine, not a cloned repo). Context files, skills, and the "Current working directory: …" line still apply on top of a custom prompt — only the identity/tools/guidelines section is replaced. Caveat: a replaced prompt drops the auto-generated "Available tools: …" line, and some models skip tool calls without it, so mention the tools you expect the model to use.

## Hooks

nanopi has two extension systems, and as of v0.12.0 they can both see every lifecycle event — the difference is what they're allowed to do with it:

| | add tools | add commands | see events | veto / transform | hold state | cost per fire |
|---|---|---|---|---|---|---|
| shell hooks | ✗ | ✗ | ✓ (11) | ✓ | ✗ — fresh process | fork + exec |
| WASM plugins | ✓ | ✓ | ✓ (11, opt-in, observe-only) | ✗ | ✓ — `Store` persists | one function call |

In one sentence: **WASM plugins observe, shell hooks can refuse** — see [`docs/v0.12-events.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.12-events.md) §8 for why nanopi keeps both instead of merging into one system like PI's `ExtensionAPI`.

Shell hooks fire around tool calls, using Claude Code's hook *protocol* (JSON on stdin, exit code 2 to block, `tool_name` / `tool_input` / `hookSpecificOutput` fields) — but PI's *event names*, not Claude Code's. Configure in `~/.nanopi/settings.toml`:

```toml
[[hooks.tool_execution_start]]
matcher = "^bash$"
command = "logger 'nanopi about to shell out'"
```

Keys are `snake_case` (`tool_execution_start`, not `ToolExecutionStart`). `matcher` is a regex, and what it is tested against depends on the event: the tool name, the session id, the turn number, or the compaction reason. `input` is the one event with nothing to match against — a user message is not a tool call — so `matcher` there must be `"*"`, and any other value is a startup error rather than a hook that silently never fires. Full protocol in [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) §6.

### Renamed in v0.12

nanopi used to borrow Claude Code's names for four hooks. v0.12 renames them to PI's names, with no alias and no deprecation period — a config using an old key on the left fails to load at startup, and the error names the replacement on the right:

| Old key (retired — hard error) | New key |
|---|---|
| `pre_tool_use` | `tool_execution_start` |
| `post_tool_use` | `tool_execution_end` |
| `user_prompt_submit` | `input` |
| `session_end` | `session_shutdown` |

### Lifecycle events (v0.11.0)

In addition to the Claude Code-protocol trio above, nanopi exposes four lifecycle hooks that mirror Pi's `before_agent_start` / `turn_start` / `turn_end` / `message_end`:

| Hook key | Fires | Blockable? |
|---|---|---|
| `before_agent_start` | Once per turn, after compaction but before the user message enters context | yes (returns early with a synthetic message) |
| `turn_start` | Top of each agent-loop iteration | no (advisory) |
| `turn_end` | Bottom of each agent-loop iteration | no (advisory) |
| `message_end` | Once after the for-loop completes | no (advisory) |

For all four, the `matcher` runs against the turn number string (so `^1$` fires only on the first turn), and stdin carries `{ "turn_count": N, ... }` plus event-specific fields. Full enumeration in [`config.toml.example`](https://github.com/ChrisZhangJin/nanopi/blob/main/config.toml.example).

Two more fire around context compaction: `session_before_compact` and `session_compact`. Both are advisory, and their `matcher` runs against the reason string (`threshold` or `manual`).

## WASM extensions (v0.11.0)

Shell hooks can observe and veto, but they can't add a tool the model is allowed to call. Extensions can. A nanopi extension is a WebAssembly component — written in Rust, Go, C, or anything else that compiles to WASM — whose exported tools show up in the model's tool list next to `bash` and `read`.

**This is opt-in at build time.** The stock release binary has no WASM runtime, so it stays ~4 MB; `[[extensions]]` entries are ignored with a warning. To use them:

```bash
cargo build --release --features wasm
```

Then declare the components in `config.toml`:

```toml
[[extensions]]
path = "~/.nanopi/extensions/my-tool.wasm"
```

A plugin exports two required functions, and optionally two more ([`wit/nanopi-extension.wit`](https://github.com/ChrisZhangJin/nanopi/blob/main/wit/nanopi-extension.wit)):

| Export | Signature | Purpose |
|---|---|---|
| `list-tools` | `() -> string` | JSON array of `{name, description, parameters}`. Called once at load. `parameters` is a JSON Schema handed to the model verbatim. |
| `execute-tool` | `(name: string, args-json: string) -> string` | Runs a tool, returns `{"content": "...", "is_error": false}`. |
| `list-commands` | `() -> string` | *Optional.* JSON array of `{name, description}` — slash commands the user can type. |
| `execute-command` | `(name: string, args: string) -> string` | *Optional.* Runs a command, returns one of `{"print": "..."}`, `{"send_user_message": "..."}`, `{"error": "..."}`. |

**Tools vs commands.** A tool is something the *model* decides to call; a command is something *you* type. Commands show up in the `/` palette with the plugin's name attached, and are interactive-mode only — `nanopi -p` has no palette, though the plugin's tools still work there.

**Seeing what loaded.** `/tools` lists every tool the model can actually call, tagged `[builtin]` or `[plugin:<name>]` with the `.wasm` it came from. It reads the live registry — the same list handed to the provider — so it is the thing to check when a plugin tool seems missing. Asking the model to list its own tools is not a substitute: it cannot tell a plugin tool from a built-in one, and will guess.

`print` goes straight to your scrollback: the model never sees it and it never enters the session transcript. `send_user_message` starts a turn as if you had typed it — always echoed verbatim first, so a plugin cannot put words in your mouth invisibly; typed mid-stream it steers the running turn instead, exactly like your own typing. `error` is shown to you and, like a trap, is never forwarded to the model.

The two command exports live in a second WIT world, `extension-commands`, which `include`s the first. A tool-only plugin keeps targeting `extension` and keeps building unchanged — WIT cannot express an optional export, so widening the original world would have broken every existing plugin's *source* even though the host still loads its compiled *binary*.

**Watching lifecycle events (v0.12.0).** A third, opt-in WIT world, `extension-events`, `include`s `extension-commands` and adds two more exports:

| Export | Signature | Purpose |
|---|---|---|
| `list-events` | `() -> string` | *Optional.* JSON array of PI event names the plugin wants to observe. |
| `handle-event` | `(event: string, payload-json: string) -> string` | *Optional.* Called for every event both requested here AND granted by config. Return value is ignored — this is observe-only, a plugin cannot veto or transform through it. |

Delivery needs **both lists to agree**: the plugin's `list-events` and the config's `[[extensions]].events` (`config.toml.example` documents the full grant syntax). Either alone grants nothing — an unsatisfied request (exported but not granted) is reported at load, so a plugin that looks like it should be receiving events but isn't says why. The payload handed to `handle-event` is byte-identical to what a shell hook receives on stdin for the same event — same `HookInput` JSON, same builder.

This is a bigger grant than it might look: an `input` subscriber sees every prompt verbatim, and a `tool_execution_start` subscriber sees every tool call's arguments — bigger than `allow_fs`. Combining `events` with `allow_network = true` on one plugin is warned about at startup, because it turns the plugin into a channel that can exfiltrate whatever those events carry. **Do not fetch from an event handler** — `host-http-get` is still reachable from `handle-event`, but a slow or hostile fetch there is bounded by the fetch's own 10s timeout stacked on top of the event budget below, not by epoch interruption (epoch instrumentation cannot preempt a running host function).

Guest code in `handle-event` gets a 2s wall-clock budget — much tighter than a tool call's 30s, since an event handler sits on the turn's critical path and fires far more often. Delivery is **drop-on-busy, never blocking**: if a plugin's `Store` is already busy with an in-flight tool call, the event for that instant is dropped rather than queued, so one busy plugin can never stall an emit for the rest of the agent loop. Dropped events are counted per plugin and logged. `/tools` lists every plugin currently subscribed, under a "Watching events" section — the same inventory that already answers "what can the model call" now also answers "what is watching me".

And may import these host functions:

| Import | Signature | Gate | Purpose |
|---|---|---|---|
| `host-log` | `(level: u8, message: string)` | always | Write to nanopi's stderr. `0`=trace `1`=info `2`=warn `3`=error. |
| `host-fs-read` | `(path: string) -> string` | `allow_fs` | Read a UTF-8 file inside the working directory. Returns contents, or a string starting with `error: `. |
| `host-http-get` | `(url: string) -> string` | `allow_network` + `url_allowlist` | Fetch an `http`/`https` URL. Returns the response body, or a string starting with `error: `. |
| `host-store-get` | `(key: string) -> string` | `allow_store` | Read this plugin's stored value for `key`. Absent reads as `""`. |
| `host-store-set` | `(key: string, value: string) -> string` | `allow_store` | Replace this plugin's value at `key`. Returns `""` once the bytes are on the filesystem, or a string starting with `error: `. |
| `host-notify` | `(text: string) -> string` | always | Put one line in the user's scrollback, prefixed by the host with this plugin's name. Rate-limited per turn. |
| `host-set-context` | `(text: string) -> string` | `allow_context` | Declare text for the model's context, attributed to this plugin. Replaces the plugin's previous contribution; `""` clears it. Returns `""`, or a string starting with `error: `. |
| `host-call-tool` | `(name: string, args-json: string) -> string` | `allow_tools`, per tool named | Run one of nanopi's built-in tools. Returns `{"content": "...", "is_error": false}` for a call that ran, or a bare string starting with `error: ` for one that did not. |
| `host-send-user-message` | `(text: string) -> string` | `allow_send_message` | Start or steer a turn with this text, as if you had typed it. Always echoed to you verbatim, attributed to the plugin. Returns `""`, or a string starting with `error: `. |

Payloads cross the boundary as JSON strings rather than WIT records — one primitive type keeps the ABI small enough that neither side needs a codegen step.

A worked example lives in [`examples/wasm-plugin/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin), including the build command. [`examples/wasm-plugin-minimal/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin-minimal) is a smaller skeleton to copy — two tools, split into boilerplate and the part you replace.

For plugins meant to be *used* rather than read: [`examples/wasm-plugin-memory/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin-memory) is durable project memory — facts are markdown files in `.nanopi/memory/` that you can diff and commit, and only the compact index enters the model's context, with `recall` fetching the full text of whatever looks relevant. [`examples/wasm-plugin-report/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin-report) records per-tool time and failures across sessions, which answers "why was that slow" — `tool_execution_end` carries a `duration_ms` that nothing else persists.

All five plugins are indexed in [`examples/README.md`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples) — a table of what each one does, the grants it needs, and which WIT world it targets.

Step-by-step guides for writing, debugging, and gating a plugin are in the [wiki](https://github.com/ChrisZhangJin/nanopi/wiki) (English and Chinese).

**Sandboxing.** Components run inside wasmtime with no ambient authority — a plugin reaches the outside world only through host functions you opt into.

`host-fs-read` is gated on `allow_fs = true`, and even then the path must resolve *inside* the working directory. Paths are canonicalized before that check, so `../` traversal and symlinks pointing outward are both refused. (The built-in `read` tool deliberately has no such guard, on the reasoning that the model can shell out anyway — but a plugin has no shell, so here the boundary is real rather than theater.)

`host-notify` is ungated because it is output, not access — it reaches nothing and leaves nothing behind. What it can do is flood your attention, so it is bounded by a rate limit rather than a grant: past the per-turn allowance further lines are dropped and one `… N more suppressed` line says how many, and the suppressed calls come back as `error: ` strings so a plugin is never told it spoke when it did not. The prefix is applied by the host from the `.wasm` file stem and the payload is never parsed for it, so one plugin cannot impersonate another. It differs from `host-log` in where the text ends up: `host-log` writes raw stderr, which lands inside the TUI's managed region and is wiped by the next redraw, whereas `host-notify` scrolls into history and stays.

`host-store-get` / `host-store-set` are gated on `allow_store = true` and are **keys, not paths** — the plugin supplies a map key and the host alone decides which file it lands in (`~/.nanopi/extensions/<stem>/store.json`, one JSON object per plugin). So none of the confinement machinery above applies or is needed: there is no path for a plugin to point outward. Bounds are 1 MiB total, 1000 keys, and keys up to 128 chars; every refusal comes back in-band and stores nothing, so a plugin is never told it failed while the value went in anyway. A `""` return from `host-store-set` means the bytes reached the filesystem — the host commits with a temp file and a rename, so a crash leaves the whole old file or the whole new one, never a torn one. Two plugins whose `.wasm` files share a file stem where either has `allow_store` fail to load rather than silently sharing one store.

`host-set-context` is gated on `allow_context = true`, and it is the one import that changes what the *agent* believes rather than what the plugin knows: the text a plugin declares is folded into the system prompt at the start of each turn. The host writes the attribution header — `[context contributed by extension "memory"]` — because without it the model cannot tell a plugin's injected instructions from your own. Each call **replaces** that plugin's previous contribution rather than appending, so it is idempotent state and ten turns cannot accumulate ten copies; `""` clears it. The bound is 4 KiB per plugin, in bytes, and it is small deliberately: this text enters *every* request for the rest of the session. An over-bound call is refused in-band and the previous contribution still stands, so being told "no" never costs you what you already had. Every change is announced in your scrollback naming the plugin — a contribution is invisible otherwise, since you never see the system prompt — and that announcement is accounted separately from the plugin's `host-notify` allowance, so a plugin cannot bury it by first flooding you with noise. Combined with `allow_network = true` this is the sharpest pair of grants nanopi has, and it warns at startup: a remote source could then shape how the agent behaves.

`host-call-tool` is gated **per tool**, on `allow_tools` naming it. Empty (the default) denies everything, the same rule as `url_allowlist`. Per tool rather than per plugin because the tools are not equivalent: `allow_tools = ["find", "read"]` lets an indexer walk and read but not write or exec, while `allow_tools = ["bash"]` is arbitrary command execution that walks straight past `allow_fs`'s cwd confinement and `url_allowlist`'s per-host approval — nanopi prints an escalated warning at startup when it sees `bash` there, because that grant makes every other grant on that plugin decorative. Built-ins only: a plugin cannot call another plugin's tool, and a name that is not a built-in is a **load error** naming the valid ones rather than a silent no-op. The return shape is the honest half of the contract: a JSON frame means the call ran (including a call that ran and failed), and a bare `error: ` means it never happened. Nothing traps. Calls are bounded at 30s — the epoch budget bounds guest code and cannot preempt a host function already executing — and a `bash` child the timeout abandons may outlive it, which is a known limit rather than a claim. Your `[[hooks.*]]` still run, so a plugin does not outrank your own policy; a block comes back as `error: blocked by hook: <reason>`. Each call that ran is announced in your scrollback naming the plugin and the tool, on the host's own budget so a `host-notify` flood cannot bury it — necessary because a plugin's calls are deliberately **not** in the session transcript (a `tool_call` entry the model never emitted is the exact shape that made sessions unresumable), which means `--continue` will not show what a plugin did. `/tools` lists what every loaded plugin holds, under "Plugin grants"; a plugin granted nothing still gets a row, reading `no grants`.

`host-send-user-message` is gated on `allow_send_message = true`, and it is the only capability nanopi has that **spends your money**. Every other grant lets a plugin learn something, change what the agent believes, or run a tool you could have run yourself; this one causes turns you are billed for. It warns at startup on its own, without needing a second grant to pair with, which no other grant does. If a turn is running the text steers it, arriving as a fresh user message at the next iteration boundary — the same path a line you type mid-stream takes, deliberately, because a plugin should not get a parallel mechanism to the one `b90b27f` repaired. If no turn is running, it starts the next one, queued **behind** anything you have already typed: a line you watched land outranks something software decided to spend your budget on. It is never dropped.

Three bounds, all announced, none silent. One unconsumed message per plugin, which clears when the text actually reaches a turn rather than when the call returns. No message during a turn that plugin's own message started — per plugin, not globally, so an auditing plugin can still speak during a turn a rules plugin started. And at most 20 turns per plugin per session, which the spec's original two rules did not provide: a `turn_end` subscriber that sends once per turn satisfies both of them forever, because each new turn's origin is a new turn and each message is consumed before the next. The cap is a backstop against a runaway subscriber, not a budget, and it survives a plugin trapping — otherwise trapping would be how a plugin buys another twenty turns.

The text is always echoed to you verbatim, attributed, and there is no quiet path: what the host accepts you see, and what it refuses the plugin is told about in-band with a reason. Under `nanopi -p` there is no turn loop to reach, so the call is **refused** rather than quietly dropped — a plugin must never believe it sent something it did not.

`host-http-get` is gated twice: on `allow_network = true`, and then on the URL's host matching `url_allowlist`. An **empty allowlist denies everything**, so switching the capability on does not by itself reach anything. Matching is on the parsed host, not a substring — `https://evil.com/?x=api.github.com` and `https://api.github.com@evil.com/` are both refused against an allowlist of `api.github.com`.

Entries are patterns, because a plugin that fetches whatever the model hands it has no finite host list to enumerate:

| Entry | Matches |
|---|---|
| `github.com` | the host **and** its subdomains, any port |
| `*.github.com` | subdomains only — the apex `github.com` is refused |
| `*` | any `http`/`https` host |

`*` is the escape hatch, and it is a real one: it turns the second gate off, leaving `allow_network` as the only check — link-local metadata endpoints included. nanopi prints a warning at startup naming the plugin whenever it sees `*` with networking on. A star anywhere else (`api.*.com`) is refused rather than widened, so a typo can't quietly broaden the gate. `*` widens hosts only; the scheme check is separate, so `file://` stays outside the network capability under every pattern. Only `http`/`https`; requests time out at 10s so a plugin cannot hang a turn; redirects are **not** followed, since a 3xx would otherwise walk the fetch onto a host you never approved. Refusals and failures come back to the plugin in-band as `error: `-prefixed strings rather than as traps. A trap in a plugin is reported to the model as a failed tool call — it does not take down nanopi, and a `.wasm` that fails to load is skipped with a warning rather than blocking startup.

**Runaway plugins.** Guest code gets a ~30s wall-clock budget per tool call, enforced by wasmtime's epoch interruption. Exceeding it is a trap, which surfaces to the model as a failed tool call and leaves the plugin callable — the instance is rebuilt, so one bad call does not disable it for the rest of the session. Without the budget, a plugin containing an infinite loop would wedge nanopi permanently: the guest holds a real thread with no yield points, so <kbd>Esc</kbd> cannot reach inside it.

The budget applies to **guest** code only. Epoch interruption is instrumentation compiled into the guest, so it cannot interrupt a host function that is already running — a plugin blocked inside `host-http-get` or `host-fs-read` is bounded by those functions' own limits (a 10s request timeout; regular-files-only and a 1 MiB cap, which is what stops a FIFO from blocking forever), not by the epoch deadline. Worst case for one call is therefore the budget plus one host call, not the budget alone.

**Name collisions.** A plugin may not register a tool whose name already exists. Collisions are reported and skipped, so a plugin cannot quietly replace `bash`.

Commands are stricter, and the two rules genuinely differ. A **tool** collision is first-wins: the tool already registered stays and the newcomer is skipped. A **command** collision refuses *both* claimants — if two plugins each register `/deploy`, neither gets it, because silently picking a winner would mean `/deploy` runs whichever plugin happened to load first. A command whose name belongs to a built-in like `/compact` is skipped. Every case prints a warning naming the plugin(s), and never affects that plugin's other commands or any of its tools.

Plugins are loaded at startup and on `/new`, `/resume`, `/fork`, `/import` — and, as of v0.12.0, on `/reload`, which re-reads `[[extensions]]` and hot-swaps the live registry.

**Hot reload.** `/reload` loads the new `.wasm` files *first*, then unregisters the old tools and commands, then registers the new ones — so a plugin whose file has stopped loading keeps the instance that is already running, and `/reload` says so in red rather than leaving you with neither the old plugin nor the new one. It reports what it did: how many plugins reloaded, how many tools and commands they brought, which plugins the config no longer lists (unregistered), and which failed. Grants are re-read from the config, so revoking `allow_network` takes effect on `/reload`; a failed plugin keeps its old grant row, because that row describes what is actually still running.

A tool call already in flight when a reload lands is **refused, not executed and not silently substituted**: the plugin's old instance recognises that it has been replaced and returns an `error:` string naming the reload as the reason, both before the call starts and after the guest returns. The second check is the one that matters — you are never told a call succeeded when its result came from code that has since been replaced. Side effects the call already had (a file written, an HTTP request sent) stand, and the refusal says so. Events are not delivered to a replaced instance at all.

Host-side plugin state survives a reload, deliberately: the plugin's host-store files (they are on disk, and a reload is not a factory reset), and the `plugin_send` loop guard and spent per-session message budget (otherwise `/reload` would be a budget refund, and a plugin could reload itself out of its own limit). The one exception is a plugin's system-prompt contribution, which is dropped if the plugin is gone from the config or has lost `allow_context` — keeping it would put text in the prompt that nothing live can account for.

## Versions

| Version | Status | Size | Notes |
|---|---|---|---|
| **v0.11.0** | current | ~1.6 MB | WASM extensions with gated `host-fs-read` / `host-http-get` and plugin-registered slash commands; Pi lifecycle hooks (`before_agent_start`, `turn_start`, `turn_end`, `message_end`); mid-stream steering; configurable tool exec mode |
| v0.10.0 | released | 1.6 MB | Custom system prompt (`--system-prompt`, `SYSTEM.md`); explicit `api_kind` beats the vendor sniff; readable tool failures in `-p`; UPX-packed release |
| v0.9.x | released | ~3.9 MB | First-run wizard, `/settings` + `/keybindings`, 8-vendor dispatch, retry envelope (0.9.2–0.9.3); v0.9.1 fixed the v0.9.0 tool loop |
| v0.9.0 | released | ~4.0 MB | Skills (PI-parity), `--skill`/`--no-skills`, folded TUI card, `UserPromptSubmit` hook |
| v0.8.x | released | ~3.9 MB | Full ratatui TUI, `/fork`, `--continue`/`--session`, hooks, JSONL sessions |
| v0.5.0 | released | ~3.0 MB | Tools (read/write/edit/bash), `-p` mode, JSON output, hooks |
| v0.1.0 | released | 2.4 MB | Single-file OpenAI streaming demo (kept as `nanopi_v0_1` binary) |

Sizes are the published musl artifact. From v0.10.0 that artifact is
UPX-packed (`make`), so 1.6 MB is not comparable to the unpacked figures
above it — the same build is 4.4 MB before packing. The v0.11.0 figure is
approximate: it is measured from a development build, not a published tag.

## Roadmap

No feature checklist. nanopi is the lightweight Rust take on Pi: the aim is to
carry Pi's core surface in one small static binary, not to match everything Pi
does. A feature gets in when it earns the weight it adds.

Known gap: Linux aarch64 is not in the CI matrix yet — build it yourself with
`cargo build --release --target aarch64-unknown-linux-musl` (see above).

## Cargo mirror (China)

Add to `~/.cargo/config.toml` for faster crate downloads:

```toml
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[target.x86_64-unknown-linux-musl]
linker = "musl-gcc"
```

## Design notes

- **musl + LTO + panic=abort + strip** → small static binary. rustls avoids the OpenSSL dep.
- **Hand-written SSE parser** — no `reqwest-eventsource`, keeps the dep tree lean.
- **JSONL over JSON** — append-only files survive crashes mid-write.
- **Provider abstraction** landed in v0.6; native Anthropic + any OpenAI-compatible endpoint.

See [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) and [`docs/PLAN.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/PLAN.md) for design + implementation notes, and [`docs/claims-and-races.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/claims-and-races.md) for what nanopi is allowed to claim about its own actions — plus the race table behind those rules, and [`docs/plugin-capabilities.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/plugin-capabilities.md) for the outbound surface a plugin gets (and the per-tool grant that keeps the sandbox worth paying for).

## Credits

- [Pi](https://github.com/earendil-works/pi) — the upstream TypeScript agent nanopi ports.
- [Claude Code](https://github.com/anthropics/claude-code) — hook protocol, `-p` mode, skills spec.
- [ratatui](https://github.com/ratatui-org/ratatui) & [crossterm](https://github.com/crossterm-rs/crossterm) — the TUI foundation.

## License

[MIT](LICENSE) © Chris Zhang

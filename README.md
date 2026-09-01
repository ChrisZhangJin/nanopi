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
- 🔌 **Multi-provider** — any OpenAI-compatible endpoint (DeepSeek, ollama, vLLM, …) plus native Anthropic
- 🛠 **Streaming tool calls** — `read` / `write` / `edit` / `bash`, rendered live in a ratatui TUI
- 🪝 **Claude Code hooks** — `PreToolUse` / `PostToolUse` / `UserPromptSubmit` shell hooks
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
- **Prebuilt for** `linux-x86_64`, `linux-x86_64-musl`, `macos-aarch64`
  and `windows-x86_64`. Linux ARM is not prebuilt yet — build from source
  with `cargo build --release --target aarch64-unknown-linux-musl`.

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
- `nanopi-<ver>-macos-aarch64` — Apple Silicon (M1+)
- `nanopi-<ver>-windows-x86_64.exe` — Windows 10/11

macOS Intel isn't prebuilt (GitHub runner supply is scarce); build from source with `cargo build --target x86_64-apple-darwin`.

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

Shell hooks fire around tool calls, matching Claude Code's `PreToolUse` / `PostToolUse` / `UserPromptSubmit` protocol. Configure in `~/.nanopi/settings.toml`:

```toml
[[hooks.pre_tool_use]]
matcher = "^bash$"
command = "logger 'nanopi about to shell out'"
```

Keys are `snake_case` (`pre_tool_use`, not `PreToolUse`). Full protocol in [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) §6.

## Versions

| Version | Status | Size | Notes |
|---|---|---|---|
| **v0.10.0** | current | 1.6 MB | Custom system prompt (`--system-prompt`, `SYSTEM.md`); explicit `api_kind` beats the vendor sniff; readable tool failures in `-p`; UPX-packed release |
| v0.9.x | released | ~3.9 MB | First-run wizard, `/settings` + `/keybindings`, 8-vendor dispatch, retry envelope (0.9.2–0.9.3); v0.9.1 fixed the v0.9.0 tool loop |
| v0.9.0 | released | ~4.0 MB | Skills (PI-parity), `--skill`/`--no-skills`, folded TUI card, `UserPromptSubmit` hook |
| v0.8.x | released | ~3.9 MB | Full ratatui TUI, `/fork`, `--continue`/`--session`, hooks, JSONL sessions |
| v0.5.0 | released | ~3.0 MB | Tools (read/write/edit/bash), `-p` mode, JSON output, hooks |
| v0.1.0 | released | 2.4 MB | Single-file OpenAI streaming demo (kept as `nanopi_v0_1` binary) |

Sizes are the published musl artifact. From v0.10.0 that artifact is
UPX-packed (`make`), so 1.6 MB is not comparable to the unpacked figures
above it — the same build is 4.4 MB before packing.

## Roadmap

- **v1.0** — full PI parity: themes, compaction, extension system
- Linux aarch64 not yet in the CI matrix (see `cargo build --release --target aarch64-unknown-linux-musl` above)

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

See [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) and [`docs/PLAN.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/PLAN.md) for design + implementation notes.

## Credits

- [Pi](https://github.com/earendil-works/pi) — the upstream TypeScript agent nanopi ports.
- [Claude Code](https://github.com/anthropics/claude-code) — hook protocol, `-p` mode, skills spec.
- [ratatui](https://github.com/ratatui-org/ratatui) & [crossterm](https://github.com/crossterm-rs/crossterm) — the TUI foundation.

## License

[MIT](LICENSE) © Chris Zhang

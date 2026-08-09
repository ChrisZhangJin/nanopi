# nanopi

A minimal Rust port of [Pi](https://github.com/earendil-works/pi) — a tiny, statically-linked coding agent CLI for resource-constrained environments.

## Goals

- Single static binary, zero runtime dependencies (musl-linked)
- Drops into old Linux boxes (CentOS 6+, glibc 2.12+) and low-resource systems
- Targets a binary under 5 MB (v0.5: **~3.0 MB**)
- Preserves Pi's core experience: multi-provider LLM, streaming tool calls, JSONL sessions, `.pi/` extension hooks, Claude Code-style `PreToolUse`/`PostToolUse` shell hooks, `-p` print mode

## Versions

| Version | Status | Size | Notes |
|---|---|---|---|
| **v0.9.0** | ✅ current | ~4.0 MB | Skills (PI-parity): SKILL.md dirs, `/skill:name` expansion, `--skill`/`--no-skills`, `disable-model-invocation`, folded TUI card + Ctrl-O expand, `UserPromptSubmit` hook wired |
| v0.8.1 | released | ~3.9 MB | Full ratatui TUI, `/fork` (PI-parity), `--continue`/`--session`/`--fork`, hooks, JSONL sessions; `--yolo` removed for PI-parity |
| v0.5.0 | released | ~3.0 MB | Tools (read/write/edit/bash), `-p` mode, JSON output, Claude Code-style hooks, JSONL sessions |
| v0.1.0 | released | 2.4 MB | Single-file OpenAI streaming demo (preserved as `nanopi_v0_1` binary) |

See `docs/v0.5-research.md` for the design and `docs/PLAN.md` for the implementation plan.

## Quick start

```bash
# Build (one-time host setup — see Setup below)
cargo build --release --target x86_64-unknown-linux-musl

# Interactive: a single question + exit (full TUI is v0.6+)
./target/x86_64-unknown-linux-musl/release/nanopi \
    --base-url https://api.deepseek.com/v1 \
    --model deepseek-v4-flash \
    --api-key sk-... \
    "用一句话介绍你自己"

# Or via stdin
echo "What is 2+2?" | ./nanopi --model deepseek-v4-flash --base-url https://api.deepseek.com/v1 --api-key sk-...

# -p mode (Claude Code semantics): non-interactive, output to stdout
./nanopi -p "读 /etc/hostname 告诉我内容" --model ... --base-url ... --api-key ...

# JSON output for programmatic use
./nanopi -p --output json "say hi" --model ... --base-url ... --api-key ...

# Tools: read/write/edit/bash are auto-available
./nanopi -p "read /etc/hostname and tell me what you see" --model ... --base-url ... --api-key ...

# Hooks: configure under ~/.nanopi/settings.toml
# NOTE: TOML keys are snake_case (`pre_tool_use`, not `PreToolUse`).
# See docs/v0.5-research.md §6 for the protocol.
```

## CLI flags

| Flag | Default | Purpose |
|---|---|---|
| `--base-url` | `https://api.openai.com/v1` | OpenAI-compatible API root |
| `--model` | (required) | Model id (provider-specific) |
| `--api-key` | `$OPENAI_API_KEY` | Bearer token |
| `-m`, `--message` | (stdin in interactive) | User message; first positional arg also accepted |
| `-p`, `--print` | false | Non-interactive mode (Claude Code's `-p` semantics) |
| `--output` | `text` | `-p` mode output: `text` \| `json` |
| `--no-hooks` | false | Disable all hooks (emergency switch) |
| `-a`, `--approve` | false | Trust project-local resources for this run |
| `-N`, `--distrust` | false | Distrust project-local resources for this run |
| `--tools` | (all) | Tool whitelist (reserved; v0.5 ships all 4 always-on) |
| `--skill <path>` | — | Load a skill file/dir (repeatable); loads even with `--no-skills` |
| `-S`, `--no-skills` | false | Disable user + project skill discovery |
| `--help`, `-h` | — | |
| `--version`, `-V` | — | |

## Skills

Nanopi implements the [Agent Skills standard](https://agentskills.io/specification) — same shape as PI, Claude Code, and OpenAI Codex. A skill is a directory holding a `SKILL.md` with YAML frontmatter:

```
~/.nanopi/skills/greet/SKILL.md
---
name: greet
description: Greet the user warmly. Use for hellos.
---
Say "hi, friend" — nothing else.
```

**Locations** (loaded in order; earlier wins on name collisions):

- User:    `~/.nanopi/skills/`
- Project: `<cwd>/.nanopi/skills/` (only when the project is trusted via `-a` or a persisted decision in `~/.nanopi/trust/`)
- CLI:     `--skill <path>` (files or dirs; additive even with `--no-skills`)

**Discovery rules** (mirror PI's `loadSkillsFromDirInternal`):

- A directory containing `SKILL.md` → skill root, no further recursion
- Root-level `*.md` files at the top-level scan are also picked up
- Subdirectories are recursed looking for `SKILL.md`
- `.dot` directories and `node_modules/` are skipped

**Frontmatter** (per Agent Skills spec):

- `name` — 1-64 chars, lowercase `a-z0-9-`, no leading/trailing/consecutive hyphens (falls back to parent-dir name if absent)
- `description` — required, ≤1024 chars (drops the skill if missing)
- `disable-model-invocation` — when `true`, hides the skill from the system prompt; still invocable via `/skill:name`

**How it works.** At startup nanopi scans the locations above and builds an `<available_skills>` block, appending it to the system prompt whenever the `read` tool is available. The model sees names + descriptions + absolute file paths, and reads the full body on demand. Users can also invoke explicitly:

```bash
/skill:greet             # expands SKILL.md into the message before sending
/skill:greet in french   # extra text is appended after the block
```

In the TUI, an invocation renders as a folded `[skill] greet (Ctrl+O to expand)` card — Ctrl-O splashes the full SKILL.md body into scrollback. Sessions persist `SkillInvocation` entries so `/resume` and `--continue` replay the card intact.

**Config:**

```toml
# ~/.nanopi/config.toml
[skills]
disabled  = ["old-skill"]        # hide a discovered skill by name
extra_dirs = []                  # reserved
```

Deferred from v0.9 (see `docs/v0.9-plan.md`): `.gitignore` awareness inside skill dirs; extension-provided skill paths (nanopi has no extension system yet).

## Setup

### One-time host setup

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
source "$HOME/.cargo/env"

rustup target add x86_64-unknown-linux-musl
sudo apt install -y musl-tools build-essential   # Debian/Ubuntu
```

### Cargo mirror (China, optional)

Drop into `~/.cargo/config.toml`:

```toml
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[target.x86_64-unknown-linux-musl]
linker = "musl-gcc"
```

### Build

```bash
cargo build --release --target x86_64-unknown-linux-musl
# Both binaries: target/.../release/nanopi (v0.5) and nanopi_v0_1 (legacy)
```

### Smoke test

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_BASE_URL=https://api.deepseek.com/v1
export OPENAI_MODEL=deepseek-v4-flash
./tests/smoke.sh
```

## Roadmap

| Version | Scope | Status |
|---|---|---|
| v0.1 | OpenAI streaming + JSONL sessions | released |
| v0.5 | Tools + -p mode + JSON output + hooks | released |
| v0.6 | Anthropic provider, parallel tool execution, TUI opt-in | released |
| v0.7 | TUI as default mode, `/fork` PI-parity, static musl release | released |
| **v0.8** | System prompt broadened to general Agent, `--yolo` removed (PI-parity) | **current** |
| v1.0 | Full Pi parity (themes, compaction) | future |

## License

TBD.

## v0.1 — Minimal Demo (this release)

**What's in:**
- CLI parsing via `clap` v4
- OpenAI-compatible HTTP `/chat/completions` streaming (works with OpenAI, DeepSeek, ollama, vLLM, etc.)
- Hand-written SSE parser (zero extra deps)
- Token-by-token stdout rendering with ANSI color
- JSONL session persistence to `~/.nanopi/sessions/<id>.jsonl`

**What's NOT in (yet):**
- No TUI (raw stdin/stdout)
- No tool calls (read/bash/edit/write)
- No Anthropic provider
- No skills/prompts/trust/hooks
- No session listing or `--continue`/`--resume`/`--fork`

## Quick start

```bash
# Build (one-time setup; see Setup section below)
cargo build --release --target x86_64-unknown-linux-musl

# Run with DeepSeek
./target/x86_64-unknown-linux-musl/release/nanopi \
    --base-url https://api.deepseek.com/v1 \
    --model deepseek-v4-flash \
    --api-key sk-... \
    --message "用一句话介绍你自己"

# Run with ollama locally
./target/x86_64-unknown-linux-musl/release/nanopi \
    --base-url http://localhost:11434/v1 \
    --model llama3 \
    --message "hi"

# Or just set env vars and drop the flags
export OPENAI_API_KEY=sk-...
export OPENAI_BASE_URL=https://api.deepseek.com/v1
./target/x86_64-unknown-linux-musl/release/nanopi \
    --model deepseek-v4-flash \
    --message "用三句话解释 SSE"
```

## CLI flags

| Flag | Default | Purpose |
|---|---|---|
| `--base-url` | `https://api.openai.com/v1` | OpenAI-compatible API root |
| `--model` | (required) | Model identifier (provider-specific) |
| `--api-key` | `$OPENAI_API_KEY` | Bearer token |
| `--message` | (read stdin) | User message |

## Session format

Each invocation writes `~/.nanopi/sessions/<uuid>.jsonl` with three lines:

```jsonl
{"type":"session","version":1,"id":"...","timestamp":"2026-08-05T04:52:02Z","model":"deepseek-v4-flash","base_url":"https://api.deepseek.com/v1"}
{"type":"message","id":"...","timestamp":"...","role":"user","content":"用一句话介绍你自己"}
{"type":"message","id":"...","timestamp":"...","role":"assistant","content":"你好，我是DeepSeek..."}
```

Sessions are append-only JSONL. Each entry carries a UUID v7-style id (currently a nanosecond timestamp — full UUID v7 will land in v0.5).

## Setup

### One-time host setup

```bash
# Install Rust + musl target + C linker
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
source "$HOME/.cargo/env"

rustup target add x86_64-unknown-linux-musl
sudo apt install -y musl-tools build-essential   # Debian/Ubuntu
```

### Cargo mirror (China, optional)

Drop into `~/.cargo/config.toml`:

```toml
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[target.x86_64-unknown-linux-musl]
linker = "musl-gcc"
```

### Build

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

Result: a fully static ~2.4 MB binary at `target/x86_64-unknown-linux-musl/release/nanopi`.

## Roadmap

| Version | Scope | Target size |
|---|---|---|
| **v0.1** ✅ | OpenAI-compat streaming + JSONL sessions | 2.4 MB |
| **v0.2** | Tool calls (read/bash/edit/write) + TUI upgrade (crossterm) | ~3 MB |
| **v0.3** | Skills/prompts/trust + `beforeToolCall` hooks | ~3.5 MB |
| **v0.4** | Anthropic-compatible provider | ~3.7 MB |
| **v0.5** | `rustyline` line editing + session listing/continue/fork | ~4 MB |
| **v1.0** | Multi-provider routing, settings.json, polished TUI | <5 MB |

## Design notes

- **musl + strip + LTO + panic=abort** gives a small static binary; rustls avoids the OpenSSL dependency.
- **SSE parser is hand-written** (no `reqwest-eventsource` crate) to keep the dep tree lean.
- **JSONL over JSON** because append-only files survive crashes mid-write.
- **Provider abstraction deferred to v0.4** — v0.1 hardcodes the OpenAI-compatible wire format.

## License

TBD.
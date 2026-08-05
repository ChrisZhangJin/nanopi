# nanopi

A minimal Rust port of [Pi](https://github.com/earendil-works/pi) — a tiny, statically-linked coding agent CLI for resource-constrained environments.

## Goals

- Single static binary, zero runtime dependencies (musl-linked)
- Drops into old Linux boxes (CentOS 6+, glibc 2.12+) and low-resource systems
- Targets a binary under 5 MB (currently 2.4 MB)
- Preserves Pi's core experience: multi-provider LLM, streaming tool calls, JSONL sessions, `.pi/` extension hooks

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
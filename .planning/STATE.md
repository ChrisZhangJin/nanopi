---
gsd_state_version: 1.0
milestone: v0.11.0
milestone_name: milestone
status: unknown
last_updated: "2026-08-22T17:08:06.847Z"
last_activity: "2026-08-19 - Completed quick task 260819-ayr: first-run wizard."
---

# Project State

Last activity: 2026-08-25 - Completed quick task 260825-kft: --system-prompt / --append-system-prompt flags + SYSTEM.md discovery.

## Current Focus

M2 · Extensions (v0.11.0) — P0: expand shell-hook event coverage to
`before_agent_start` / `turn_start` / `turn_end` / `message_end`.
No new dependencies, pure `src/agent/` change.

## What's Built

- Core agent loop (`src/agent`), providers (`src/provider`), tool suite (`src/tool`).
- TOML config loader (`src/config.rs`) with global + project layering.
- Trust prompt (`src/trust.rs`), settings/keybindings pickers (v0.9.3).
- Static musl release pipeline (Makefile, dist/).
- **First-run wizard** (`src/wizard.rs`): provider pick-list (OpenAI, DeepSeek, Anthropic direct, Gemini gateway, Ollama, Custom), probe-before-write validation, `~/.nanopi/api_key` at mode 0600, `~/.nanopi/config.toml` with tilde-form `api_key_file`. Exposed as `nanopi init` and auto-launched on first run when no config/env/flags provide credentials.

## What's Next

- Nothing scoped — add via `/gsd:quick`.

## Blockers/Concerns

None currently tracked.

## Quick Tasks Completed

| # | Description | Date | Commit | Directory |
|---|-------------|------|--------|-----------|
| 260819-ayr | First-run wizard for config bootstrap | 2026-08-19 | d977c94 | [260819-ayr-add-a-first-run-wizard-to-nanopi-console](./quick/260819-ayr-add-a-first-run-wizard-to-nanopi-console/) |
| 260825-kft | `--system-prompt` / `--append-system-prompt` flags + `SYSTEM.md` / `APPEND_SYSTEM.md` discovery | 2026-08-25 | 885b0a7 | [260825-kft-add-system-prompt-append-system-prompt-c](./quick/260825-kft-add-system-prompt-append-system-prompt-c/) |

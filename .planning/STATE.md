---
gsd_state_version: 1.0
milestone: v0.11.0
milestone_name: milestone
status: unknown
last_updated: "2026-08-28T15:12:27.371Z"
last_activity: "2026-08-28 - Completed quick task 260828-l4d: gated host-http-get for WASM plugins."
---

# Project State

Last activity: 2026-08-28 - Completed quick task 260828-l4d: gated `host-http-get` for WASM plugins.

## Current Focus

M2 · Extensions (v0.11.0). Two of the three plugin capabilities are now
shipped — `host-fs-read` (`788705c`) and `host-http-get` (`260828-l4d`).
Remaining: **plugin slash-command registration**, the largest of the three.
Upstream Pi's dispatch model is traced in
[`.planning/reference/pi-slash-commands.md`](./reference/pi-slash-commands.md);
its trust model does not transfer, since nanopi plugins are sandboxed and
capability-gated where Pi's extensions are unsandboxed in-process imports.

Also still open from before: expand shell-hook event coverage to
`before_agent_start` / `turn_start` / `turn_end` / `message_end`
(pure `src/agent/` change, no new dependencies).

## What's Built

- Core agent loop (`src/agent`), providers (`src/provider`), tool suite (`src/tool`).
- TOML config loader (`src/config.rs`) with global + project layering.
- Trust prompt (`src/trust.rs`), settings/keybindings pickers (v0.9.3).
- Static musl release pipeline (Makefile, dist/).
- **First-run wizard** (`src/wizard.rs`): provider pick-list (OpenAI, DeepSeek, Anthropic direct, Gemini gateway, Ollama, Custom), probe-before-write validation, `~/.nanopi/api_key` at mode 0600, `~/.nanopi/config.toml` with tilde-form `api_key_file`. Exposed as `nanopi init` and auto-launched on first run when no config/env/flags provide credentials.
- **WASM plugin capabilities** (`src/wasm/`): `host-fs-read` (read-only, cwd-confined, symlink-aware) and `host-http-get` (gated on `allow_network`, then a host-matching `url_allowlist` that is deny-by-default; 10s timeout, 1 MiB body cap, redirects not followed). Both return in-band `error: ` strings rather than trapping. The `Store`/`Config` stay synchronous — network is bridged by a worker thread + `mpsc`, so `ComponentBridge` is untouched and no dependency was added.

## What's Next

- **Plugin slash-command registration** — the last of the three M2 capabilities. See `.planning/reference/pi-slash-commands.md`.
- Shell-hook event coverage (`before_agent_start` / `turn_start` / `turn_end` / `message_end`).

## Blockers/Concerns

- **Pre-existing test flakiness under parallel execution.** Roughly 1 `cargo test` run in 3 fails a varying subset of lib tests that mutate process-global env (`HOME`, `NANOPI_HOME`, cwd) and race on the default thread pool — `agent::prompt_override::tests::*` (as a cluster), `session::tests::roundtrip_all_entry_types`, `paths::tests::nanopi_home_honors_env`, `agent::hook::tests::run_hook_exit_2_means_block`, and others. **Not** caused by `260828-l4d`; reproduces at base commit `324a7d8` and in the default suite, which contains none of that task's code. Both suites are deterministically green with `-- --test-threads=1`. Fix is an env mutex or injecting paths instead of reading globals — its own commit. Detail in `quick/260828-l4d-.../deferred-items.md`.
- **Non-canonicalized path guard** in `src/tool/write.rs:48` and `src/tool/edit.rs` — `starts_with` on an uncanonicalized path accepts `<cwd>/../../etc/passwd`. Real bug, deferred to its own commit. `resolve_readable` in `src/wasm/loader.rs` is the reference fix.

## Quick Tasks Completed

| # | Description | Date | Commit | Directory |
|---|-------------|------|--------|-----------|
| 260819-ayr | First-run wizard for config bootstrap | 2026-08-19 | d977c94 | [260819-ayr-add-a-first-run-wizard-to-nanopi-console](./quick/260819-ayr-add-a-first-run-wizard-to-nanopi-console/) |
| 260825-kft | `--system-prompt` / `--append-system-prompt` flags + `SYSTEM.md` / `APPEND_SYSTEM.md` discovery | 2026-08-25 | 885b0a7 | [260825-kft-add-system-prompt-append-system-prompt-c](./quick/260825-kft-add-system-prompt-append-system-prompt-c/) |
| 260828-l4d | Gated `host-http-get` for WASM plugins (`allow_network` + host-matching `url_allowlist`, no-redirect guard, 10s timeout) | 2026-08-28 | `83bbe68`…`28c2e75` | [260828-l4d-finish-gated-host-http-get-capability-fo](./quick/260828-l4d-finish-gated-host-http-get-capability-fo/) |

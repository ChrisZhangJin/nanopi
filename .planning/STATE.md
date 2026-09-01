---
gsd_state_version: 1.0
milestone: v0.11.0
milestone_name: milestone
status: unknown
last_updated: "2026-09-01T00:00:00.000Z"
last_activity: "2026-09-01 - Shipped v0.10.1 (Windows first-run fix, MiniMax default, error-body flattening); cherry-picked all three onto v0.11.0; reconciled this file and ROADMAP.md against the tree."
---

# Project State

Last activity: 2026-09-01 — released `v0.10.1`, cherry-picked its three
fixes onto `v0.11.0`, and re-derived this file from the code (it had
drifted 30 commits behind).

## Current Focus

**M2 · Extensions (v0.11.0).** The four planned phases (P0–P3) are
shipped, and so are both capability-gated host functions. The feature
surface looks closed; what remains is one deferred capability plus a
release decision.

Remaining in the plugin story: **plugin slash-command registration** —
the WIT world (`wit/nanopi-extension.wit:73,85`) exports only
`list-tools` and `execute-tool`, so a plugin cannot register a command.
Upstream Pi's dispatch model is traced in
[`.planning/reference/pi-slash-commands.md`](./reference/pi-slash-commands.md);
its trust model does not transfer, since nanopi plugins are sandboxed
and capability-gated where Pi's extensions are unsandboxed in-process
imports.

Then: decide whether v0.11.0 merges to `main` and ships. See
ROADMAP M2. That is a release call, not a routine merge.

## What's Built

- Core agent loop (`src/agent`), providers (`src/provider`), vendor
  dispatch (`src/vendor`), tool suite (`src/tool`).
- TOML config loader (`src/config.rs`) with global + project layering;
  `src/paths.rs` owns every `~/.nanopi` / `NANOPI_HOME` path **and the
  single home-expansion point** (`expand_home`).
- Trust prompt (`src/trust.rs`), settings/keybindings pickers (v0.9.3).
- Static musl release pipeline (Makefile, dist/). VERSION is
  centralized: `VERSION` file → `include_str!` in `src/main.rs:29`,
  `make bump VERSION=x.y.z`, and a tag-vs-VERSION gate in
  `release.yml`. `.planning/PLAN-VERSION.md` is **implemented**.
- **First-run wizard** (`src/wizard.rs`): provider pick-list (OpenAI,
  DeepSeek, Anthropic direct, Gemini gateway, Ollama, Custom),
  probe-before-write validation, key file at mode 0600. Exposed as
  `nanopi init` and auto-launched when no config/env/flags supply
  credentials. It writes an **absolute** `api_key_file` — the old
  tilde-form literal was the Windows first-run bug, fixed in
  `e9425b8`.
- **Hook lifecycle** — all ten events implemented
  (`src/agent/hook.rs:38`): `PreToolUse`, `PostToolUse`,
  `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `BeforeAgentStart`
  (the only one that can Block or Transform), `TurnStart`, `TurnEnd`,
  `MessageEnd`, `SessionBeforeCompact`, `SessionCompact`.
  `post_tool_use` can transform the result, not just observe it.
- **Steer / follow-up injection** — mid-stream typing steers the
  running turn; queued follow-ups auto-start the next one.
- **`tool_exec_mode`** — parallel vs sequential tool execution
  (`src/config.rs:114`). Pi's per-tool override is still deferred.
- **WASM plugin system** behind `--features wasm` (`src/wasm/`).
  Components declared in `[[extensions]]` are compiled, instantiated,
  and their exported tools registered alongside the built-ins.
  Capabilities: `host-log`, `host-fs-read` (read-only, cwd-confined,
  symlink-aware), `host-http-get` (gated on `allow_network`, then a
  deny-by-default host-matching `url_allowlist`; 10 s timeout, 1 MiB
  body cap, redirects not followed). Both I/O functions return in-band
  `error: ` strings rather than trapping. The `Store`/`Config` stay
  synchronous — network is bridged by a worker thread + `mpsc`, so
  `ComponentBridge` is untouched and no dependency was added.
- **Hardening pass** — ~20 `fix(...)` commits after the feature work:
  plugin epoch deadlines, trap isolation, `url_allowlist` backslash
  bypass, cwd-guard escapes in `write`/`edit`, cancel-safety of
  parallel tool batches, session-file corruption on cancel, bash
  timeouts returning partial output.

## What's Next

- **Plugin slash-command registration** — the last plugin capability.
  See `.planning/reference/pi-slash-commands.md`.
- **v0.11.0 release decision** — merge to `main` + bump + tag, or keep
  developing. The release procedure (and its traps) is documented per
  ROADMAP M3.

## Blockers/Concerns

- **Pre-existing test flakiness under parallel execution.** ~3 of 10
  `cargo test --lib` runs fail a varying subset. Reproduces at base
  commits with none of the suspect code, so **take a baseline before
  blaming a change**.
  *Root cause (established 2026-09-01):* ~50 sites hand-roll
  `NANOPI_HOME` set/restore. `TEST_LOCK` (`src/lib.rs`) is meant to
  serialize them, but a test that panics **while holding it poisons
  the mutex**, and almost every call site is `.lock().unwrap()` — so
  one real failure cascades into 13–14 reported ones. Only
  `settings_toml.rs::_lock()` recovers, via
  `unwrap_or_else(|e| e.into_inner())`.
  *Fix, two independent steps:* (1) swap every
  `TEST_LOCK.lock().unwrap()` for the `into_inner()` recovery form —
  kills the cascade so failures are legible; (2) collapse the 50
  boilerplate blocks into one `with_temp_nanopi_home()` helper — kills
  the race. New tests should inject paths instead of touching env;
  `paths::expand_against` is the pattern.
  Deterministically green with `-- --test-threads=1` (491 passed,
  1 ignored as of 2026-09-01).

- ~~Non-canonicalized path guard in `src/tool/write.rs` /
  `src/tool/edit.rs`~~ — **RESOLVED.** Fixed across `1149f38`,
  `72feae0`, `cd26474`; the deepest existing ancestor is now
  canonicalized so a symlinked directory inside cwd cannot escape it.
  Regression tests cover both the absolute and relative `..` shapes
  and the symlink case (`write.rs::rejects_write_through_symlinked_dir`,
  `edit.rs::rejects_traversal_out_of_cwd`).

## Quick Tasks Completed

| # | Description | Date | Commit | Directory |
|---|-------------|------|--------|-----------|
| 260819-ayr | First-run wizard for config bootstrap | 2026-08-19 | d977c94 | [260819-ayr-add-a-first-run-wizard-to-nanopi-console](./quick/260819-ayr-add-a-first-run-wizard-to-nanopi-console/) |
| 260825-kft | `--system-prompt` / `--append-system-prompt` flags + `SYSTEM.md` / `APPEND_SYSTEM.md` discovery | 2026-08-25 | 885b0a7 | [260825-kft-add-system-prompt-append-system-prompt-c](./quick/260825-kft-add-system-prompt-append-system-prompt-c/) |
| 260828-l4d | Gated `host-http-get` for WASM plugins (`allow_network` + host-matching `url_allowlist`, no-redirect guard, 10s timeout) | 2026-08-28 | `83bbe68`…`28c2e75` | [260828-l4d-finish-gated-host-http-get-capability-fo](./quick/260828-l4d-finish-gated-host-http-get-capability-fo/) |
| — | v0.10.1 fixes cherry-picked onto this branch (Windows `~` expansion, MiniMax default, error-body flattening) | 2026-09-01 | `e9425b8`, `777eb8a`, `642f696` | — |

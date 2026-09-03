---
gsd_state_version: 1.0
milestone: v0.11.0
milestone_name: milestone
status: unknown
last_updated: "2026-09-02T00:00:00.000Z"
last_activity: "2026-09-02 - v0.12 events A+B: hook events renamed to PI names, honest session payloads, and WASM plugins observing all eleven lifecycle events."
---

# Project State

Last activity: 2026-09-02 — Stages A and B of the v0.12 event work are
both in: hook events renamed to PI's vocabulary with honest session payloads
and retired keys refused (`7a15138`…`9caa4c5`), and WASM plugins can now
observe all eleven lifecycle events under a config-granted,
observe-only subscription (`5d2e90f`…`009f236`).

The two extension systems can finally see the same events: shell hooks
keep the veto, plugins get to watch, and both read one payload built
once per event. What remains for v0.12 is a release decision, not a
feature — `make bump VERSION=…` first, since `release.yml` hard-fails
the whole matrix on a tag/VERSION mismatch *after* publishing an empty
release.

## Current Focus

**M2 · Extensions (v0.11.0) — feature surface closed.** The four
planned phases (P0–P3) shipped, both capability-gated host functions
shipped, and plugin slash commands shipped on 2026-09-02.

Next is a decision, not a feature: whether v0.11.0 merges to `main` and
ships. That is a release call. Note it needs `make bump VERSION=…`
first — release branches do not carry their own version, and
`release.yml` hard-fails the whole build matrix on a tag/VERSION
mismatch *after* publishing an empty release. See ROADMAP M3.

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
- **Plugin slash commands** (2026-09-02) — a component may export
  `list-commands` / `execute-command` from the second WIT world
  `extension-commands`, and its commands appear in the `/` palette
  tagged with the plugin name. A command returns an action rather than
  calling back into the host, so the import list stays at three:
  `{"print"}` writes to scrollback (never seen by the model),
  `{"send_user_message"}` starts or steers a turn (always echoed
  verbatim first), `{"error"}` is shown only to the user. Collisions
  are refused, never renamed. `src/command.rs` is deliberately
  non-gated so `mode/tui.rs` keeps zero `cfg(feature = "wasm")`.
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

- **v0.11.0 release decision** — merge to `main` + bump + tag, or keep
  developing. The release procedure (and its traps) is documented per
  ROADMAP M3.
- Deferred plugin capabilities, none blocking: per-tool `executionMode`
  override, provider registration from plugins, richer session
  metadata. Plugin **hot reload** is the notable one — `/reload`
  deliberately skips `[[extensions]]` because `ToolRegistry` has no
  unregister path; doing it properly needs that plus a generation
  counter on `ComponentBridge`.

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
  Deterministically green with `-- --test-threads=1`: as of
  2026-09-02, 508 lib tests in the default build, 550 with
  `--features wasm`, plus 20 in `wasm_plugin_integration` and 6 in
  `skills_integration`; 1 ignored.

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
| — | Plugin slash-command registration (WIT second world, non-gated `command` vocabulary, collision-refusing registry, TUI dispatch on the blocking pool) + two adjacent fixes: plugins now load on resumed sessions, and a leading space no longer sends a slash command to the model | 2026-09-02 | `4df493b`…`e9ce962` | — |
| 260902-m0z | Stage A of the v0.12 event work: the four Claude-Code-named hook events take PI's names (`tool_execution_start` / `tool_execution_end` / `input` / `session_shutdown`), retired keys are a hard config error naming the replacement, `session_start`/`session_shutdown` carry PI's `reason` and fire on `/new` `/resume` `/fork` `/import`, and the session payload stops lying (`session_id` + `NANOPI_SESSION_ID` are the real id; the reason moved to `arguments.reason`) | 2026-09-02 | `7a15138`…`9caa4c5` | [260902-m0z-rename-hook-events-pi-names](./quick/260902-m0z-rename-hook-events-pi-names/) |
| 260902-nms | Stage B of the v0.12 event work: WASM plugins observe lifecycle events. Third WIT world `extension-events` (`list-events` / `handle-event`), delivery at all eleven emit sites, config-granted subscriptions (both the plugin's request and the `events` grant must agree), observe-only with `try_lock`-and-drop so a busy plugin can never extend a turn, a 2s event epoch budget, `/tools` shows subscriptions, and a committed fixture + example plugin | 2026-09-02 | `5d2e90f`…`009f236` | [260902-nms-wasm-event-subscribers](./quick/260902-nms-wasm-event-subscribers/) |
| 260903-l1s | Inline `<think>…</think>` blocks on the OpenAI wire render as thinking instead of reply text. Split in the provider adapter, so the same fix keeps reasoning out of the context, the session transcript and the `--output json` envelope — and makes the OpenAI wire produce the same transcript as the Anthropic one for the same model. Vendor-gated (xiaomi, minimax) with an `inline_think_tags` escape hatch | 2026-09-03 | `3cc93ee`…`b2d5759` | [260903-l1s-inline-think-tags](./quick/260903-l1s-inline-think-tags/) |

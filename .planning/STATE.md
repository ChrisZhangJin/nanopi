---
gsd_state_version: 1.0
milestone: v0.11.0
milestone_name: milestone
status: unknown
last_updated: "2026-09-07T15:08:40.630Z"
last_activity: 2026-09-04 — manual acceptance of v0.12. Eight defects
---

# Project State

Last activity: 2026-09-04 — manual acceptance of v0.12. Eight defects
found by hand and fixed, all in the same family: nanopi describing its
own actions more confidently than it performed them.
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
| 260903-l1s | Inline `<think>…</think>` blocks on the OpenAI wire render as thinking instead of reply text. Split in the provider adapter, so the same fix keeps reasoning out of the context, the session transcript and the `--output json` envelope — and makes the OpenAI wire produce the same transcript as the Anthropic one for the same model. Gated on POSITION, not vendor: only a leading `<think>` block counts as reasoning, so a mid-answer mention or a code fence stays literal and the fix reaches every OpenAI-wire vendor including ollama/vLLM. `inline_think_tags` remains the escape hatch both ways | 2026-09-03 | `3cc93ee`…`78958cd` | [260903-l1s-inline-think-tags](./quick/260903-l1s-inline-think-tags/) |
| — | Manual acceptance of v0.12 surfaced eight defects, fixed with regression tests: reasoning rendered in two colors and the reply losing its color after any interruption (`d48f1aa`); the exit hint naming the startup session instead of the one you ended in (`af9f18b`); a no-op `/compact` claiming it compacted and leaving an orphaned `session_before_compact` hook with no matching `session_compact` (`87a81b4`); a mid-stream steer silently dropped when the turn ended without tool calls, so the saved session attributed a reply to a question never asked (`b90b27f`); `make bump` rewriting the wasmtime dependency version (`1777fb8`); stderr notices staircasing in raw mode across 35 call sites (`53bd497`); a spurious `api_kind` warning for a correct dual-surface MiniMax config, unsatisfiable by construction (`f1b27f5`); and a refused prompt shown as `[error: [ … ]]` framed as a malfunction rather than the user's own policy (`c0ce35c`). Plus PI-style grouped startup notices (`4dc8f2a`) and the dropped abbreviated session id (`5f2574e`) | 2026-09-04 | `d48f1aa`…`c0ce35c` | — |
| 260907-d87 | Stage 1 of `docs/plugin-capabilities.md` — the outbound surface a plugin gets. Three host imports: `host-store-get`/`host-store-set` behind a new `allow_store` grant (keyed JSON store under `~/.nanopi/extensions/<stem>/`, quotas checked before any write, temp+rename so a crash cannot tear the file, host-side so it survives the instance rebuild a trap triggers), and `host-notify` — ungated because it is output not access, bounded by a per-turn rate limit that announces its own suppression count, prefixed by the HOST so a plugin cannot impersonate another. Every capability is an IMPORT, which is why the `Mutex`/`try_lock` model, the `EventHandler` signature and the observe-only argument in `docs/v0.12-events.md` §3 are all untouched; no new WIT world either, since the linear-ladder constraint binds exports only and the three committed fixtures still load. Two gaps surfaced rather than papered over: `ExtensionConfig` carries no `deny_unknown_fields` (only `HooksSection` does), so a typo'd grant parses and grants nothing silently — pinned, not fixed, because enabling it is a decision about every `[[extensions]]` key; and §3's `/tools` grant row is deferred with the grants pipe stages 2-3 need, with the spec amended so it stops claiming a row that does not exist | 2026-09-07 | `b657aee`…`5e1d884` | [260907-d87-plugin-outbound-surface-stage-1-of-docs-](./quick/260907-d87-plugin-outbound-surface-stage-1-of-docs-/) |
| 260907-edb | Stage 2 of `docs/plugin-capabilities.md` — `host-set-context` behind a new `allow_context` grant, so a plugin can put attributed text in front of the model without deciding anything. The design problem worth remembering: `compose_system_prompt` runs ONCE at Agent construction while a plugin contributes later, so injecting there captures nothing. `Agent` now holds `system_base` and `context.system` is DERIVED per turn from base + rendered blocks — nothing appends to it, so turns cannot stack, and re-deriving the base by stripping a suffix was rejected because it makes correctness depend on a string search over attacker-controlled text. The refresh sits before `maybe_compact` so `estimate_chars` counts the contribution's real cost. `plugin_context.rs` is deliberately NOT feature-gated, following `subscriber.rs`, which is what keeps `loop_.rs` free of `cfg(feature = "wasm")` and the non-wasm prompt byte-identical. One plan decision was overridden: the disclosure line must not spend from the plugin's own `MAX_NOTIFY_PER_TURN`, because `notify.rs` drops lines once the budget is gone — a plugin could emit ten lines of noise and then rewrite the agent's instructions undisclosed. Separate counter, bidirectionally isolated. Also folds in the `deny_unknown_fields` on `ExtensionConfig` deferred from stage 1, so a typo'd grant is now a load error instead of parsing and granting nothing in silence | 2026-09-07 | `04069fa`…`3ca242f` | [260907-edb-plugin-context-contribution-stage-2-of-d](./quick/260907-edb-plugin-context-contribution-stage-2-of-d/) |
| 260907-i8f | Stage 3 of `docs/plugin-capabilities.md` — `host-call-tool`, the last and sharpest import: a plugin can run nanopi's own built-in tools behind a per-tool `allow_tools` grant (empty denies everything; a name that is not a built-in is a LOAD ERROR naming the valid ones; `bash` in the list gets the escalated `[Extensions]` warning because it walks past `allow_fs`'s cwd confinement and `url_allowlist`'s per-host approval). Executed through ONE path plus a `ToolCallOrigin` flag rather than a second narrower path — the codebase's own history (`b90b27f`, `87a81b4`) is what argues against two paths for one situation — and the flag decides exactly three things: no `SessionEntry` (a `tool_call` the model never emitted is the shape that made sessions unresumable until `f70e5cc`), no `AgentEvent`, and a 30s deadline. The deadline wraps `tool.execute` ONLY, not `run_one_tool`, so `tool_execution_end` still fires after a timeout — wrapping the function would have manufactured a second instance of the unbalanced-hook-pair defect `87a81b4` in order to bound a timeout. Two design decisions worth remembering: the spec's prescribed `Arc<ToolRegistry>` in `PluginState` is NOT implementable (the registry is still being built while `load_all` runs, `EventSubscribers` does not exist yet, and the `AgentEvent` sender is per-turn), so the seam is a process-wide installed dispatch refreshed once per `run_turn`, following `notify::install_sink`; and plugins may call built-ins only, which is not permission tidiness but the deadlock fix — `execute_tool` takes a blocking lock unlike `handle_event`'s `try_lock`, so two mutually-granted plugins would hang, and built-ins-only removes the cycle by construction rather than by cycle detection. Also builds the grants pipe stages 1 and 2 deferred: `/tools` now shows a `Plugin grants` row per loaded plugin, and a plugin granted nothing still gets one reading `no grants`, because "installed, powerless" and "not installed" must not look identical | 2026-09-07 | `07464d0`…`5fa5acb` | [260907-i8f-plugin-tool-calls-stage-3-of-docs-plugin](./quick/260907-i8f-plugin-tool-calls-stage-3-of-docs-plugin/) |
| 260907-r3k | Stage 4 of `docs/plugin-capabilities.md`, the last import — `host-send-user-message` behind `allow_send_message`: a plugin can start or steer a turn with text of its own, always echoed verbatim and attributed. This is the ONLY grant that spends the user's money, so it warns at startup ALONE, unlike the `allow_network` pairs — the `allow_context`-alone precedent does not apply because that grant is only sharp in combination. Three things worth remembering. (1) **§2.4's two mandated rules do not bound the loop they are aimed at, and implementing them made that plain**: a `turn_end` subscriber sending once per turn satisfies both forever, because each new turn's origin is a NEW turn and each message is consumed before the next. Hence a third bound, `MAX_PLUGIN_TURNS_PER_SESSION = 20`, per plugin, announced, surviving a guest trap so trapping is not how a plugin buys another twenty turns; recorded in the spec as a stage-4 addition rather than backdated. (2) **The spec asked for the echo BEFORE the send and the code does the opposite on purpose** — `b90b27f` is the bug where echoing first printed `[steer] …` and then discarded the text when the receiver died. Ordering is unobservable anyway; what the user needs is a biconditional (no send without an echo, no echo without a send), which is pinned as one countable partition — `echoes + overflow == accepted sends`, disjoint — rather than as two anecdotes. The spec was amended, not quietly diverged from. (3) The plan's "install at both Agent build sites" is NOT implementable and would have broken headless: the Agent never holds a steer *sender* (`run_turn` takes `steer_rx`, `build.rs` has no `SteerMessage`), so an Agent-side install could only publish an empty sink — redundant in the TUI and, in `-p`, exactly the silent no-op invariant 9 forbids. The per-turn refresh lives at `KeyAction::StartTurn`, the one turn boundary the TUI already has. Rule 1 clears at HAND-OFF, not on return, or it is decorative; rule 2 is per plugin, not global, or the audit-plus-rules pair silences itself. Of 12 reversions, 11 red at once and one — deleting the per-turn sink install — passed all 781 tests, because the call site needs a live `Term` to reach; strengthened with a deliberately brittle source-reading test | 2026-09-07 | `1fd55f4`…`e9931ce` | [260907-r3k-plugin-send-user-message-stage-4-of-do](./quick/260907-r3k-plugin-send-user-message-stage-4-of-do/) |

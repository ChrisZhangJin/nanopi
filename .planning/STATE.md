---
gsd_state_version: 1.0
milestone: v0.12.0
milestone_name: milestone
status: ready-to-release
last_updated: "2026-09-07T17:19:10.284Z"
last_activity: "2026-09-07 — v0.12.0 feature-complete and bumped. plugin-capabilities stages 4-5, plugin hot reload, per-tool executionMode, session metadata, all three known defects, and the flaky-suite debt. 836 lib tests green, 0 ignored, 0 warnings, parallel 5/5. Not tagged and not pushed — both are the owner's call."
---

# Project State

Last activity: 2026-09-07 — v0.12.0 closed out. Every remaining item on
the roadmap and in the v0.12 docs shipped except one, which is blocked
on a decision rather than on work.

Three things are worth carrying forward more than the feature list:

- **`ModelChange` had a reader, a replay path, an `/export` renderer and
  a roundtrip test — and no writer, since the session format existed.**
  It survived because the only test naming it asserted that the variant
  serializes, which it always did. Its five checks were bare
  `matches!(entry, Variant { .. });` STATEMENTS: the macro returns a
  bool, the `;` discarded it, so the test asserted nothing at all.
  Confirmed by rewriting one to the wrong variant and watching it pass.
  When a variant looks unused, check for a writer, not just a reader.

- **The flaky suite was two defects, not one**, which is why partial
  fixes kept not working. A poisoning cascade made failures illegible
  (one real failure reported as 13-14); a restore-on-the-happy-path-only
  leak was the race itself. Recovering from the poisoned mutex did
  nothing about the leak.

- **Two spec claims were disproved by implementing them**, and the specs
  were amended rather than quietly diverged from: `§Required tests`
  demanded an echo-before-send ordering that recreates `b90b27f`, and
  §2.4's two "mandatory" loop-guard rules do not bound the loop they
  target (a `turn_end` subscriber satisfies both forever).

## Current Focus

**v0.12.0 is feature-complete, tested, and version-bumped. What remains
is not development.**

Three things are waiting, all of them the owner's call:

1. **Tag and release.** `VERSION`, `Cargo.toml [package]` and
   `Cargo.lock` are all `0.12.0` (`d3e0332`), and `nanopi --version`
   reports it. The bump is done FIRST on purpose: `release.yml` gates
   the build matrix on tag == VERSION *after* publishing the release
   object, so a mismatch leaves an empty release and every job red.
   Nothing is tagged and nothing is pushed.

2. **Push the wiki.** 13 commits sit unpushed in
   `/root/workspace/tmp/nanopi.wiki` (+2116/-220 across 17 files, both
   languages). One is urgent rather than cosmetic: `Hooks.md` /
   `Hooks-zh.md` were teaching the RETIRED event names, which are now a
   hard config error — the published wiki currently tells users to write
   a config that makes nanopi refuse to start.

3. **Decide on provider registration from plugins**, the one deferred
   item that did not ship. It is blocked on a security question, not on
   effort: a plugin-supplied provider must reach its own endpoint, so it
   necessarily bypasses `url_allowlist`. Is it exempt, and if so what
   replaces the guard? That answer determines whether the other two
   blockers (a `&'static str` in the `Provider` trait, and streaming
   inverting the guest-calls-host direction all nine imports rely on)
   are worth solving. Full argument in `docs/BACKLOG.md`.

One judgement call made during this work that is cheap to reverse if you
disagree: **`bash` now runs sequentially by default**, so a batch of two
long bash calls costs the sum instead of the max. It buys the fix for
concurrent bash silently losing updates while both calls reported
success — a bug that had been sitting `#[ignore]`d. `[tool_exec_overrides]`
with `bash = "parallel"` takes the speed back, and both directions are
pinned by wall-clock tests.

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

- **Tag + release v0.12.0.** Bump already done (`d3e0332`). Merge to
  `main`, tag `v0.12.0`, push. Nothing else blocks it.

- **Push the wiki** — 13 commits unpushed, and the Hooks pages currently
  published teach retired event names that are now a hard config error.

- **Provider registration from plugins** — the one deferred capability
  that did not ship. Blocked on a security decision (does a plugin
  provider bypass `url_allowlist`?), not on effort. See
  `docs/BACKLOG.md`.

- **`custom entries` in the session format** — deliberately not built.
  Needs a decision about who writes and who reads; a plugin-written
  entry runs into invariant 15 (a plugin-initiated tool call is never
  persisted, because it replays as a `tool_use` the model never
  requested — the `f70e5cc` failure mode).

- Everything else the roadmap deferred has shipped: per-tool
  `executionMode` (`003399f`), plugin hot reload (`34866aa`…`0a2f10c`),
  richer session metadata (`483aec8`). `ToolRegistry::unregister_plugin`
  and per-plugin instance ids were the two prerequisites hot reload
  needed, and both exist now.

- Manual acceptance: `docs/v0.12-manual-test-plan.md` has an EMPTY
  known-defect list for the first time. T2.8, T3.9 and the rewritten
  T2.7 / T4.6 / T4.7 are new or changed and have not been run by a
  human yet.

## Blockers/Concerns

- ~~**Pre-existing test flakiness under parallel execution.**~~ —
  **RESOLVED 2026-09-07.** Parallel runs are 8/8 green. If a run goes
  red now, it is a real failure: **stop taking a baseline before
  believing it.**
  *It was two defects, not one*, which is why the earlier partial fixes
  never held. (1) A test panicking while holding `TEST_LOCK` poisoned
  it, and almost every site was `.lock().unwrap()`, so one real failure
  was reported as 13–14 and the true one was not first — fixed by making
  `crate::test_lock()` the only way in, with a test that walks `src/`
  and fails the build on a direct acquisition (`a5f5ce9`). (2) All 127
  hand-rolled blocks put the restore at the END OF THE BODY, so a
  failing assertion skipped it and left `$NANOPI_HOME` pointing at a
  temp dir about to be deleted — that was the race itself, and
  recovering from the poisoned mutex did nothing about it. Fixed with
  `TempNanopiHome`, RAII, restore on unwind (`30e5ddf`, `1a8051a`,
  `8205d9c`).
  *Two things the migration turned up*, both a local copy getting the
  hard part right and the easy part wrong: `config.rs`'s own
  `HomeGuard` restored correctly and never took the lock; and
  `permission.rs::persist_and_session_only_is_noop` had no guard at all.
  *A trap worth remembering*: swapping in the shared guard first
  produced a DEADLOCK, not a failure — six tests held `lock()` and then
  constructed a guard that locks again, and `std::sync::Mutex` is not
  reentrant. The symptom was a 400-second timeout with no output, which
  reads like a hung build rather than a test bug.
  Current counts, `-- --test-threads=1`: **836 lib with `--features
  wasm`, 716 default**, plus 37 `wasm_plugin_integration`, 11
  `print_mode_e2e`, 6 `skills_integration`. **0 ignored, 0 warnings.**
  New tests should still prefer injecting paths over touching env;
  `paths::expand_against` is the pattern, and `TempNanopiHome` is for
  when the env genuinely has to move.

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

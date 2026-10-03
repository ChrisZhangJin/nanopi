---
phase: 01-in-process-runtime
plan: 04
subsystem: agent-runtime
tags: [tokio, subagent, in-process, cancellation, panic-audit]

requires:
  - "SubagentRegistry, PermissionBroker, FileStateTracker, widened ToolContext from 01-01 (src/agent/subagent_registry.rs, src/tool/mod.rs)"
  - "Agent.agent_id/limits/stop_reason/subagents/file_state + StopReason, ToolRegistry::for_subagent(), run_one_tool wiring from 01-03 (src/agent/loop_.rs)"
provides:
  - "run_subagent(): in-process subagent dispatcher built on Agent::build_fresh + tokio::spawn, mapping StopReason/Result to status ok/limit_reached/cancelled/failed"
  - "Per-subagent transcript at .nanopi/agents/<run>/<id>/transcript.jsonl, never written into the parent session"
  - "Zero non-test unwrap()/expect() on the subagent execution path (subagent.rs, subagent_registry.rs, loop_.rs, all provider/*.rs)"
affects: [01-05, 01-06]

tech-stack:
  added: []
  patterns:
    - "tokio::select! races ctx.registry.acquire_permit() against the derived cancel token so a subagent cancelled while still queued reports status:cancelled instead of hanging"
    - "Provider adapters fall back to reqwest::Client::new() instead of panicking on a TLS backend init failure, since every subagent dispatch constructs its own fresh Provider via provider_factory"
    - "run_item checks ctx.registry.template().is_none() before resolving the agent name, so an uninitialised runtime always reports the same in-band error regardless of whether the requested agent exists"

key-files:
  created: []
  modified:
    - src/tool/subagent.rs
    - src/agent/loop_.rs
    - src/provider/anthropic.rs
    - src/provider/openai.rs

key-decisions:
  - "Mid-stream cancellation (Esc while the HTTP response was still streaming) in Agent::run_turn now sets stop_reason = Some(Cancelled) before returning Ok, matching the pre-iteration cancel check — previously only the pre-check path set it, so a caller inspecting stop_reason after a successful run_turn (like the new subagent dispatcher) would misreport a mid-stream cancel as a normal completion"
  - "reqwest client construction failures in AnthropicProvider::new/OpenAiProvider::new fall back to reqwest::Client::new() rather than panicking, since the failure depends only on the TLS backend (never per-request input) and every subagent spawn builds its own Provider instance via the registry's provider_factory"
  - "Template-missing check moved ahead of agent-name resolution in run_item, so 'subagent runtime not initialised' is reported deterministically instead of being masked by 'unknown agent' when neither is set up"

requirements-completed: [RT-01, RT-02, RT-04, RT-08]

duration: ~70min
completed: 2026-10-03
---

# Phase 1 Plan 04: In-Process Subagent Dispatcher Summary

**`src/tool/subagent.rs` fully rewritten to spawn subagents as `tokio::spawn`ed `Agent::build_fresh` instances sharing the parent's registry/broker/file-state — the old child-process (`nanopi -p --output json`) runtime is gone with no fallback, and a panic audit closed the two remaining non-test `unwrap()`/`expect()` sites reachable from a subagent dispatch**

## Performance

- **Duration:** ~70 min
- **Tasks:** 2
- **Files modified:** 4 (`src/tool/subagent.rs`, `src/agent/loop_.rs`, `src/provider/anthropic.rs`, `src/provider/openai.rs`)

## Accomplishments

- `src/tool/subagent.rs` rewritten end to end: `nanopi_invocation`, `final_assistant_text`/`JsonEnvelope` parsing, `run_single`, `spawn_and_collect`, `write_prompt_tempfile` and all `Command`/tempfile/`Stdio` usage deleted. The only path left for a `single`/`parallel`/`chain` dispatch is `run_subagent`, which builds a fresh `Agent` via `Agent::build_fresh` and `tokio::spawn`s its `run_turn`.
- `run_subagent` per item: fetches the parent's `SpawnTemplate` from `ctx.registry` (missing → in-band `"subagent runtime not initialised"`, no panic); allocates an id via `next_id()`; reserves a live slot via `reserve(id, foreground=true)` (`max_live` enforced, D-08); derives a cancel token as a child of `ctx.turn_cancel` when present or the registry's background token otherwise (D-05); races `acquire_permit()` against that token so a subagent cancelled while still queued reports `status: "cancelled"` instead of running or hanging; builds the sub-`Agent` with `ToolRegistry::for_subagent()` further narrowed by the agent file's own `tools:` allowlist (built entirely through `ToolRegistry`'s public `get`/`register_external`, no changes needed to `src/tool/mod.rs`); writes its transcript to `.nanopi/agents/<run>/<id>/transcript.jsonl`; `tokio::spawn`s `run_turn` and awaits the `JoinHandle`, draining the sub-agent's event channel locally so nothing leaks into the parent's event stream or session file.
- Outcome mapping: `Ok(text)` + `StopReason::LimitReached{limit}` → `status: "limit_reached"`, content prefixed `"partial result (limit reached: {limit})"`; `Ok(text)` + `StopReason::Cancelled` → `status: "cancelled"`, `is_error: true`; `Ok(text)` + anything else → `status: "ok"`; `Err(AgentError)` → `status: "failed"` carrying the error text; a `JoinError` (the spawned task never completing) is also mapped to `"failed"` rather than propagating — no subagent failure or panic-in-task can take down nanopi's own process (D-11, RT-08).
- `run_parallel` now relies entirely on the registry's own global semaphore inside `run_subagent` for concurrency bounding — the old per-call `Semaphore` is gone, closing the one `.expect("subagent semaphore is never closed")` the previous implementation had. `run_chain` keeps the existing `{previous}` substitution and stop-on-first-failure semantics, both covered by tests.
- Fixed a real bug in `Agent::run_turn` (`src/agent/loop_.rs`) surfaced by writing the cancellation test: the mid-stream cancel path (Esc while the HTTP response was still streaming, i.e. the `tokio::select!` at the provider call) returned `Ok(final_text)` without ever setting `self.stop_reason`, unlike the pre-iteration cancel check which did set `Some(StopReason::Cancelled)`. A caller that only inspects `stop_reason` after a successful `run_turn` — which is exactly what the new subagent dispatcher does — would have misreported a mid-stream-cancelled subagent as a normal completion. Fixed by setting `self.stop_reason = Some(StopReason::Cancelled)` on that path too.
- Task 2 panic audit of the full subagent execution path (`src/tool/subagent.rs`, `src/agent/subagent_registry.rs`, `src/agent/loop_.rs`, and all five `src/provider/*.rs` files): `subagent.rs`, `subagent_registry.rs` and `loop_.rs` already had zero non-test `unwrap()`/`expect()` (the previous single site, in the now-deleted child-process semaphore code, left with that code). Two remained in the provider adapters, both reachable because every subagent spawn builds its own fresh `Provider` via `provider_factory`: `AnthropicProvider::new`/`OpenAiProvider::new`'s `.expect("build reqwest client")` now falls back to `reqwest::Client::new()` instead of panicking on a TLS-backend-only failure condition no caller can act on; `AnthropicProvider`'s request builder had an `.expect("checked is_array above")` after a match guard that already proved the array-ness — converted to a `match` that falls through to the same "start a fresh user turn" behavior the `_` arm already used, so a future refactor of that guard degrades instead of panicking. No remaining indexing site needed an `// infallible:` annotation; the two left (`steps[idx]` in `format_chain`, `named[0]` in `openai.rs`'s `coalesce_index_split_call`) are each proven in-bounds by an immediately preceding length/membership check on the same collection — the same pattern already used elsewhere in the codebase.

## Task Commits

1. **Task 1: In-process subagent dispatcher** - `bdc47f3` (feat)
2. **Task 2: Panic audit of the subagent execution path** - `08365bb` (fix)

## Files Created/Modified
- `src/tool/subagent.rs` — complete rewrite: child-process runtime deleted; `run_subagent`/`run_item`/`run_parallel`/`run_chain`/`subagent_tool_registry` implement the in-process dispatcher; 16 new tests covering all 8 plan behaviors plus the pure helper functions
- `src/agent/loop_.rs` — one-line fix: mid-stream cancel path now sets `stop_reason = Some(StopReason::Cancelled)`
- `src/provider/anthropic.rs` — `.expect("build reqwest client")` → `unwrap_or_else(|_| reqwest::Client::new())`; `.expect("checked is_array above")` → non-panicking `match` with the same fallback the existing `_` arm used
- `src/provider/openai.rs` — same reqwest-client fallback as anthropic.rs

## Audit Checklist (Task 2)

Non-test `unwrap()`/`expect()` counts, `sed '/#\[cfg(test)\]/,$d' FILE | grep -v '^\s*//' | grep -cE '\.unwrap\(\)|\.expect\('` (matches the plan's acceptance-criteria grep exactly):

| File | Before | After |
|------|-------:|------:|
| `src/tool/subagent.rs` (old file, pre-rewrite) | 1 | 0 |
| `src/agent/subagent_registry.rs` | 0 | 0 |
| `src/agent/loop_.rs` | 0 | 0 |
| `src/provider/anthropic.rs` | 2 | 0 |
| `src/provider/openai.rs` | 1 | 0 |
| `src/provider/sse.rs` | 0 | 0 |
| `src/provider/retry.rs` | 0 | 0 |
| `src/provider/think_tags.rs` | 0 | 0 |
| `src/provider/mod.rs` | 0 | 0 |

`grep -c "infallible:"` across all nine files: 0 — no indexing site required the annotation; both remaining index expressions are proven in-bounds by an adjacent check on the same collection (documented inline with existing-style comments rather than the `// infallible:` tag, since neither introduces a *new* risk this plan needed to flag).

## Decisions Made
- Mid-stream cancellation now sets `stop_reason` consistently with the pre-iteration cancel check (Rule 1 bug fix — a pre-existing gap from 01-03 that only became observable once something, the subagent dispatcher, started relying on `stop_reason` after a successful `run_turn` return).
- Provider construction failures become a graceful fallback (`reqwest::Client::new()`) rather than a panic, scoped narrowly to the one failure mode (`ClientBuilder::build()`) that is reachable per-subagent-spawn and depends on no per-request input — not a broader `Result`-ifying of the `Provider::new` constructors, which would have been an architectural signature change across dozens of call sites outside this plan's declared file list (Rule 4 boundary: deferred, not attempted).
- `run_item` checks `ctx.registry.template()` before resolving the agent name so "runtime not initialised" is reported deterministically rather than racing against "unknown agent" when both conditions are true (only matters in test/misconfiguration scenarios, since production always sets a template before the tool is ever registered).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] `run_turn` did not set `stop_reason` on mid-stream cancellation**
- **Found during:** Task 1, writing `cancelling_the_turn_token_stops_a_hanging_subagent`
- **Issue:** The `tokio::select!` race between the cancel token and the provider's `stream_turn` (added in an earlier phase) returned `Ok(final_text)` on cancel without setting `self.stop_reason`, unlike the pre-iteration cancel check a few lines above it, which did set `Some(StopReason::Cancelled)`. The new subagent dispatcher maps `(Ok(text), stop_reason)` to its reported status, so this silently turned a cancelled subagent into a reported `"ok"`.
- **Fix:** Set `self.stop_reason = Some(StopReason::Cancelled)` immediately before the existing `return Ok(final_text)` on that path.
- **Files modified:** `src/agent/loop_.rs`
- **Commit:** `bdc47f3`

**2. [Rule 1 - Bug] `run_item` reported "unknown agent" instead of "runtime not initialised" when both were true**
- **Found during:** Task 1, writing `no_template_is_an_in_band_error_not_a_panic`
- **Issue:** The initial implementation checked `ctx.registry.template()` only inside `run_subagent`, which runs after agent-name resolution in `run_item` — so a test (or misconfigured caller) with neither an agent registered nor a template set got "Unknown agent" instead of the plan's required `"subagent runtime not initialised"` message.
- **Fix:** Moved the template-missing check to the top of `run_item`, before `resolve_agent` is called.
- **Files modified:** `src/tool/subagent.rs`
- **Commit:** `bdc47f3`

**3. [Rule 2 - Missing critical functionality / Rule 1 - Bug, found during Task 2's audit] Two panic sites in provider adapters reachable from every subagent spawn**
- **Found during:** Task 2
- **Issue:** `AnthropicProvider::new`/`OpenAiProvider::new` panicked via `.expect("build reqwest client")` on a `ClientBuilder::build()` failure, and `AnthropicProvider`'s request builder panicked via `.expect("checked is_array above")` after a match guard that already proved the invariant. Both are on the subagent execution path because every dispatched subagent constructs its own `Provider` via `provider_factory`, and the plan's T-01-12 threat (provider/loop panics under `panic = "abort"`) explicitly calls these out as mitigation targets for Task 2.
- **Fix:** `.expect("build reqwest client")` → `.unwrap_or_else(|_| reqwest::Client::new())` in both files; the array-mut `.expect(...)` → a `match` that falls through to the same "start a fresh user turn" behavior the `_` arm already used.
- **Files modified:** `src/provider/anthropic.rs`, `src/provider/openai.rs`
- **Commit:** `08365bb`

None of these changed observable behavior beyond converting a would-be panic/misreport into the plan-specified in-band result — no new functionality, no architectural changes.

## Issues Encountered
None blocking. All deviations were caught by the plan's own acceptance tests/criteria before any commit was made.

## User Setup Required
None.

## Requirements Completed

- **RT-01** — Subagents run as in-process tasks; `run_single`/`spawn_and_collect` and all child-process machinery are removed with no fallback.
- **RT-02** — Each subagent gets its own cancel token (a child of the parent turn's token for foreground dispatches, or the registry's background token otherwise); `cancelling_the_turn_token_stops_a_hanging_subagent` proves a foreground subagent stops within 2s of the parent's turn token being cancelled, with `live_count` returning to 0.
- **RT-04** — Each subagent writes its own transcript to `.nanopi/agents/<run>/<id>/transcript.jsonl`; the parent's own session/event stream never sees the sub-agent's turn events (the sub-agent's event channel is drained locally and dropped).
- **RT-08** — A subagent provider error, limit, cancellation, or `JoinHandle` failure is always reported as an in-band failed/limit_reached/cancelled `ToolOutput`, never a process panic; the Task 2 audit closed the two remaining reachable panic sites.

RT-05/RT-06/RT-07 remain `[x]` from 01-01/01-03 (no change needed — this plan consumes those interfaces, doesn't newly satisfy them). RT-03 remains `[x]` from before this plan (stop-all shortcut is TUI-side, 01-05's job).

## Next Phase Readiness
- `cargo test --lib -- --test-threads=1`: 830 passed / 0 failed / 1 ignored (+4 over 01-03's 826 — the old `subagent.rs` test module had 12 tests, the rewritten one has 16).
- `cargo test --lib --features wasm -- --test-threads=1`: 951 passed / 0 failed / 1 ignored.
- `cargo build --release`: green.
- `src/tool/subagent.rs` now has a real, tested, in-process dispatcher that 01-05 (TUI/print wiring: Ctrl+X stop-all, inline permission prompt) and 01-06 (end-to-end success-criteria tests) can build on directly — `SubagentRegistry::permissions()`, `agents_dir()`, `snapshot()` and the per-agent `AgentState` transitions this plan drives (`Queued`→`Running`→`Done`/`Failed`/`Cancelled`/`LimitReached`) are all real and observable now, not placeholders.
- No blockers for 01-05/01-06.

---
*Phase: 01-in-process-runtime*
*Completed: 2026-10-03*

## Self-Check: PASSED
`01-04-SUMMARY.md` exists; both task commits (`bdc47f3`, `08365bb`) verified present in git log; `src/tool/subagent.rs`, `src/agent/loop_.rs`, `src/provider/anthropic.rs`, `src/provider/openai.rs` all exist with the expected content; `cargo test --lib -- --test-threads=1` (830 passed), `cargo test --lib --features wasm -- --test-threads=1` (951 passed), and `cargo build --release` all green as of this summary.

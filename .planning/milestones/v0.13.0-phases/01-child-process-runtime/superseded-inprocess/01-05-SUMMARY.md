---
phase: 01-in-process-runtime
plan: 05
subsystem: tui-print-wiring
tags: [tokio, subagent, keybindings, permission-broker]

requires:
  - "SubagentRegistry, PermissionBroker, SpawnTemplate from 01-01 (src/agent/subagent_registry.rs)"
  - "Agent.subagents field from 01-03 (src/agent/loop_.rs)"
  - "run_subagent() dispatcher consuming ctx.registry.template()/permissions() from 01-04 (src/tool/subagent.rs)"
provides:
  - "ActionId::StopAllSubagents with default ctrl+x binding (D-06), persisted via settings_toml action_toml_key"
  - "print mode: one SubagentRegistry per process, SpawnTemplate inheriting provider/model/base_url/api_key (D-04), broker left in Deny mode (D-14)"
  - "TUI: App.subagents (process-wide registry, interactive broker from startup) + App.pending_permission; install_subagent_runtime() reinstalls the template and reassigns agent.subagents at every rebuild site"
  - "TUI Ctrl+X stop_all() (works mid-turn) and an inline FIFO y/n/Esc permission prompt (D-13)"
affects: [01-06]

tech-stack:
  added: []
  patterns:
    - "install_subagent_runtime(app, &mut agent) as the single reinstall point called after every build_fresh/hydrate_resumed/model-swap site, so the SpawnTemplate's provider_factory always closes over the agent's *current* base_url/api_key/model rather than the ones captured at process startup"
    - "Permission prompt polled once per 120ms tick from PermissionBroker::front() rather than awaited on its internal Notify — avoids adding a new select! arm and a lost-wakeup race, at the cost of up to one tick of latency showing the prompt"

key-files:
  modified:
    - src/keys.rs
    - src/settings_toml.rs
    - src/mode/print.rs
    - src/mode/tui.rs

key-decisions:
  - "ActionId::StopAllSubagents defaults to ctrl+x (D-06) — confirmed unbound by grepping tui.rs for existing ctrl+x/Char('x') handling before claiming the chord"
  - "Permission prompt is polled from the 120ms ticker rather than a new select! arm racing PermissionBroker's internal tokio::sync::Notify — simpler, no new cross-task wakeup path to get wrong, and the existing ticker already drives every other quick-action pickup (summarize_task, command_task, note!/host-notify drains)"
  - "install_subagent_runtime() is a free fn taking &App (not &mut App) — it only reads app.api_kind/cfg_provider/inline_think_tags/subagents, so passing the whole App by shared reference keeps call sites simple (no borrow conflict with the Agent being mutated alongside it)"
  - "TUI's broker is set to Interactive immediately after the registry is built, before the first Agent is constructed — the main agent's own hook ask outcomes (01-03) and any subagent's permission ask share the exact same FIFO queue and prompt rendering path, so there is no separate code path for '[main]' vs '[a3]'"

requirements-completed: [RT-02, RT-03]

duration: ~55min
completed: 2026-10-03
---

# Phase 1 Plan 05: TUI and Print-Mode Subagent Wiring Summary

**Both front-ends now install a real `SubagentRegistry`: print mode gets a Deny-mode broker and a provider-inheriting `SpawnTemplate` (`-p` cannot ask, D-14), and the TUI gets an Interactive broker, a Ctrl+X stop-all shortcut that works mid-turn, and an inline FIFO `[a3] wants to run: … (y/n)` permission prompt answered through the same queue the main agent's own hook `ask` outcome uses**

## Performance

- **Duration:** ~55 min
- **Tasks:** 2
- **Files modified:** 4 (`src/keys.rs`, `src/settings_toml.rs`, `src/mode/print.rs`, `src/mode/tui.rs`)

## Accomplishments

- `ActionId::StopAllSubagents` added to `keys.rs` with label "Stop all subagents", included in `all()`, and defaulted to **ctrl+x** (D-06) — confirmed free by grepping `tui.rs` for any existing `Char('x')`/`CONTROL` handling before claiming it. `settings_toml::action_toml_key` maps it to `stop_all_subagents` for persistence; the enum's `#[serde(rename_all = "snake_case")]` already gives the same name on the load/deserialize side for free.
- Print mode (`src/mode/print.rs`): builds one `Arc<SubagentRegistry>` from `cfg_for_build.subagent` for the process, installs a `SpawnTemplate` whose `provider_factory` wraps `crate::provider::build` with the parent's captured `api_kind`/`base_url`/`api_key`/`cfg_provider`/`inline_think_tags` (D-04), and assigns `agent.subagents = subagent_registry` for both the fresh-build and resumed-session branches. The broker is left in its default `Deny` mode — `-p` never calls `set_interactive()` — so a queued subagent permission request resolves to `false` immediately instead of hanging (D-14).
- TUI (`src/mode/tui.rs`): `App` gained two fields — `subagents: Arc<SubagentRegistry>` (built from `[subagent]` config right after the initial Agent build, switched to `Interactive` immediately, and copied onto `app.subagents` right after `App::new`) and `pending_permission: Option<PermissionRequest>`. A new free function `install_subagent_runtime(app: &App, agent: &mut Agent)` rebuilds the `SpawnTemplate` from the *agent's current* cwd/model/base_url/api_key/hooks/permission/tool_exec fields and reassigns `agent.subagents = app.subagents.clone()`; it is called at every agent-construction/rebuild site: the initial build (inline, before `App` exists), `/new`, `/resume`, `/import`, `/fork` (`execute_fork`), and `/model` swap (`SwapModel`) — six sites total, so a subagent spawned after any of those actions always inherits the session's *current* model/provider, not the one from process startup.
- Ctrl+X dispatches `KeyAction::StopAllSubagents`, checked in `interpret_key` ahead of every modal/picker branch (so it fires even while a picker or the summary modal is open) and ahead of the normal palette/thinking/settings dispatch chain; `handle_action` calls `app.subagents.stop_all()` and prints `[stopped N subagents]` using the live count taken just before cancelling — works mid-turn, the same way `ToolCancel`/Esc does, without touching the main turn's own cancel token.
- Interim permission prompt (D-13): the 120ms ticker polls `app.subagents.permissions().front()` once per tick when `pending_permission` is `None`, and on a hit sets `app.status_note` to `"[<agent_id>] wants to run: <summary>  (y/n)"` and stashes the request. `interpret_key` checks `pending_permission` as its very first branch — `y`/`Y` → `answer_front(true)`, `n`/`N`/`Esc` → `answer_front(false)` (Esc here does **not** also cancel the running turn — `CancelTurn`/`ToolCancel` is a separate binding checked later), both clearing `status_note`; any other key is swallowed and the prompt put back, so the input box cannot be typed into while the prompt is up. Because the main agent's own hook `ask` outcome (01-03) and every subagent's permission request share one `PermissionBroker`, there is never a second competing prompt — both arrive through the identical front()/answer_front() path, just with different `agent_id` labels (`"main"` vs `"a3"`).

## Task Commits

1. **Task 1: StopAllSubagents key + print-mode wiring** - `90f60dd` (feat)
2. Deviation fix folded in immediately after - `aa0b846` (fix)
3. **Task 2: TUI registry wiring, Ctrl+X handler, inline FIFO permission prompt** - `8892006` (feat)

## Files Created/Modified

- `src/keys.rs` — `ActionId::StopAllSubagents`, default ctrl+x binding, `default_bindings_stop_all_subagents_ctrl_x` test
- `src/settings_toml.rs` — `action_toml_key` arm for the new action
- `src/mode/print.rs` — `SubagentRegistry` + `SpawnTemplate` installation for both the fresh and resumed-session branches, Deny broker left as-is
- `src/mode/tui.rs` — `App.subagents`/`App.pending_permission` fields, `install_subagent_runtime()` helper, six call sites wiring it into every agent-rebuild path, `KeyAction::StopAllSubagents` + its `interpret_key`/`handle_action` arms, the tick-loop permission-prompt poll, and 3 new tests (`ctrl_x_yields_stop_all_subagents_action`, `pending_permission_prompt_intercepts_y_and_answers_front`, `pending_permission_prompt_ignores_unrelated_keys`)

## Decisions Made

- Permission prompt polled from the existing 120ms ticker rather than adding a new `select!` arm racing `PermissionBroker`'s internal `Notify` — the ticker already owns every other "pick up a finished background thing" responsibility in this loop (`summarize_task`, `command_task`, `note!`/`host-notify` drains), and polling `front()` (which is idempotent until popped) has no lost-wakeup failure mode a `Notify`-based arm would need to guard against.
- `install_subagent_runtime` takes `&App` (immutable) rather than `&mut App` — it only reads four App fields, so call sites that also need `app: &mut App` for other reasons (inserting scrollback lines, updating `app.model`) don't hit a double-mutable-borrow.
- TUI's broker goes `Interactive` immediately at startup, before any session exists — simpler than switching modes later, and matches the plan's framing that the main agent's `ask` and a subagent's `ask` are the same queue from day one.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] print-mode `provider_factory` passed an empty api_key**
- **Found during:** Task 1, immediately after writing the closure
- **Issue:** The first draft of print mode's `SpawnTemplate.provider_factory` closure passed `""` for `api_key` instead of the parent's real key — every `-p` subagent dispatch would have called the provider unauthenticated.
- **Fix:** Captured `api_key.to_string()` into the closure alongside `base_url`/`api_kind`/`cfg_provider`.
- **Files modified:** `src/mode/print.rs`
- **Commit:** `aa0b846`

None of these changed observable behavior beyond fixing a bug caught before any downstream test ran against it — no new functionality beyond what the plan specified.

## Issues Encountered

None blocking. `interpret_key` already had a deep stack of modal-priority `if` branches (capture_key_for → keybindings_menu → settings_menu → ExpandLastTool → summary_prompt → resume_picker → fork_picker → …); the new permission-prompt and Ctrl+X checks were placed as the first two branches, ahead of all of those, since neither should ever be blocked by a currently-open picker.

## User Setup Required

None.

## Requirements Completed

- **RT-02** — "Ctrl+X cancels every running subagent; Esc on the main turn cascades to foreground subagents" is now reachable from both the main turn (unchanged Esc → turn-token cancel, which 01-04 already made a parent of foreground subagent tokens) and explicitly via Ctrl+X → `stop_all()`, exercised end-to-end through the TUI for the first time in this plan.
- **RT-03** — the stop-all shortcut (previously only the registry-level `stop_all()` method from 01-01) is now bound to a real key, configurable through the existing keybindings system, with a passing default-binding test.

RT-07 (hook `ask` → permission queue) remains satisfied by 01-03's implementation; this plan is the first to give it a UI (the inline prompt) and a `-p`-mode resolution (Deny), rather than newly implementing the routing itself.

## Next Phase Readiness

- `cargo test --lib -- --test-threads=1`: 834 passed / 0 failed / 1 ignored (+3 over 01-04's 831 after 01-04 itself added 4 — the delta here is the 1 new `keys::` test plus the 3 new `mode::tui::` tests covering Ctrl+X dispatch and the permission-prompt intercept/pass-through).
- `cargo build --features wasm`: green.
- Both front-ends now exercise the full D-01..D-14 subagent runtime end-to-end: `SubagentRegistry`, `SpawnTemplate`, `PermissionBroker` (Deny in `-p`, Interactive in the TUI), and `stop_all()` are all reachable from real user input, not just from `tool/subagent.rs`'s internal dispatch logic.
- No blockers for 01-06 (phase-level end-to-end success-criteria tests).

---
*Phase: 01-in-process-runtime*
*Completed: 2026-10-03*

## Self-Check: PASSED
All modified files and all three task/fix commits (`90f60dd`, `aa0b846`, `8892006`) verified present.

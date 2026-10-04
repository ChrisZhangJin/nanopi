# Phase 6: Orchestrator Mode - Validation Architecture

**Companion to:** `06-RESEARCH.md`
**Gate:** Nyquist validation (required — no `workflow.nyquist_validation: false` in `.planning/config.json`)

## Test Framework

| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` (sync) / `#[tokio::test]` (async), `cargo test` |
| Config file | none — standard Cargo test harness; no `pytest.ini`/`jest.config` equivalent in this repo |
| Quick run command | `cargo test --lib <module_path>` e.g. `cargo test --lib tool::mod::tests::orchestrator` |
| Full suite command | `cargo test --lib && cargo test --test agent_spawn --test agent_archive --test agent_runtime` |
| Baseline (pre-phase, verified this session) | `cargo test --lib` → **1025 passed, 1 ignored, 0 failed** at HEAD |
| Binary size baseline (QA-02) | `target/release/nanopi` = **4,881,744 bytes** (release profile: `opt-level="z"`, `lto=true`, `strip=true`) — measure post-phase with the same `cargo build --release` invocation and diff |

## Phase Requirements → Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|---------------------|-------------|
| ORC-01 | `/orchestrator` and `/orchestrator on\|off` toggle `App.orchestrator`; `experimental.orchestrator` config key seeds the default; off by default | unit | `cargo test --lib mode::tui::tests::orchestrator_slash_toggles_flag -x` | ❌ Wave 0 |
| ORC-01 | `experimental.orchestrator` defaults to `false` when absent from config.toml | unit | `cargo test --lib config::tests::experimental_orchestrator_defaults_false -x` | ❌ Wave 0 |
| ORC-02 | `ToolRegistry::orchestrator()` returns exactly `{read, grep, find, agent, list_agents, stop_agent, send_message}` — no `write`/`edit`/`bash`/`ls` | unit | `cargo test --lib tool::mod::tests::orchestrator_registry_excludes_write_edit_bash -x` | ❌ Wave 0 |
| ORC-02 | Toggling orchestrator on mid-session rebuilds `agent.registry` AND `agent.context.tools` (both, not just one — Pitfall 1) | integration | `cargo test --lib mode::tui::tests::orchestrator_toggle_updates_context_tools -x` | ❌ Wave 0 |
| ORC-03 | `system_prompt::build_orchestrator()` output mentions analyse/split/dispatch/monitor/synthesise (D-08 workflow) | unit | `cargo test --lib agent::system_prompt::tests::orchestrator_prompt_covers_workflow_steps -x` | ❌ Wave 0 |
| ORC-03 | Orchestrator prompt tells the model it has no write/edit/bash and must delegate (D-09 brief-writing guidance reflected) | unit | `cargo test --lib agent::system_prompt::tests::orchestrator_prompt_states_tool_restriction -x` | ❌ Wave 0 |
| ORC-04 | With mode off, `compose_system_prompt(..., orchestrator=false)` is byte-identical to the pre-phase baseline string | regression (snapshot via `assert_eq!` against a captured constant) | `cargo test --lib agent::build::tests::default_prompt_byte_identical_to_v0_12_baseline -x` | ❌ Wave 0 |
| ORC-04 | With mode off, `ToolRegistry::standard_with_control().all_specs()` (serialized to a canonical JSON string) is byte-identical to the pre-phase baseline | regression | `cargo test --lib tool::mod::tests::standard_with_control_specs_byte_identical_to_baseline -x` | ❌ Wave 0 |
| ORC-05 | Status line renders `⎈ orchestrator` segment iff `app.orchestrator == true` | unit (pure render helper, ratatui `Buffer` assertion — same style as existing `draw_dock` tests in `mode::tui::tests`) | `cargo test --lib mode::tui::tests::status_line_shows_orchestrator_segment_when_on -x` | ❌ Wave 0 |
| ORC-05 | Status line omits the segment when off (no stray `·` separator either) | unit | `cargo test --lib mode::tui::tests::status_line_omits_orchestrator_segment_when_off -x` | ❌ Wave 0 |
| D-01 | Print mode (`-p`) ignores `experimental.orchestrator = true` and still builds the full `standard()`/`standard_with_control()` registry | integration (real binary, mirrors existing `tests/agent_spawn.rs` style) | `cargo test --test agent_spawn print_mode_ignores_orchestrator_config -x` | ❌ Wave 0 |
| D-01 | Print mode emits a one-line stderr warning when `experimental.orchestrator = true` | integration | `cargo test --test agent_spawn print_mode_warns_when_orchestrator_config_set -x` | ❌ Wave 0 |
| D-02 | Toggling mid-session does not affect already-running (background) agents | integration | `cargo test --lib agent_registry::tests::orchestrator_toggle_does_not_touch_running_agents -x` (reuses existing `AgentRegistry` test fixtures) | ❌ Wave 0 |
| QA-01 | Manual E2E plan has rows for amend, stop, expand, toggle, clean | manual-only (justification: terminal rendering + live-model behavior, same category as existing `docs/v0.12-manual-test-plan.md` rows) | N/A — manual checklist execution | ❌ Wave 0 (new doc section) |
| QA-02 | Release binary grows ≤ ~150 KB; no new crates added without justification | manual/CI measurement | `cargo build --release && ls -la target/release/nanopi` (diff against 4,881,744 bytes) + `git diff Cargo.toml` reviewed for new `[dependencies]` entries | ❌ Wave 0 (no existing automated size-diff script) |

## Sampling Rate

- **Per task commit:** `cargo test --lib <touched module path>` — e.g. after editing `src/tool/mod.rs`, run `cargo test --lib tool::mod::tests`; after `src/mode/tui.rs`, run `cargo test --lib mode::tui::tests`. Each should complete in well under 30s given the existing suite's 10.02s full-run time.
- **Per wave merge:** `cargo test --lib` (full unit suite) + the three integration test binaries (`agent_spawn`, `agent_archive`, `agent_runtime`).
- **Phase gate:** Full suite green (`cargo test --lib` + all `--test` binaries) AND the QA-02 binary-size diff recorded AND the manual E2E plan rows (QA-01) executed by a human before `/gsd:verify-work`.

## Wave 0 Gaps

- [ ] `src/tool/mod.rs` — add `orchestrator()` constructor + `#[cfg(test)] mod tests` additions: `orchestrator_registry_excludes_write_edit_bash`, `standard_with_control_specs_byte_identical_to_baseline` (capture the baseline JSON string as a `const` BEFORE implementing any change, per Pitfall 2 in 06-RESEARCH.md)
- [ ] `src/agent/system_prompt.rs` — add `build_orchestrator()` + its two tests
- [ ] `src/agent/build.rs` — thread `orchestrator: bool` through `AgentBuildInputs`/`compose_system_prompt`; capture the pre-phase baseline prompt string as a `const` fixture BEFORE any implementation change, for the `default_prompt_byte_identical_to_v0_12_baseline` test
- [ ] `src/config.rs` — add `ExperimentalConfig` + `experimental_orchestrator_defaults_false` test
- [ ] `src/mode/tui.rs` — `SlashCmd::Orchestrator`, `KeyAction::SetOrchestrator(Option<bool>)` (bare `/orchestrator` toggles; `/orchestrator on|off` sets explicitly), `App.orchestrator: bool` field, the four registry-rebuild call sites' new branch, the status-line segment, and their respective tests listed above
- [ ] `src/mode/print.rs` — the ignore + warning logic and its test (likely lands in `tests/agent_spawn.rs` as a real-binary integration test rather than a `print.rs` unit test, matching existing style for print-mode behavior)
- [ ] `docs/v0.13-manual-test-plan.md` — new file (per D-10/QA-01), following `docs/v0.12-manual-test-plan.md`'s format, with rows for: amend, stop, stop-all (Ctrl+X), expand (Ctrl+G), approve/deny/always, `/orchestrator` toggle, `/agents clean`, auto-prune, worktree merge, merge conflict
- [ ] No test-framework install needed — `cargo test` is already fully configured; this phase adds test modules/functions only, not infrastructure

*(Every listed test is net-new; Phase 6 has not started implementation, so 100% of its requirement coverage is a Wave 0 gap by definition. This is expected for a not-yet-planned phase and is not itself a red flag — it is the input the planner uses to schedule Wave 0 test-writing before/alongside the first implementation tasks.)*

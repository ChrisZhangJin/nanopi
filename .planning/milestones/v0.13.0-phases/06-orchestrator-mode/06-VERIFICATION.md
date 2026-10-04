---
phase: 06-orchestrator-mode
verified: 2026-10-04T00:00:00Z
status: human_needed
score: 8/8 must-haves verified (automated); 1 live-behavior item requires human execution
overrides_applied: 0
human_verification:
  - test: "Give the orchestrator (mode on) a genuinely multi-part, independent-subtask task and observe it split the work, dispatch multiple agents, monitor their reports, and present one synthesised summary (ROADMAP SC#3)."
    expected: "Orchestrator states/confirms a plan per D-05, dispatches to the minimum necessary number of agents per D-06, and the final reply to the user is a single combined summary covering what was done, changed files, open issues, and archive path (D-08)."
    why_human: "Requires a live LLM call reasoning over a real task and producing a qualitatively correct split/dispatch/synthesis; not mechanically checkable by grep or unit test. This is explicitly scheduled as a manual row in docs/v0.13-manual-test-plan.md chapter 3 (QA-01) and left unchecked pending human execution."
---

# Phase 6: Orchestrator Mode Verification Report

**Phase Goal:** An experimental, opt-in TUI mode in which the main agent only analyses, splits the work, delegates it, monitors the agents, and synthesises their results. Built entirely from Phases 3-5. When the mode is off, behaviour is unchanged. Closes QA-01 and QA-02. Covers ORC-01..05 and QA-01..02.

**Verified:** 2026-10-04
**Status:** human_needed
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | `ToolRegistry::orchestrator()` registers exactly `agent, find, grep, list_agents, read, send_message, stop_agent` — write/edit/bash/ls absent | VERIFIED | `src/tool/mod.rs:615-625` hand-registers the 7 tools via `Arc::new(...)` constructors (no derivation/filtering from `standard()`); test `orchestrator_registry_excludes_write_edit_bash` (line ~1309) asserts the exact sorted name list, passes. |
| 2 | With orchestrator off, `compose_system_prompt`/tool specs are byte-identical to pre-phase baseline (ORC-04) | VERIFIED | `src/agent/build.rs` holds `BASELINE_DEFAULT_PROMPT` const (captured pre-change) and `default_prompt_byte_identical_to_v0_12_baseline` asserts `compose_system_prompt(..)` output (cwd placeholder-substituted) equals it exactly, byte for byte; `compose_system_prompt` is a one-line delegation to `compose_system_prompt_mode(.., false)` (build.rs:768-775) so every existing caller is structurally untouched. `src/tool/mod.rs` has the analogous `standard_with_control_specs_byte_identical_to_baseline` test. Both pass. |
| 3 | `build_orchestrator()` prompt covers understand→plan→dispatch→monitor→verify→report, plan-confirmation, fewest-agents, worktree isolation, self-contained briefs, states write/edit/bash unavailable | VERIFIED | `src/agent/system_prompt.rs:130` `pub fn build_orchestrator`; tests `orchestrator_prompt_covers_workflow_steps` and `orchestrator_prompt_states_tool_restriction` assert the required phrases/keywords are present; both pass in the full suite run. |
| 4 | `experimental.orchestrator` config key parses and defaults to `false` | VERIFIED | `src/config.rs:178` `ExperimentalConfig { orchestrator: bool }`, wired into `Config.experimental`, `builtin_defaults()`, and `merge()`; test `experimental_orchestrator_defaults_false` (line ~860) confirms empty TOML → `false`, `[experimental]\norchestrator = true` → `true`. Passes. |
| 5 | `/orchestrator` (bare) toggles, `on`/`off` set explicitly, bad arg shows usage and changes nothing, state seeded from config at startup, toggle swaps registry+context.tools+prompt in place without touching `AgentRegistry`, status line shows `⎈ orchestrator` iff on | VERIFIED | `src/mode/tui.rs`: `SlashCmd::Orchestrator` (line 137), `KeyAction::SetOrchestrator(Option<bool>)` / `OrchestratorUsage(String)` (1258, 1261), `parse_orchestrator_args` dispatch (1970-1972), `apply_orchestrator_mode` helper (1999) mutating `agent.registry`+`agent.context.tools`+system base together, `App.saved_registry` stash/restore only on off→on edge, re-applied at all 4 rebuild sites (lines 2641, 2804, 3354, 4235), status-line segment at 5748 (`"⎈ orchestrator"`), and the full set of tests at 8090-8233 covering toggle parsing, idempotent double-on, and status-line presence/absence. All pass. |
| 6 | Print mode ignores the config key for registry/prompt construction and emits exactly one stderr note when set; no note when unset; agent children never print it | VERIFIED | `src/mode/print.rs:251-259`: the flag is read only inside an `eprintln!` guarded by `!child.agent_mode && cfg_for_build.experimental.orchestrator`; nothing downstream branches on it. `tests/agent_spawn.rs` real-binary tests `print_mode_warns_when_orchestrator_config_set` / `print_mode_ignores_orchestrator_config` (verifies `write` tool still works via an actual file-write side effect) both pass. |
| 7 | QA-01: consolidated manual E2E plan exists with required row groups, including absence-checks for superseded controls | VERIFIED | `docs/v0.13-manual-test-plan.md` exists (18,215 bytes), `grep -c "Ctrl+G"` = 5, `grep -c "/orchestrator"` = 8, `docs/agents.md:133` points to it; file contains 6 chapters including the Ctrl+X/approve-deny-always absence rows citing the 2026-10-03 context revision (manually confirmed by reading relevant plan/summary text — doc content itself is prose, not independently re-verified word-for-word here beyond the required greps and section headers). |
| 8 | QA-02: release binary size delta recorded and ≤ ~150 KB; new crates justified | VERIFIED | Independently rebuilt: `cargo build --release` → `target/release/nanopi` = 4,892,864 bytes, matching the SUMMARY's claimed figure exactly. Delta vs. the 4,881,744-byte baseline (recorded in 06-VALIDATION.md, captured this same session per that doc) = +11,120 bytes, well under the 150 KB budget. `docs/v0.13-manual-test-plan.md` appendix records the same numbers and the `unicode-width = "0.2"` dependency with justification. |
| 9 (SC#3) | Given a multi-part task, the orchestrator splits it, dispatches agents, and presents a synthesised summary | UNCERTAIN (human) | No automated test exercises a live LLM-driven split/dispatch/synthesis cycle — this is inherent to the capability (depends on model judgment, not mechanically checkable). Correctly scheduled as a manual checklist row in `docs/v0.13-manual-test-plan.md` chapter 3, left unchecked (`☐`) pending human execution, which is appropriate given this is genuine live-behavior and not a stub/gap. |

**Score:** 8/8 automated must-haves verified; 1 item requires human execution (not a failure — inherent human-verification need, consistent with QA-01's design).

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `src/tool/mod.rs` | `ToolRegistry::orchestrator()` + ORC-02/04 tests | VERIFIED | Hand-registered 7-tool constructor at line 615; baseline snapshot + exclusion tests present and passing. |
| `src/agent/system_prompt.rs` | `build_orchestrator()` | VERIFIED | Line 130, new sibling function, does not edit `build()`. |
| `src/agent/build.rs` | `compose_system_prompt_mode(.., orchestrator: bool)`; `compose_system_prompt` delegates with `false` | VERIFIED | Lines 768 (`compose_system_prompt`, 1-line delegation) and 786 (`compose_system_prompt_mode`). |
| `src/config.rs` | `ExperimentalConfig { orchestrator: bool }` | VERIFIED | Line 178, wired into `Config`, `builtin_defaults`, `merge`. |
| `src/mode/tui.rs` | `App.orchestrator`, `SlashCmd::Orchestrator`, `KeyAction::SetOrchestrator`, `apply_orchestrator_mode`, status segment, tests | VERIFIED | All present and exercised by tests; see truth #5 evidence. |
| `src/mode/print.rs` | unconditional one-line stderr note, registry/prompt unaffected | VERIFIED | Lines 251-259. |
| `tests/agent_spawn.rs` | `print_mode_ignores_orchestrator_config`, `print_mode_warns_when_orchestrator_config_set` | VERIFIED | Both present, both pass. |
| `docs/v0.13-manual-test-plan.md` | consolidated manual E2E plan (QA-01) | VERIFIED | Exists, correct format, required sections present (grep-verified). |
| `docs/agents.md` | Orchestrator mode section + pointer | VERIFIED | `## Orchestrator mode` section (line 137) documents toggle, config key, 7-tool set, print-mode note, byte-identical guarantee; pointer to manual test plan at line 133. |

### Key Link Verification

| From | To | Via | Status | Details |
|------|-----|-----|--------|---------|
| `compose_system_prompt` | `compose_system_prompt_mode(.., false)` | thin delegation | WIRED | build.rs:775, confirmed by `compose_mode_false_matches_compose` test passing. |
| `compose_system_prompt_mode` | `system_prompt::build_orchestrator` | `None if orchestrator` match arm | WIRED | Confirmed by `compose_mode_true_uses_orchestrator_prompt` test (output starts with `build_orchestrator` text; custom override still wins). |
| `KeyAction::SetOrchestrator` handler | `ToolRegistry::orchestrator` + `compose_system_prompt_mode` + `set_system_base` | `apply_orchestrator_mode` | WIRED | tui.rs:3067 calls the helper; helper mutates registry+context.tools+prompt together (tests assert all three). |
| `draw_dock` | `app.orchestrator` | status-line segment | WIRED | tui.rs:5748, tests at 8233 assert presence/absence. |
| print.rs startup | `config.experimental.orchestrator` | read-only, note emission | WIRED (correctly inert) | Flag read only inside the `eprintln!` guard; no downstream branch — confirmed by test proving `write` tool still functions with the flag set. |

### Data-Flow / Behavioral Spot-Checks

Not applicable in the traditional dynamic-data sense (no DB/API data source) — this phase's core "wiring" is registry/prompt swapping and was verified via the unit/integration tests above plus independent rebuild (binary size cross-check) rather than a separate spot-check pass.

### Requirements Coverage

| Requirement | Source Plan | Status | Evidence |
|---|---|---|---|
| ORC-01 | 06-01, 06-02, 06-03 | SATISFIED | Toggle, config default, print-mode exemption all verified above. |
| ORC-02 | 06-01, 06-02 | SATISFIED | Restricted registry (static + live toggle) verified. |
| ORC-03 | 06-01 | SATISFIED | `build_orchestrator` prompt content verified via tests. |
| ORC-04 | 06-01, 06-03 | SATISFIED | Byte-identical snapshots for prompt and tool specs, both passing; independently re-ran full suite. |
| ORC-05 | 06-02 | SATISFIED | Status-line segment verified. |
| QA-01 | 06-04 | SATISFIED (doc exists; execution pending) | Manual test plan doc created and grep-verified; actual human execution of the checklist is the remaining step, tracked as the human-verification item above (SC#3 row lives inside this same document). |
| QA-02 | 06-04 | SATISFIED | Independently re-measured binary size matches SUMMARY's claim exactly; within budget. |

No orphaned requirements found — `.planning/REQUIREMENTS.md` ORC-01..05 and QA-01..02 all map to plans claiming them.

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| `src/mode/tui.rs` | 5963 | `TODO: unify via a trait` | None (pre-existing, unrelated) | Confirmed via `git blame` this line dates to commit `7edb6700` (2026-08-07), months before phase 6; it is in `draw_menu`, unrelated to orchestrator code. Not a phase-6 debt marker. |

No TBD/FIXME/XXX/HACK/PLACEHOLDER markers found in any of the 7 files this phase modified (`src/tool/mod.rs`, `src/agent/system_prompt.rs`, `src/agent/build.rs`, `src/config.rs`, `src/mode/tui.rs`, `src/mode/print.rs`, `tests/agent_spawn.rs`) beyond the one pre-existing, unrelated TODO above.

### Independent Test Run

`cargo test -- --test-threads=1` executed fresh in this verification session (not trusting SUMMARY's reported numbers):

```
lib:                    1039 passed, 0 failed, 1 ignored
tests/agent_archive:       2 passed
tests/agent_runtime:       4 passed
tests/agent_spawn:         9 passed
tests/...(other int.):    13 passed
tests/print_mode_e2e:     33 passed
tests/skills_integration:  6 passed
wasm_plugin_integration:   0 passed (feature-gated off)
doctests:                  0 passed
```

Total: 1106 passed, 0 failed, 1 ignored — matches 06-04-SUMMARY.md's claimed totals exactly.

`cargo build --release` independently rebuilt: `target/release/nanopi` = 4,892,864 bytes — matches SUMMARY's claimed figure exactly.

### Human Verification Required

#### 1. Live multi-part orchestration (ROADMAP SC#3)

**Test:** With orchestrator mode on, give the agent a task with at least two genuinely independent subtasks (e.g., "investigate module A's test coverage and separately check module B's error handling") and observe its behavior.
**Expected:** It states/confirms a brief plan (D-05), dispatches to the minimum number of agents needed for the independent work (D-06), monitors their reports, and replies with one combined summary covering what was done, changed files, open issues, and the archive path (D-08). Per D-07, if multiple parallel code-writing agents are used, they run in `isolation: "worktree"`.
**Why human:** Requires live LLM reasoning and judgment over a real task; cannot be reduced to a grep or unit assertion. This is correctly captured as an unchecked row in `docs/v0.13-manual-test-plan.md` chapter 3 rather than silently skipped.

### Gaps Summary

No code-level gaps found. All 8 automated must-haves (ORC-02..05, ORC-01, QA-02, and the structural/doc artifacts for QA-01) are verified against the actual codebase — not just SUMMARY claims — via direct source reading, an independently re-run full test suite (1106 passed, matching the SUMMARY's reported count), and an independently rebuilt release binary (byte count matches SUMMARY's claim exactly). The only open item is the live-behavior manual check for SC#3 (multi-part task splitting/dispatch/synthesis), which is inherently a human-verification task and is already correctly scheduled as an unchecked checklist row rather than omitted. Status is `human_needed`, not `gaps_found`.

---

*Verified: 2026-10-04*
*Verifier: Claude (gsd-verifier)*

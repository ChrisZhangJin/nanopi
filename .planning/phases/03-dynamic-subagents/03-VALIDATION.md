---
phase: 3
slug: dynamic-subagents
status: draft
nyquist_compliant: true
wave_0_complete: false
created: 2026-10-04
---

# Phase 3 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.
> Extracted from 03-RESEARCH.md "Validation Architecture".

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` / `#[tokio::test]` via `cargo test` |
| Config file | none (plain `Cargo.toml`, no custom test harness) |
| Quick run command | `cargo test --lib tool::agent::` |
| Full suite command | `cargo test --lib` (917 passed, 1 ignored at time of writing; `cargo test --lib --features wasm` for the wasm-gated suite) |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| DYN-01 | `{task}` only dispatches a general-purpose agent and returns a result | unit (pure resolution) + integration (`sh` fake binary, following `brief_and_dir_exist_before_spawn`'s pattern) | `cargo test --lib tool::agent::tests::` | ❌ Wave 0 — new test, e.g. `no_agent_name_uses_general_purpose_default` |
| DYN-02 | Inline `role`+`tools`+`model` dispatch runs with exactly those, validated against allowlist | unit (`validate_tools`/`apply_inline_overrides` pure fns) + integration (asserts child argv/brief reflect overrides, following `build_child_args_contract`'s pattern) | `cargo test --lib tool::agent::tests::` | ❌ Wave 0 — new tests for `apply_inline_overrides`, `validate_tools` rejection message shape |
| DYN-03 | Existing predefined-agent + single/parallel/chain modes unchanged | regression — **run the existing suite unmodified** | `cargo test --lib tool::agent::` | ✅ Existing (`resolve_agent`, `run_item`, `run_parallel`, `run_chain`, `select_mode`, `parse_items` tests already present, lines 1188–1911 of `src/tool/agent.rs`) |
| DYN-04 | Per-agent model selection, validated and resolvable | unit (`validate_model` pure fn, incl. unknown-id rejection and the A1 same-vendor boundary once resolved) | `cargo test --lib tool::agent::tests::` or `cargo test --lib models::` | ❌ Wave 0 — depends on resolving Open Question 1 first |
| DYN-05 | Parent receives capped summary, never full transcript | regression + one updated assertion (cap value, pointer message) | `cargo test --lib tool::agent::tests::` | ✅ Existing cap mechanism (`REPORT_CAP`); ❌ Wave 0 — update/add a test asserting the new ~8 KB cap and the `report.md` path in the truncation message |

### Sampling Rate
- **Per task commit:** `cargo test --lib tool::agent::`
- **Per wave merge:** `cargo test --lib` (full suite; add `--features wasm` once per phase if any plugin-adjacent code is touched — it is not expected to be, since this phase stays inside `src/tool/agent.rs` and `src/agent/agents.rs`)
- **Phase gate:** Full suite green (`cargo test --lib`, currently 917 passed / 1 ignored baseline) before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] No test file gaps — `src/tool/agent.rs`'s existing `#[cfg(test)] mod tests` block is the right home for every new test; no new test file needed.
- [ ] New tests needed (see table above): `no_agent_name_uses_general_purpose_default`, `apply_inline_overrides_appends_role_for_builtin_overrides_for_named`, `validate_tools_rejects_unknown_and_agent_itself`, `validate_model_rejects_unknown_id`, `report_cap_is_8kb_with_path_pointer` (name an existing `REPORT_CAP`-asserting test if one exists to update it, else add a new one).
- [ ] Framework install: none — no new dependency.


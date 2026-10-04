---
phase: 1
slug: child-process-runtime
status: draft
nyquist_compliant: true
wave_0_complete: false
created: 2026-10-03
---

# Phase 1 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution. Derived from 01-RESEARCH.md "Validation Architecture".

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | cargo test (built-in) + `#[tokio::test]`; e2e via fake OpenAI endpoint in `tests/print_mode_e2e.rs` |
| **Config file** | none (Cargo.toml) |
| **Quick run command** | `cargo test --lib` filtered to the task's module (see map) |
| **Full suite command** | `cargo test` |
| **Phase gate** | `cargo test && cargo test --release --test subagent_runtime && cargo clippy -- -D warnings` |
| **Estimated runtime** | quick ~20 s, full ~120 s |

---

## Sampling Rate

- **After every task commit:** the task's `<automated>` command (quick, module-filtered)
- **After every plan wave:** `cargo test`
- **Wave 1:** `cargo test --lib tool:: agent:: subagent_registry config::`
- **Wave 2 (01-04):** `cargo test` (adds print_mode_e2e session/limit/allowlist tests)
- **Wave 3 (01-06):** `cargo test` (adds brief e2e)
- **Wave 4 (01-05):** `cargo test` (adds supervisor unit + real spawn integration)
- **Wave 5 (01-07):** phase gate (incl. release build, panic=abort)
- **Before `/gsd:verify-work`:** full suite + phase gate green
- **Max feedback latency:** 60 s per task

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Threat Ref | Secure Behavior | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|------------|-----------------|-----------|-------------------|-------------|--------|
| 1-01-01 | 01 | 1 | ISO-03 | T-01-01 | changed file detected by re-hash | unit | `cargo test --lib tool::file_state` | ❌ W0 | ⬜ pending |
| 1-01-02 | 01 | 1 | ISO-03 | T-01-01, T-01-02 | stale edit/write refused; symlink refusal kept | unit | `cargo test --lib tool::` | ✅ (extend) | ⬜ pending |
| 1-02-01 | 02 | 1 | RT-06 | T-01-04 | loop stops at max_turns / token_budget | unit | `cargo test --lib agent::loop_` | ✅ (extend) | ⬜ pending |
| 1-02-02 | 02 | 1 | RT-09, RT-07 | T-01-05 | amendment parse; brief 0o600; agent_id only when set | unit | `cargo test --lib agent::` | ❌ W0 (brief.rs) | ⬜ pending |
| 1-03-01 | 03 | 1 | RT-05, RT-06 | — | config defaults | unit | `cargo test --lib config::` | ✅ (extend) | ⬜ pending |
| 1-03-02 | 03 | 1 | RT-02, RT-05 | T-01-06 | max_live cap, semaphore, killpg on drop | unit | `cargo test --lib subagent_registry` | ❌ W0 | ⬜ pending |
| 1-04-01 | 04 | 2 | RT-05, RT-07, RT-02 | T-01-08, T-01-10 | subagent stripped in agent mode; PDEATHSIG | unit | `cargo test --bin nanopi args && cargo test --lib tool::` | ✅ (extend) | ⬜ pending |
| 1-04-02 | 04 | 2 | RT-04, RT-06, RT-07 | T-01-08 | own transcript; limit_reached; no prompt | e2e | `cargo test --test print_mode_e2e` | ✅ (extend) | ⬜ pending |
| 1-05-01 | 05 | 4 | RT-01, RT-02, RT-03, RT-08 | T-01-11, T-01-14 | all child faults in-band; group killed on cancel | unit | `cargo test --lib tool::subagent` | ✅ (extend) | ⬜ pending |
| 1-05-02 | 05 | 4 | RT-01, RT-05, RT-07 | T-01-12, T-01-13 | key not in argv; agent dir + brief before spawn; real child spawn | unit + integration | `cargo test --lib tool::subagent && cargo test --test subagent_spawn` | ❌ W0 | ⬜ pending |
| 1-06-01 | 06 | 3 | RT-09 | T-01-15 | torn-read guard | unit | `cargo test --lib mode::brief_watch` | ❌ W0 | ⬜ pending |
| 1-06-02 | 06 | 3 | RT-09 | T-01-16 | self-check ≤2 turns; report.md always | e2e | `cargo test --test print_mode_e2e brief_` | ✅ (extend) | ⬜ pending |
| 1-07-01 | 07 | 5 | RT-02, RT-03 | T-01-17 | kill_all on exit; no keybindings | build + grep | `cargo build && ! grep -in subagent src/keys.rs` | n/a | ⬜ pending |
| 1-07-02 | 07 | 5 | all | T-01-17 | 6 roadmap criteria, debug + release | e2e | `cargo test --test subagent_runtime && cargo test --release --test subagent_runtime` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

Each is created by the first task that needs it (test written before implementation, `tdd="true"`):

- [ ] `src/tool/file_state.rs` test module — ISO-03 (01-01 T1)
- [ ] `src/agent/brief.rs` test module — RT-09 parser (01-02 T2)
- [ ] `src/subagent_registry.rs` test module — RT-02/RT-05 (01-03 T2)
- [ ] fake-endpoint helper extension in `tests/print_mode_e2e.rs`: scripted multi-response, usage numbers, per-response delay (01-04 T2, 01-06 T2)
- [ ] injectable child program seam in `src/tool/subagent.rs` for failure-mode tests (01-05 T1)
- [ ] `tests/subagent_spawn.rs` — real child spawn with the 01-04 flag contract (01-05 T2)
- [ ] `src/mode/brief_watch.rs` test module (01-06 T1)
- [ ] `tests/subagent_runtime.rs` — process-group / PDEATHSIG / success-criteria e2e (01-07 T2)

No framework install needed.

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------|
| Grandchild bash survives a hard SIGKILL of nanopi | RT-02 | Accepted known gap, documented only | Not tested; see docs/subagents.md "Known gaps" |

All other phase behaviors have automated verification.

---

## Validation Sign-Off

- [x] All tasks have `<automated>` verify or Wave 0 dependencies
- [x] Sampling continuity: no 3 consecutive tasks without automated verify
- [x] Wave 0 covers all MISSING references
- [x] No watch-mode flags
- [x] Feedback latency < 60s per task
- [x] `nyquist_compliant: true` set in frontmatter

**Approval:** pending

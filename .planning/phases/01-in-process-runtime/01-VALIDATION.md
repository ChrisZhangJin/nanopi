---
phase: 1
slug: in-process-runtime
status: draft
nyquist_compliant: true
wave_0_complete: false
created: 2026-10-03
---

# Phase 1 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in harness (`cargo test`), `#[tokio::test]` for async |
| **Config file** | none — standard cargo; suite runs with `--test-threads=1` (shared `TempNanopiHome`/`test_lock()`) |
| **Quick run command** | `cargo test --lib <module>:: -- --test-threads=1` |
| **Full suite command** | `cargo test -- --test-threads=1 && cargo test --features wasm -- --test-threads=1` |
| **Estimated runtime** | ~60–120 seconds (full); <15s (module) |

---

## Sampling Rate

- **After every task commit:** Run the task's `<automated>` module test
- **After every plan wave:** Run full suite command (both feature configs)
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** ~15 seconds (module-scoped)

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Threat Ref | Secure Behavior | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|------------|-----------------|-----------|-------------------|-------------|--------|
| 1-01-01 | 01 | 1 | RT-03, RT-05, RT-06, RT-07 | — | max_live cap + semaphore bound concurrency | unit | `cargo test --lib subagent_registry:: -- --test-threads=1 && cargo test --lib config:: -- --test-threads=1` | ❌ W0 | ⬜ pending |
| 1-01-02 | 01 | 1 | ISO-03 | — | FileStateTracker records content hashes | unit | `cargo build && cargo test --lib file_state:: -- --test-threads=1 && cargo test --lib tool:: -- --test-threads=1` | ❌ W0 | ⬜ pending |
| 1-02-01 | 02 | 2 | ISO-03 | — | stale write refused in-band | unit | `cargo test --lib stale -- --test-threads=1` | ❌ W0 | ⬜ pending |
| 1-02-02 | 02 | 2 | ISO-03 | — | concurrent two-agent write serialized | integration | `cargo test --lib tool::write::concurrent -- --test-threads=1` | ❌ W0 | ⬜ pending |
| 1-03-01 | 03 | 2 | RT-05, RT-07 | — | deny-list; hook `ask` routes to queue | unit | `cargo test --lib agent::hook:: -- --test-threads=1 && cargo test --lib tool::tests::subagent_registry_denies -- --test-threads=1` | ❌ W0 | ⬜ pending |
| 1-03-02 | 03 | 2 | RT-02, RT-06 | — | limits → LimitReached | unit | `cargo test --lib agent::loop_:: -- --test-threads=1` | ✅ | ⬜ pending |
| 1-04-01 | 04 | 3 | RT-01, RT-02, RT-04, RT-06, RT-08 | — | in-process dispatch, isolated transcript, no crash | integration | `cargo test --lib tool::subagent:: -- --test-threads=1` | ✅ | ⬜ pending |
| 1-04-02 | 04 | 3 | RT-01, RT-08 | — | child-process code removed; no unwrap in path | build+suite | `cargo test --lib -- --test-threads=1 && cargo build --release` | ✅ | ⬜ pending |
| 1-05-01 | 05 | 3 | RT-03 | — | Ctrl+X stop-all bound | unit | `cargo test --lib keys:: -- --test-threads=1 && cargo build` | ✅ | ⬜ pending |
| 1-05-02 | 05 | 3 | RT-02, RT-07 | — | TUI FIFO prompt; -p denies | unit | `cargo build && cargo test --lib mode::tui:: -- --test-threads=1` | ✅ | ⬜ pending |
| 1-06-01 | 06 | 4 | all | — | e2e per success criterion | integration | `cargo test --lib subagent_e2e_tests -- --test-threads=1` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `src/agent/subagent_registry.rs` inline `#[cfg(test)] mod tests` (created by 01-01)
- [ ] `src/tool/file_state.rs` inline tests (created by 01-01)
- [ ] Reuse fake `Provider` from `src/agent/loop_.rs` test module for limit/error paths
- [ ] `SubagentRegistry::live_count()` test introspection helper

No framework install needed — `cargo test` already configured. Test files are created inline by the TDD tasks that own them.

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Esc stops foreground subagent in live TUI | RT-02 | real terminal key handling | Plan 01-06 checkpoint: dispatch subagent, press Esc |
| Ctrl+X stops all subagents | RT-03 | real terminal key handling | Plan 01-06 checkpoint |
| Inline `[aN] wants to run: …` prompt | RT-07 | interactive TUI | Plan 01-06 checkpoint with hook returning `ask` |

---

## Validation Sign-Off

- [x] All tasks have `<automated>` verify or Wave 0 dependencies
- [x] Sampling continuity: no 3 consecutive tasks without automated verify
- [x] Wave 0 covers all MISSING references
- [x] No watch-mode flags
- [ ] Feedback latency < 15s
- [x] `nyquist_compliant: true` set in frontmatter

**Approval:** pending

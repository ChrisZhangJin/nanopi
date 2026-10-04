---
phase: 04-background-control
verified: 2026-10-04T00:00:00Z
status: human_needed
score: 5/5 success criteria verified
overrides_applied: 0
re_verification:
  previous_status: gaps_found
  previous_score: 4/5
  gaps_closed:
    - "CTL-06: The model can continue a finished agent with a new message, including an agent dispatched by an earlier nanopi process in the same run."
  gaps_remaining: []
  regressions: []
human_verification:
  - test: "Launch a background agent from the TUI, amend it mid-run via send_message, then observe the brief.md amendment timing (it should take effect at the agent's next turn boundary, never mid tool call)."
    expected: "Amendment text appears in brief.md as a new ## Amendment N section before the agent's next turn begins; no observable mid-tool-call interruption."
    why_human: "Turn-boundary timing is a runtime behavior that unit/e2e tests approximate with sleeps/mocks; a human running the real TUI against a live model call can confirm there is no visible glitch or race."
  - test: "Dispatch a code-writing agent with isolation: \"worktree\" against a real repo with uncommitted changes in the main tree, let it finish and attempt an auto-merge, and inspect the resulting report text for the conflict case."
    expected: "On conflict, the report shown to the user clearly states the worktree/branch were kept and gives enough information to resolve manually (per D-11); on success, the worktree and branch are gone and the main tree has the merged changes."
    why_human: "The textual quality/clarity of the conflict report surfaced to an end user is a UX judgment, not purely mechanical; also exercises the real git merge path end-to-end interactively."
---

# Phase 4: Background launch & control Verification Report

**Phase Goal:** The model can launch agents in the background and amend, stop, list and continue them, with reports delivered back automatically.
**Verified:** 2026-10-04
**Status:** human_needed
**Re-verification:** Yes — after gap closure (04-06: CTL-06 cross-process continue)

## Goal Achievement

### Observable Truths (ROADMAP Success Criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | The model launches a background agent, gets its id immediately and keeps working; `list_agents` shows its status. | VERIFIED | Unchanged since initial verification; regression check: `background_dispatch_does_not_block_caller` still passes. |
| 2 | Amending a running agent appends to its brief and takes effect at its next turn boundary (never mid tool call); stopping it yields a partial report. | VERIFIED (mechanism); turn-boundary timing flagged for human check | Unchanged since initial verification; `SendMessageTool`'s non-terminal branch (append_amendment only, no continue) is untouched by 04-06. |
| 3 | A finished background report starts a new main turn when idle, or is queued as a follow-up while streaming; a finished agent can be continued by a new `-p` on its session. | VERIFIED | Report injection unchanged. Continue is now VERIFIED for both same-process (`continue_finished_agent_same_id`) and cross-process (`continue_finished_agent_from_earlier_process` in tests/agent_spawn.rs; `continue_agent_from_earlier_print_process` in tests/print_mode_e2e.rs, two real `nanopi -p` processes joined via `NANOPI_RUN_ID`). Gap closed: `SendMessageTool::execute` (src/tool/agent_ctl.rs ~line 261-274) now falls back to `AgentRegistry::adopt_from_disk` on an in-memory snapshot miss before returning "no such agent", then feeds the reconstructed terminal `AgentEntry` into the unchanged reactivate/prepare_continue/spawn_continue_background path. |
| 4 | In `-p` mode, nanopi waits for (or stops) background agents before exiting — no orphans. | VERIFIED | Unchanged; drain-loop tests (`print_mode_waits_for_background_agent_before_exit`, `print_mode_ctrl_c_stops_all_background_agents`, `print_mode_zero_background_agents_behaves_as_before`) still pass; `continue_agent_from_earlier_print_process` additionally exercises the drain loop waiting for a cross-process continue before exit. |
| 5 | A writer dispatched with worktree isolation reports its worktree path and branch; unchanged worktrees are removed, changed ones kept and listed. | VERIFIED (single-mode); minor gap noted (unchanged) | Unchanged since initial verification; worktree tests still pass (`worktree_isolation_creates_branch_and_path_and_removes_when_unchanged`, `worktree_merge_no_conflict_auto_merges_and_cleans_up_background`, etc.). parallel/chain isolation schema note from 04-05 remains, not a regression, not blocking SC #5. |

**Score:** 5/5 truths fully verified. CTL-06 gap from the prior verification is closed.

### Required Artifacts

| Artifact | Expected | Status | Details |
|---|---|---|---|
| `src/agent_registry.rs` | track_background/stop/stop_all/reactivate/push_report/take_reports/wait_background/has_background, plus `with_run_id`/`run_id_from_env_value`/`adopt_from_disk`/`seed_counter_from_disk`/`valid_agent_id`/`AgentState::from_disk_str` | VERIFIED | All new symbols present (`grep -n` confirms); ~10 new unit tests cover reconstruction, non-terminal/interrupted rejection, missing/malformed id rejection, path-traversal-shaped id rejection, idempotency, counter seeding (both via adopt and via bare reserve on a joined run dir), env-value validation, and report.md state fallback. |
| `src/tool/agent.rs` | spawn_background, prepare_run/run_body split, isolation wiring, CONTROL_TOOLS denylist, plus `.env_remove("NANOPI_RUN_ID")` on both prepare_run and prepare_continue child commands | VERIFIED | `env_remove("NANOPI_RUN_ID")` present at both call sites (lines ~1339, ~1533); unit tests assert `get_envs()` shows `("NANOPI_RUN_ID", None)` for both. |
| `src/tool/agent_ctl.rs` | ListAgentsTool, StopAgentTool, SendMessageTool, now with disk-adoption fallback | VERIFIED — CTL-06 gap closed | Snapshot-miss path now calls `reg.adopt_from_disk(...)` before giving up; doc comment above `SendMessageTool` updated to describe the resolved behavior instead of the prior documented gap. |
| `src/archive.rs` | `is_run_id_shaped` made `pub`, reused to validate `NANOPI_RUN_ID` | VERIFIED | Confirmed via code read; used in `run_id_from_env_value`. |
| `src/worktree.rs` | detect/create/finish/report_line | VERIFIED | Unchanged, 9 unit tests still pass. |
| `src/mode/tui.rs` | follow-up injection of agent reports | VERIFIED | Unchanged. |
| `src/mode/print.rs` | drain loop + Ctrl-C stop_all | VERIFIED | Unchanged. |
| `tests/agent_spawn.rs` | `continue_finished_agent_from_earlier_process` + 3 negative-case tests | VERIFIED | All 4 new tests present and passing: `continue_finished_agent_from_earlier_process`, `send_message_from_earlier_process_refuses_non_continuable_states`, `send_message_from_earlier_process_unknown_id_is_in_band_error`, `send_message_never_adopts_across_different_run_ids`. |
| `tests/print_mode_e2e.rs` | `continue_agent_from_earlier_print_process` — two real `nanopi -p` processes sharing `NANOPI_RUN_ID` | VERIFIED | Test present (line ~1561) and passing; genuinely spawns two real `nanopi -p` child processes against two separate SSE fixture servers, joined via a fixed `NANOPI_RUN_ID`. |
| `.planning/REQUIREMENTS.md` | CTL-06 flipped to `[x]` / Complete | VERIFIED | `- [x] **CTL-06**` and `| CTL-06 | Phase 4 | Complete |` both confirmed present. |

### Key Link Verification

| From | To | Via | Status |
|---|---|---|---|
| `agent` tool `background: true` | `spawn_background` | tool::agent::execute dispatch | WIRED (unchanged) |
| `send_message` (finished, same-process) | `spawn_continue_background` | `prepare_continue`/reactivate | WIRED (unchanged) |
| `send_message` (finished, cross-process, snapshot miss) | `AgentRegistry::adopt_from_disk` | `reg.adopt_from_disk(&project_agents_dir(&ctx.cwd), &id)` on snapshot miss | WIRED — gap closed |
| `AgentRegistry::adopt_from_disk` | `AgentRegistry::reactivate` -> `prepare_continue` -> `spawn_continue_background` | adopted entry is terminal, flows unchanged into the existing continue branch | WIRED |
| `src/main.rs` `AgentRegistry::new` | `NANOPI_RUN_ID` env | `run_id_from_env_value(std::env::var("NANOPI_RUN_ID")...)` validated against `archive::is_run_id_shaped` before use | WIRED |
| `prepare_run`/`prepare_continue` child `Command` | NANOPI_RUN_ID isolation | `.env_remove("NANOPI_RUN_ID")` at both sites, unit-tested | WIRED |
| `AgentRegistry::push_report` | TUI follow-up queue | `pick_follow_up_with_registry` | WIRED (unchanged) |
| `-p` exit | `wait_background`/`stop_all` | print.rs drain loop | WIRED (unchanged); now also exercised across a real cross-process continue in `continue_agent_from_earlier_print_process` |
| `prepare_run` (isolation: worktree) | `worktree::create`/`finish` | run_body + StateGuard::drop | WIRED (unchanged) |

### Data-Flow Trace (adopt_from_disk path)

| Artifact | Data Source | Produces Real Data | Status |
|---|---|---|---|
| `adopt_from_disk` reconstructed `AgentEntry` | Reads `brief.md` front matter (`id`, `state`), falls back to `report.md` front matter for `state` if absent from brief | Yes — reads real on-disk files written by the earlier process's `StateGuard::drop`/`finish()`, not static/stubbed values; id must match requested id (no spoofing by directory name alone) | FLOWING |
| `reserve()` id-collision avoidance after adopt/join | `seed_counter_from_disk` scans the run dir's actual subdirectory names via `valid_agent_id` | Yes — real filesystem scan, not a cached/static guess | FLOWING |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|---|---|---|---|
| Cross-process continue, in-process harness | `cargo test --test agent_spawn continue_finished_agent_from_earlier_process -- --test-threads=1` | 1 passed | PASS |
| Cross-process continue, two real `nanopi -p` binaries | `cargo test --test print_mode_e2e continue_agent_from_earlier_print_process -- --test-threads=1` | 1 passed | PASS |
| Non-continuable / unknown-id / cross-run-id negative cases | `cargo test --test agent_spawn -- --test-threads=1` (includes the 3 negative tests) | 11 passed, 0 failed | PASS |
| NANOPI_RUN_ID stripped from children | `cargo test --lib tool::agent -- --test-threads=1` (includes env_remove assertions) | part of 988 lib tests, all passed | PASS |
| Full workspace suite | `cargo test -- --test-threads=1` | 988 lib + 2 + 4 + 9 + 11 + 33 + 6 = 1053 tests passed, 1 pre-existing ignored, 0 failed | PASS |

### Requirements Coverage

| Requirement | Source Plan | Status | Evidence |
|---|---|---|---|
| CTL-01 | 04-01 | SATISFIED | background dispatch, immediate id return (unchanged) |
| CTL-02 | 04-03 | SATISFIED | send_message amend path (unchanged) |
| CTL-03 | 04-03 | SATISFIED | stop_agent / stop_all (unchanged) |
| CTL-04 | 04-03 | SATISFIED | list_agents (unchanged; now also surfaces adopted entries, consistent with D-04) |
| CTL-05 | 04-01/04-04 | SATISFIED | report outbox + follow-up injection (unchanged) |
| CTL-06 | 04-03, closed by 04-06 | SATISFIED | Same-process continue (unchanged) + cross-process continue via `NANOPI_RUN_ID` join + `adopt_from_disk`, covered by unit, integration, and real two-process e2e tests. REQUIREMENTS.md checkbox flipped to `[x]` / Complete. |
| CTL-07 | 04-04 | SATISFIED | print-mode drain + Ctrl-C (unchanged) |
| ISO-01 | 04-02/04-05 | SATISFIED | worktree creation, dispatch wiring (unchanged) |
| ISO-02 | 04-02/04-05 | SATISFIED | unchanged-removal / changed-kept-and-listed (unchanged) |

No orphaned requirements found for this phase.

### Anti-Patterns Found

None blocking. No TODO/FIXME/XXX/TBD/placeholder markers found in the files modified by 04-06 (src/archive.rs, src/agent_registry.rs, src/tool/agent.rs, src/tool/agent_ctl.rs, src/main.rs). The one documented design self-correction (seed_counter_from_disk reuse to avoid an id-collision near-miss) was caught and fixed during implementation per the SUMMARY's own Rule 1 disclosure, verified by dedicated tests (`reserve_after_adopt_never_reuses_on_disk_id`, `reserve_seeds_from_joined_run_dir_without_any_adopt`), both passing.

One documented, non-regression limitation (disclosed in 04-06-SUMMARY.md, not hidden): the in-process `tests/agent_spawn.rs` cross-process test cannot inject a custom `ChildProgram` into `SendMessageTool`'s launcher (it resolves via `current_exe()`, which is the test binary), so literal second-answer content verification is deferred to the genuine two-real-process `print_mode_e2e.rs` test, which does perform that verification. This mirrors a pre-existing limitation already present in `continue_finished_agent_same_id` and is not a new gap.

### Test Suite

`cargo test -- --test-threads=1` (full workspace, independently re-run by this verifier): **988 lib tests + 2 + 4 + 9 + 11 + 33 + 6 integration tests across all binaries = 1053 tests passed, 1 pre-existing ignored test, 0 failures.** This matches the 04-06-SUMMARY.md claim (1053 tests) and is independently reproduced here.

### Human Verification Required

See frontmatter `human_verification`. These two items carry over unchanged from the initial verification (turn-boundary amendment timing; worktree-conflict report UX) — neither is affected by the 04-06 gap-closure work, and neither was resolved by it. Per the status decision rule, their presence sets `status: human_needed` even though all truths and the CTL-06 gap are now fully verified.

### Gaps Summary

No gaps remain. The CTL-06 cross-process continue gap identified in the initial verification is closed: `AgentRegistry::adopt_from_disk` reconstructs a terminal `AgentEntry` from on-disk `brief.md`/`report.md` front matter when a `send_message` target is absent from the in-memory registry snapshot, gated by strict id-shape validation (path-traversal defense), a terminal-and-not-Interrupted check (prevents double-running a live agent), and same-run-id scoping. A second `nanopi` process can now reach this situation by joining an existing run via the validated `NANOPI_RUN_ID` environment variable; children never inherit it (`env_remove`). `reserve()` is disk-aware so newly dispatched agents in a joined run never collide with on-disk ids. All of this is covered by new unit tests (src/agent_registry.rs, src/tool/agent.rs), an in-process integration test and three negative-case tests (tests/agent_spawn.rs), and — critically — a genuine two-real-process `nanopi -p` end-to-end test (tests/print_mode_e2e.rs) that proves the mechanism works across actual process boundaries, not just in a mocked harness. REQUIREMENTS.md CTL-06 is flipped to `[x]`/Complete. No regressions found in CTL-01..05, CTL-07, or ISO-01/02; the full test suite (1053 tests) is green.

Remaining items are both pre-existing human-verification needs unrelated to CTL-06 (turn-boundary timing UX, worktree-conflict report UX), carried forward from the initial verification.

---

*Verified: 2026-10-04*
*Verifier: Claude (gsd-verifier)*
</content>

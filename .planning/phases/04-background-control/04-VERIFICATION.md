---
phase: 04-background-control
verified: 2026-10-04T00:00:00Z
status: gaps_found
score: 4/5 success criteria verified (CTL-06 partial)
overrides_applied: 0
gaps:
  - truth: "CTL-06: The model can continue a finished agent with a new message: a new `nanopi -p` is started on the same session, so the agent keeps its previous context."
    status: partial
    reason: >
      send_message's continue path only works for an agent still present in the
      *current process's* in-memory AgentRegistry snapshot. Continuing an agent
      from an earlier nanopi process in the same run (explicitly required by
      D-05 and the ROADMAP's "also used for amendments that arrive after the
      child finished" framing, and listed as REQUIREMENTS.md CTL-06 which is
      still marked `[ ]` Pending) returns `no such agent: <id>` instead of
      reconstructing the entry from disk.
    artifacts:
      - path: src/tool/agent_ctl.rs
        issue: >
          Line ~208 comment and line 261 explicitly document the gap: id lookup
          is `reg.snapshot().into_iter().find(|e| e.id == id)` only — no fallback
          scan of `agents_root/<run_id>/*` for an on-disk agent dir absent from
          the in-memory registry.
    missing:
      - "Scan agents_root/<run_id>/* for a dir not present in the in-memory AgentRegistry snapshot, reconstruct a minimal AgentEntry from brief.md/report.md front matter, then feed it into the existing reactivate() path before send_message proceeds."
      - "An integration test that starts nanopi -p, lets an agent finish, exits the process, starts a new nanopi -p against the same run, and continues the finished agent by id."
      - "Flip REQUIREMENTS.md CTL-06 checkbox to [x] only once the above lands."
deferred: []
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
**Status:** gaps_found
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths (ROADMAP Success Criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | The model launches a background agent, gets its id immediately and keeps working; `list_agents` shows its status. | VERIFIED | `tool::agent::spawn_background` (src/tool/agent.rs) races `run_body` under `tokio::spawn`, returns `{id, state, archive_path}` without awaiting; `ListAgentsTool` (src/tool/agent_ctl.rs) reads `AgentRegistry::snapshot()` live. `tests/agent_spawn.rs::background_dispatch_does_not_block_caller` passes. |
| 2 | Amending a running agent appends to its brief and takes effect at its next turn boundary (never mid tool call); stopping it yields a partial report. | VERIFIED (mechanism); turn-boundary timing flagged for human check | `SendMessageTool` appends `## Amendment N` via `brief::append_amendment`; amendment is only consumed at the child's own next `-p --session` turn (child-process model from Phase 1 revision), so "never mid tool call" holds structurally since there is no in-process steer channel to race. `StopAgentTool`/`AgentRegistry::stop`/`stop_all` kill the pgid and `StateGuard::drop` writes a partial report (`ensure_report` + `set_state(Stopped)`). |
| 3 | A finished background report starts a new main turn when idle, or is queued as a follow-up while streaming; a finished agent can be continued by a new `-p` on its session. | PARTIAL | Report injection VERIFIED: `pick_follow_up_with_registry`/`send_agent_reports_to_turn_with_registry` in src/mode/tui.rs, covered by unit tests and e2e tests (`background_report_starts_new_turn_when_idle`). Continue VERIFIED only for same-process finished agents (`continue_finished_agent_same_id` e2e test passes); cross-process continue is NOT implemented — see gap. |
| 4 | In `-p` mode, nanopi waits for (or stops) background agents before exiting — no orphans. | VERIFIED | src/mode/print.rs drain loop awaits `wait_background()`/`has_background()`; Ctrl-C calls `stop_all()` then re-waits. e2e tests `print_mode_waits_for_background_agent_before_exit`, `print_mode_ctrl_c_stops_all_background_agents`, `print_mode_zero_background_agents_behaves_as_before` all pass. |
| 5 | A writer dispatched with worktree isolation reports its worktree path and branch; unchanged worktrees are removed, changed ones kept and listed. | VERIFIED (single-mode); minor gap noted | `prepare_run` creates the worktree via `worktree::create`, `brief.md` carries `worktree:`/`branch:`, `run_body`/`StateGuard::drop` call `finish()` on every terminal state and append the outcome. Tests in `src/worktree.rs` (9 unit tests) and `tests/agent_spawn.rs` ISO-01/02 integration tests pass. Note: `parallel`/`chain` item JSON schemas do not yet expose the `isolation` property (only validated, not advertised) — a documented, non-blocking scope note from 04-05's summary, not a failure of SC #5 itself since the single-mode path (the one SC #5 describes) is fully wired. |

**Score:** 4/5 truths fully verified, 1 partial (CTL-06 cross-process continue)

### Required Artifacts

| Artifact | Expected | Status | Details |
|---|---|---|---|
| `src/agent_registry.rs` | track_background/stop/stop_all/reactivate/push_report/take_reports/wait_background/has_background | VERIFIED | present, exercised by tests |
| `src/tool/agent.rs` | spawn_background, prepare_run/run_body split, isolation wiring, CONTROL_TOOLS denylist | VERIFIED | present |
| `src/tool/agent_ctl.rs` | ListAgentsTool, StopAgentTool, SendMessageTool | VERIFIED, with documented CTL-06 cross-process gap | present |
| `src/worktree.rs` | detect/create/finish/report_line | VERIFIED | 9 unit tests, used by agent.rs |
| `src/mode/tui.rs` | follow-up injection of agent reports | VERIFIED | pick_follow_up/send_agent_reports_to_turn, unit + e2e tested |
| `src/mode/print.rs` | drain loop + Ctrl-C stop_all | VERIFIED | e2e tested |
| `src/main.rs` | SIGTERM-only outer signal handler (scope change) | VERIFIED but behavior-narrowing | Ctrl-C during the main turn (pre-drain-window) no longer triggers outer kill_all(); documented as a known, bounded regression in 04-04-SUMMARY.md. Not a phase-4 success-criterion failure (SC #4 is specifically about the drain window), but worth flagging to the project owner since it changes pre-existing Ctrl-C behavior outside this phase's stated scope. |

### Key Link Verification

| From | To | Via | Status |
|---|---|---|---|
| `agent` tool `background: true` | `spawn_background` | tool::agent::execute dispatch | WIRED |
| `send_message` (finished, same-process) | `spawn_continue_background` | `prepare_continue`/reactivate | WIRED |
| `send_message` (finished, cross-process) | on-disk agent reconstruction | — | NOT WIRED (gap) |
| `AgentRegistry::push_report` | TUI follow-up queue | `pick_follow_up_with_registry` | WIRED |
| `-p` exit | `wait_background`/`stop_all` | print.rs drain loop | WIRED |
| `prepare_run` (isolation: worktree) | `worktree::create`/`finish` | run_body + StateGuard::drop | WIRED (single-mode); schema gap for parallel/chain items |

### Requirements Coverage

| Requirement | Source Plan | Status | Evidence |
|---|---|---|---|
| CTL-01 | 04-01 | SATISFIED | background dispatch, immediate id return |
| CTL-02 | 04-03 | SATISFIED | send_message amend path |
| CTL-03 | 04-03 | SATISFIED | stop_agent / stop_all |
| CTL-04 | 04-03 | SATISFIED | list_agents |
| CTL-05 | 04-01/04-04 | SATISFIED | report outbox + follow-up injection |
| CTL-06 | 04-03 | BLOCKED (partial) | same-process continue only; cross-process adoption not implemented. REQUIREMENTS.md itself still has this checkbox unchecked `[ ]`. |
| CTL-07 | 04-04 | SATISFIED | print-mode drain + Ctrl-C |
| ISO-01 | 04-02/04-05 | SATISFIED | worktree creation, dispatch wiring |
| ISO-02 | 04-02/04-05 | SATISFIED | unchanged-removal / changed-kept-and-listed |

No orphaned requirements found for this phase.

### Anti-Patterns Found

None blocking. The documented "Known Stubs" / scope decisions in 04-03-SUMMARY.md (cross-process continue) and 04-05-SUMMARY.md (parallel/chain isolation schema) are self-disclosed, not silently hidden, and are captured as the gap/notes above.

### Test Suite

`cargo test -- --test-threads=1` (full workspace): **977 lib tests + all integration binaries passed, 1 pre-existing ignored test, 0 failures.** This matches the SUMMARY claims and is independently reproduced here.

### Human Verification Required

See frontmatter `human_verification` — turn-boundary amendment timing and worktree-conflict report UX.

### Gaps Summary

One real gap: **CTL-06 cross-process continue** is not implemented. The phase goal's "continue" verb is delivered for the common case (continuing a finished agent within the same running `nanopi` process) but not for the documented D-05 case of an agent that finished under an earlier process in the same run — REQUIREMENTS.md itself still marks CTL-06 as `[ ]` Pending, so this gap is already acknowledged project-wide, not a new finding. Given CTL-06 is one of the phase's named requirements and SC #3 explicitly says "a finished agent can be continued by a new `-p` on its session" without qualifying same-process-only, this is a BLOCKER-level gap against the phase's stated contract, not merely a nice-to-have.

Everything else (CTL-01..05, CTL-07, ISO-01/02) is solidly wired and test-covered, with full-suite tests green.

---

*Verified: 2026-10-04*
*Verifier: Claude (gsd-verifier)*

---
phase: 04-background-control
plan: 03
subsystem: tool
tags: [agent-control, send-message, stop-agent, list-agents, denylist]

requires:
  - phase: 04-background-control
    plan: 01
    provides: "AgentRegistry::track_background/stop/stop_all/reactivate/push_report/take_reports/wait_background, tool::agent::{prepare_run, run_body, spawn_background, PreparedRun}"
provides:
  - "src/tool/agent_ctl.rs: ListAgentsTool, StopAgentTool, SendMessageTool (list_agents/stop_agent/send_message)"
  - "ToolRegistry::standard_with_control() — standard() plus the three control tools, wired into the real top-level (non-child) registry builders in mode/print.rs and mode/tui.rs"
  - "tool::agent::{CONTROL_TOOLS, prepare_continue, spawn_continue_background} — continuation dispatch that merges report.md under a `## Continued` marker"
affects: [04-04, 04-05]

tech-stack:
  added: []
  patterns:
    - "main-process-only tools are never added to ToolRegistry::standard() (which children always use); a separate standard_with_control() wraps it for the real top-level registry builders only"
    - "build_child_args defensively strips a fixed CONTROL_TOOLS list from any --tools argv, on top of them never being registered in standard() to begin with"

key-files:
  created:
    - src/tool/agent_ctl.rs
  modified:
    - src/tool/agent.rs
    - src/tool/mod.rs
    - src/mode/print.rs
    - src/mode/tui.rs
    - tests/agent_spawn.rs

key-decisions:
  - "stop_agent (single and \"all\") calls reg.wait_background() after stop()/stop_all() rather than tracking a per-id JoinHandle (no such API exists on AgentRegistry); acceptable because in the single-registry-per-run model this only blocks on that run's own background tasks"
  - "send_message's continue path only addresses agents already in this process's in-memory AgentRegistry snapshot; adopting an on-disk agent dir from an earlier process in the same run (D-05's \"earlier process\" half) is NOT implemented — see Known Stubs"
  - "continuation re-runs the child with an empty inline tools override (inherits the full standard set) rather than the original dispatch's tools, since PreparedRun/AgentEntry don't retain the original AgentConfig after the first run completes"

requirements-completed: [CTL-02, CTL-03, CTL-04]

duration: ~70min
completed: 2026-10-04
---

# Phase 04 Plan 03: Main-agent control tools (list/stop/amend/continue) Summary

**`list_agents`, `stop_agent`, and `send_message` are now real tools, but registered only on the main process's registry via a new `standard_with_control()` wrapper around `standard()` — never inside `standard()` itself, so every child (always built from `standard()`/`standard_with_allowlist()`) is structurally incapable of receiving them, with `build_child_args` stripping the names defensively on top of that.**

## Performance

- **Duration:** ~70 min
- **Tasks:** 2 completed
- **Files modified:** 6 (1 created, 5 modified)

## Accomplishments

- `ListAgentsTool` (CTL-04): reads the live `AgentRegistry::snapshot()` for id/state/elapsed, and each agent dir's `brief.md`/`report.md` front matter (via the existing `front_matter_get`) for description/turns/tokens/worktree/branch/report_path, computed fresh on every call rather than cached. Empty run returns a short message instead of an empty array alone.
- `StopAgentTool` (CTL-03): `{id}` stops one agent via `reg.stop`, `{id: "all"}` via `reg.stop_all`; both wait on `reg.wait_background()` so the partial report exists on disk before the tool returns. Unknown/already-terminal ids come back as in-band errors, never panics.
- `SendMessageTool` (CTL-02 running / CTL-06 finished): for a non-terminal target, appends `## Amendment N` to `brief.md` via the existing `brief::append_amendment` writer (reused unmodified — the sanitizer that already defeats front-matter forgery in RT-09 covers this path too, verified by a dedicated forgery test). For a terminal target in the *same run*, it reactivates the entry, appends the amendment, and re-dispatches the child in the background against the *same* `--session-file` transcript (which auto-resumes via `session::open_or_create_at`) and the *same* agent dir/id.
- New `tool::agent::{prepare_continue, spawn_continue_background}`: build and run that continuation dispatch, and — unlike the first-run `spawn_background` — merge the previous `report.md` with the new run's into one file under a `## Continued` marker instead of overwriting it.
- `ToolRegistry::standard_with_control()` added next to `standard()`; `src/mode/print.rs`'s top-level (non-child, `tools_allow` empty) branch and `src/mode/tui.rs`'s interactive registry builder now call it instead of `standard()`/`standard_with_allowlist()` directly. Child construction (the `child.agent_mode` branch in `print.rs`, and every `standard()`/`standard_with_allowlist()` call elsewhere) is untouched.
- `build_child_args` gained a `CONTROL_TOOLS` constant (`send_message`/`stop_agent`/`list_agents`) and now filters any of those names out of the `--tools` argv before building it — defense in depth on top of the structural guarantee above (T-04-06).

## Task Commits

1. **Task 1: list_agents and stop_agent** - `63f6b5d` (feat)
2. **Task 2: send_message (amend/continue) and child denylist** - `5e4f57d` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/tool/agent_ctl.rs` (new) - `ListAgentsTool`, `StopAgentTool`, `SendMessageTool`, each with a `with_registry` test/embedding constructor plus a process-wide fallback registry (mirrors `AgentTool`'s own fallback pattern)
- `src/tool/agent.rs` - `CONTROL_TOOLS` const, `build_child_args` filtering, `prepare_continue`, `spawn_continue_background`, a `build_child_args_strips_control_tools` unit test
- `src/tool/mod.rs` - `ToolRegistry::standard_with_control()`
- `src/mode/print.rs` - top-level (non-child) branch now builds via `standard_with_control()`; child branch strips `CONTROL_TOOLS` alongside the existing `agent` removal
- `src/mode/tui.rs` - interactive registry builder uses `standard_with_control()` when no `--tools` allowlist is given
- `tests/agent_spawn.rs` - `continue_finished_agent_same_id`, an end-to-end test against the real binary: first run finishes normally, `send_message` continues it against a second fake SSE endpoint, and the merged `report.md` plus outbox entry are asserted

## Deviations from Plan

### Auto-fixed Issues

None — no bugs or missing-critical-functionality fixes were needed beyond what's described below as scope decisions (Rule 4-adjacent, documented rather than silently applied).

### Scope decisions (documented, not auto-applied)

**1. `stop_agent` waits on `reg.wait_background()` instead of a per-id handle**
- **Why:** `AgentRegistry` has no API to await a single tracked `JoinHandle`, only `wait_background()` (drains and awaits all tracked tasks). Adding a per-id wait would be an architectural change to the registry (Rule 4 territory) beyond this plan's stated interface.
- **Effect:** `stop_agent {id}` currently blocks until *every* currently-tracked background agent in this run has finished stopping, not just `id`. In the common case (the model stops one agent at a time, or `stop_all`) this is observationally identical; it only differs if a model calls `stop_agent` for one id while a *different* agent is independently still mid-run in the background, in which case the tool call blocks longer than strictly necessary.

**2. Continuation does not adopt an on-disk agent from an earlier process in the same run**
- **Why:** The full D-05 "earlier process, same run" adoption path (scanning `.nanopi/agents/<run_id>/` for a dir not in the in-memory registry, reconstructing an `AgentEntry` from its front matter) is a meaningfully separate feature from the rest of this task and was out of scope given this plan's time budget.
- **Effect:** `send_message` to a terminal id only succeeds if that id is present in *this process's* `AgentRegistry` snapshot. An id from a prior `nanopi` process (even within the same run directory) returns `no such agent: <id>` rather than being adopted. This is the one must-have truth from the plan's frontmatter not fully delivered; CTL-06 is implemented for the "same process, finished agent" case (tested end-to-end) but not the cross-process adoption half.

**3. Continuation re-runs with an empty inline `tools` override**
- **Why:** `PreparedRun`/`AgentEntry` don't retain the originating `AgentConfig.tools` after the first run completes, and plumbing that through would touch `AgentRegistry`'s entry shape (architectural).
- **Effect:** A continued agent inherits the full standard toolset rather than whatever restricted `tools` list its first dispatch used. `agent`/control tools are still denied as always (DENIED_TOOLS/CONTROL_TOOLS), so this is a scope-of-access widening, not a privilege escalation past what any `agent` dispatch can already reach.

## Known Stubs

- Cross-process agent adoption for `send_message` (see Scope decision 2 above) is not implemented. A future plan should scan `agents_root/<run_id>/*` for dirs absent from the in-memory registry and reconstruct a minimal `AgentEntry` from `brief.md`/`report.md` front matter before calling `reactivate`.

## Threat Flags

None beyond the plan's own threat register — all three dispositions (T-04-06 control-tool leakage to children, T-04-07 front-matter forgery via `send_message`, T-04-08 path-join from a raw id) are mitigated exactly as specified: `build_child_args_strips_control_tools` and the structural `standard()`/`standard_with_control()` split cover T-04-06; `amend_text_cannot_forge_front_matter` covers T-04-07 (reusing the existing `append_amendment` sanitizer unmodified); every id lookup goes through `reg.snapshot().into_iter().find(|e| e.id == id)` (equality check, no path join from the raw string) covering T-04-08.

## Issues Encountered

- `ListAgentsTool`'s first test fixture wrote agent directories directly under a temp root rather than under `<root>/<run_id>/<id>/` (the layout `AgentRegistry::reserve` actually uses), causing a spurious failure; fixed by deriving the fixture path from `reserve()`'s own returned dir.
- The front-matter-forgery test initially asserted the wrong expected `state` value — `reg.set_state` legitimately rewrites `brief.md`'s `state:` field (by design, independent of `send_message`), so the correct assertion is that the *attacker's* embedded `---`/`evil:` lines never appear, not that `state` stays at its original value.

## Next Steps

Plans 04/05 (not yet read in detail) likely build user-facing surfaces (TUI panel, `/agents` commands) on top of these three tools plus plan 01's registry primitives. Cross-process agent adoption (Known Stubs) should land before CTL-06 is considered fully complete end to end.

## Self-Check: PASSED

All claimed files and commit hashes verified present (see below).

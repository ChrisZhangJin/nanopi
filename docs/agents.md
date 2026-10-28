# Agents

The `agent` tool runs each agent as a separate `nanopi -p` child process.
Modes: single, parallel (up to 8 tasks), chain. A child that crashes, panics,
is killed or times out comes back to the model as an in-band `status: failed`
result; the parent keeps running.

There is no slash command, prompt or control action for agents in the TUI.
They are controlled only by the model through the tool. The TUI does show a
read-only agents strip while agents are running or queued — see below.

## Child command line

```
nanopi -p --output json \
  --brief <agent dir>/brief.md \
  --session-file <agent dir>/transcript.jsonl \
  --max-turns N --token-budget K \
  [--model M] [--base-url U] [--api-kind K] [--tools a,b] \
  --approve | --distrust
```

- The child inherits the parent's model, base url and api kind.
- Trust: an approved parent passes `--approve`; otherwise `--distrust`. A child never prompts.
- In agent mode the `agent` tool is removed, so children cannot recurse.
- stdin is null; the child runs in its own process group.

## Environment

| Variable | Meaning |
|----------|---------|
| `NANOPI_AGENT_ID` | agent id (`a1`, `a2`, ...); turns on agent mode |
| `NANOPI_PARENT_PID` | parent pid; the child exits at once if its parent differs |
| `OPENAI_API_KEY` | API key. Never passed in argv |

## Config

```toml
[agent]
max_live = 8          # tracked non-terminal children; beyond -> "agent limit reached"
max_concurrency = 4   # running at once; the rest queue
max_turns = 50        # per child
token_budget = 300000 # per child
timeout_secs = 1800   # per child wall clock
```

## Agent dir layout

```
.nanopi/agents/<run>/<id>/     (0700)
  brief.md         (0600) task brief, amendments appended here
  transcript.jsonl child session (the parent session holds only the tool call and result)
  report.md        (0600) final report with checklist
```

## Brief and amendments

`brief.md` holds the role and the task. To amend a running child, append:

```
## Amendment 1
<text>
```

The child polls the brief (500 ms, size must be stable over two polls) and
injects each new amendment as a steering message at its next turn, exactly
once. After the task the child runs up to 2 self-check turns that re-read the brief.

## Report

`report.md` always gets written. It contains a checklist with one `- [x]` or
`- [ ]` line per task item and amendment. The envelope's `report_path` points
to it, and its text (capped at 64 KiB) becomes the tool result.

## Stale writes (ISO-03)

Each process tracks the files it has read. If another process (for example a
sibling child) changes a file after it was read, `edit`/`write` refuse with
"file changed since you read it" and the model must re-read first.

## Shutdown

Every child is killed (whole process group, SIGKILL) when:
- the parent turn is cancelled or the tool call is dropped,
- the TUI quits,
- print mode finishes,
- print mode receives SIGINT (exit 130) or SIGTERM (exit 143),
- a child times out.

## Agents strip (TUI)

A read-only strip shows every live agent at a glance while any are running or
queued. It sits at the bottom of the dock, under the footer (Claude Code
style), and is hidden whenever the current run has no live agents:

```
● main  3 agents working · ctrl+g details
○ yearning-dump        read raw.json                                 12s
○ msg-framework-dev    bash cargo test                             1m05s
○ write release notes  queued                                         0s
```

The first row is the main flow (the orchestrator) with a count of the agents
it is waiting on. Each worker row shows the agent's name (its `description`
label, else `#id`), its live status — the latest tool call while running,
refreshed about once a second, or `queued`/`starting…` — and its elapsed time.
The row colour carries the state (cyan running, grey queued). Once an agent
reaches a terminal state (done, failed, stopped, limit reached, interrupted)
it disappears from the strip on the next refresh — its report is still
delivered to the conversation through the normal injection path; the strip
itself is not where you read results.

There is no "waiting for permission" state: children run with `--approve` or
`--distrust` decided at dispatch and never prompt, so the strip has nothing to
approve or deny (display-only, UI-03).

At most 3 worker rows are shown — running agents first, then queued. With more
than 3 live agents the last row folds the rest into `+K more (R running)`.

`Ctrl+G` prints a full detail block (activity history, turns/tokens,
worktree/branch, report path) for the live agents into scrollback; `Ctrl+G` or
`Esc` closes it.

On narrow terminals (under 40 columns) the status text drops first, then the
name is truncated; on short terminals (under ~15 rows) the strip shows only
its `● main` row (D-08).

The `Ctrl+G` binding (`ActionId::ToggleAgentsStrip`) is configurable through
the same keybindings/settings menu as every other action — see `src/keys.rs`.

### Manual test

See `docs/v0.13-manual-test-plan.md` for the consolidated manual E2E plan
(QA-01), which includes this strip's Ctrl+G rows alongside orchestrator
mode, worktree merge, and the dispatch/amend/stop/continue control flow.

## Orchestrator mode

The default main flow of the TUI (ORC-01..05): the main agent is an
orchestrator that never does any concrete work itself. It creates dedicated
agents via the `agent` tool, assigns the work to them, monitors them, and
synthesises their results — it does not read files, search, edit, write, or
run shell commands.

- **Toggle:** `/orchestrator` (bare = toggle, `/orchestrator on|off` sets it
  explicitly, anything else prints `Usage: /orchestrator [on|off]` without
  changing state). Toggling mid-session takes effect from the next turn and
  never touches already-running agents.
- **Config key:** `[experimental] orchestrator = true` (default) sets the
  startup value; set it to `false` to start in the classic single-agent mode.
  TUI only — print mode (`-p`) ignores it entirely, so a scripted run is never
  restricted.
- **Exact toolset:** `agent`, `list_agents`, `stop_agent`, `send_message` —
  four tools, hand-registered (never derived by filtering a broader set). No
  file or shell tool (`read`, `grep`, `find`, `ls`, `write`, `edit`, `bash`)
  is registered at all, not just discouraged (a test asserts the exact name
  list).
- **Off-mode guarantee:** with the mode off, the system prompt and tool
  specs sent to the provider are byte-identical to the pre-orchestrator
  (v0.12) behavior — pinned by a snapshot test.
- **Status line:** the badge marks the deviation, not the default. In
  orchestrator mode (the default) there is no badge; turning it off shows
  `⎈ solo` next to `think:`/`vendor:`.

## Known gaps

- If the parent nanopi is itself SIGKILLed (or crashes hard), children die via
  `PR_SET_PDEATHSIG` (SIGKILL), but bash grandchildren they started may be
  orphaned and keep running.
- `PDEATHSIG` is Linux-only. On other unix targets the child polls
  `getppid()` instead, which is slower to notice.

# Agents

The `agent` tool runs each agent as a separate `nanopi -p` child process.
Modes: single, parallel (up to 8 tasks), chain. A child that crashes, panics,
is killed or times out comes back to the model as an in-band `status: failed`
result; the parent keeps running.

In the TUI the model dispatches and steers agents through the tool, and you
watch and stop them with `/agents`: a picker over every agent of the run,
`/agents show` for one agent's detail card, `/agents list`, `/agents stop` and
`/agents clean` (see "Inspecting agents" and "Esc and stopping agents"). While
agents are running or queued the TUI also shows a read-only agents strip — see
below.

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

## Inspecting agents (`/agents`)

The strip drops an agent the moment it finishes. `/agents` does not: it covers
every agent of the current run, finished ones included, for as long as the
session lasts (the registry keeps them). All of it is read-only and works while
a turn is streaming.

- **`/agents`** opens a picker, the same widget as `/resume`: `Up`/`Down` move,
  typing filters (by name, id or state), `Enter` prints that agent's detail
  card into scrollback and closes the picker, `Esc` closes the picker and does
  nothing else (it never cancels the turn or touches an agent). One row per
  agent — running and queued first (by id), then finished ones, most recently
  finished first: state glyph, id, name (the agent's label, else its role, else
  the start of its task), and on the right the state with its elapsed time, or
  its total duration once finished. With no agents yet it prints
  `no agents in this session` and opens nothing. The rows are a snapshot taken
  when the picker opens; the card is read fresh when you choose.
- **`/agents show <id|name-prefix>`** (or just `/agents <id>`) prints the card
  without the picker. An id is `a3`; a name is a case-insensitive prefix of an
  agent's label, role or task and must match exactly one agent of the run,
  finished or not. An ambiguous name shows nothing and lists the candidate ids;
  an unknown id or name is a one-line message.
- **`/agents list`** is the plain-text listing: the live agents exactly as
  `Ctrl+G` prints them, then a `── finished ──` block with the run's finished
  agents, one line each (`✓ a2  name  done 1m05s`).

The card, for an agent that failed:

```
✗ a3  doc translator
  failed · took 1s · model gpt-4o · turns 1 · tokens 0
  tools  read, grep
task
  Translate the docs directory into Japanese and ...
amendments (1 so far)
  #1 also keep the code blocks untranslated
activity
  … 3 earlier entries not shown
  → read(docs/index.md)
  ← ok  # Index
  ◆ Translating the first file now
  error  provider error: provider returned status 400: quota exceeded
report
  /project/.nanopi/agents/<run>/a3/report.md
  │ # Report
  │ ## Summary
  │ error: provider error: ...
paths
  dir         /project/.nanopi/agents/<run>/a3
  transcript  /project/.nanopi/agents/<run>/a3/transcript.jsonl
```

Fields the card cannot know are left out rather than shown as placeholders:
`model` comes from the brief, else from the transcript's session header;
`turns` and `tokens` exist only once the agent has written its report; the
worktree and branch only for an isolated agent; `tools` (names only) from
`tools.json`, else the brief. The task is cut at about 300 characters. The
activity is the last 12 transcript entries (assistant text and your own
messages by first line, tool calls as `name(argument)`, tool results as
`ok`/`error` plus their first line); how many earlier ones there are is stated.
The reason line (`error`, `stopped`, `limit reached`) is the first lines of the
report's summary, for an agent that did not complete. The report shows its
first 15 lines. The card is at most 60 lines, and the paths are never cut, so
you can open the full files.

Nothing on the card is trusted: everything that came from a model or from a file
an agent can write has its control characters (escape sequences included)
stripped before it is laid out, and is cut by display width, so CJK text and
narrow terminals are safe. Reads are bounded: the last 64 KiB of the
transcript, the first 256 KiB of the brief and 16 KiB of the report, so a huge
or half-written file costs the same as a small one, and an unreadable or
non-UTF-8 one just leaves its fields out.

## Esc and stopping agents

In the TUI, `Esc` (and `Ctrl-C` while a turn is streaming) interrupts **the
orchestrator's own turn and nothing else**. Dispatched agents keep running,
whichever way they were dispatched: `background: true`, `background: false`,
`parallel` and `chain` all run the agent on a task of its own that the tool
call merely waits for, so cancelling the turn drops the wait and not the agent.
A `chain` keeps going too: later steps still start. When the agents finish,
their reports reach the conversation like any background agent's, as a new
turn. The cancelled `agent` call itself returns nothing to the model; what the
model reads in its place says the agents were **not** stopped and will report
(so it does not dispatch them again).

If agents are running when you press `Esc`, the `Operation aborted` line says
so: `Operation aborted  (2 agents still running — /agents stop to stop them)`.

Agents are stopped only by:

- **`/agents stop <id|name-prefix>`** — stop one agent. An id is `a3`; a name is
  a prefix of the name the strip shows, case-insensitive, and must match
  exactly one live agent (otherwise nothing is stopped and the candidate ids
  are listed). Works while a turn is streaming.
- **`/agents stop all`** — stop every live agent.
- **The orchestrator's `stop_agent`**, when you ask it to.
- **Quitting nanopi** (`/quit`, `Ctrl-D`, SIGTERM/SIGHUP): the orderly shutdown
  kills every agent's process group.

`/agents stop` prints which ids it stopped. An unknown or already-finished id
(or a name that matches nothing, or several agents) is a one-line message, not
an error: the TUI carries on. A stopped agent ends `stopped` with a partial
report, which is delivered like any other. `/agents clean` still prunes the
on-disk archive. `/agents` (the picker) and `/agents show` are how you look at
an agent before deciding to stop it.

Non-interactive runs are unchanged: in `nanopi -p`, `Ctrl-C` stops every agent,
and a foreground dispatch dies with the process that is waiting for it.

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

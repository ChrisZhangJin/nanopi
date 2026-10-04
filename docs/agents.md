# Agents

The `agent` tool runs each agent as a separate `nanopi -p` child process.
Modes: single, parallel (up to 8 tasks), chain. A child that crashes, panics,
is killed or times out comes back to the model as an in-band `status: failed`
result; the parent keeps running.

There is no keybinding, slash command, prompt or TUI action for agents.
They are controlled only by the model through the tool.

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

## Known gaps

- If the parent nanopi is itself SIGKILLed (or crashes hard), children die via
  `PR_SET_PDEATHSIG` (SIGKILL), but bash grandchildren they started may be
  orphaned and keep running.
- `PDEATHSIG` is Linux-only. On other unix targets the child polls
  `getppid()` instead, which is slower to notice.

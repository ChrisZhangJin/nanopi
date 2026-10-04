# Agents

The `agent` tool runs each agent as a separate `nanopi -p` child process.
Modes: single, parallel (up to 8 tasks), chain. A child that crashes, panics,
is killed or times out comes back to the model as an in-band `status: failed`
result; the parent keeps running.

There is no slash command, prompt or control action for agents in the TUI.
They are controlled only by the model through the tool. The TUI does show a
read-only agents strip while agents are running or recently finished — see
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

A read-only strip shows every agent at a glance while any are running or
recently finished. It sits between the status line and the input box, and is
hidden whenever the current run has no agents.

**Glyphs:**

| Glyph | Meaning |
|-------|---------|
| `●` | running |
| `◐` | queued |
| `✓` | done |
| `✗` | failed |
| `■` | stopped |
| `⏱` | limit reached |
| `?` | interrupted |

There is no "waiting for permission" state: children run with `--approve` or
`--distrust` decided at dispatch and never prompt, so the strip has nothing to
approve or deny (display-only, UI-03).

**Collapsed** (default), the strip is 1–3 lines: a header
`agents (N) · Ctrl+G expand`, then at most 3 rows — agents needing attention
first, then running, then most recently finished. With more than 3 agents the
last row folds the rest into `+K more (R running)`.

**Expanded** (`Ctrl+G` toggles), the in-dock rows keep showing each agent's
latest activity and the dock grows to show more of the list, while a full
detail block (activity history, turns/tokens, worktree/branch, report path)
for the agents in view is printed into scrollback. `Ctrl+G` or `Esc` collapses
it back.

On narrow terminals (under ~60 columns) the activity text drops first, then
the description; on short terminals (under ~15 rows) the strip shows only its
header line (D-08).

The `Ctrl+G` binding (`ActionId::ToggleAgentsStrip`) is configurable through
the same keybindings/settings menu as every other action — see `src/keys.rs`.

### Manual test

| 前提 Precondition | 步骤 Steps | 期望 Expected | 结果 Result |
|---|---|---|---|
| Model has access to the `agent` tool | Dispatch 1 agent, then dispatch 4+ more (background) in the same or a later turn | Strip appears between status line and input once the first agent starts; with 4+ agents, strip shows at most 3 rows plus a `+K more (R running)` row | ☐通过 ☐失败 |
| Strip visible, collapsed | Press `Ctrl+G` | Strip expands: detail is printed to scrollback and in-dock rows keep showing latest activity | ☐通过 ☐失败 |
| Strip expanded | Press `Ctrl+G` again, or `Esc` | Strip collapses back to the 1–3 line summary | ☐通过 ☐失败 |
| An agent has finished (done) | Observe the strip after all agents finish | Strip remains visible showing `✓` for finished agents (does not vanish immediately) | ☐通过 ☐失败 |
| Agents idle/no new activity | Watch the strip for several seconds | No flicker or redraw churn while idle (draws come from the existing tick, not per-event) | ☐通过 ☐失败 |
| Input box focused, no modifier held | Type the letter `g` (no Ctrl) | `g` is inserted into the input as a normal character; the strip does not toggle | ☐通过 ☐失败 |

Phase 6 owns the consolidated manual test plan (QA-01); this table is the
Ctrl+G row set it will collect into that plan.

## Known gaps

- If the parent nanopi is itself SIGKILLed (or crashes hard), children die via
  `PR_SET_PDEATHSIG` (SIGKILL), but bash grandchildren they started may be
  orphaned and keep running.
- `PDEATHSIG` is Linux-only. On other unix targets the child polls
  `getppid()` instead, which is slower to notice.

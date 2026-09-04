# Hook examples

Shell hooks for nanopi's `[[hooks.*]]` config. Every script here has
been run against real payloads, and `session-log.sh` / `audit-log.sh`
have been run end to end through nanopi itself.

`config.toml.example` has referred to `~/.nanopi/hooks/check-rm-rf.sh`
and `redact-secrets.sh` since v0.5 without either file existing
anywhere. These are those files.

## Why these are hooks and not WASM plugins

nanopi has two extension systems and they are not interchangeable
(`docs/v0.12-events.md` §1, §8):

| | shell hooks | WASM plugins |
|---|---|---|
| can refuse an action | **yes** | no, by design |
| can rewrite arguments | **yes** | no |
| keeps state across events | no — one process per event | **yes** |
| registers tools / commands | no | **yes** |
| needs `--features wasm` | no | yes |

PI folded its hook layer into extensions and kept only the extension
API. nanopi keeps both, because the veto is the one thing a WASM
plugin structurally cannot do — see `docs/v0.12-events.md` §8 for the
3 MB measurement behind that decision.

So PI's example catalog splits cleanly along this line. Its
"Lifecycle & Safety" examples — `permission-gate.ts`,
`protected-paths.ts`, `dirty-repo-guard.ts` — are all vetoes, and in
nanopi they are hooks. Its custom-tool examples are WASM plugins
(`examples/wasm-plugin/`). Its renderers, overlays, providers and games
have no nanopi equivalent at all.

## Install

```bash
mkdir -p ~/.nanopi/hooks
cp examples/hooks/*.sh ~/.nanopi/hooks/
chmod +x ~/.nanopi/hooks/*.sh
```

`_lib.sh` must sit beside them — each script sources it by
`$(dirname "$0")`.

Then in `~/.nanopi/config.toml` (global) or `./.nanopi/config.toml`
(project):

```toml
[[hooks.tool_execution_start]]
matcher = "^bash$"
command = "~/.nanopi/hooks/check-rm-rf.sh"
timeout = 5000

[[hooks.tool_execution_start]]
matcher = "^(write|edit)$"
command = "~/.nanopi/hooks/protected-paths.sh"

[[hooks.input]]
matcher = "*"
command = "~/.nanopi/hooks/redact-secrets.sh"

[[hooks.tool_execution_end]]
matcher = "*"
command = "~/.nanopi/hooks/audit-log.sh"

[[hooks.session_start]]
command = "~/.nanopi/hooks/session-log.sh"
[[hooks.session_shutdown]]
command = "~/.nanopi/hooks/session-log.sh"
```

Hook vectors are **cumulative**, not overriding: a global and a project
config each declaring `[[hooks.tool_execution_start]]` gives you both,
firing in declaration order. A hook that appears to fire twice is
usually this and not a bug.

## The examples

| Script | Event | Shape | PI equivalent |
|---|---|---|---|
| `check-rm-rf.sh` | `tool_execution_start` | refuse | `permission-gate.ts` |
| `protected-paths.sh` | `tool_execution_start` | refuse | `protected-paths.ts` |
| `dirty-repo-guard.sh` | `before_agent_start` | refuse a whole turn | `dirty-repo-guard.ts` |
| `redact-secrets.sh` | `input` | **rewrite** | `input-transform.ts` |
| `audit-log.sh` | `tool_execution_end` | observe | — |
| `session-log.sh` | `session_start` + `session_shutdown` | observe | `auto-commit-on-exit.ts` |

Between them they cover all three decision shapes a hook can return.

## Protocol, in the amount you need to write one

Payload arrives as one JSON object on **stdin**:

```json
{
  "event": "tool_execution_start",
  "tool_name": "bash",
  "tool_call_id": "call_4b4e9e40…",
  "arguments": {"command": "ls -la"},
  "cwd": "/home/you/project",
  "session_id": "01a06bc8-425c-7993-a1cf-152cc89dafb4"
}
```

Decide by **exit code** or by **stdout JSON**:

| Want | Exit code way | JSON way |
|---|---|---|
| allow | any exit ≠ 2 | `{"decision":"allow"}` |
| refuse | `exit 2`, reason on **stderr** | `{"decision":"block","reason":"…"}` |
| rewrite args | — | `{"decision":"allow","updated_input":{…}}` |

Three things that are easy to get wrong:

**`updated_input` replaces `arguments` wholesale.** Emit every field
the tool needs, not just the one you changed.

**Only `tool_execution_start` can rewrite or refuse a tool.** The
others are advisory. `before_agent_start` is the only hook that can
refuse an entire turn before any model call happens
(`docs/v0.12-events.md` §7).

**Failure is fail-open.** A missing script, a non-executable one, a
timeout, or any non-zero exit that is not 2 allows the action. nanopi
now prints the exit code and a hint (127 = not found, 126 = not
executable), because a typo'd path used to be indistinguishable from a
hook that simply never matched.

### Read the payload, not `$NANOPI_*`

The `NANOPI_EVENT`, `NANOPI_TOOL_NAME`, `NANOPI_CWD`,
`NANOPI_TOOL_CALL_ID` and `NANOPI_SESSION_ID` env vars carry the same
facts and are convenient for a one-liner. But `NANOPI_EVENT` spells the
event in PascalCase (`ToolExecutionEnd`) while the payload, the config
key and the WASM `events` grant all use snake_case
(`tool_execution_end`). Two spellings for one name is a trap, and
`session-log.sh` had exactly that bug — comparing `$NANOPI_EVENT`
against `session_shutdown`, which never matched — until it was run end
to end. These examples read the payload throughout.

### Parse the JSON. Do not grep it.

`arguments` is arbitrary JSON. A bash command can contain quotes,
newlines, backslashes and `}`; a `write` call carries a whole file. A
regex over the raw payload works until the first command with a quote
in it, and then fails in a way that looks like the hook not firing.

`_lib.sh` uses `python3` because it is present on essentially every
machine that has nanopi. Use `jq` if you have it. The rule is only
that you parse.

### Consume stdin

nanopi writes the payload and closes the pipe. A hook that never reads
leaves that write to be reaped as `EPIPE`. The host tolerates it, but
`cat > /dev/null` (or `read_payload`) is cheaper and clearer than
relying on that.

## Testing a hook without burning tokens

The scripts are ordinary programs — feed them a payload directly:

```bash
printf '%s' '{"event":"tool_execution_start","tool_name":"bash",
  "arguments":{"command":"rm -rf /"}}' \
  | ~/.nanopi/hooks/check-rm-rf.sh
echo "exit=$?"      # 2 = refused
```

That separates "is my hook right" from "will the model call this tool",
which are the two failure modes people conflate. See
`docs/v0.12-manual-test-plan.md` §0.4 — a model that answers
`echo hello` from memory never calls bash at all, and the hook looks
broken when nothing ran.

## Deliberate limits

**`input` matchers only support `*`.** The event has no tool name, so
the matcher runs against an empty subject. Filter inside the script.
(`docs/v0.12-manual-test-plan.md` T2.7.)

**A tool-name matcher guards the tools it names.** `protected-paths.sh`
on `^(write|edit)$` does not stop `bash` from writing `.env` — that is
a bash call whose `arguments.command` is opaque to it. Pair the two
hooks, or do not give the agent bash.

**A hook cannot ask the user.** stdin carries the payload and there is
no TTY, so a hook decides on its own. PI's `permission-gate.ts` can
open a dialog; `check-rm-rf.sh` refuses instead. For interactive
approval use nanopi's own permission gate.

**Every hook costs a process spawn per event.** `turn_start` and
`message_end` fire often. If you want to accumulate state across
events, that is what a WASM event subscriber is for
(`examples/wasm-plugin-events/`).

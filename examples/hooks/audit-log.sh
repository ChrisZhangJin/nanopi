#!/bin/sh
# tool_execution_end — append one line per tool call to an audit log.
#
#   [[hooks.tool_execution_end]]
#   matcher = "*"
#   command = "~/.nanopi/hooks/audit-log.sh"
#
# Observation only: this never blocks. It is the hook to reach for when
# you want to know what the agent did, and the counterpart to the
# WASM-plugin event subscriber — a hook is one process per event, a
# plugin keeps state across them (examples/wasm-plugin-events).
#
# One line of TSV per call, appended. Deliberately not JSON-per-line:
# the payload's `arguments` can be a multi-kilobyte file body, and an
# audit log you cannot `grep`/`cut` is one you never read. If you want
# the full payload, `command = "cat >> ~/.nanopi/tool-audit.jsonl"`
# needs no script at all.
#
# Every field comes from the PAYLOAD, not from the `NANOPI_*` env vars.
# Both carry the same facts, but `NANOPI_EVENT` spells the event name
# in PascalCase (`ToolExecutionEnd`) while the payload, the config key
# and the WASM `events` grant all use snake_case. One vocabulary is
# easier to remember than two, and the payload is the one the other
# three agree with.
. "$(dirname "$0")/_lib.sh"
read_payload

LOG="${NANOPI_AUDIT_LOG:-$HOME/.nanopi/tool-audit.tsv}"
mkdir -p "$(dirname "$LOG")" 2>/dev/null

# Flatten to one line and cap it: a `write` call carries the whole file.
event=$(field event)
tool=$(field tool_name)
call_id=$(field tool_call_id)

subject=$(field arguments.command)
[ -z "$subject" ] && subject=$(field arguments.path)
[ -z "$subject" ] && subject=$(field arguments.pattern)
subject=$(printf '%s' "$subject" | tr '\n\t' '  ' | cut -c1-200)

printf '%s\t%s\t%s\t%s\t%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "$(field session_id)" \
    "${tool:-?}" \
    "${call_id:-?}" \
    "$subject" >> "$LOG"

allow

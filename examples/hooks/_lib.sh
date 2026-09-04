#!/bin/sh
# Shared helpers for the example hooks. Source it, don't run it.
#
# Every hook gets the event as ONE JSON object on stdin and must
# consume it — nanopi writes the payload and closes the pipe, and a
# hook that never reads leaves that write to be reaped as EPIPE. The
# host tolerates it, but reading is both cheaper and clearer.
#
# `arguments` is arbitrary JSON: a bash command can contain quotes,
# newlines, backslashes and `}`. Parse it, never grep it. These
# examples use python3 because it is the one JSON parser present on
# essentially every machine that has nanopi; if you have `jq`, prefer
# it. What matters is that you do not pattern-match raw JSON.

# Read stdin once into $PAYLOAD.
read_payload() {
    PAYLOAD=$(cat)
}

# field <dotted.path> — print one string field, or empty if absent.
# Usage: cmd=$(field arguments.command)
field() {
    printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
path = sys.argv[1].split(".")
try:
    node = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for p in path:
    if not isinstance(node, dict) or p not in node:
        sys.exit(0)
    node = node[p]
if node is None:
    sys.exit(0)
sys.stdout.write(node if isinstance(node, str) else json.dumps(node))
' "$1" 2>/dev/null
}

# refuse <reason> — block the action. stderr becomes the reason the
# model and the user see, so write it for them, not for a log.
refuse() {
    echo "$1" >&2
    exit 2
}

# rewrite <json-object> — replace the tool's arguments.
# The object REPLACES `arguments` wholesale; include every field the
# tool needs, not just the one you changed.
rewrite() {
    printf '{"decision":"allow","updated_input":%s}\n' "$1"
    exit 0
}

allow() { exit 0; }

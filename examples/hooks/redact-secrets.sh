#!/bin/sh
# input — rewrite the prompt before the model sees it.
#
#   [[hooks.input]]
#   matcher = "*"
#   command = "~/.nanopi/hooks/redact-secrets.sh"
#
# The one example here that TRANSFORMS rather than refuses. Mirrors
# PI's examples/extensions/input-transform.ts.
#
# Refusing a prompt that contains a key is the wrong trade: the user
# still needs an answer, and they will paste it again with the key
# spelled differently. Replacing the key lets the turn proceed with the
# secret never reaching the provider.
#
# The user is shown the ORIGINAL text they typed — the echo happens
# before hooks run — so a silent rewrite would be invisible to them.
# That is why this prints a line to stderr as well.
#
# NOTE on matchers: `input` has no tool name, so its matcher runs
# against an empty subject. Only `matcher = "*"` matches. That is a
# known limitation (docs/v0.12-manual-test-plan.md T2.7); filter inside
# the script instead.
. "$(dirname "$0")/_lib.sh"
read_payload

prompt=$(field arguments.prompt)
[ -z "$prompt" ] && allow

redacted=$(printf '%s' "$prompt" | sed -E \
    -e 's/sk-[A-Za-z0-9_-]{16,}/[REDACTED-OPENAI-KEY]/g' \
    -e 's/sk-ant-[A-Za-z0-9_-]{16,}/[REDACTED-ANTHROPIC-KEY]/g' \
    -e 's/gh[pousr]_[A-Za-z0-9]{20,}/[REDACTED-GITHUB-TOKEN]/g' \
    -e 's/AKIA[0-9A-Z]{16}/[REDACTED-AWS-KEY-ID]/g' \
    -e 's/-----BEGIN [A-Z ]*PRIVATE KEY-----/[REDACTED-PRIVATE-KEY]/g')

[ "$redacted" = "$prompt" ] && allow

echo "redact-secrets: a credential-shaped string in your prompt was replaced before it left this machine." >&2

# `updated_input` REPLACES `arguments`, so emit the whole object.
# Built with python3 rather than string concatenation: the prompt can
# contain quotes and newlines, and hand-built JSON breaks on both.
printf '%s' "$redacted" | python3 -c '
import json, sys
print(json.dumps({"decision": "allow",
                  "updated_input": {"prompt": sys.stdin.read()}}))
'

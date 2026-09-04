#!/bin/sh
# before_agent_start — refuse to start a turn on a dirty repo.
#
#   [[hooks.before_agent_start]]
#   matcher = "*"
#   command = "~/.nanopi/hooks/dirty-repo-guard.sh"
#
# Mirrors PI's examples/extensions/dirty-repo-guard.ts.
#
# `before_agent_start` is the ONLY nanopi hook that can block a turn
# before any model call happens — see docs/v0.12-events.md §7. Blocking
# `input` also stops the turn but fires later; blocking a tool stops one
# call, not the turn.
#
# Opt-in per project rather than globally: an agent that refuses to run
# in every scratch directory gets uninstalled. Set NANOPI_REQUIRE_CLEAN=1
# in the environment, or drop a .nanopi/require-clean marker file.
. "$(dirname "$0")/_lib.sh"
read_payload

cwd=$(field cwd)
[ -z "$cwd" ] && cwd="$PWD"

if [ "${NANOPI_REQUIRE_CLEAN:-0}" != "1" ] && [ ! -f "$cwd/.nanopi/require-clean" ]; then
    allow
fi

git -C "$cwd" rev-parse --git-dir >/dev/null 2>&1 || allow

dirty=$(git -C "$cwd" status --porcelain 2>/dev/null)
[ -z "$dirty" ] && allow

count=$(printf '%s\n' "$dirty" | grep -c .)
refuse "policy: $count uncommitted change(s) in $cwd. This project requires a clean tree before the agent runs, so a failed edit can be undone with 'git checkout .'. Ask the user to commit or stash first — do not commit on their behalf."

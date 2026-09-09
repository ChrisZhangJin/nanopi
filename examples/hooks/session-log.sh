#!/bin/sh
# session_start / session_shutdown — record the session lifecycle.
#
#   [[hooks.session_start]]
#   command = "~/.nanopi/hooks/session-log.sh"
#   [[hooks.session_shutdown]]
#   command = "~/.nanopi/hooks/session-log.sh"
#
# One script for both events: the payload's `event` field says which.
#
# Read `event` from the PAYLOAD, not from `$NANOPI_EVENT`. The two use
# different spellings: the payload (and the config key, and the WASM
# `events` grant) say `session_shutdown`, while the env var says
# `SessionShutdown`. Comparing the env var against the documented
# snake_case name silently never matches — this example had that bug
# until it was run end to end.
#
# The point of the example is `arguments.reason`. Both events fire on
# far more than program start and exit — `/new`, `/resume`, `/fork` and
# `/import` each fire a shutdown for the session you left and a start
# for the one you entered. A hook that assumes shutdown means "nanopi
# is exiting" will run its cleanup four times in one sitting.
#
# Before v0.12 the reason was smuggled in the `session_id` field, so a
# hook could not have both. Now `session_id` is always the real id and
# the reason lives in `arguments.reason`.
. "$(dirname "$0")/_lib.sh"
read_payload

LOG="${NANOPI_SESSION_LOG:-$HOME/.nanopi/sessions.log}"
mkdir -p "$(dirname "$LOG")" 2>/dev/null

event=$(field event)
reason=$(field arguments.reason)

printf '%s\t%s\t%s\t%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "${event:-?}" \
    "${reason:-?}" \
    "$(field session_id)" >> "$LOG"

# Only the real exit — not a session switch.
if [ "$event" = "session_shutdown" ] && [ "$reason" = "quit" ]; then
    : # your once-per-run cleanup goes here
fi

allow

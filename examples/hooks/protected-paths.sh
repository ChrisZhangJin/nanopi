#!/bin/sh
# tool_execution_start — refuse writes to paths that must not be edited.
#
#   [[hooks.tool_execution_start]]
#   matcher = "^(write|edit)$"
#   command = "~/.nanopi/hooks/protected-paths.sh"
#
# Mirrors PI's examples/extensions/protected-paths.ts. Note the
# matcher: this only guards the `write` and `edit` tools. It does NOT
# stop `bash` from writing the same file — `sh -c 'echo x > .env'` is a
# bash call, and its `arguments.command` is opaque to this script.
#
# That gap is real and worth stating rather than papering over: a hook
# matching on tool name protects the tools it names. Pair this with
# check-rm-rf.sh, or drop `bash` entirely if the guarantee has to hold.
. "$(dirname "$0")/_lib.sh"
read_payload

path=$(field arguments.path)
[ -z "$path" ] && allow

case "$path" in
    *.env|*.env.*|*/.env|*/.env.*)
        refuse "policy: .env files hold credentials and are not editable through nanopi. Ask the user to change it by hand, and tell them which key you need set." ;;
    */.git/*|.git/*)
        refuse "policy: refusing to write inside .git — use git commands via bash instead of editing the object store." ;;
    */node_modules/*|node_modules/*)
        refuse "policy: node_modules is generated. Change package.json (or the lockfile) and reinstall." ;;
    */target/*|target/*)
        refuse "policy: target/ is Cargo's build output and is regenerated. Edit the source instead." ;;
    *id_rsa|*id_ed25519|*/.ssh/*)
        refuse "policy: refusing to touch SSH key material." ;;
esac

allow

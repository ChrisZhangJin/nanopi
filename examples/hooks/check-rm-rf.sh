#!/bin/sh
# tool_execution_start — refuse bash commands that delete broadly.
#
#   [[hooks.tool_execution_start]]
#   matcher = "^bash$"
#   command = "~/.nanopi/hooks/check-rm-rf.sh"
#   timeout = 5000
#
# PI's equivalent (examples/extensions/permission-gate.ts) can ask the
# user in a dialog. A shell hook cannot — stdin carries the payload and
# there is no TTY — so this refuses outright rather than prompting.
# Deciding without asking is the whole shape of the hook layer: if you
# want a prompt, use nanopi's own `tool_exec_mode` / permission gate.
#
# The reason on stderr reaches the MODEL, so it is written to tell the
# model what to do differently, not just that it was stopped.
. "$(dirname "$0")/_lib.sh"
read_payload

cmd=$(field arguments.command)
[ -z "$cmd" ] && allow

# Deliberately narrow. A broad "looks dangerous" regex trains the model
# to reword until it passes, which is worse than no hook: you get the
# same deletion with a command you no longer recognize.
case "$cmd" in
    *"rm -rf /"*|*"rm -fr /"*|*"rm -rf /"*)
        refuse "policy: refusing 'rm -rf /' — if you need to delete a specific directory, name it explicitly (e.g. rm -rf ./build) and run that instead." ;;
esac

# `rm -rf` with no path at all, or with a bare glob.
case "$cmd" in
    *"rm -rf *"*|*"rm -fr *"*)
        refuse "policy: refusing 'rm -rf *' — a glob at the top of the working directory is unrecoverable. List what you intend to delete and remove those paths by name." ;;
esac

# Force-push to the default branch. Not deletion, but equally hard to undo.
case "$cmd" in
    *"push --force"*|*"push -f"*)
        case "$cmd" in
            *main*|*master*)
                refuse "policy: refusing a force-push to main/master. Push to a branch and open a pull request." ;;
        esac ;;
esac

allow

# nanopi-memory — durable project memory as markdown

A memory system for nanopi. Facts you want kept past the end of a
session are written to `.nanopi/memory/` as one markdown file each,
indexed by `MEMORY.md`, and the index is injected into the model's
context so it knows what it can look up.

This is the first example here that is meant to be *used* rather than
to demonstrate a mechanism. It is also the canonical use case for the
capability surface — `host-set-context`'s own tests use
`plugin_context::set("memory", …)` as their fixture.

| capability | grant | what it is for |
|---|---|---|
| `host-fs-read` | `allow_fs = true` | reading the index and memory bodies |
| `host-call-tool` (`write`) | `allow_tools = ["write"]` | the only way a plugin can write a file |
| `host-set-context` | `allow_context = true` | putting the index in the model's prompt |
| `host-notify` | *(ungated)* | telling you when the index is truncated |

No network, no `allow_store`, no `allow_send_message`. The files are the
storage.

## Install

```toml
[[extensions]]
path = "/abs/path/to/dist/nanopi-memory-plugin.component.wasm"
allow_fs = true
allow_tools = ["write"]
allow_context = true
```

Use an absolute path — `~` expands to `$HOME` and ignores `NANOPI_HOME`.
The host must be built with `--features wasm`.

## Build

From the repo root, so `wit/` resolves:

```bash
make plugin-memory
```

## What you get

```
remember(name, description, body)   save a durable fact
recall(name)                        read one back in full
forget(name)                        drop it from the index
/memory                             list everything + budget used
```

On disk:

```
.nanopi/memory/MEMORY.md          ← the index, one line per memory
.nanopi/memory/wiki-repo.md       ← one memory, frontmatter + body
.nanopi/memory/test-threads.md
```

Both are ordinary markdown. Edit them by hand, diff them, commit them,
review them in a PR. The index format is
`- [name](name.md) — description`.

## The one design decision that matters

**Only the index is injected, never the bodies.**

`host-set-context` is capped at 4 KiB and that text enters *every*
request for the rest of the session. Spending it on full memory text
would tax every single turn. So the model gets a compact index — one
line per memory — reads the descriptions, decides what is relevant, and
calls `recall` for the full text of the one it wants.

This puts the relevance judgement where it belongs. A keyword-matching
heuristic written in `no_std` WASM would be bad at deciding "is this
memory related to what the user just asked"; the model is good at it.
Per-turn cost stays fixed and small, and it works: asked "anything I
should watch out for before running the tests?" in a fresh session, the
model found the relevant index line and recalled it without being told
the plugin existed.

## Why there is no automatic capture

An obvious feature is missing on purpose: this plugin does **not**
subscribe to `message_end` and save conclusions by itself.

Event delivery is drop-on-busy and does **not** queue — a plugin already
inside another guest call simply misses the event. A memory system built
on that would quietly forget things, and would have no way to tell you
which. "Silently believing something was saved when it wasn't" is the
exact failure `docs/plugin-capabilities.md` invariant 9 exists to
prevent, and it is worse in a memory system than almost anywhere else.

So capture is explicit. The injected header tells the model to call
`remember` when it sees a durable preference, a project convention, or a
pitfall already hit — and `remember`'s return value is a real answer
about what happened, not a hope.

## Known limitations — read these

**1. `forget` does not delete the file.** This plugin holds `write`, not
`bash`, so it cannot remove anything. Asking for `bash` to support one
tool would hand the plugin a capability that walks straight past
`allow_fs`'s cwd confinement — a bad trade. So `forget` removes the
entry from `MEMORY.md` and overwrites the body with a tombstone note.
The file stays on disk; `rm` it or drop it in git if you want it gone.
The tool's return value says so explicitly.

**2. A file you add by hand is not discovered automatically.** The
plugin has no `ls` or `find` grant, so `MEMORY.md` is the only source of
truth about what exists. If you create `.nanopi/memory/foo.md` yourself,
add its line to `MEMORY.md` too. (This is the same discipline a
hand-maintained memory index needs anyway.) If you want it automatic
later, add `find` to `allow_tools` and a `/memory sync` subcommand.

**3. `remember` does not take effect until the next turn.** Per the WIT
contract, a context contribution lands at the *next* turn's assembly.
Within the same turn, use `recall` for something just saved. The tool's
return value says this too.

**4. The index is truncated when it exceeds 4 KiB, and says so.** At
roughly 30–60 memories the budget fills. The block itself then carries a
line saying how many are not shown, `/memory` still lists all of them,
and you get one `host-notify` line. Nothing is dropped silently — but do
note the model genuinely cannot see the omitted entries, so keep
descriptions short and prune with `forget`.

**5. Nothing the plugin does appears in the transcript.** By invariant
15 a plugin-initiated tool call is deliberately not written as a
`SessionEntry`, so `--continue` will not show the writes. For this
plugin that is fine: the markdown files *are* the record, and they are
in version control. nanopi still prints
`[nanopi-memory-plugin] called the "write" tool` live, on the host's own
budget, so a plugin cannot suppress it.

## Notes on the source

- Everything above the `YOUR TOOLS` line in `src/lib.rs` is boilerplate
  copied verbatim from `examples/wasm-plugin-minimal/`. Start there if
  you are writing your own.
- This is the first plugin in the repo to call `host-call-tool` and
  `host-set-context`, so the two-string import shape
  (`p1, l1, p2, l2, ret_area`) had no precedent here. It is the plain
  canonical-ABI flattening; getting it wrong fails at
  `wasm-tools component new`, not at runtime.
- No state lives in the guest across calls, which matters more than it
  looks. The bump arena is never rewound, so a long session can exhaust
  it and trap — and for this plugin that costs nothing, because a
  rebuilt instance reads the same files. A plugin caching memories in
  guest memory would lose them.
- Two failure modes are told apart deliberately, and both were bugs in
  the first draft: "I could not read the index" is not "there are no
  memories", and "access denied" is not "that memory does not exist". A
  tool that cannot see must say so rather than answer as though it had
  looked.

## See also

- `examples/wasm-plugin-minimal/` — the smallest plugin, the one to copy
- `examples/wasm-plugin/` — the fuller tools + commands reference
- `examples/wasm-plugin-events/` — lifecycle event subscription
- `docs/plugin-capabilities.md` — what a plugin may do, and why

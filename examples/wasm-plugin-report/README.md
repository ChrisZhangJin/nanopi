# nanopi-tools-report — where the time went, and what keeps failing

Records per-tool call counts, total duration and failures, and keeps
them across sessions. Answers two questions that currently have no
answer:

- **Why was this session slow?** Which tool ate the wall clock?
- **What keeps failing**, with which arguments, and with what error?

```
/tools-report        the human report
tool_stats(scope)    the same numbers, for the model to reason about
```

## Why it can exist at all

`tool_execution_end`'s payload carries `tool_response.duration_ms`, and
**that number is persisted nowhere else**. The TUI prints `Took 5ms` on
the tool card and it scrolls away; the session transcript does not record
it; the status bar counts tokens, not time. So during a long session the
information you would need to answer "what is slow" streams past you and
is gone.

Same for failures: a failed tool call shows a red card, the model works
around it, and twenty turns later you have no idea it happened three
times.

## Install

```toml
[[extensions]]
path = "/abs/path/to/dist/nanopi-report-plugin.component.wasm"
allow_store = true
events = ["tool_execution_end", "session_start"]
```

That is the whole grant set: one capability and two events. No
filesystem, no network, no context contribution, no tool calls. Compare
`examples/wasm-plugin-memory/`, which uses the other half of the surface
(`allow_fs` + `allow_tools` + `allow_context`) and subscribes to no
events at all.

**Both events matter.** `tool_execution_end` is the data.
`session_start` is what makes "this session" *mean* this session — see
below. If you grant only the first, nanopi warns about the ungranted one
and the report relabels itself honestly rather than lying.

## Build

From the repo root, so `wit/` resolves:

```bash
make plugin-report
```

## Example

```
All time (3 session(s)): ≥3 call(s), 1.0s total, 1 failed
  bash              2 call(s)      1.0s total    502ms avg
  read              1 call(s)       0ms total      0ms avg  (1 failed)

This session: nothing recorded yet

Last 1 failure(s), newest last:
  read  {"path":"/tmp/reptest/nope.txt"}
    → tool error: execution failed: cannot read /tmp/reptest/nope.txt: No such file or directory (os error 2)
```

Sorted slowest-first, because that is the order that answers "where did
the time go" without the reader doing arithmetic.

## This is NOT an audit log — read this before relying on it

Event delivery is **drop-on-busy and does not queue**. If the plugin is
already inside a guest call when the next event fires, that event is
dropped, not deferred. The likeliest moment for that is exactly the
interesting one: two tools finishing at the same instant in a parallel
batch.

So **every count here is a lower bound**, which is why the report prints
`≥` rather than a total. That is fine for "which tool is slow" —
statistics tolerate sampling loss — and it is useless for anything that
has to be complete.

**If you need a record with no holes, use a shell hook instead:**

```toml
[[hooks.tool_execution_end]]
matcher = "*"
command = "cat >> ~/tool-audit.jsonl"
```

Hooks are synchronous and cannot be dropped. A plugin cannot offer that,
and pretending otherwise would be worse than not shipping this.

## Other limitations

**`alltime` lags by one session.** Counts land in a per-session table
and are folded into the all-time totals when the next session starts, so
nothing double-counts. Every reader sums the two; if you inspect
`store.json` by hand, do the same.

**Duration is the tool's own execution time**, not time the model spent
thinking, and not the hook time around it. It is
`tool_response.duration_ms` exactly as nanopi measured it.

**Blocked calls do not appear.** A call refused by a
`tool_execution_start` hook never executes, so there is no execution to
end and no event. If you want blocked calls recorded, that is a
`tool_execution_start` hook's job.

**Failures are capped at the last 20**, with 200 characters each of
arguments and error text. The store is capped at 1 MiB total, and an
unbounded failure list is the one field here that would grow into it —
at which point `host-store-set` would start refusing and the numbers
would quietly stop moving. Bounded on purpose, with the cap named in the
output.

## Notes on the source

- Everything above the `YOUR TOOLS` line in `src/lib.rs` is boilerplate
  copied verbatim from `examples/wasm-plugin-minimal/`.
- The event handler is deliberately cheap: it runs on the critical path
  of every tool call inside a 2 s guest budget, so it does one store read
  and one store write and does not parse the tool's output beyond its
  error flag.
- `handle-event` does **not** call `reset_arena()` — the host already put
  the payload in this arena via `cabi_realloc`, so rewinding would free
  the bytes about to be read.
- A bug worth keeping in mind if you write something similar: the
  per-session table originally rolled over on the first *tool* event, so
  a session that called no tools left the **previous** session's numbers
  sitting under a "This session" heading. Subscribing to `session_start`
  is what fixes it, and the `cur_from_start` flag is what keeps the label
  honest when that event was not granted. Found by running three
  sessions and reading `store.json`, not by reading the code.

## See also

- `examples/wasm-plugin-memory/` — the other capability half, and a
  plugin you would actually install
- `examples/wasm-plugin-events/` — the event mechanism on its own
- `examples/wasm-plugin-minimal/` — the skeleton to copy
- `docs/v0.12-events.md` — the eleven events and their payloads

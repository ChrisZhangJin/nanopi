# Upstream Pi: how slash commands actually work

**Status:** reference notes for the deferred *plugin slash-command registration*
capability. Not a spec, not a decision record — background for whoever plans it.

**Provenance:** traced from `/root/workspace/pi` (the TypeScript project nanopi
ports) by a scout agent on 2026-08-28, captured here during quick task
`260828-l4d` because that transcript is not durable. **Line numbers and claims
below are second-hand and were NOT re-verified against Pi's source.** Re-check
before relying on any specific line. The architectural shape is the valuable
part; the line numbers will rot.

---

## The headline

**Pi has no unified command registry.** Four independent namespaces resolved at
three layers, with precedence that is *structural* — a hardcoded `if`-chain —
rather than data-driven.

Resolution order for a line starting with `/`:

1. **Built-in** — `if`-chain in the TUI submit handler (`interactive-mode.ts`),
   checked **before** `session.prompt()` is ever called
2. **Extension command** — `agent-session.ts` → `_tryExecuteExtensionCommand`
3. **Skill** — `/skill:<name>`
4. **Prompt template** — a `.md` file, pure text substitution
5. **No match → sent to the LLM verbatim as an ordinary user message**

There is **no "unknown command" error anywhere**. `/typo` silently becomes a
prompt.

Because built-ins short-circuit in the TUI layer, **an extension can never
override a built-in**. That is a consequence of where the check lives, not a
policy someone wrote down.

## Things that will bite a port

**Args are a raw untrimmed remainder**, not argv. Split on the *first* space;
everything after is passed verbatim including interior whitespace. `/cmd` with
no args yields `""`, never `undefined`. Only prompt templates get real argv
(via a bash-style quote-aware splitter supporting `$1`, `$@`, `$ARGUMENTS`,
`${N:-default}`, `${@:N:L}`).

Built-ins each parse args with **hardcoded slice offsets** — `.slice(7)` for
`"/model "`, `.slice(9)` for `"/compact "`, `.slice(7)` for `"/skill:"`. One
`split_once(' ')` helper replaces all of them in Rust.

**A throwing handler still counts as handled** — the catch block returns
`true`, so the text is swallowed rather than forwarded to the LLM.

**Command names are never validated** at registration. No lowercase check, no
whitespace check, no leading-`/` strip. A name containing a space becomes
permanently unreachable, because both the dispatcher and the TUI guard split on
the first space.

## Collision behavior differs per pair — none of them error

| Pair | Behavior |
|---|---|
| Same extension, same name twice | Silent overwrite (bare `Map.set`), last wins |
| Two extensions, same name | **Both** renamed `name:1` / `name:2`; the bare name vanishes entirely |
| Extension vs built-in | Extension is dead — shadowed at dispatch, filtered from autocomplete. Warning only |
| Extension vs template/skill | Silent shadow, no warning. Both still show in autocomplete |

The second row is the surprising one: two extensions both registering `deploy`
means **neither** is reachable as `/deploy`.

There is a genuine bug worth not porting: an extension colliding with a
built-in is filtered from autocomplete by exact name, but `name:1` / `name:2`
are not in the built-in set, so *two* colliding extensions produce reachable
commands where *one* produces a dead one. Colliding twice works better than
colliding once.

**Drift already exists upstream:** `/debug`, `/arminsayshi`, and
`/dementedelves` are dispatched in the `if`-chain but absent from the
`BUILTIN_SLASH_COMMANDS` list — so they work, never autocomplete, and don't
participate in conflict warnings. The list and the implementation are two
sources of truth. Don't reproduce that; derive one from the other.

Also note `/help` and `/clear` do **not** exist in Pi. `/hotkeys` and `/new`
are the analogues.

## Performance note

The resolved command list is **recomputed on every lookup** — including once
per keystroke through the TUI's `isExtensionCommand` guard. Trivially cacheable
in Rust with invalidation on reload.

## Handler capabilities

Handlers receive two objects and the split matters: `ctx` (per-invocation,
freshly constructed each call) and the `pi` API (captured by closure).
**Printing to the transcript is on `pi`, not `ctx`.**

- Print to scrollback → `pi.appendEntry` (persisted, not sent to the LLM), or
  `pi.sendMessage` with a registered renderer
- Start a turn → `pi.sendUserMessage(...)`, or `pi.sendMessage(..., { triggerTurn: true })`
- Ask the user → `ctx.ui.select` / `confirm` / `input` / `editor`
- Session control → `ctx.newSession`, `ctx.fork`, `ctx.navigateTree`, `ctx.switchSession`
- Exit → `ctx.shutdown()`

**Return values are ignored.** Returning a string does not inject a message;
the handler must call `pi.sendUserMessage()` explicitly.

Every `ctx` method is a thunk guarded by an "is this extension still active"
assertion, and the laziness is load-bearing — a reload invalidates the old
instance, and a stale handler must throw rather than read a frozen snapshot.
In Rust this is naturally a generation/epoch counter checked per call, or a
weak handle to the runtime.

Extension commands **cannot be queued**: attempting to queue one during
streaming throws. They execute immediately even mid-stream, while ordinary text
is queued.

---

## Relevance to nanopi

nanopi's WASM plugins are sandboxed and capability-gated
(`allow_fs` / `allow_network` + `url_allowlist`); Pi's extensions are
unsandboxed in-process imports with full Node globals. So the *dispatch* design
above is portable but the *trust* model is not — a nanopi plugin registering a
slash command is far less privileged than a Pi extension doing the same, and
the collision rules should probably be tightened rather than copied. In
particular, "silently shadow with no warning" is a poor fit for a system that
otherwise fails loudly and in-band.

See also: `[[.continue-here.md]]` listed plugin slash-command registration as
the largest of the three M2 capabilities and deferred it.

# Deferred work

Things that are worth doing but explicitly deferred. Each entry
should describe (a) what it is, (b) why we deferred, (c) what
would trigger us to pick it up.

---

## Ctrl+O toggle (collapse back after expanding a tool card)

**Current state (v0.7)**: `Ctrl+O` expands the last tool result's
full output *once* into scrollback, styled with the same green/red
bg as the collapsed card. Pressing `Ctrl+O` again does nothing —
the expansion cannot be un-drawn.

**PI's behavior**: `setExpanded(true)` flips a boolean on a live
`ToolExecutionComponent`; the next render cycle re-materializes
the transcript in memory and PI's TUI framework (`tui-main-screen.ts`)
computes a line-level diff, emitting only ANSI cursor moves +
line rewrites for the changed rows. Collapse ↔ expand happens in
place with no scroll disruption.

**Why we can't just mirror it**: nanopi uses ratatui's inline
viewport with `insert_before(N, |buf| …)`, which is fundamentally
append-only against terminal scrollback. Once lines are in the
scrollback they belong to the terminal emulator, not us —
scrolling, selection, copy/paste all depend on that.

**What it would take**: move tool cards (and, ideally, all
per-turn assistant content) out of scrollback and into a
redrawable region owned by ratatui — a `Vec<TranscriptBlock>` in
`App` that `draw_dock` fully re-renders each frame. Estimated
~400 LOC + a rewrite of `on_agent_event` handlers. Not free.

**Trigger to revisit**:
- A user request that specifically needs toggle (not just "view
  full output" — that's already covered by expanding once and
  scrolling in the terminal).
- Or an unrelated refactor that already puts the transcript into
  a reactive component tree.

See conversation on 2026-08-07 for the full analysis.

---

## ~~Test suite is flaky under parallel execution (env-var race)~~ — RESOLVED 2026-09-07

**Kept rather than deleted**, because the shape of the bug is worth
remembering and because the entry predicted three fixes and the real one
was a combination of two of them.

**What it was**: ~3 of 10 `cargo test` runs failed, a different subset
each time. 127 hand-rolled `set_var("NANOPI_HOME", …)` / restore blocks
across 12 modules, in a process-global environment that cargo runs as
parallel threads.

**Two independent defects, not one** — which is why partial fixes kept
not working:

1. **The cascade.** `TEST_LOCK` existed to serialize these, but a test
   that panicked WHILE HOLDING IT poisoned the mutex, and almost every
   call site was `.lock().unwrap()`. One real failure was reported as
   13-14, and the true one was not first in the list, so anyone
   debugging had to take a baseline before they could believe a red
   suite. Sixteen sites had already been converted to a recovering form
   one at a time; seven had not — the tell that a copied idiom is the
   wrong unit. Fixed by making `crate::test_lock()` the only way in,
   with a test that walks `src/` and fails the build on a direct
   acquisition (`a5f5ce9`).

2. **The leak.** Every block put the restore at the END OF THE BODY, so
   a failing assertion jumped over it and left `$NANOPI_HOME` pointing
   at a temp dir about to be deleted. That is the race itself, and
   recovering from a poisoned mutex does nothing about it. Fixed with
   `TempNanopiHome`, an RAII guard whose restore runs on unwind
   (`30e5ddf`, migrated in `1a8051a` and `8205d9c`).

**Two findings that only surfaced during the migration**, both of the
same kind — a local copy that got the hard part right and the easy part
wrong:

- `config.rs` had its own `HomeGuard`: correct RAII restore, and it
  never took the lock. Six tests mutated the environment with no
  serialization at all.
- `agent/permission.rs::persist_and_session_only_is_noop` had no guard
  of any kind.

**A trap worth recording**: replacing `HomeGuard` first produced a
DEADLOCK, not a failure. Six tests held `lock()` and then constructed a
guard that locks again; `std::sync::Mutex` is not reentrant. The symptom
was a 400-second timeout with no output, which reads like a hung build
rather than a test bug.

**Outcome**: parallel runs 8/8 green, 836 lib tests (`--features wasm`),
0 ignored, 0 warnings. One site is deliberately unmigrated —
`paths::nanopi_home_honors_env` asserts on a literal path, so it sets
the var inside the guard's scope on purpose.

The entry's option 1 ("thread the home through as a parameter") remains
the ideal and was NOT done: it is a far bigger diff, and
`paths::expand_against` already gives new tests that pattern without
touching the environment. What shipped is options 1+2 combined — one
shared guard that owns both the lock and the env, so they cannot be
acquired out of order.

---

## Provider registration from plugins

**What it is**: let a WASM extension supply an LLM provider, so a user
could add a backend without a nanopi release. Listed in ROADMAP M2's
deferred set alongside per-tool `executionMode`, hot reload, and richer
session metadata — all three of which have now shipped. This one is
different in kind, not just in size.

**Why deferred — three blockers, none of them "more code"**:

1. **`fn id(&self) -> &'static str`.** A plugin's name is a runtime
   string and cannot outlive the call. Honouring this means either
   changing the trait signature (12 implementors, 10 of them test
   doubles) or leaking. The signature was written for a set of
   providers known at compile time.

2. **Streaming inverts the data flow the sandbox is built on.** All nine
   host imports are "the guest calls out, a string comes back", and
   `plugin-capabilities.md` invariant 1 — every capability is an import,
   `handle-event`'s return value stays discarded — rests on exactly
   that. A provider must push `AgentEvent`s into a channel repeatedly
   over one call: the HOST calling the GUEST, with the guest yielding
   many times. In WIT that needs stream/future types or a polling
   export. It is a new ABI shape, not a tenth import.

3. **It inverts the sandbox's most expensive guarantee.** A provider
   holds the api_key and decides which host the request goes to. Today
   `allow_network` is a master switch, then a deny-by-default
   host-matching `url_allowlist`, a 10 s timeout, a 1 MiB cap, and no
   redirects. A plugin provider must bypass all of it to reach its own
   endpoint. That is not another grant; it is a dedicated hole.

**Trigger to revisit**: blocker 3 is a decision for the project owner,
not an implementation detail — is a plugin-supplied provider exempt from
`url_allowlist`, and if so what replaces it? Nothing should be built
until that is answered, because the answer determines whether blockers 1
and 2 are worth solving at all.

---

---

## `settings.toml`'s `thinking_level` is written but never read back

**What it is**: `/settings` shows a "Thinking level" row, cycles it, and
persists the choice to `settings.toml` via `settings_toml::save`. Nothing
ever reads it back. `App` is constructed with `thinking: None`
unconditionally (`src/mode/tui.rs`), and `grep thinking_level src/` hits
only `settings_toml.rs` (the struct + serializer) and the two
`mode/tui.rs` sites that render and cycle the row. Neither `main.rs` nor
`mode/print.rs` mentions it.

Consequences:

- Set a thinking level in `/settings`, quit, come back — it is off again.
  The setting looks persistent (it *is* on disk) and behaves as though it
  is not.
- `-p` mode has **no way at all** to enable thinking: no CLI flag, and
  this is the only config surface for it.

This is a writer with no reader — the mirror image of
`SessionEntry::ModelChange`, which had a reader with no writer for
several releases (fixed in `483aec8`). Both are the failure mode
`docs/claims-and-races.md` exists to catalogue.

**Why deferred**: the patch is a few lines; the *decision* is not. Three
questions have to be answered first, and answering them wrong is worse
than the current honest-if-useless state:

1. Should `settings.toml`'s level apply to `-p` mode, or only the TUI?
   `-p` is scripted and non-interactive, so a persisted level silently
   changing cost and latency for every scripted run is a real argument
   against.
2. After `Shift+Tab` changes the level mid-session, what wins on the next
   launch — the file, or nothing? If the cycle key also writes the file,
   the two stop being distinguishable.
3. Does a resumed session restore the level in force when it was
   suspended? `thinking_change` entries are in the transcript now
   (`483aec8`), so replay *could* reconstruct it — which is a third
   possible source of truth.

**Trigger to pick it up**: anyone reporting that the thinking setting
"does not stick", or wanting thinking in `-p`. Pick a precedence order
(suggested: CLI flag > session replay > `settings.toml` > off), write it
into the doc comment, then wire it.

**Found**: 2026-09-08, while running T7.2 of
`docs/v0.12-manual-test-plan.md` — the row needed reasoning output in
`-p` mode and there was no way to ask for any.

**Related, needs a separate yes/no rather than deferral**:
`agent::thinking::supports_thinking_rejects_older_and_unknown` asserts
`claude-haiku-4-5` does not support extended thinking. Probing the live
API shows it returns a `thinking` content block. The assertion reads as
deliberate, so it was left alone rather than flipped — but one of the two
is wrong.

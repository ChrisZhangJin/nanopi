---
phase: 260907-edb-plugin-context-contribution-stage-2
plan: 01
type: execute
wave: 1
depends_on:
  - 260907-d87-plugin-outbound-surface-stage-1
files_modified:
  - src/plugin_context.rs
  - src/lib.rs
  - src/agent/loop_.rs
  - src/agent/build.rs
  - src/mode/tui.rs
  - wit/nanopi-extension.wit
  - src/config.rs
  - src/wasm/loader.rs
  - src/wasm/mod.rs
  - config.toml.example
  - README.md
  - README_zh.md
autonomous: true
requirements:
  - CAP-CONTEXT (docs/plugin-capabilities.md §2.2 — host-set-context + allow_context, replace semantics, 4 KiB, attribution)
  - CAP-TURN-ASSEMBLY (§2.2 "appended to the system prompt at turn assembly")
  - CAP-GRANT-WARN (§3 — escalated warning for allow_context + allow_network)
  - CLAIM-REPORT (docs/claims-and-races.md §1/§2 — an over-bound contribution is refused AND reported, never silent)
  - CLAIM-DISCLOSE (a contribution is invisible by nature; the host announces set/clear through stage 1's host-notify machinery)
  - CFG-DENY-UNKNOWN (deferred stage-1 decision — #[serde(deny_unknown_fields)] on ExtensionConfig so a typo'd grant is a load error)

must_haves:
  truths:
    - "A plugin with allow_context = true can call host-set-context during an event, after Agent construction, and the text is present in the NEXT turn's system prompt."
    - "The contribution appears inside an attributed block naming the contributing plugin, so the model can tell it from the user's own instructions."
    - "A second call replaces the first; the prompt never carries both, and running ten turns does not stack ten copies."
    - "Passing \"\" clears the contribution, and the following turn's prompt is byte-identical to the base."
    - "A contribution over 4 KiB is refused with a reported `error: ` string AND the previous contribution still stands."
    - "Two plugins contributing produce two separate attributed blocks in a deterministic order."
    - "A plugin without allow_context gets an `error: ` string and keeps running; nothing enters the prompt."
    - "A guest trap does not lose the contribution or the grant — both are host-side (invariant 14)."
    - "The host announces a contribution being set or cleared once per CHANGE, not once per turn."
    - "A custom --system-prompt still gets its context-files/skills tail byte-identical to the default branch, with the contribution after it (build.rs invariants (a) and (b) intact)."
    - "A typo'd key inside [[extensions]] is a config load error naming the valid fields, not a silently ignored no-op."
  artifacts:
    - path: "src/plugin_context.rs"
      provides: "Process-wide, non-feature-gated registry of per-plugin context contributions plus the attributed-block renderer"
      contains: "pub fn render_blocks"
    - path: "src/agent/loop_.rs"
      provides: "Agent::system_base holding the composed base separately, refreshed into context.system once per turn"
      contains: "system_base"
    - path: "wit/nanopi-extension.wit"
      provides: "host-set-context import on `world extension`"
      contains: "host-set-context"
    - path: "src/config.rs"
      provides: "allow_context grant and deny_unknown_fields on ExtensionConfig"
      contains: "allow_context"
  key_links:
    - from: "src/agent/loop_.rs"
      to: "src/plugin_context.rs"
      via: "run_turn refreshing context.system from system_base + render_blocks()"
      pattern: "plugin_context::render_blocks"
    - from: "src/wasm/loader.rs"
      to: "src/plugin_context.rs"
      via: "host-set-context func_wrap closure, gated on allow_context"
      pattern: "host-set-context"
    - from: "src/wasm/loader.rs"
      to: "src/wasm/notify.rs"
      via: "host-applied disclosure line on a contribution change"
      pattern: "notify::notify"
---

<objective>
Stage 2 of `docs/plugin-capabilities.md`: `host-set-context`, gated on a
new `allow_context`, letting a plugin put attributed text in front of
the model without deciding anything. Plus the `deny_unknown_fields`
decision deferred from stage 1, so a typo'd grant stops failing
silently.

Purpose: stage 1 gave plugins memory and a voice to the user. Neither
reaches the model. §2.2 is the piece that makes a rules loader or a
preference memory actually change what the agent knows, while leaving
invariants 1-2 (no veto, no rewrite) untouched — the plugin declares
text, the host decides where it goes and whose name is on it.

Output: one new host import, one new grant, a non-feature-gated
contribution registry, a turn-assembly seam that composes the base ONCE
and concatenates contributions per turn, a host-emitted disclosure line
on every change, `deny_unknown_fields` on `ExtensionConfig`, and docs.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@docs/plugin-capabilities.md
@docs/claims-and-races.md
@.planning/quick/260907-d87-plugin-outbound-surface-stage-1-of-docs-/260907-d87-SUMMARY.md
@src/wasm/notify.rs
@src/wasm/store.rs
@src/wasm/loader.rs
@src/wasm/mod.rs
@src/config.rs
@src/agent/build.rs
@src/agent/loop_.rs
@src/subscriber.rs

Project skills that apply: `.claude/skills/tdd/SKILL.md` — test at the
seam, never at internals; expected values come from the spec's literals
(the `[context contributed by extension "memory"]` header shape, the
4 KiB bound), never recomputed the way the code computes them; one
vertical slice per cycle.

REUSE stage 1's patterns rather than inventing parallel ones:
`src/wasm/notify.rs` is the model for a process-wide `LazyLock<Mutex<…>>`
sink reachable from a synchronous `func_wrap` closure, for host-applied
attribution, and for a bound that announces its own truncation.
`src/wasm/store.rs` is the model for host-side, trap-surviving,
quota-bounded per-plugin state. `src/wasm/loader.rs`'s
`store_get_gated` / `store_set_gated` free functions are the model for a
testable gate seam behind a three-line closure.

<scope_fences>
Stage 2 ONLY. Do NOT implement `host-call-tool`, `allow_tools`,
`host-send-user-message`, or `allow_send_message` (stages 3-4). Do NOT
extend any event payload (stage 5, §5).

Do NOT change the `EventHandler` trait in `src/subscriber.rs`, and do
NOT change the `Mutex`/`try_lock` model in `loader.rs`. Both are
load-bearing for the observe-only argument in `docs/v0.12-events.md` §3.
`src/agent/loop_.rs` MAY be modified — turn assembly lives there — but
only along the prompt-assembly path. Do not touch its event-delivery or
lock paths.

Do NOT add a new WIT world. Stage 1 already established and commented
the reasoning: the linear-ladder note binds EXPORTS; `wasm-tools
component new` fails on a missing export, not an unreferenced import,
and the three committed fixtures prove it. Put `host-set-context` on
`world extension` beside the other four imports.

Do NOT bump `VERSION` and do NOT create a tag — `release.yml` publishes
an empty release and THEN fails the whole matrix on a mismatch.

Do NOT build the `/tools` per-plugin grant row. It stays deferred to
stage 3 where `allow_tools` makes it clearly necessary; §3 of the spec
already says "a later stage", so omitting it makes the document
overclaim nothing. Task 3's disclosure line is what stage 2 does
instead, and it is strictly better than a static row for this
capability.
</scope_fences>

<the_central_design_problem>
`compose_system_prompt` runs ONCE, at Agent construction
(`build.rs:251` for a fresh Agent, `build.rs:353` for a resumed one,
`tui.rs:3452` for `/reload`). A plugin calls `host-set-context` LATER —
while handling an event, after load. Injecting inside
`compose_system_prompt` therefore captures nothing: at build time no
plugin has contributed yet. THIS IS THE DECISION, made here so it is
not rediscovered mid-execution:

**Compose the base once; concatenate contributions at turn assembly.**

1. `Agent` gains `system_base: Option<String>` holding exactly what
   `compose_system_prompt` returned. `context.system` becomes a DERIVED
   value: `system_base` plus the rendered blocks. Nothing ever appends
   to `context.system`, so no turn's append can stack on the previous
   turn's, and no code has to re-derive the base by stripping a suffix
   off the assembled prompt. Suffix-stripping is explicitly rejected: it
   breaks the moment a plugin's text happens to contain the header, and
   it makes correctness depend on a string search.

2. The refresh happens once per `run_turn`, not per provider iteration
   and not per Agent construction. Per turn is the correct grain: §2.2
   calls the contribution "idempotent state", and a call the host never
   made simply means the previous contribution stands for one more turn.
   A contribution set mid-turn lands on the next turn. Say that in the
   WIT doc so a plugin author is not surprised.

3. `compose_system_prompt` is NOT re-run per turn. It reads AGENTS.md /
   CLAUDE.md and the skills tree from disk; doing that every turn is
   real I/O on the critical path of a tool whose whole point is running
   on constrained hardware. `Agent` does carry `cwd`, `skills`,
   `no_context_files`, `prompt_overrides` and the registry, so
   recomposition is possible — it is simply the wrong trade here. The
   per-turn work is a string concatenation and one mutex lock over a
   small map.

4. `build.rs`'s doc comment states two invariants: (a) a custom
   `--system-prompt` replaces only the identity/tools/guidelines
   section, with context files, skills and the cwd line still applying;
   (b) the base section always ends with the "Current working
   directory: …" line so the append/context/skills tail is
   byte-identical across both branches. Both are preserved for free by
   this mechanism, and that is the reason for choosing it: the
   contribution is concatenated onto whatever `compose_system_prompt`
   returned, AFTER both branches have converged. `compose_system_prompt`
   itself is not modified at all — its signature, its body and its tests
   stay as they are. Assert (a)/(b) survive with a test that runs the
   custom-prompt branch with a contribution active.

5. The refresh is placed BEFORE `maybe_compact` in `run_turn`, so
   `estimate_chars` counts the contribution. It enters every request; a
   compaction decision that pretends it is free would understate the
   context by up to 4 KiB per plugin.

6. The registry lives in a NEW non-feature-gated `src/plugin_context.rs`
   at the crate root, following the precedent `src/subscriber.rs`
   documents in its own module doc: the vocabulary lives in an
   unconditionally compiled module so `src/agent/loop_.rs` stays free of
   `#[cfg(feature = "wasm")]`, and the plugin layer reaches into it.
   Without the feature the registry is always empty and
   `render_blocks()` returns `""`, so the non-wasm build's prompt is
   byte-identical to today's.
</the_central_design_problem>

<interfaces>
<!-- Extracted from the codebase. Use these directly; no exploration. -->

`src/wasm/loader.rs` — `PluginState` after stage 1. `allow_context`
joins it:

```rust
pub struct PluginState {
    url_allowlist: Vec<String>,
    cwd: PathBuf,
    allow_fs: bool,
    allow_network: bool,
    allow_store: bool,
    store: Arc<crate::wasm::store::PluginStore>,
    plugin_name: Arc<str>,
}
```

`PluginState` is built in TWO places and both must be updated, or the
capability works until the first trap and then dies silently:
`PluginEngine::load` (~line 809) and `PluginRebuild::build` (~line 1223).
`PluginRebuild` (~line 1184) carries the ingredients across a reset and
needs the same field. Stage 1 hit exactly this and its teeth-reversion
#6 pins it; add the analogous test here.

Stage 1 also extracted `PluginEngine::build_linker` (~line 603), called
by both `load` and by tests — the new `func_wrap` goes there, once, and
the recovery path inherits it. `load` is already 9 positional
parameters; adding `allow_context: bool` touches ~20 mechanical call
sites in `tests/wasm_plugin_integration.rs` and `src/wasm/loader.rs`.

The gate-seam shape to copy verbatim (~line 83):

```rust
fn store_get_gated(allow_store: bool, store: &PluginStore, key: &str) -> String {
    if !allow_store {
        return STORE_DENIED.to_string();
    }
    // …
}
```

with `const STORE_DENIED: &str = "error: store access denied (set \
allow_store = true on this plugin's [[extensions]] entry)";` (~line 118)
as the message template.

`src/wasm/notify.rs` — stage 1's sink; the disclosure line reuses it:

```rust
pub fn notify(plugin: &str, text: &str) -> Result<(), String>;  // host applies "[{plugin}] "
pub const MAX_NOTIFY_PER_TURN: usize = 10;
```

`src/config.rs` — `ExtensionConfig` is `#[derive(Deserialize)]` +
`#[serde(default)]` with a hand-written `Default` impl (~line 224).
Fields today: `path`, `max_files`, `allow_network`, `allow_fs`,
`allow_store`, `url_allowlist`, `events`.

`src/settings.rs:47-58` — the ONLY `deny_unknown_fields` in the crate
today, with the doc comment that is the precedent to follow:

```rust
/// `deny_unknown_fields` is what turns a retired hook key
/// (…) into a parse error instead of a silently-ignored no-op. …
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksSection { … }
```

`src/config.rs:449-473` — the pin stage 1 deliberately left:
`a_typod_extension_grant_is_currently_ignored_not_refused`, whose doc
comment describes this gap and says flipping it must be deliberate and
visible. THIS is the flip.

`src/agent/loop_.rs` — `Agent` (fields ~line 100-148) and
`run_turn` (~line 670). The refresh point is immediately before
`self.maybe_compact(tx).await;` at ~line 679. The provider reads
`ctx.system` at request build (`provider/anthropic.rs:189`,
`provider/openai.rs:304`), reached through `self.provider
.stream_turn(&self.context, …)` at ~line 1083/1091.

`context.system` is NOT persisted to the session file — session entries
carry messages and tool calls only — so a derived value in it cannot
contaminate `--continue`. Confirm this in Task 2 rather than assuming
it; if a persistence path is found, the plan is wrong and the developer
should be told.

`src/agent/build.rs` — the three base-setting sites:
- `build_fresh`, ~line 251 composes `prompt`, ~line 261 sets
  `system: Some(prompt)` in the `Context` literal.
- `hydrate_resumed`, ~line 351: `if self.context.system.is_none() { … }`
  — this guard must become a `system_base` guard.
- `src/mode/tui.rs:3452` (`/reload`): `a.context.system = Some(…)`.
</interfaces>
</context>

<tasks>

<task type="auto" tdd="true">
  <name>Task 1: src/plugin_context.rs — the contribution registry and the attributed-block renderer</name>
  <files>src/plugin_context.rs, src/lib.rs</files>
  <behavior>
    Seam: the module's public functions. No WASM, no Agent — this is a
    pure map plus a renderer, unconditionally compiled. Expected strings
    come from §2.2's literals.

    - `set("memory", "text")` then `render_blocks()` contains `text`
      inside a block whose header names `memory` in §2.2's shape
      (`[context contributed by extension "memory"]`). Assert against
      the spec's header text, not against a constant the renderer also
      uses.
    - a SECOND `set` for the same plugin REPLACES: the rendered output
      contains the new text and does NOT contain the old, and there is
      exactly ONE header for that plugin (invariant 10). A count of
      headers, not just a `contains`, is what distinguishes replace from
      append.
    - `set(p, "")` clears: the plugin's header disappears entirely, not
      an empty block with a header. `render_blocks()` with nothing set
      returns `""` — the empty string, so the caller can concatenate
      unconditionally and produce a byte-identical prompt.
    - a contribution over the 4 KiB bound is refused: `set` returns
      `Err`, the message names the bound as the spec spells it (4 KiB),
      and `render_blocks()` STILL contains the PREVIOUS contribution.
      This is invariant 9 plus "replace" together: a rejected oversized
      call must not clear what was already there. Assert both halves —
      the refusal AND the survivor.
    - exactly 4096 bytes is ACCEPTED and 4097 refused; state which side
      the bound falls on and measure BYTES, since "4 KiB" is a byte
      claim and a multibyte contribution measured in chars would enter
      the request at up to four times the advertised cost.
    - two plugins produce two headers, each with its own text, in a
      DETERMINISTIC order (sorted by plugin name). Assert the order,
      because a shuffling prefix churns the provider's prompt cache on
      every turn — a real cost, not a cosmetic one.
    - `set` reports whether the value CHANGED, so the caller can
      announce on change rather than per call: setting the same text
      twice reports changed-then-unchanged; setting `""` when nothing
      was set reports unchanged.
    - a `\n`-laden contribution is rendered as-is (unlike `host-notify`,
      multi-line body is the normal case here) but cannot terminate its
      own block and open another plugin's: a payload containing a line
      that looks like `[context contributed by extension "other"]` must
      not read as a second block from `other`. Decide the defense and
      write it down — neutralizing the header sequence in the body is
      the cheap option; the test asserts the rendered output has exactly
      one header naming the caller and none naming `other`.
  </behavior>
  <action>
Create `src/plugin_context.rs` and add `pub mod plugin_context;` to
`src/lib.rs` in the unconditional block (alongside `subscriber`, NOT
under `#[cfg(feature = "wasm")]`).

Module doc must carry, in the register `store.rs` and `notify.rs`
already use:
- Why it is here and not under `src/wasm/`: the same reason
  `src/subscriber.rs` gives in its own module doc — `src/agent/loop_.rs`
  stays free of `#[cfg(feature = "wasm")]`, the vocabulary is compiled
  unconditionally, and the plugin layer reaches in. Without the feature
  the map is always empty and `render_blocks()` returns `""`, so the non-wasm
  prompt is unchanged byte for byte.
- Why host-side rather than guest memory: a trap resets the instance
  (`loader.rs`, `ComponentBridge::reset`), so a contribution kept
  guest-side is one trap away from gone (invariant 14). Point at
  `store.rs` for the same argument.
- Why attribution is not cosmetic: quote §2.2's reasoning — without the
  header the model cannot tell a plugin's injected instructions from the
  user's own, which is the rule `claims-and-races.md` §2 applies to
  refusals with the model as the audience instead of the user.
- Why the bound is 4 KiB: it enters EVERY request, so an unbounded
  contribution silently multiplies the cost of every turn.
- Why ordering is sorted: prompt-prefix stability, i.e. provider prompt
  caching.

Shape:
- `static CONTRIBUTIONS: LazyLock<Mutex<BTreeMap<String, String>>>` —
  `BTreeMap` IS the sorted-order mechanism; say so in a comment so
  nobody "optimizes" it into a `HashMap`. Lock helper recovers from
  poisoning the way `notify::lock` does, with the same reasoning
  (a poisoned map should not silently disable the capability for the
  rest of the session).
- `pub const MAX_CONTEXT_BYTES: usize = 4096;` with the "enters every
  request" comment.
- `pub fn set(plugin: &str, text: &str) -> Result<bool, String>` — `Ok(true)`
  when the stored value changed, `Ok(false)` when it was already that.
  `Err` carries the message body WITHOUT the `error: ` prefix; the
  caller in `loader.rs` adds it exactly once, matching how
  `PluginStore::set` and `resolve_readable` hand back bare messages.
  Check the bound BEFORE touching the map so a refusal mutates nothing.
- `pub fn render_blocks() -> String` — `""` when empty; otherwise each
  plugin's block, header then body, blocks separated by a blank line.
  The leading separator belongs to the CALLER (Task 2) or to this
  function — pick one, state it in the doc, and have the "nothing set
  produces a byte-identical prompt" test in Task 2 pin it.
- `pub fn clear_all()` for test isolation, and reuse `crate::TEST_LOCK`
  in the test module exactly as `notify.rs` does — the map is
  process-wide, so these tests must not interleave.

TDD: one behavior per cycle, red before green. Before implementing the
bound, confirm the "refused oversized keeps the previous" test FAILS
against a variant that inserts first and validates after — a test that
cannot distinguish those two is not testing invariant 9.
  </action>
  <verify>
    <automated>cargo test plugin_context && cargo test --features wasm plugin_context</automated>
  </verify>
  <done>`src/plugin_context.rs` exists with `pub fn render_blocks`, declared unconditionally in `src/lib.rs`; every behavior above has a test; both commands pass; the "refused keeps the previous" and "replace not append" tests were each observed failing against the pre-implementation behavior.</done>
</task>

<task type="auto" tdd="true">
  <name>Task 2: Turn assembly — Agent::system_base, refreshed into context.system once per turn</name>
  <files>src/agent/loop_.rs, src/agent/build.rs, src/mode/tui.rs</files>
  <behavior>
    Seam: `Agent`'s prompt fields plus one refresh method, driven
    directly. Do NOT test by reaching into the provider — assert on
    `agent.context.system` after the refresh, which is exactly what
    `stream_turn` reads.

    - with no contribution set, the refresh leaves `context.system`
      BYTE-IDENTICAL to what `compose_system_prompt` returned. Assert
      equality against `system_base`, not merely `contains` — this is
      the test that guarantees stage 2 changes nothing for a user with
      no plugins.
    - a contribution set AFTER the Agent is built appears in
      `context.system` after the refresh. This is the whole point of the
      task: it must be observably impossible for a build-time-only
      injection to pass it.
    - refreshing TEN times with the same contribution active leaves
      exactly ONE attributed block. Count headers. This is the
      no-stacking test and it is the one that fails if anything appends
      to `context.system` instead of to `system_base`.
    - clearing the contribution and refreshing returns `context.system`
      to byte-identical with `system_base`. No residue, no stray blank
      lines.
    - with a custom `--system-prompt` in `PromptOverrides` AND a
      contribution active, `context.system` still ends the base section
      with the `Current working directory: …` line and still carries the
      context-files/skills tail, with the contribution AFTER all of it.
      This pins `build.rs`'s documented invariants (a) and (b) against
      this change specifically.
    - `hydrate_resumed` sets `system_base` when it is absent and leaves
      it alone when present (the existing guard's behavior, moved to the
      right field).
    - `/reload`'s recomposition replaces the BASE and leaves an active
      contribution standing — a plugin's contribution is not a skill and
      `/reload` does not reload extensions (the handler's doc comment
      already says so).
  </behavior>
  <action>
`src/agent/loop_.rs`:
- add `pub system_base: Option<String>` to `Agent`, with a doc comment
  stating the whole design decision in the terms
  `<the_central_design_problem>` uses: this holds what
  `compose_system_prompt` returned; `context.system` is DERIVED from it
  plus `plugin_context::render_blocks()`; nothing appends to
  `context.system`, because re-deriving the base by stripping a suffix
  breaks the moment a plugin's text contains the header. Mention that
  `compose_system_prompt` is deliberately NOT re-run per turn (it reads
  context files and the skills tree from disk, which is real I/O on a
  constrained-hardware critical path) and that the per-turn cost is one
  concatenation plus one small mutex.
- add two methods:
  - `pub fn set_system_base(&mut self, base: String)` — stores the base
    and refreshes. The single door through which a base is set, so the
    two fields cannot drift.
  - `pub fn refresh_system_prompt(&mut self)` —
    `context.system = system_base + render_blocks()`, unconditionally
    compiled, no `cfg`. With no contributions this must produce the base
    unchanged; that is what Task 1's `""`-return contract buys.
- call `self.refresh_system_prompt();` in `run_turn` immediately BEFORE
  `self.maybe_compact(tx).await;` (~line 679), with a comment giving
  both reasons: the contribution enters every request so compaction must
  see its cost, and once per turn rather than per iteration is the grain
  §2.2's idempotent-state framing asks for (a contribution set mid-turn
  lands on the next turn).

`src/agent/build.rs`:
- `build_fresh` (~line 251-261): keep composing `prompt` exactly as
  today; set `system_base: Some(prompt.clone())` and
  `system: Some(prompt)` in the literal. `compose_system_prompt` itself
  is NOT modified — not its signature, not its body, not its tests.
- `hydrate_resumed` (~line 351): change the guard from
  `if self.context.system.is_none()` to `if self.system_base.is_none()`
  and route the result through `set_system_base`. Leave the surrounding
  ordering comments intact; they are about plugin/tool registration and
  still apply.

`src/mode/tui.rs` (~line 3452): replace the direct
`a.context.system = Some(compose_system_prompt(…))` with
`a.set_system_base(compose_system_prompt(…))`. Add one line to
`handle_reload`'s doc comment: it rebuilds the BASE, and an active
plugin contribution survives, consistent with `[[extensions]]` not being
reloaded.

Then fix the remaining `Agent` construction sites the compiler names —
add `system_base` to each. Purely mechanical; change no assertions.

BEFORE writing any of it, confirm the claim this design rests on:
`context.system` is not written to the session file. Grep the session
writer and the `SessionEntry` variants. If any path persists it, STOP
and report — a derived value leaking into `--continue` would bake a
plugin's contribution in permanently, and the design would need the
restore-after-turn variant instead.
  </action>
  <verify>
    <automated>cargo test --features wasm && cargo test</automated>
  </verify>
  <done>`Agent::system_base` exists and is the only thing a base is written to; `run_turn` refreshes once per turn before compaction; the no-contribution prompt is byte-identical to before; ten refreshes leave one block; the custom-prompt invariants (a)/(b) are pinned by a test; `context.system` confirmed absent from session persistence; both suites pass.</done>
</task>

<task type="auto" tdd="true">
  <name>Task 3: host-set-context + allow_context, the change disclosure, and deny_unknown_fields on ExtensionConfig</name>
  <files>wit/nanopi-extension.wit, src/config.rs, src/wasm/loader.rs, src/wasm/mod.rs, config.toml.example, README.md, README_zh.md</files>
  <behavior>
    Seams: a `set_context_gated` free function in `loader.rs` (the
    closure over it stays three lines of plumbing), `PluginRebuild::build`,
    `PluginHost::load_all`, and `Config` parsing.

    - with `allow_context = false`, `set_context_gated` returns a string
      starting `error: ` that names `allow_context` and the
      `[[extensions]]` entry — and NOTHING enters the registry
      (assert `render_blocks()` is still empty afterwards, not just that
      the return was an error).
    - with `allow_context = true`, it returns `""` on success.
    - a registry-level refusal (over 4 KiB) surfaces with the `error: `
      prefix added exactly once — no `error: error: `. Same assertion
      stage 1 makes for the store.
    - a rebuilt `PluginState` after a trap carries `allow_context` AND
      the contribution is still rendered — drive `PluginRebuild::build`
      directly, as stage 1's
      `a_rebuild_after_a_trap_keeps_the_store_and_its_grant` does.
      Invariant 14 and §4's "plugin traps mid-`host-set-context`" row.
    - a successful CHANGE emits exactly one notify line naming the
      plugin, distinguishing set from clear; a repeat call with the same
      text emits NOTHING. Assert the count, since a per-turn or
      per-call announcement would repeat forever and train the user to
      ignore it.
    - a REFUSED call emits no disclosure line: nothing changed, so
      announcing a change would be a false claim.
    - `allow_context = true` together with `allow_network = true`
      produces a `Notice::warn` in the `[Extensions]` block naming both
      grants — a plugin that writes the agent's instructions from a
      remote source can shape its behaviour.
    - `allow_context` parses and defaults to `false`.
    - a typo'd key inside `[[extensions]]` (`allow_contxt = true`) is now
      a config LOAD ERROR whose message names the field and the valid
      alternatives, replacing
      `a_typod_extension_grant_is_currently_ignored_not_refused`.
      Assert on the error, and assert a config using every VALID key
      still loads — the second half is what catches an accidentally
      renamed field.
  </behavior>
  <action>
WIT first. Add to `world extension` in `wit/nanopi-extension.wit`,
beside stage 1's imports and under the existing no-new-world comment,
signature verbatim from §2.2:

    import host-set-context: func(text: string) -> string;

Doc comments at the density of the other four: the `allow_context`
gate; REPLACES rather than appends, so it is idempotent state and a
dropped delivery means the previous contribution stands for one more
turn; `""` clears; the 4 KiB bound and WHY (it enters every request);
that the host applies the attribution header so the model can tell the
plugin's instructions from the user's; that a refusal is in-band and
leaves the PREVIOUS contribution standing; and that the contribution
takes effect at the NEXT turn's assembly, not mid-turn.

`src/config.rs`:
- add `pub allow_context: bool` to `ExtensionConfig` and
  `allow_context: false` to the `Default` impl. Doc comment says why it
  is gated: the plugin writes text the model reads as instruction, and
  combined with `allow_network = true` it can shape the agent's
  behaviour from a remote source, which is why that combination warns
  at load (§3).
- add `deny_unknown_fields` to the existing `#[serde(default)]` on
  `ExtensionConfig`, with a doc comment in the register of
  `settings.rs:47-58`: what it turns from a silent no-op into an error
  (a typo'd grant — `allow_stroe = true` reads as a plugin the user
  believes they granted and did not), that this was a deliberate
  deferred decision from stage 1 rather than an oversight, and the
  acknowledged cost (a config carrying a stray key that loads today
  will now fail loudly). No alias to soften it, same as the retired hook
  keys.
- DELETE `a_typod_extension_grant_is_currently_ignored_not_refused` and
  replace it with a test pinning the new behavior. Do not leave both.
  The replacement's doc comment should record that it supersedes the
  stage-1 pin, so the history is legible.
- BEFORE flipping the attribute, verify nothing documented breaks:
  every `[[extensions]]` key appearing in `config.toml.example`,
  `README.md`, `README_zh.md`, `docs/v0.12-events.md`,
  `docs/v0.12-manual-test-plan.md`, `docs/pi-vs-nanopi.md` and any test
  fixture. The survey done during planning found only `path`,
  `max_files`, `allow_network`, `allow_fs`, `allow_store`,
  `url_allowlist`, `events` — all valid — plus `allow_tools` in
  `docs/plugin-capabilities.md` §2.5, which is a stage-3 SPEC example
  and never loaded, so it is not a break. RE-RUN the survey anyway
  (`grep -rn -A8 '\[\[extensions\]\]'` across those files) and if any
  loaded example carries an undefined key, that example is the bug —
  fix the example, and say which file in the SUMMARY.

`src/wasm/loader.rs`:
- add `allow_context: bool` to `PluginState` (~line 46) AND to
  `PluginRebuild` (~line 1184); set it in BOTH `PluginEngine::load`
  (~line 809) and `PluginRebuild::build` (~line 1223). Missing the
  second is stage 1's exact near-miss: the capability works until the
  first trap, then dies silently.
- extend `PluginEngine::load` with `allow_context: bool` and update its
  call sites (`src/wasm/mod.rs::load_all` plus the ~20 mechanical ones
  in `tests/wasm_plugin_integration.rs` and `loader.rs`'s own tests).
- add a `set_context_gated(allow_context: bool, plugin_name: &str, text: &str) -> String`
  free function beside `store_set_gated`, and a
  `const CONTEXT_DENIED: &str` message in the shape of `STORE_DENIED`.
  Gate first, then the operation, matching `host-http-get`'s order.
  On `Ok(true)` (changed), emit the disclosure through
  `crate::wasm::notify::notify(plugin_name, …)` — one line saying the
  contribution was set (with its size) or cleared. On `Ok(false)`
  nothing is emitted. On `Err`, prefix with `error: ` exactly once and
  emit nothing. Comment WHY the disclosure exists: a context
  contribution is invisible by nature — the user never sees the system
  prompt — so a plugin silently rewriting the agent's instructions is
  precisely what `claims-and-races.md` exists to prevent, and stage 1's
  `host-notify` machinery is already in place. Comment that it announces
  on CHANGE, not per turn, so the line does not repeat forever, and note
  the accepted side effect: the disclosure consumes one of the plugin's
  own `MAX_NOTIFY_PER_TURN` lines.
- wire the `func_wrap` in `build_linker` (~line 603) so `load` and the
  post-trap rebuild inherit the same linker. Return `Ok((String,))` in
  every branch; nothing traps.

`src/wasm/mod.rs::load_all`:
- pass `cfg.allow_context` through to `engine.load`.
- add the escalated `Notice::warn` for `allow_context && allow_network`
  beside the existing `events && allow_network` and
  `allow_store && allow_network` ones, before the directory paths
  expand — same reason as stage 1: a directory entry must not repeat the
  warning once per file.

Docs, each in the register it already uses:
- `config.toml.example`: an `allow_context` entry in the Fields block
  (what it does, replace-not-append, the 4 KiB bound and why, that the
  text is attributed to the plugin in the prompt, the `allow_network`
  combination warning) and a commented `[[extensions]]` example — extend
  the existing `memory.wasm` block (~line 204), which already has
  `allow_store` + `events`, since a preference memory is exactly §2.2's
  motivating case. Also add a line to the section preamble noting that
  an unknown key inside `[[extensions]]` is now a load error.
- `README.md` / `README_zh.md`: one row in the host-function table
  (~line 292 / ~line 287) and a sentence after the store paragraph
  covering attribution, replace semantics and the bound. Keep the two in
  content parity; per project convention the Chinese docs may discuss
  restricted-network workarounds and the English ones never mention
  them — neither applies here.
  </action>
  <verify>
    <automated>cargo test --features wasm && cargo test</automated>
  </verify>
  <done>WIT declares `host-set-context` on `world extension` with no new world; `allow_context` exists on `ExtensionConfig` defaulting false; `ExtensionConfig` carries `deny_unknown_fields` and the old typo pin is REPLACED by one pinning the error; the import is gated, linked through `build_linker`, and carried across `PluginRebuild`; a change emits exactly one attributed disclosure line and a no-op emits none; the `allow_context` + `allow_network` warning appears; every documented example config still loads; docs updated in all three files; both suites pass.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| guest → host import | a fully attacker-controlled string crosses into host code and is then read by the MODEL as instruction |
| host → model | plugin-authored text enters the system prompt of every request |
| host → user | the disclosure line is rendered into the user's scrollback |
| config file → host | a user-authored `[[extensions]]` table decides which grants exist |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-edb-01 | Spoofing | plugin text posing as the user's own instructions to the model | mitigate | The host writes the attribution header; the body never supplies it, and a body containing a header-shaped line cannot open a second block (Task 1). §2.2, invariant 11. |
| T-edb-02 | Spoofing | one plugin's contribution attributed to another | mitigate | The key is the `.wasm` file stem the host computed in `load_all`; the guest never names the plugin (Tasks 1, 3). |
| T-edb-03 | Denial of Service | unbounded contribution inflating every request | mitigate | 4 KiB per plugin, in BYTES, checked before the map is touched; refusal is in-band and mutates nothing (Task 1). |
| T-edb-04 | Denial of Service | per-turn recomposition doing disk I/O on the critical path | mitigate | The base is composed once; the per-turn work is a concatenation plus one small mutex. `compose_system_prompt` is not re-run (Task 2). |
| T-edb-05 | Tampering | a contribution silently accumulating turn over turn | mitigate | `context.system` is derived from `system_base`, never appended to; the ten-refresh header count pins it (Tasks 1, 2). Invariant 10. |
| T-edb-06 | Repudiation | a `""` return for a call that changed nothing / a refusal reported as success | mitigate | `""` only after the map accepted the value; an over-bound call returns `error: ` and the previous contribution survives (Tasks 1, 3). Invariant 9. |
| T-edb-07 | Information Disclosure | a plugin rewriting the agent's instructions unseen | mitigate | Host-emitted disclosure line on every change, through stage 1's `host-notify` (Task 3). Stands in for the deferred `/tools` grant row. |
| T-edb-08 | Elevation of Privilege | `allow_context` + `allow_network` — instructions shaped from a remote source | accept + warn | Both are explicit user grants; the combination gets the escalated `[Extensions]` warning (Task 3). |
| T-edb-09 | Tampering | a typo'd grant silently granting nothing | mitigate | `deny_unknown_fields` on `ExtensionConfig` (Task 3). The gap stage 1 pinned. |
| T-edb-SC | Tampering | npm/pip/cargo installs | n/a | No new dependencies. If any task finds it needs one, stop and run the legitimacy gate before installing. |
</threat_model>

<verification>
Both suites, since the imports are behind the feature flag and the
config fields are not:

```bash
cargo test --features wasm
cargo test
```

Teeth check — standing project rule, not optional. Each reversion is
applied to the real code, run, observed failing with its message
recorded, then reverted. This project has caught two vacuous tests, so
"it passes" is not evidence on its own.

| # | Put this old behaviour back | Test that must go RED |
|---|---|---|
| 1 | Inject `render_blocks()` inside `compose_system_prompt` instead of at turn assembly | the "contribution set AFTER build appears in the next turn's prompt" test — this is the reversion that proves the central design decision was necessary rather than stylistic |
| 2 | Append to `context.system` instead of deriving it from `system_base` | the ten-refreshes-one-block test |
| 3 | Make `set` append to the existing value instead of replacing | the replace test's header count (invariant 10) |
| 4 | Move the 4 KiB check after the map insert | the "refused oversized, previous still stands" test (invariant 9) |
| 5 | Remove the `allow_context` gate from `set_context_gated` | the denial test — including its assertion that the registry stayed empty |
| 6 | Drop the `PluginRebuild` half of the `allow_context` wiring | the post-trap rebuild test (mirrors stage 1's reversion 6) |
| 7 | Remove `deny_unknown_fields` from `ExtensionConfig` | the new typo-is-a-load-error test |
| 8 | Drop the attribution header from `render_blocks` | the attributed-block test (invariant 11) |
| 9 | Announce the contribution on every refresh instead of on change | the "one line per change, none on a repeat" count test |
| 10 | Swap the `BTreeMap` for a `HashMap` | the deterministic two-plugin ordering test (may need repeated runs to red; if it cannot be made reliably red, say so rather than claiming it was verified) |

Also confirm the additive-imports property the way stage 1 did — the
three committed fixtures predate all five imports:

```bash
cargo test --features wasm --test wasm_plugin_integration
```

A green run there IS the demonstration that a fifth import on
`world extension` broke no old guest and needed no fourth world. Call
it out in the SUMMARY, and confirm the WIT file still declares exactly
three worlds.
</verification>

<success_criteria>
- `cargo test --features wasm` and `cargo test` both green.
- With no plugin contributing, `context.system` is byte-identical to
  what it is today — stage 2 costs a user with no plugins nothing.
- The three committed fixtures still load and run.
- Every must-have truth has a test, and each teeth reversion in the
  table above was observed failing, with its message recorded in the
  SUMMARY.
- No change to `src/subscriber.rs`, the `EventHandler` signature, or the
  `Mutex`/`try_lock` model. `src/agent/loop_.rs` changed only along the
  prompt-assembly path.
- No new WIT world. No event payload extended. `VERSION` unchanged, no
  tag.
- `build.rs`'s two documented custom-prompt invariants are intact and
  pinned by a test; `compose_system_prompt` itself is unmodified.
- `docs/plugin-capabilities.md` is NOT edited to narrow the spec. If
  something in it turns out not to match the code, say so in the SUMMARY
  and let the developer rule.
</success_criteria>

<source_audit>
## Multi-Source Coverage Audit

| Source | Item | Covered by |
|---|---|---|
| GOAL | Stage 2: `host-set-context` + `allow_context` | Tasks 1-3 |
| GOAL | fold in the deferred `deny_unknown_fields` decision | Task 3 |
| REQ | §2.2 import signature, gate, `""` clears, in-band error | Task 3 |
| REQ | §2.2 replace-not-append (idempotent state under dropped delivery) | Tasks 1, 2 |
| REQ | §2.2 4 KiB per plugin, and why | Task 1 |
| REQ | §2.2 attribution header, and why it is not cosmetic | Task 1 |
| REQ | §2.2 "appended to the system prompt at turn assembly" | Task 2 (the central design problem) |
| REQ | §3 `allow_context` row, off by default, two-sided | Task 3 |
| REQ | §3 escalated warning for the `allow_network` combination | Task 3 |
| SPEC | §4 "`host-set-context` called, then the plugin traps → contribution survives" | Tasks 1, 3 |
| SPEC | §4 "two plugins both `host-set-context` → two blocks, capped separately" | Task 1 |
| SPEC | §4 "plugin reset after a trap → the contribution is not lost" | Task 3 |
| SPEC | Invariant 10 (replaces, never accumulates) | Tasks 1, 2; reversions 2-3 |
| SPEC | Invariant 11 (attributed in the prompt) | Task 1; reversion 8 |
| SPEC | Invariant 14 (host-side state survives a trap) | Task 3; reversion 6 |
| SPEC | Invariants 3, 9 (in-band refusal, always reported) | Tasks 1, 3; reversion 4 |
| SPEC | Required tests → Context, all six bullets | Task 1 (blocks, replace, clear, over-bound, two plugins) + Task 3 (trap) |
| SPEC | Staging table: stage 2 is exactly `host-set-context` | `<scope_fences>` |
| CLAIMS | §1 proof vs observation — no claim beyond what the mechanism delivers | doc comments in Tasks 1-3; `note!`/`Notice` conventions |
| CLAIMS | a refusal is always reported, never silent | Tasks 1, 3; reversions 4-5 |
| CONTEXT | disclosure requirement replacing the deferred `/tools` row | Task 3 (announce on CHANGE, via stage 1's `host-notify`) |
| CONTEXT | reuse stage 1's patterns, do not invent parallel ones | `<context>`, `<interfaces>`; Task 1 follows `notify.rs`, Task 3 follows `store_*_gated` |
| CONTEXT | `PluginState` is built in TWO places | Task 3; reversion 6 |
| CONTEXT | verify `deny_unknown_fields` breaks no documented example | Task 3 (survey re-run, with the planning-time result recorded) |
| CONTEXT | tests must have teeth, each reversion named | `<verification>` table, 10 rows |
| CONTEXT | do not touch `subscriber.rs` / the lock model; `loop_.rs` only for prompt assembly | `<scope_fences>`, success criteria |
| CONTEXT | no `VERSION` bump, no tag | `<scope_fences>` |
| CONTEXT | no `/tools` grant row | `<scope_fences>` |

Out of scope by decision (not gaps): §2.5 `host-call-tool` /
`allow_tools`, §2.4 `host-send-user-message` / `allow_send_message`, §5
`message_end` payload — stages 3-5. The `/tools` grant row — stage 3,
where `allow_tools` makes the pipe unavoidable; §3 already states the
gap rather than the aspiration, so leaving it out overclaims nothing.

No unplanned items. Nothing was reduced to a "v1" or a placeholder.

### Notes, not tasks

1. **Wiki.** Per project convention the user-facing plugin docs live in
   the separate wiki repo, unreachable from here. After this ships the
   wiki's plugin page needs `allow_context`, `host-set-context`, and the
   fact that an unknown `[[extensions]]` key is now a load error. Record
   it as a follow-up in the SUMMARY.
2. **`deny_unknown_fields` is a behaviour change for existing users.**
   A config with a stray key that loads today will fail after this. That
   is the point, and the user asked for it, but the SUMMARY should say
   so plainly so it can reach release notes.
3. **Stage 1 follow-up 4 still stands** (`host-notify` in `-p` mode
   falls back to `note!` and is subject to the T4.7 wipe). The
   disclosure line inherits that. Not fixed here; worth restating in the
   SUMMARY since stage 2 adds a second consumer of the same path.
</source_audit>

<output>
Create `.planning/quick/260907-edb-plugin-context-contribution-stage-2-of-d/260907-edb-SUMMARY.md` when done.
</output>

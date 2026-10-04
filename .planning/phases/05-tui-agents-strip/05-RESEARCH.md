# Phase 5: TUI agents strip - Research

**Researched:** 2026-10-04
**Domain:** Rust TUI rendering (ratatui, inline viewport), in-process agent registry snapshotting
**Confidence:** HIGH (codebase-verified; no external library research needed — ratatui API already in use, no version change)

## Summary

This phase adds a display-only "agents strip" between the status line and the
input box in the existing inline-viewport TUI. The mechanics are already
established by two things already in the codebase: (1) `draw_dock` in
`src/mode/tui.rs` composes the dock from `Layout` constraints and is a pure
`fn(&mut Buffer, Rect, &App)` function, directly unit-testable with
`ratatui::backend::TestBackend` / a bare `Buffer::empty(rect)` — no terminal
needed; (2) `AgentRegistry::snapshot()` already returns `Vec<AgentEntry>` with
`id`, `pid`, `state`, `started: Instant`, `dir: PathBuf` — enough for state
glyph and elapsed time, but NOT description/role/task/activity/worktree/report
path. Those richer fields exist only via `describe_entry` in
`src/tool/agent_ctl.rs`, which re-reads `brief.md`/`report.md` front matter
from disk on every call (CTL-04's `list_agents` tool). The strip must do the
same disk read, cheaply, on the 120ms tick — or (recommended) only on
snapshot change, to avoid filesystem IO on every tick.

The two things that make this phase nontrivial are not rendering: they are
(1) the **inline viewport height is a compile-time constant** (`DOCK_HEIGHT:
u16 = 10`), so adding a variable-height strip requires either growing
`Viewport::Inline(N)` dynamically or capping the strip within the existing
budget and reducing other rows; and (2) **state/activity data for the strip
must come from a cheap, non-blocking source** compatible with the "redraw
only on tick, not per event" requirement (UI-04, D-07) — meaning brief/report
re-parsing must be throttled, not done unconditionally every 120ms per agent.

**Primary recommendation:** Extend `AgentEntry`/registry with an in-memory
cache of description + latest-activity + report path + worktree/branch +
turns/tokens, refreshed opportunistically (on state transition, not every
tick), so `draw_dock`'s new strip-drawing function reads only in-memory data
each tick — zero disk IO on the hot path. Grow the inline viewport
dynamically using `ratatui`'s existing `Viewport::Inline` resize support
(already proven by `max_input_lines`/`input_content_h` growing/shrinking the
input box per-frame) rather than inventing a new mechanism.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Agent state/elapsed tracking | Registry (`src/agent_registry.rs`) | — | Already the source of truth for live process state; in-process, no IO |
| Agent description/activity/report path | Registry (new cache fields) | Disk (`brief.md`/`report.md`) | Keeps the TUI's hot path (tick) IO-free; disk is refreshed on transition, not per-frame |
| Strip layout + glyphs + truncation | TUI render (`src/mode/tui.rs::draw_dock` + new `draw_agents_strip`) | — | Pure rendering, no business logic |
| Expand/collapse state + scroll offset | TUI `App` struct | — | UI-only ephemeral state, not registry state |
| Keybinding (Ctrl+G) | `src/keys.rs` (`ActionId`) + `interpret_key` in `tui.rs` | `src/settings.rs`/`settings_toml` | Matches existing `ExpandLastTool` (Ctrl+O) pattern exactly |

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| ratatui | (pinned in Cargo.toml, 0.2x — NOT upgrading to 0.30 per REQUIREMENTS.md Out of Scope) | TUI widgets, `Buffer`, `Layout`, `Paragraph`, `Viewport::Inline` | Already the project's only TUI crate; no new dependency needed |
| crossterm | (pinned, already a dependency) | Key events (`KeyCode::Char('g')`, `KeyModifiers::CONTROL`) | Already used for every other keybinding |

No new crates are required for this phase — confirmed by inspecting
`src/mode/tui.rs`, `src/keys.rs`, `src/render/panel.rs`: every primitive
needed (Buffer cell writes, Layout::Vertical, Span/Line styling, Instant
elapsed math) is already imported and used for the status strip and input
box. This directly supports QA-02 ("no unjustified new crates", ~150 KB
budget).

### Supporting
None — no supporting libraries needed.

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| In-memory registry cache for description/activity | Re-read brief.md/report.md every tick (8/sec) | Simple, but disk IO on every 120ms tick for every live agent violates D-07's "no redraw storm" spirit and risks latency spikes under many agents + slow disks |
| Dynamic `Viewport::Inline` resize | Fixed `DOCK_HEIGHT` with strip squeezed into existing rows | Fixed height is simpler but directly contradicts D-02/D-04 ("1-3 lines collapsed", "up to ~40% of screen expanded") which require variable height |

**Installation:** None — no new dependencies.

**Version verification:** N/A — no package changes this phase. Verified via:
```bash
grep -n "^ratatui\|^crossterm" Cargo.toml
```

## Package Legitimacy Audit

Not applicable — this phase installs no new external packages. Existing
`ratatui`/`crossterm` versions are unchanged.

## Architecture Patterns

### System Architecture Diagram

```
 [AgentRegistry]                         [TUI tick loop, 120ms]
  entries: Vec<AgentEntry>  ──snapshot()──▶  draw_dock(buf, area, app)
  (id, pid, state, started,                        │
   dir, + NEW: cached                              ├─▶ draw_status_strip (existing)
   description/activity/                           ├─▶ draw_agents_strip (NEW)
   report_path/worktree/                           │     - reads app.agents_cache (in-memory,
   turns/tokens)                                   │       refreshed on state-change, not per-tick)
         ▲                                          │     - collapsed: header + up to 3 lines
         │ refresh on state                         │     - expanded: scrollable detail list
         │ transition only                          ├─▶ draw_input_box (existing, height now
         │ (Running→Completed etc.)                 │     shares budget with agents strip)
  [brief.md / report.md on disk]                    └─▶ draw_footer (existing)
   (read via agent::brief::front_matter_get,
    same helper agent_ctl.rs::describe_entry uses)

 [keys.rs / interpret_key]
  Ctrl+G  ──▶ toggles app.agents_strip_expanded (ephemeral UI state)
  (only when input box empty, per D-06-lite: display-only now, so no
   permission-approval keys needed — UI-03 revision removed those)
```

### Recommended Project Structure
No new files needed — this is a modification phase:
```
src/
├── mode/tui.rs       # add: draw_agents_strip(), agents_cache refresh,
│                      #      App.agents_strip_expanded: bool,
│                      #      Ctrl+G handling in interpret_key, viewport resize
├── agent_registry.rs  # add: cached description/activity/report fields on
│                      #      AgentEntry (or a sibling struct), populated at
│                      #      state-transition points (set_state, push_report)
├── keys.rs            # add: ActionId::ToggleAgentsStrip, default Ctrl+G
└── render/panel.rs     # (check for reusable glyph/truncation helpers first)
```

### Pattern 1: Pure buffer-drawing function, independently testable
**What:** `draw_dock(buf: &mut Buffer, area: Rect, app: &App)` takes a raw
`Buffer` and `Rect`, not a `Terminal`. This is why `draw_dock` (and the
analogous input-box logic) is unit-tested today without a real terminal.
**When to use:** The new `draw_agents_strip` must follow the identical
signature shape so it can be tested the same way: construct `App` with a
fake/injected agent list, call the function with `Buffer::empty(rect)`, then
assert on cell contents.
**Example:**
```rust
// Source: src/mode/tui.rs:5254 (draw_dock), verified in this session
fn draw_dock(buf: &mut Buffer, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([ /* ... */ ])
        .split(area);
    draw_status_strip(buf, chunks[1], app);
    // ...
}
```

### Pattern 2: Dynamic height within a fixed-constant viewport
**What:** `DOCK_HEIGHT: u16 = 10` is currently a compile-time constant used
both for `Viewport::Inline(DOCK_HEIGHT)` at terminal setup AND the `Layout`
constraints inside `draw_dock`. The input box already varies its *content*
height (`input_content_h`, 1..=`MAX_INPUT_LINES`) within that fixed total by
using `Constraint::Min(0)` for the slack region above it — but the total
`DOCK_HEIGHT` itself never changes per-frame today.
**When to use:** D-02/D-04 require 1-3 lines collapsed and up to ~40% of
screen height expanded — this cannot fit inside a frozen 10-row budget.
Two viable approaches, in order of recommendation:
1. **Resize the inline viewport per-frame.** `ratatui::Terminal` supports
   changing the viewport area between draws for `Viewport::Inline` — the
   project must call `terminal.resize(...)` (or re-set via
   `Terminal::insert_before`/viewport area adjustment) when the strip's
   line count changes, recomputing a `DYNAMIC_DOCK_HEIGHT` each frame as
   `BASE_DOCK_HEIGHT + strip_lines`. This needs verification against the
   exact ratatui version pinned (`Terminal::set_viewport_area` or
   equivalent — check `ratatui` docs for the pinned version before
   implementing; API name may differ across 0.2x releases). [ASSUMED —
   needs Context7/docs confirmation for the exact API name in the pinned
   version]
2. **Fallback if dynamic resize proves awkward:** keep `DOCK_HEIGHT` fixed
   and cap the agents strip at a hard max (e.g. always ≤5 lines
   collapsed, expanded view becomes a captured scrollback insert via
   `insert_line`/`insert_before` the same way tool-output expansion
   already works at `src/mode/tui.rs:1277` for Ctrl+O) — i.e. treat
   "expand" as inserting a block above the viewport (scrollback) rather
   than growing the live dock. This matches an existing, already-tested
   pattern (`ExpandLastTool`) exactly and avoids viewport-resize risk
   entirely. **This is the lower-risk option** given Ctrl+O already does
   something analogous (expand into scrollback) for tool output.

### Anti-Patterns to Avoid
- **Reading brief.md/report.md from disk on every 120ms tick for every
  agent:** violates D-07 ("no redraw storm... agent activity does not
  trigger redraws by itself") in spirit even if it doesn't literally
  redraw more often — under many agents (`max_live` cap, Phase 1) this is
  N syscalls every 120ms. Cache and refresh only on registry state
  transitions (`set_state`, `push_report`) or on a slower cadence (e.g.
  every ~1s) distinct from the render tick.
- **Polling the strip from `interpret_key`'s per-keystroke path:** the
  strip must only update from the tick-driven `draw_dock` call, never
  from key handling, matching D-07/UI-04 exactly.
- **Adding stop/approve/message key handling to the strip:** explicitly
  out of scope per the Revision 2026-10-03 in 05-CONTEXT.md — UI-03 was
  revised to pure display-only, no approval UI at all (D-06 in the
  original context section is **superseded** by the revision; do not
  implement it).

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Elapsed-time formatting | Custom duration formatter | Whatever helper the status strip already uses for elapsed/spinner display (search `src/render/status_line.rs` and the existing status-strip code near `draw_status_strip`) | Consistency with existing "Elapsed X.Xs" display already in the dock |
| Brief/report front-matter parsing | New YAML/front-matter parser | `crate::agent::brief::front_matter_get` (already used by `agent_ctl.rs::describe_entry`) | Single source of truth for the front-matter format; avoids drift |
| Text truncation to terminal width | Ad-hoc `.chars().take(n)` | Check `src/render/panel.rs` and `wrap_chars`/`split_at_col` helpers already in `tui.rs` for width-aware truncation that accounts for CJK width handling noted in `insert_line`'s doc comment | The codebase has already solved CJK-width edge cases once (see `insert_line` comment re: East-Asian Wide chars) — redoing this in the strip risks reintroducing that bug for Chinese task descriptions (the mockup itself uses Chinese text: "读取 src/config") |

**Key insight:** Nearly everything this phase needs already has a
counterpart elsewhere in `tui.rs` (elapsed formatting, truncation, buffer
rendering, pure testable draw functions). The main net-new work is state
plumbing (registry → cache → App) and the Ctrl+G toggle + viewport-height
decision, not new rendering primitives.

## Common Pitfalls

### Pitfall 1: Strip grows/shrinks every frame, causing visible jitter
**What goes wrong:** If the strip's line count is recomputed from live,
noisy state (e.g. activity text changing length every tick) the dock
height flickers.
**Why it happens:** D-02 says the collapsed strip is "1-3 lines" based on
agent count only (not content length) — line count should depend solely on
`min(agent_count, 3) + 1 header` (+ "+K more" line), never on text content
length. Content truncates within a fixed-width line; it must never change
line *count*.
**How to avoid:** Derive strip height purely from `agent_count` and
`expanded` bool, never from text length or activity updates.
**Warning signs:** Viewport resize calls happening more than once per
agent-count change.

### Pitfall 2: Registry snapshot taken on tick includes terminal/completed agents forever
**What goes wrong:** UI-01 says the strip "disappears when none exist" —
but the registry's `entries` Vec likely keeps terminal entries around for
the lifetime of the run (for CTL-04's `list_agents` history). If the strip
renders unconditionally off a non-empty `entries` Vec, it would never
disappear even after all agents finish and are no longer relevant to show.
**Why it happens:** Conflating "all agents ever in this run" (archive/list
semantics) with "agents currently worth showing" (strip semantics — D-02
says done agents show briefly: "then the most recently finished").
**How to avoid:** Confirm with the planner/CONTEXT whether "none exist"
means zero *live* (non-terminal) agents, or zero agents in the run at all.
[Open question — see below]
**Warning signs:** Strip stays visible for the rest of the session after
the only agent finishes.

### Pitfall 3: Ctrl+G collides with terminal/OS chord
**What goes wrong:** Ctrl+G is BEL in some terminals/readline contexts;
most modern terminal emulators pass it through to the TUI app fine (it's
not reserved by crossterm), but worth a quick grep to be sure no existing
binding or OS-level intercept exists.
**Why it happens:** Assumed availability without checking.
**How to avoid:** `grep -n "Char('g')" src/keys.rs src/mode/tui.rs` already
run in this research — confirmed free (only `Char('o')` with CONTROL
exists for `ExpandLastTool`). [VERIFIED: codebase grep]
**Warning signs:** None found — Ctrl+G is free as of this research.

## Code Examples

### Existing registry snapshot access pattern
```rust
// Source: src/agent_registry.rs:468, verified in this session
pub fn snapshot(&self) -> Vec<AgentEntry> { /* clones entries under lock */ }
```

### Existing disk-read-for-display pattern (to mirror for cache refresh)
```rust
// Source: src/tool/agent_ctl.rs:69-99, verified in this session
fn describe_entry(dir: &Path, id: &str, state: &str, elapsed_secs: u64) -> Value {
    let brief = std::fs::read_to_string(dir.join("brief.md")).unwrap_or_default();
    let description = front_matter_get(&brief, "label")
        .filter(|s| !s.trim().is_empty() && s != "(none)")
        .unwrap_or_else(|| "(none)".to_string());
    let report = std::fs::read_to_string(dir.join("report.md")).ok();
    let (turns, tokens, worktree, branch) = match &report {
        Some(r) => (
            front_matter_get(r, "turns"),
            front_matter_get(r, "tokens"),
            front_matter_get(r, "worktree"),
            front_matter_get(r, "branch"),
        ),
        None => (None, None, None, None),
    };
    // ...
}
```
This exact helper (or a thin wrapper around it) should back the registry's
cache-refresh-on-transition logic, so the strip and `list_agents` tool stay
consistent in what they show.

### Existing keybinding registration pattern (for Ctrl+G)
```rust
// Source: src/keys.rs:14-44, verified in this session
pub enum ActionId {
    ThinkingCycle,
    ToolCancel,
    ExpandLastTool,   // Ctrl+O — the direct precedent for Ctrl+G
    NewlineInInput,
    OpenSlashPalette,
    OpenSettings,
}
// default KeySpec for ExpandLastTool:
// KeySpec { code: KeyCode::Char('o'), mods: KeyModifiers::CONTROL }  (src/keys.rs:167)
```
Add `ActionId::ToggleAgentsStrip` following the identical shape, with
default `KeySpec { code: KeyCode::Char('g'), mods: KeyModifiers::CONTROL }`.

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|---------------|--------|
| N/A | N/A | N/A | This is new functionality on an established, unchanged internal architecture — no external "state of the art" shift applies. |

**Deprecated/outdated:** None relevant.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | The pinned `ratatui` version supports resizing an already-created `Viewport::Inline` terminal area per frame (e.g. via `Terminal::set_viewport_area` or equivalent) | Architecture Patterns, Pattern 2 | If unsupported, the dynamic-height approach (option 1) is infeasible and the team must use the scrollback-insert fallback (option 2) instead — changes task structure materially |
| A2 | "disappears when none exist" (UI-01) means zero *live* (non-terminal) agents in the current run, not zero agents ever dispatched | Common Pitfalls, Pitfall 2 | If wrong, strip visibility logic needs the opposite filter, and recently-finished agents (D-02's "most recently finished" line) need a separate TTL-based inclusion rule instead |
| A3 | The existing elapsed-time/spinner formatter in the status strip can be reused as-is for per-agent elapsed time in the strip | Don't Hand-Roll | Low risk — worst case, a small duplicate formatter is written; no correctness risk |

**None of these are package/library legitimacy risks** — they are internal-API and product-semantics questions, resolvable by reading more of `tui.rs` / confirming with the user during planning or at implementation time.

## Open Questions

1. **Does "none exist" (UI-01) include recently-finished agents, or only live ones?**
   - What we know: D-02 says recently-finished agents appear at the bottom
     of the collapsed list ("waiting for attention... then running...
     then the most recently finished"), implying finished agents DO show
     for some period.
   - What's unclear: Whether there's a time-based eviction (e.g. show a
     finished agent for N seconds then drop it) or whether finished
     agents persist in the strip until the *next* `/agents clean` or
     process exit.
   - Recommendation: Default to "strip is visible iff `registry.snapshot()`
     is non-empty for the current run" (simplest, matches ARC-04's run-
     scoped semantics) and show ALL entries (live + terminal) subject to
     the existing D-02 ordering and the 3-line cap — i.e. don't invent a
     TTL. This is a safe default the planner can adjust in CONTEXT if the
     user wants eviction.

2. **Exact ratatui API for per-frame inline-viewport resize in the pinned version.**
   - What we know: `ratatui`'s `Viewport::Inline(n)` is already used
     (`src/mode/tui.rs:737`); the crate is pinned below 0.30 (Out of Scope
     per REQUIREMENTS.md).
   - What's unclear: The exact method name/signature for resizing an
     inline viewport after terminal creation in the pinned minor version
     — this varies across ratatui 0.2x releases and needs a direct check
     against `Cargo.lock`'s resolved version before planning task details.
   - Recommendation: Planner should add a first task/spike step: `grep
     "ratatui" Cargo.lock` for the exact resolved version, then check that
     version's docs (via Context7 if available, else crates.io docs.rs)
     for `Terminal::{set_viewport_area, resize}` before committing to the
     dynamic-resize approach. If no safe per-frame resize API exists in
     the pinned version, fall back to the scrollback-insert design
     (Pattern 2, option 2) which requires zero new ratatui API surface.

3. **Where does per-agent "latest activity" (expanded view, D-04) come
   from?**
   - What we know: `report.md`/`brief.md` front matter gives static
     fields (turns, tokens, worktree, branch) but not a rolling "last 2-3
     activity lines (tool calls)" — that implies either tailing the
     child's own transcript file or some existing activity-log file not
     yet located in this research pass.
   - What's unclear: Whether Phase 1-4 already wrote an activity log file
     per agent that this phase can tail, or whether this phase must add
     one.
   - Recommendation: Planner should `grep -rn "activity" src/agent*.rs
     src/archive.rs` and inspect the child's transcript/session file
     format (`.nanopi/agents/<run>/<id>/`) during planning to confirm
     whether an existing file already captures per-turn tool-call
     summaries, or whether a new "activity.log" append needs to be added
     to the child's runtime loop (this would be a cross-cutting change
     touching Phase 1's child supervisor code, not just the TUI).

## Environment Availability

Skipped — this phase has no external tool/service dependencies beyond the
Rust toolchain already used to build the project (`cargo build`/`cargo
test`), which is confirmed present in this environment.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` via `cargo test` (existing `mod tests` blocks in `src/mode/tui.rs`, `src/agent_registry.rs`, `src/keys.rs`) |
| Config file | none — standard `cargo test`, no separate test-runner config |
| Quick run command | `cargo test --lib mode::tui:: -- --test-threads=4` |
| Full suite command | `cargo test` |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| UI-01 | Strip shows id/role/task/state/elapsed for each agent; absent when none exist | unit (buffer-content assertion) | `cargo test --lib mode::tui::tests::agents_strip_shows_entries -x` | ❌ Wave 0 (new test module in `tui.rs`) |
| UI-01 | Strip disappears when `snapshot()` is empty | unit | `cargo test --lib mode::tui::tests::agents_strip_hidden_when_empty -x` | ❌ Wave 0 |
| UI-02 | Ctrl+G toggles `app.agents_strip_expanded`; expanded view shows activity + report path | unit (`interpret_key` + buffer assertion) | `cargo test --lib mode::tui::tests::ctrl_g_toggles_strip -x` | ❌ Wave 0 |
| UI-02 | Ctrl+G is configurable via `settings.toml` like `ExpandLastTool` | unit | `cargo test --lib keys::tests::toggle_agents_strip_rebindable -x` | ❌ Wave 0 (mirror existing `ExpandLastTool` rebind test in `keys.rs`, if one exists — confirm during Wave 0) |
| UI-03 | No stop/approve/message key handling reachable from the strip | unit (negative test: send relevant keys while strip focused, assert no registry mutation) | `cargo test --lib mode::tui::tests::strip_is_display_only -x` | ❌ Wave 0 |
| UI-04 | Strip renders only from tick-driven `draw_dock`, not per agent event; drawing a fixed snapshot twice produces identical buffers (no hidden per-event mutation) | unit | `cargo test --lib mode::tui::tests::strip_renders_from_snapshot_not_events -x` | ❌ Wave 0 |
| UI-04 | Many agents (near `max_live`) render without panic/overflow and respect the 3-line cap + "+K more" | unit | `cargo test --lib mode::tui::tests::strip_caps_at_three_lines -x` | ❌ Wave 0 |
| D-08 (narrow/short terminal) | Narrow terminal drops activity text then description; short terminal shows header only | unit (parametrized `Rect` sizes) | `cargo test --lib mode::tui::tests::strip_narrow_terminal_degrades -x` | ❌ Wave 0 |

All tests are unit-level, operating on `Buffer`/`Rect`/`App` directly per
the existing `draw_dock` test pattern (no real terminal, no process
spawning, no I/O) — these run in milliseconds and are suitable for
per-commit sampling.

### Sampling Rate
- **Per task commit:** `cargo test --lib mode::tui:: --lib agent_registry:: --lib keys::`
- **Per wave merge:** `cargo test` (full suite, includes existing Phase 1-4 regression tests — e.g. `agent_registry.rs` tests at lines 636-1100+ already verified in this session)
- **Phase gate:** Full suite green, plus manual QA-01 row added for Ctrl+G to the manual E2E test plan (owned by Phase 6 per ROADMAP, but the row itself should be added now per QA-01's literal wording: "each new control... has a row")

### Wave 0 Gaps
- [ ] New `#[test]` module additions to `src/mode/tui.rs`'s existing `mod tests` (line ~5830) covering the 7 rows above — no new test *file* needed, existing harness covers it.
- [ ] Confirm whether `src/keys.rs` already has a generic "any ActionId is rebindable" test that would cover `ToggleAgentsStrip` for free, vs. needing one written per-action (grep `mod tests` in `keys.rs` during Wave 0).
- [ ] Resolve Open Question 3 (activity-log source) before writing the expanded-view test, since the test needs to know what the "latest activity" data source actually is.
- [ ] Resolve Open Question 2 (ratatui dynamic-resize API) before committing to whether a dedicated "viewport resize" unit test is even meaningful (if falling back to scrollback-insert, no resize test is needed at all).

## Security Domain

Not applicable in the ASVS sense — this phase has no authentication,
session, network input, or cryptography surface. It renders internal
process state (already trusted, same-process) to a local terminal. The one
adjacent concern — UI-03's explicit removal of approve/deny/stop controls
from the strip — is itself the security-relevant decision already locked
by CONTEXT.md's Revision 2026-10-03 (agents are controlled only through the
orchestrator's tool calls, never directly from the TUI), and this research
does not second-guess it.

## Project Constraints (from CLAUDE.md)

No project-local `./CLAUDE.md` exists in `/root/workspace/nanopi` (only the
user's global `~/.claude/CLAUDE.md`, which describes the container/network
environment and is not a coding-convention file). No additional constraints
to layer on top of CONTEXT.md's locked decisions.

## Sources

### Primary (HIGH confidence)
- `src/mode/tui.rs` (7430 lines) — `draw_dock` (line 5254), `App` struct
  (line 766), tick loop (line 1890-1910), `DOCK_HEIGHT`/`MAX_INPUT_LINES`
  constants (line 389/397), `insert_line`/`wrap_chars` (CJK-width handling),
  existing test module (`mod tests`, line 5830) — all read directly in this
  session.
- `src/agent_registry.rs` (1111 lines) — `AgentEntry`/`AgentRegistry`
  struct and all public methods, read directly in this session.
- `src/tool/agent_ctl.rs` (509 lines) — `describe_entry`, `ListAgentsTool`,
  front-matter field list, read directly in this session.
- `src/keys.rs` (361 lines) — `ActionId`, `KeySpec`, existing Ctrl+O
  binding, read directly in this session.
- `.planning/phases/05-tui-agents-strip/05-CONTEXT.md` — locked
  decisions D-01..D-08 and the superseding Revision 2026-10-03.
- `.planning/REQUIREMENTS.md` — UI-01..04 verbatim text.
- `.planning/ROADMAP.md` — Phase 5 goal/success criteria/requirements.

### Secondary (MEDIUM confidence)
None used — no external web sources were needed; this phase is entirely
internal-codebase research.

### Tertiary (LOW confidence)
- Exact ratatui per-frame inline-viewport resize API name — not verified
  against the pinned `Cargo.lock` version in this session; flagged as
  Open Question 2 and Assumption A1 for the planner to resolve first.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — no new dependencies, confirmed by direct grep of existing imports/usage.
- Architecture: HIGH for data flow and existing patterns (directly read code); MEDIUM for the dynamic-viewport-resize mechanism specifically (ratatui API name unverified against pinned version).
- Pitfalls: HIGH — derived directly from locked CONTEXT.md decisions and observed registry/TUI code structure.

**Research date:** 2026-10-04
**Valid until:** No external dependency; valid until `src/mode/tui.rs`, `src/agent_registry.rs`, or the pinned `ratatui` version changes (effectively valid for the life of this phase).

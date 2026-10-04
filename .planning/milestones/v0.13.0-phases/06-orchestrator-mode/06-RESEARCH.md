# Phase 6: Orchestrator Mode - Research

**Researched:** 2026-10-04
**Domain:** Rust CLI/TUI agent orchestration — tool-set gating, system prompt variants, config/slash-command toggles, status-line rendering
**Confidence:** HIGH

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

### Toggle
- **D-01:** `/orchestrator` toggles the mode (`/orchestrator on|off`
  also works). The config key `experimental.orchestrator = false` sets
  the default at startup. The mode is TUI only: print mode ignores it,
  with a one-line warning when the key is set.
- **D-02:** Toggling in the middle of a session takes effect from the
  next turn. Running agents are not affected. The status line shows
  `⎈ orchestrator` while the mode is on.

### Toolset
- **D-03:** The orchestrator's tools are read, grep, glob, `agent`,
  `send_message`, `stop_agent` and `list_agents`. Write, edit and bash
  are not registered at all, not just discouraged. A test asserts they
  are absent.
- **D-04:** With the mode off, the system prompt and the tool specs
  sent to the provider are byte-identical to v0.12. A snapshot test
  checks this.

### Orchestrator behaviour (system prompt)
- **D-05 (owner decision): plan confirmation.** When the task is clear
  and nothing is ambiguous, the orchestrator states its plan briefly
  and dispatches immediately. When anything needs confirming (unclear
  scope, a choice between approaches, a risky action), it asks the user
  first and dispatches only after the answer.
- **D-06 (owner decision): use as few agents as possible.**
  - The orchestrator first judges whether parallelism is worth it.
  - Sequential or interdependent development work goes to **one**
    agent, which is the safer practice.
  - Several parallel agents are used only for genuinely independent
    work.
  - The concurrency cap stays `agent.max_concurrency` (default 4,
    configurable). That is a ceiling, not a target.
- **D-07:** For parallel code-writing agents the orchestrator sets
  `isolation: "worktree"`. Merging follows Phase 4 D-11: automatic,
  except that conflicts are escalated to the user.
- **D-08:** The prompt's workflow is: understand (using read-only tools
  or an explore agent), then plan, then dispatch, then monitor (react
  to reports, and amend or stop when the direction changes), then
  verify (a verify agent where worthwhile), then report a combined
  summary to the user covering what was done, changed files, open
  issues, and the archive path.
- **D-09:** The orchestrator writes clear, self-contained briefs, since
  agents do not inherit its context. A brief covers the goal,
  relevant files, constraints and the expected report.

### Quality
- **D-10 (QA-01):** Add manual end-to-end test rows for amend, stop,
  stop-all (Ctrl+X), expand (Ctrl+G), approve / deny / always, the
  `/orchestrator` toggle, `/agents clean`, auto-prune, worktree merge,
  and merge conflict. Put them in a new
  `docs/v0.13-manual-test-plan.md` that follows the v0.12 format.
- **D-11 (QA-02):** Measure the release binary size before and after
  the milestone. The growth must be no more than about 150 KB, with no
  new crates unless justified in the summary.

### Revision 2026-10-03 (supersedes conflicting decisions above)
Phase 1 changed to a child-process runtime. Agents are `nanopi -p`
children controlled only by the orchestrator; the user never controls
them directly. Control tools are dispatch/amend/stop/list/continue;
there is no user stop-all or approval surface.

### Claude's Discretion
- Exact prompt wording, as long as it follows D-05 to D-09.

### Deferred Ideas (OUT OF SCOPE)
- None listed in 06-CONTEXT.md.
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-------------------|
| ORC-01 | User can turn orchestrator mode on/off from the TUI with `/orchestrator`; also `experimental.orchestrator` config key; off by default | Pattern 3 (deferred-effect toggle), config key design, slash-command dispatch example |
| ORC-02 | In orchestrator mode, main agent's tools limited to read/grep/glob, dispatch, amend, stop, list, continue; write/edit/bash not registered | Pattern 2 (register-by-hand `ToolRegistry::orchestrator()`), Common Pitfalls 1 & 3 |
| ORC-03 | Orchestrator's system prompt tells it to analyse, split, dispatch, monitor, synthesise | Pattern 1 (sibling `build_orchestrator()`), Code Examples |
| ORC-04 | With mode off, prompts and tool specs byte-identical to v0.12 | Pattern 1, Common Pitfall 2, Validation Architecture snapshot tests |
| ORC-05 | Status line shows when orchestrator mode is active | Code Examples (status-line segment), Common Pitfall 4 |
| QA-01 | Each new control has a row in the manual E2E test plan | Validation Architecture / 06-VALIDATION.md Wave 0 gaps |
| QA-02 | Release binary grows ≤~150 KB; no new crates without justification | Standard Stack (no new deps), Assumptions Log A3, Validation Architecture binary-size baseline |
</phase_requirements>

## Summary

Phase 6 is additive wiring over code that already exists and is fully
tested from Phases 1–5: `ToolRegistry::standard_with_control()`
(`src/tool/mod.rs:586`), the three control tools in
`src/tool/agent_ctl.rs`, `AgentTool` in `src/tool/agent.rs`, the
registry-rebuild call sites in `src/mode/tui.rs` (used today by
`/model`, `/new`, `/resume`, `/fork`), and the `think:`/`vendor:`
status-line segment pattern at `src/mode/tui.rs:5537-5600`. No new
crates, no new child-process behavior, no changes to Phases 1–5's
code paths are required — this phase only needs a **mode flag**
(`Agent.orchestrator: bool` or equivalent), a **filtered registry
constructor**, a **separate system-prompt builder**, a **slash
command + config key**, and a **status-line segment**.

The hard constraint is ORC-04 (byte-identical off-mode prompt/tools).
The codebase's own pattern for this is already visible: `src/lib.rs`,
`src/plugin_send.rs`, `src/plugin_tools.rs` all carry "byte-identical
apart from this dead-but-compiled module" doc comments for the
non-`wasm` build, achieved by branching on a flag/feature and never
touching the existing code path when the flag is off. The orchestrator
mode must follow the identical shape: a new `orchestrator: bool`
parameter threaded through `AgentBuildInputs`/`compose_system_prompt`
that is `false` on every pre-existing call site, with a brand-new
`system_prompt::build_orchestrator()` function (not a patch to
`system_prompt::build()`) and a brand-new `ToolRegistry::orchestrator()`
constructor (not a patch to `standard()`/`standard_with_control()`).

**Primary recommendation:** Add `Agent.orchestrator: bool` (defaulted
`false` everywhere today), a `ToolRegistry::orchestrator()` constructor
that returns exactly `{read, grep, find, agent, list_agents,
stop_agent, send_message}`, a `system_prompt::build_orchestrator()`
sibling function, wire both behind the existing registry-rebuild call
sites plus a new `/orchestrator` slash command and `[experimental]
orchestrator = false` config key, and add a `⎈ orchestrator`
status-line segment next to the existing `think:`/`vendor:` segments.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Mode toggle (`/orchestrator`, config key) | TUI (slash command dispatch) | Config loader | Parsed in `src/mode/tui.rs` interpret_key/dispatch_slash; default sourced from `config.rs` `[experimental]` |
| Tool-set restriction | Tool registry (`src/tool/mod.rs`) | Agent construction (`src/agent/build.rs`) | `ToolRegistry` is the single choke-point for what's callable; `build_fresh`/`hydrate_resumed` just pick which constructor to call |
| System prompt content | Agent construction (`src/agent/build.rs` + `src/agent/system_prompt.rs`) | — | `compose_system_prompt` is the only place `Context.system` is derived from tool names |
| Status line indicator | TUI render (`src/mode/tui.rs` `draw_dock`) | — | Same function that already renders `think:`/`vendor:` segments |
| Print-mode behavior | Print mode (`src/mode/print.rs`) | — | Must detect the config key and emit a one-line warning, never enter the mode |
| Dispatch/monitor/synthesize behavior | Already-built tools (`agent`, `agent_ctl`) | — | No new runtime logic — this is pure prompt guidance consumed by the model, not new Rust control flow |

## Standard Stack

No new external dependencies. This phase is entirely internal wiring over existing crates (`serde`, `toml`, `ratatui`, `clap` — all already in `Cargo.toml`).

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| (none new) | — | — | Phase reuses `ToolRegistry`, `Agent`, `Config`, `ratatui` already in the tree |

### Supporting
N/A — no new packages.

### Alternatives Considered
N/A — no package decisions to make.

**Installation:** none required.

## Package Legitimacy Audit

No external packages are installed by this phase. Table omitted per the audit protocol ("required whenever this phase installs external packages" — it does not).

## Architecture Patterns

### System Architecture Diagram

```
User input
  │
  ├─ "/orchestrator [on|off]" ──────────────┐
  │                                          ▼
  │                              SlashCmd::Orchestrator
  │                              → KeyAction::SetOrchestrator(bool)
  │                              → app.orchestrator = bool (effective
  │                                 from the NEXT run_turn, same as
  │                                 /model's existing deferred-rebuild
  │                                 shape — no special "pending" state
  │                                 needed because slash commands only
  │                                 run between turns)
  │
  config.toml [experimental] orchestrator = true/false
  │   → read once at startup (src/config.rs), seeds app.orchestrator
  │
  ▼
run_turn() entry (src/agent/loop_.rs) — BEFORE the model call
  │
  ├─ registry = if app.orchestrator { ToolRegistry::orchestrator() }
  │             else { ToolRegistry::standard_with_control() }   (same
  │             site that already branches on app.tools_allow)
  │
  ├─ agent.registry = registry.clone()
  ├─ agent.context.tools = registry.all_specs()
  ├─ prompt = if app.orchestrator { system_prompt::build_orchestrator(cwd, tool_names) }
  │           else { system_prompt::build(cwd, tool_names) }     (compose_system_prompt
  │                                                                 branches once, at the top)
  ├─ agent.set_system_base(prompt)         (reuses refresh_system_prompt)
  │
  ▼
Model turn runs with the filtered tool set + orchestrator prompt
  │
  ├─ read/grep/find   → understand the task (D-08 "understand" step)
  ├─ agent            → dispatch (single/parallel/chain, D-06 "fewest agents")
  ├─ list_agents      → monitor
  ├─ send_message     → amend running / continue finished (D-08 "monitor")
  ├─ stop_agent       → abort a wrong-direction agent
  │
  ▼
Child reports arrive (existing Phase 4 report-injection path, unchanged)
  │
  ▼
Orchestrator synthesizes → one user-facing summary (D-08 "report")
  │
  ▼
draw_dock() status line: "⎈ orchestrator" segment shown iff app.orchestrator
```

### Recommended Project Structure

No new files needed; everything is additions to existing modules:

```
src/
├── tool/mod.rs            # + ToolRegistry::orchestrator()
├── agent/system_prompt.rs # + build_orchestrator()
├── agent/build.rs         # compose_system_prompt() + AgentBuildInputs gain `orchestrator: bool`
├── agent/loop_.rs         # Agent gains `orchestrator: bool` field (status-line read)
├── config.rs              # + ExperimentalConfig { orchestrator: bool }
├── mode/tui.rs             # SlashCmd::Orchestrator, KeyAction::SetOrchestrator,
│                           #   registry-rebuild call sites branch on app.orchestrator,
│                           #   draw_dock() status-line segment
└── mode/print.rs          # one-line warning if experimental.orchestrator is set
```

### Pattern 1: Flag-gated byte-identical code path (ORC-04)
**What:** Add a boolean parameter to the shared builder function(s) and route ALL existing callers through `false` explicitly, with the new behavior living in a sibling function that the flag calls instead of a branch inside the existing one.
**When to use:** Any time a "byte-identical when off" requirement exists.
**Example (existing precedent in this codebase):**
```rust
// src/agent/build.rs:768 — compose_system_prompt's existing shape.
// Recommended extension (not yet in the tree):
pub fn compose_system_prompt(
    cwd: &Path,
    tool_names: &[String],
    skills: &[Skill],
    no_context_files: bool,
    overrides: &PromptOverrides,
    orchestrator: bool,          // NEW — defaults false at every existing call site
) -> String {
    let resolved = overrides.resolve(cwd);
    let mut prompt = match resolved.custom {
        Some(text) => format!("{text}\n\nCurrent working directory: {}", cwd.display()),
        None if orchestrator => crate::agent::system_prompt::build_orchestrator(cwd, tool_names),
        None => crate::agent::system_prompt::build(cwd, tool_names),
    };
    // ... rest unchanged
}
```
This guarantees ORC-04: when `orchestrator` is `false`, the function executes the exact same branch and bytes it always has.

### Pattern 2: Dedicated registry constructor, not a runtime filter
**What:** `ToolRegistry::orchestrator()` as a direct sibling of `standard()`/`standard_with_control()`, built by registering exactly the allowed tools — not by taking `standard_with_control()` and subtracting names at runtime.
**When to use:** Whenever "tool X must not be registered at all" (not just hidden) is a tested requirement (ORC-02's "write, edit and bash are not registered at all, not just discouraged").
**Example:**
```rust
// src/tool/mod.rs, sibling of standard_with_control() at line 586
/// Orchestrator-mode registry (ORC-02): read/grep/find plus dispatch +
/// the three control tools. write/edit/bash/ls are never registered —
/// a test asserts `names()` excludes them, not merely that they are
/// unreachable.
pub fn orchestrator() -> Self {
    let mut r = Self::new();
    r.register(Arc::new(read::ReadTool));
    r.register(Arc::new(grep::GrepTool));
    r.register(Arc::new(find::FindTool));
    r.register(Arc::new(agent::AgentTool::new()));
    r.register(Arc::new(agent_ctl::ListAgentsTool::new()));
    r.register(Arc::new(agent_ctl::StopAgentTool::new()));
    r.register(Arc::new(agent_ctl::SendMessageTool::new()));
    r
}
```
This mirrors `standard()`/`standard_with_control()`'s own construction style exactly (register-by-hand, not filter-by-name), so `ToolRegistry::orchestrator().names()` is provably `{agent, find, grep, list_agents, read, send_message, stop_agent}` with no runtime allowlist logic to audit.

### Pattern 3: Deferred-effect toggle (D-02, "next turn")
**What:** Because slash commands are processed only when the TUI is idle (`interpret_key` dispatches `SlashCmd` between turns, never mid-stream), a boolean flag on `App`/`Agent` flipped by the slash command automatically takes effect "from the next turn" with zero extra state machine — the existing `/model` rebuild call sites are the proof this works today.
**When to use:** Any TUI-only toggle whose semantics are "effective from the next turn."
**Example:** see the four `standard_with_allowlist(&app.tools_allow)` call sites at `src/mode/tui.rs:2506,2674,3185,4062` — add the same `if app.orchestrator { ToolRegistry::orchestrator() } else { ... }` branch at each.

### Anti-Patterns to Avoid
- **Patching `system_prompt::build()` with an `if orchestrator` branch inline:** breaks the "never touch the existing path" guarantee that makes ORC-04 trivially true; a sibling function is strictly safer to verify.
- **Filtering `standard_with_control()`'s tool map by name at runtime** (e.g. `.retain(|k,_| ALLOWED.contains(k))`): works today but makes "not registered at all" a property of a filter list rather than of construction — a future tool added to `standard()` silently becomes orchestrator-visible unless the filter list is remembered and updated. The register-by-hand sibling constructor (Pattern 2) doesn't have this failure mode: a new tool added to `standard()` is invisible to `orchestrator()` unless someone explicitly adds the line.
- **Mutating `AgentConfig`/`[agent]` for this feature:** `[agent]` governs child-process caps (RT-05..09 territory); the mode toggle is a new, separate `[experimental]` table, matching the CONTEXT.md config key `experimental.orchestrator`.
- **Reusing `TrustConfig`'s `Option<String>` pattern for a boolean:** `TrustConfig.default: Option<String>` exists because `ask/always/never` is a tri-state with "unset" meaning "use default". `orchestrator` has exactly one meaningful default (`false`) — use a plain `bool` with `#[serde(default)]`, matching `ToolExecMode`'s precedent of a concrete default rather than an `Option`.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Dispatch/monitor/stop/continue control flow | A new orchestrator-specific control loop | The existing `agent`, `list_agents`, `stop_agent`, `send_message` tools (Phases 1–4) | They are fully built, tested (1025 passing lib tests), and already enforce the structural guarantee that children never get control tools (T-04-06) |
| Worktree isolation for parallel writers (D-07) | New git worktree code | `src/worktree.rs` (Phase 4-02) + the `isolation: "worktree"` dispatch param already in `tool::agent` | Already handles create/commit/cleanup/auto-merge/conflict; D-07 only asks the orchestrator's *prompt* to request it, not new runtime code |
| Report synthesis / capping | A new summarization pipeline | `cap_report()` / `PARENT_REPORT_CAP` (Phase 3-03, `src/tool/agent.rs`) | Already caps every agent report at 8 KiB with a path pointer; the orchestrator's "synthesise" behavior (D-08) is a prompt instruction consuming data this already returns, not new code |
| Byte-identical off-mode detection | A manual diff script | A `#[test]` snapshot comparing `compose_system_prompt(..., orchestrator=false)` output against the pre-phase baseline string, plus `ToolRegistry::standard_with_control().names()` equality | `insta`-style snapshot isn't in the tree; a plain `assert_eq!` against a captured baseline constant is consistent with the rest of the test suite's style (no snapshot-testing crate present anywhere in `Cargo.toml`) |

**Key insight:** every runtime capability the orchestrator needs (dispatch, amend, stop, list, continue, worktree isolation, report capping) was deliberately built in Phases 1–4 so that Phase 6 is prompt + registry wiring only. Scope creep risk for this phase is adding *new* agent-control capabilities instead of just exposing the existing ones under a restricted toolset.

## Common Pitfalls

### Pitfall 1: Forgetting `Context.tools` on toggle
**What goes wrong:** Flipping `app.orchestrator` and rebuilding `agent.registry` alone does nothing — `Context.tools` (what's actually sent to the provider) is a separate field set once at construction from `registry.all_specs()`.
**Why it happens:** `ToolRegistry` and `Context.tools` are two different pieces of state that happen to usually be rebuilt together; it's easy to update one and not the other.
**How to avoid:** Every registry-rebuild call site in `tui.rs` already does `new_agent.context.tools = registry.all_specs();` right after constructing the registry (see `src/mode/tui.rs:4062-4063`) — follow that exact two-line pattern for the orchestrator toggle's call site too.
**Warning signs:** A test that checks `registry.names()` passes but the model still sees `write`/`edit`/`bash` in its tool-call options (because `Context.tools` was stale).

### Pitfall 2: Prompt/tools identical only "by eye"
**What goes wrong:** ORC-04 requires BYTE-identical, not "looks the same". A stray `format!` with an extra space, a changed guideline ordering, or a schema key reordered by a `HashMap` iteration (non-deterministic) would silently violate it.
**Why it happens:** `serde_json::Value`/`HashMap` iteration order is not guaranteed; if `compose_system_prompt`'s default branch or `all_specs()`'s tool ordering changes incidentally while adding the orchestrator branch, the "byte-identical" snapshot test is what catches it — nothing else will.
**How to avoid:** Write the ORC-04 snapshot test FIRST (capture the current output of `compose_system_prompt(..., false)` and `ToolRegistry::standard_with_control().all_specs()` as of the pre-phase commit), then implement, then confirm the snapshot test still passes unchanged.
**Warning signs:** `cargo test` green but a manual `diff` of two captured prompt strings (before/after the phase) shows any difference when orchestrator is off.

### Pitfall 3: `orchestrator` tool filter and child dispatch denylist interacting
**What goes wrong:** `src/tool/agent.rs` already has `DENIED_TOOLS`/`CONTROL_TOOLS` constants used by `build_child_args` to strip control tools from children (T-04-06). A naive orchestrator-registry implementation that reuses this child-facing denylist machinery (rather than Pattern 2's register-by-hand constructor) risks conflating "what a child may never get" with "what the orchestrator itself may have" — they are different sets (a child still needs `write`/`edit`/`bash`; the orchestrator needs the opposite).
**Why it happens:** Both are "restrict this agent's tools" problems and it's tempting to share one mechanism.
**How to avoid:** Keep `ToolRegistry::orchestrator()` a wholly separate constructor (Pattern 2), never built from or filtered through `CONTROL_TOOLS`/`DENIED_TOOLS`.
**Warning signs:** A code review comment asking "why does the orchestrator registry import `agent::CONTROL_TOOLS`?" — it shouldn't need to.

### Pitfall 4: Status line segment desyncs from actual mode
**What goes wrong:** If the `⎈ orchestrator` segment reads a *different* flag than the one that actually gates the registry rebuild (e.g. a stale `Config` value instead of `App.orchestrator`), the UI can show the mode as on/off incorrectly after a mid-session toggle.
**Why it happens:** The config key seeds the *startup* default; the live state after `/orchestrator` must live on `App` (mutable), not be re-read from `Config` (loaded once).
**How to avoid:** Single source of truth: `App.orchestrator: bool`, seeded from `config.experimental.orchestrator` at startup, mutated only by the slash command, read by both the registry-rebuild call sites and `draw_dock`.
**Warning signs:** Toggling `/orchestrator off` but the status line segment doesn't disappear (or vice versa).

### Pitfall 5: Print mode silently entering the mode
**What goes wrong:** D-01 says print mode "ignores it, with a one-line warning when the key is set" — if `-p` mode's startup path builds its registry the same way the TUI does and happens to read `config.experimental.orchestrator`, it could accidentally apply the restricted toolset to a one-shot `-p` invocation, breaking existing `-p` users silently.
**Why it happens:** `src/mode/print.rs` and `src/mode/tui.rs` share most config-loading code; a shared helper that both call must explicitly NOT apply the orchestrator registry switch in the print path.
**How to avoid:** Gate the registry-switch logic behind `is_tui` (or simply never read `experimental.orchestrator` from `src/mode/print.rs`'s own top-level, non-child branch) and add the warning print as a separate, explicit one-liner near the top of `print.rs`'s startup.
**Warning signs:** `-p` mode's regression tests (RT-04..09's existing suite) start failing or a `-p` invocation unexpectedly lacks `write`/`edit`/`bash`.

## Code Examples

### Status-line segment (mirrors the existing `think:`/`vendor:` pattern)
```rust
// src/mode/tui.rs, inside draw_dock's l2 construction, alongside the
// existing `if let Some(lvl) = app.thinking { ... }` block (around line 5588):
if app.orchestrator {
    l2.push(Span::raw(" · "));
    l2.push(Span::styled(
        "⎈ orchestrator",
        Style::default()
            .fg(Color::Indexed(108))
            .add_modifier(Modifier::ITALIC),
    ));
}
```
Source: pattern directly copied from the adjacent `vid != "fallback"` vendor-id segment already in `src/mode/tui.rs:5574-5582` [VERIFIED: repo inspection].

### Config key
```rust
// src/config.rs, sibling of TrustConfig/SkillsConfig
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExperimentalConfig {
    /// `experimental.orchestrator = true` opts into orchestrator mode
    /// by default at startup (ORC-01). TUI only; print mode (`-p`)
    /// ignores this and prints a one-line warning instead (D-01).
    pub orchestrator: bool,
}
```
Added as a new `pub experimental: ExperimentalConfig` field on `Config`, following the exact `#[serde(default)]` + `Default` derive shape every other sub-config in `src/config.rs` already uses (`TrustConfig`, `SkillsConfig`, `AgentConfig`).

## State of the Art

No external "state of the art" shift applies — this is internal-only wiring. The one piece of prior art named in CONTEXT.md (`claude-code-haha-main/src/coordinator/coordinatorMode.ts`, Roo's "Boomerang" orchestrator) is a reference for the SHAPE of the system prompt (research → plan → dispatch → monitor → verify → report, and "delegating mode cannot edit"), not for any Rust implementation pattern — nanopi's existing tool-registry/prompt architecture already supports that shape without borrowing code.

**Deprecated/outdated:** N/A.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | `ls` is excluded from the orchestrator toolset (D-03 lists exactly `read, grep, glob, agent, send_message, stop_agent, list_agents` — 7 tools, no `ls`) | Pattern 2 / Common Pitfalls | If the owner actually intended `ls` to be included (reading D-03's "glob" loosely as "any read-only exploration tool"), the planner should treat this as a one-line discretionary addition; low risk either way since `find` already covers path-matching exploration and `ls` is `grep`/`find`-adjacent. Flagged here because D-03's own success-criteria text in ROADMAP.md ("only read/grep/glob plus dispatch/amend/stop/list/continue") also omits `ls`, so two independent locked-decision sources agree — recommendation treated as near-certain, not purely assumed, but listed for explicit confirmation since excluding a tool is a testable, user-visible boundary. |
| A2 | "continue" in D-03/ORC-02's tool list is not a separate tool — it's `send_message` to a terminal agent (CTL-06), already implemented in Phase 4-03 | Architecture Patterns | If a reviewer expects a literal `continue_agent` tool name, the planner would need to add one; grep of `src/tool/agent_ctl.rs` confirms only `ListAgentsTool`/`StopAgentTool`/`SendMessageTool` exist today, and `SendMessageTool`'s own doc comment explicitly handles both amend (non-terminal) and continue (terminal) cases, so this is backed by direct code inspection, not guesswork — low risk. |
| A3 | The release-binary-size check (QA-02, ~150 KB budget) should be measured as `target/release/nanopi` size delta before/after the phase, using the project's existing `opt-level="z"` + `lto=true` + `strip=true` release profile | Validation Architecture | If CI measures a different artifact (e.g. a stripped+compressed `dist/` tarball), the raw number will differ from a local `cargo build --release` measurement; recommend the plan capture both the pre-phase baseline (4,881,744 bytes, measured during this research session) and the post-phase size in the same environment for an apples-to-apples delta. |

**If this table is empty:** N/A — see rows above.

## Open Questions (RESOLVED)

1. **Should `ls` be included in the orchestrator toolset?**
   - What we know: D-03 (CONTEXT.md) and ROADMAP.md's Phase 6 success criteria #2 both enumerate the toolset without `ls`.
   - What's unclear: Whether this omission was deliberate (keep the set minimal) or incidental (the prose just didn't list every read-only tool).
   - Recommendation: Exclude `ls` for the initial implementation (matches both locked-decision sources literally), and add a one-line note in the plan's deviation/assumptions log if the owner later wants it added — adding a tool later is a 1-line change to `ToolRegistry::orchestrator()`; removing one after users depend on it is a breaking change. Conservative default: exclude.

2. **Exact wording/placement of the print-mode warning (D-01)?**
   - What we know: "print mode ignores it, with a one-line warning when the key is set."
   - What's unclear: Whether the warning goes to stderr unconditionally or only when `-v`/verbose; whether it should also fire for `--orchestrator` as a hypothetical CLI flag (no such flag is requested anywhere in CONTEXT.md/REQUIREMENTS.md — ORC-01 only names `/orchestrator` and the config key).
   - Recommendation: stderr, unconditional, one line, only triggered by `config.experimental.orchestrator == true` at `-p` startup (no CLI flag needed since none was requested). Example: `eprintln!("note: experimental.orchestrator is set but ignored in print mode (-p)");` — matches the project's existing `Notice`/stderr convention for startup-time advisories (see `load_extensions`'s notice handling in `src/agent/build.rs`).

3. **Does toggling orchestrator mode mid-session need to rebuild `event_subscribers`/plugin state too?**
   - What we know: The four existing `tui.rs` registry-rebuild call sites (`/model`, `/new`, `/resume`, `/fork`) rebuild `registry` + `context.tools` but are full `Agent` reconstructions (`load_session`/`hydrate_resumed`), not in-place mutations.
   - What's unclear: Whether `/orchestrator` should be implemented as a full `hydrate_resumed`-style rebuild (heavier, reuses existing tested path) or a lighter in-place `agent.registry = ...; agent.context.tools = ...; agent.set_system_base(...)` mutation (new, untested path).
   - Recommendation: lighter in-place mutation — `/orchestrator` changes neither provider nor session nor cwd (unlike `/model`/`/resume`/`/fork`), so a full `hydrate_resumed` round-trip is unnecessary machinery; the three-line mutation (registry, context.tools, set_system_base) is the minimal correct change and keeps the diff auditable for the ORC-04 snapshot test.

## Environment Availability

Skipped — this phase has no external tool/service/runtime dependencies beyond the Rust toolchain already used throughout the project (confirmed via `cargo test --lib` succeeding locally: 1025 passed, 1 ignored, 0 failed, at HEAD before this phase's changes).

## Validation Architecture

> Required per `.planning/config.json` (no `workflow.nyquist_validation` key present — treated as enabled). Full detail in the companion `06-VALIDATION.md`, required per the Nyquist gate; summarized here for the planner.

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` / `cargo test`, async via `#[tokio::test]` where needed (already used throughout `src/tool/agent.rs`, `src/agent_registry.rs`) |
| Config file | none — standard `cargo test` |
| Quick run command | `cargo test --lib orchestrator` (once tests are named with an `orchestrator` substring) or targeted module paths, e.g. `cargo test --lib tool::mod::tests` |
| Full suite command | `cargo test --lib && cargo test --test agent_spawn --test agent_archive --test agent_runtime` |

See `06-VALIDATION.md` for the full requirement-to-test map, sampling rate, and Wave 0 gaps.

## Security Domain

> Required (`security_enforcement` absent = enabled).

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-------------------|
| V2 Authentication | no | Out of scope — no new auth surface |
| V3 Session Management | no | Session handling unchanged (Phase 1/2 territory) |
| V4 Access Control | yes | `ToolRegistry::orchestrator()` is itself the access-control boundary — enforced by construction (Pattern 2), verified by a test asserting the exact `names()` set, not by a runtime permission check |
| V5 Input Validation | no (new surface) | `/orchestrator [on|off]` argument parsing reuses the existing `parse_agents_args`-style small-grammar parser pattern; any unrecognized argument should error with usage text, not silently default |
| V6 Cryptography | no | Not applicable |

### Known Threat Patterns for this phase

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Orchestrator registry accidentally includes `write`/`edit`/`bash` due to a future `standard()` addition being auto-inherited | Elevation of Privilege | Pattern 2 (register-by-hand constructor, never derived from `standard()` by filtering) + a test asserting the exact tool-name set, re-run on every future tool addition |
| Config `experimental.orchestrator = true` silently takes effect in `-p` (print) mode, giving a scripted/CI invocation an unexpectedly restricted toolset | Denial of Service (to the calling script) | D-01's explicit "print mode ignores it" — enforce by never branching `-p`'s own top-level registry construction on this flag (Pitfall 5) |
| A model inside orchestrator mode talks its way into asking the user to paste file contents into chat, effectively bypassing the write/edit/bash removal via exfiltration-and-manual-reapply | Elevation of Privilege (social) | Out of scope for Phase 6 code — this is a known, accepted limitation of any tool-removal-based sandboxing (same class of limitation already accepted for the existing dispatch-time permission model; not unique to this phase) — note in RESEARCH only, no code mitigation expected |

## Sources

### Primary (HIGH confidence)
- Direct repo inspection: `src/tool/mod.rs`, `src/tool/agent.rs`, `src/tool/agent_ctl.rs`, `src/agent/build.rs`, `src/agent/loop_.rs`, `src/agent/system_prompt.rs`, `src/config.rs`, `src/mode/tui.rs`, `src/mode/print.rs`, `src/worktree.rs` existence, `Cargo.toml`
- `.planning/phases/06-orchestrator-mode/06-CONTEXT.md` (locked decisions D-01..D-11)
- `.planning/ROADMAP.md` Phase 6 section (success criteria, requirement IDs)
- `.planning/REQUIREMENTS.md` ORC-01..05, QA-01..02 full text
- `.planning/phases/03-dynamic-subagents/03-03-SUMMARY.md`, `.planning/phases/04-background-control/04-03-SUMMARY.md`, `.planning/phases/05-tui-agents-strip/05-03-SUMMARY.md` (what Phases 3–5 actually shipped, verified against code)
- `cargo test --lib` run locally at HEAD: 1025 passed, 1 ignored, 0 failed
- `ls -la target/release/nanopi`: 4,881,744 bytes (pre-phase baseline for QA-02)

### Secondary (MEDIUM confidence)
- None used — all claims traced to direct repo inspection or locked CONTEXT.md decisions.

### Tertiary (LOW confidence)
- None.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — no new dependencies, pure internal wiring
- Architecture: HIGH — every pattern cited has a verified existing precedent in this exact codebase
- Pitfalls: HIGH — each pitfall was found by tracing actual code paths (`Context.tools` vs `registry`, `CONTROL_TOOLS` vs a hypothetical shared denylist, print-mode registry construction)

**Research date:** 2026-10-04
**Valid until:** 2026-11-03 (30 days — stable internal codebase, no fast-moving external dependency)

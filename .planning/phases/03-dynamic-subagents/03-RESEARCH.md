# Phase 3: Dynamic agents - Research

**Researched:** 2026-10-04
**Domain:** Rust CLI agent orchestration — extending an existing `nanopi -p` child-process dispatcher
**Confidence:** HIGH (this is almost entirely a codebase-extension exercise, not a new-library exercise)

## Summary

Phase 3 extends the already-complete, already-tested `agent` tool
(`src/tool/agent.rs`) so a dispatch can skip the predefined-agent-file
step entirely. Today `single`/`parallel`/`chain` all require an `agent`
field naming a markdown file under `~/.nanopi/agents` or
`.nanopi/agents`; `resolve_agent()` hard-fails with "Unknown agent" if
no file matches. This phase makes `agent` optional, adds three new
per-item override fields (`role`, `tools`, `model`), adds a built-in
general-purpose `AgentConfig` used when no file is named, and makes the
parent-side dispatch validate `tools`/`model` *before* spawning so a bad
request surfaces an in-band list of alternatives instead of a child
crash. Nothing about the child-process runtime, the brief/report
archive, or the registry (Phases 1–2) needs to change — this phase is
additive to `AgentItem`, `resolve_agent`, and `run_single`'s
`BriefSpec` construction.

**Primary recommendation:** Add the four optional fields to the tool
schema and to `AgentItem`; replace the current "look up by name or
fail" resolution with "look up by name if given, else use a built-in
default `AgentConfig`, then apply inline overrides (role/tools/model) on
top of whichever `AgentConfig` was selected" — this is a pure function,
extend `select_mode`/`parse_items`'s sibling tests the same way the
existing suite already does. Validate `tools` with
`ToolRegistry::standard().canonical_name()` (already exists,
case-insensitive) at dispatch time, not by letting the child's own
`--tools` parse fail. Validate `model` with `models::context_window()`
plus a same-vendor check derived from `vendor::pick_vendor`, and surface
the one genuine open question (cross-vendor requires "a provider
configured for that vendor", but nanopi's config is single-provider) to
the user rather than guessing at an answer.

## User Constraints (from CONTEXT.md)

### Locked Decisions

- **D-01:** The tool is named `agent` (user decision 2026-10-04;
  supersedes the earlier "keep the tool name `subagent`"). Its fields
  are:
  - `task`, required.
  - `agent`, optional. It names a predefined agent file.
  - `role`, optional. An inline role prompt, appended to the
    general-purpose base prompt.
  - `tools`, optional. An array of tool names.
  - `model`, optional.
  - `description`, optional. A short label of 3–6 words for the strip.
  - `tasks` and `chain` keep their current meaning. Each item accepts
    the same optional fields.
- **D-02:** With no `agent`, use a built-in general-purpose agent. Its
  base prompt covers working autonomously, finishing the task, and
  ending with a structured report (summary, files changed, open
  issues). By default it gets every tool that is not on the deny-list.
- **D-03:** When both `agent` and `role` / `tools` / `model` are given,
  the inline fields override the agent file's values.
- **D-04:** `tools` is checked against the registered tools, and the
  deny-list from Phase 1 D-10 always wins. Unknown or denied names fail
  the dispatch with an in-band error that lists the allowed tools.
- **D-05:** `model` must be one nanopi can resolve: a model id from the
  `models.rs` registry or the config. An unknown model is an in-band
  error. Cross-vendor is allowed only if a provider is configured for
  that vendor.
- **D-06:** The parent receives the agent's final report text, capped
  at about 8 KB. If it is longer, it is truncated with a pointer to
  `report.md`. The parent never receives the transcript.
- **D-07:** The tool description tells the model when to delegate:
  independent or exploratory work, or large reads that would flood the
  context. It also says to prefer one agent for sequential work.

### Claude's Discretion

- Exact wording of the general-purpose prompt and the tool description.

### Deferred Ideas (OUT OF SCOPE)

None recorded in CONTEXT.md for this phase (Phase 4–6 items — background
launch, TUI strip, orchestrator mode — are separate phases, not deferred
ideas within Phase 3's own scope).

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| DYN-01 | Dispatch by describing the task only; no agent name uses a general-purpose agent | `resolve_agent()` in `src/tool/agent.rs` becomes "name given → look up; else → built-in default" — see Architecture Patterns §1 |
| DYN-02 | Ad-hoc role prompt + toolset per call, checked against allowlist/deny-list | New `tools`/`role` fields on `AgentItem`; validate via `ToolRegistry::standard().canonical_name()` before spawn — see §2 |
| DYN-03 | Predefined agent files and single/parallel/chain modes keep working | Additive change only: `agent` field becomes optional, not removed; existing `resolve_agent`/`run_item`/`run_parallel`/`run_chain` tests must stay green unmodified |
| DYN-04 | Per-agent model choice | New `model` field on `AgentItem`, resolved via `models::context_window()` + vendor check — see §3 (also flags the one genuine open question, A1) |
| DYN-05 | Parent gets capped summary, not full transcript | **Already implemented** — `run_single`'s `REPORT_CAP = 64 * 1024` truncation exists today; D-06 asks for ~8 KB, a cap *reduction*, not new plumbing |

</phase_requirements>

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Mode selection (single/parallel/chain) | Tool layer (`src/tool/agent.rs`) | — | Pure parsing, already in `select_mode`/`parse_items`; extend in place |
| Agent resolution (named file vs. built-in default) | Tool layer (`resolve_agent`) | Agent definitions (`src/agent/agents.rs`) | `AgentConfig` is the existing unit of "role+tools+model+prompt"; a built-in default is just another `AgentConfig` value, not a new type |
| Inline override merge (role/tools/model) | Tool layer (new pure fn, e.g. `apply_overrides`) | — | D-03 override semantics are easiest to unit-test as a pure `AgentConfig -> AgentConfig` transform, following the existing `select_mode`/`substitute_previous` pattern |
| Tool allowlist validation | Tool layer, parent-side, pre-spawn | `ToolRegistry` (`src/tool/mod.rs`) | `canonical_name`/`standard()` already exist; validating before `build_child_args` turns a would-be child crash into an immediate in-band error (D-04) |
| Model validation | Tool layer, parent-side, pre-spawn | `models.rs`, `vendor/mod.rs` | `context_window()` and `pick_vendor()` already exist; no new module needed, just a new pure function composing them |
| Child process spawn, brief/report, registry | Unchanged (Phase 1/2) | — | `run_single`, `AgentRegistry`, `ChildLaunchSpec`, `brief.rs` need no changes for DYN-01..05 beyond passing the resolved `AgentConfig` through |

## Standard Stack

No new crates. This phase is pure extension of existing in-tree code.

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| (none new) | — | — | Phase 1/2 already settled on `tokio::process`, `serde_json`, `async_trait`; Phase 3 adds no I/O surface |

### Package Legitimacy Audit

Not applicable — no new packages are installed in this phase.

## Architecture Patterns

### System Architecture Diagram

```
Model emits `agent` tool call
        │
        ▼
AgentTool::execute(args, ctx)
        │
        ├─ select_mode(args)  ─────────────► Single | Parallel | Chain   [UNCHANGED]
        │
        ▼
parse item(s): { agent?, task, role?, tools?, model?, description?, cwd? }
        │                                              ▲ NEW optional fields
        ▼
resolve_agent_config(item, scope, run_cwd)        ◄── NEW merge point (replaces resolve_agent)
        │
        ├─ item.agent is Some(name)
        │       │
        │       ▼
        │   discover_agents() lookup, trust-gate (Project source)   [UNCHANGED]
        │       │
        │       ▼
        │   base AgentConfig from file
        │
        └─ item.agent is None
                │
                ▼
            built-in general-purpose AgentConfig (DYN-01, D-02)      [NEW]
        │
        ▼
apply_inline_overrides(base, role, tools, model)                      [NEW, pure fn]
        │  role   -> overrides system_prompt (appended, not replaced, per D-01 wording)
        │  tools  -> overrides tools list
        │  model  -> overrides model
        │
        ▼
validate_tools(resolved.tools, &ToolRegistry::standard())             [NEW — D-04]
   │  unknown/denied name -> in-band soft_error listing allowed tools, STOP (no spawn)
   ▼
validate_model(resolved.model)                                        [NEW — D-05]
   │  unresolvable id -> in-band soft_error, STOP (no spawn)
   ▼
run_single(launcher, &resolved_agent, task, cwd)                      [UNCHANGED]
   │
   ▼
build_child_args / build_child_env / spawn_and_collect_with           [UNCHANGED — Phase 1]
   │
   ▼
brief.md / report.md / registry state                                 [UNCHANGED — Phase 1/2]
   │
   ▼
ToolOutput capped to REPORT_CAP (tighten to ~8 KB per D-06)
```

### Recommended Project Structure

No new files. All changes land in the existing files:

```
src/tool/agent.rs     # AgentItem gains role/tools/model/description;
                       # parse_items validates them; resolve_agent_config
                       # replaces resolve_agent; apply_inline_overrides is new;
                       # validate_tools/validate_model are new pure fns;
                       # the tool spec() JSON schema gains the 4 fields
                       # and D-07's updated description text
src/agent/agents.rs   # a new `AgentConfig::general_purpose()` (or similar)
                       # constructor for the built-in default (D-02)
```

### Pattern 1: Optional-name resolution (DYN-01, D-02)

**What:** `resolve_agent()` currently does `discover_agents().find(name)
or soft_error`. Replace the signature to take `Option<&str>` for the
agent name; `None` returns a built-in `AgentConfig` instead of erroring.

**When to use:** Every dispatch path (`run_item`, called from single,
parallel-per-item, chain-per-item) goes through this one function, so
the built-in default is available identically in all three modes
(DYN-03 is satisfied by construction, not by special-casing).

**Example (following the existing style in `src/tool/agent.rs`):**
```rust
// Source: pattern extension of src/tool/agent.rs:492 resolve_agent()
fn resolve_agent_config(
    agent_name: Option<&str>,
    scope: AgentScope,
    run_cwd: &Path,
) -> Result<AgentConfig, ToolOutput> {
    match agent_name {
        Some(name) => resolve_agent(name, scope, run_cwd), // existing fn, unchanged
        None => Ok(AgentConfig::general_purpose()),         // NEW, D-02
    }
}
```

The built-in's `source` field needs a third `AgentSource` variant (e.g.
`AgentSource::BuiltIn`) or, more simply, reuse `AgentSource::User` since
the trust gate in `resolve_agent` only special-cases
`AgentSource::Project` — a built-in prompt is exactly as trusted as a
user-level file and should not need a new trust branch. **Recommend
reusing `AgentSource::User`** to avoid touching the trust-gate match arm
at all (keeps the DYN-03 "existing behavior unchanged" guarantee
mechanical rather than asserted).

### Pattern 2: Inline override merge (D-03)

**What:** A pure `AgentConfig -> AgentConfig` transform applied after
resolution, before the brief is rendered. CONTEXT.md D-01 says `role` is
"appended to the general-purpose base prompt" when there is no `agent`
file, but D-03 says inline fields "override the agent file's values"
when both are given. These are two different verbs (append vs.
override) for two different cases — this is worth being precise about
in the plan rather than flattening to one rule:

- No `agent` file (built-in default) + `role` given → `role` is
  **appended** to the built-in base prompt (D-01's wording, specific to
  the no-agent case).
- `agent` file given + `role` given → `role` **overrides** (replaces)
  the file's `system_prompt` (D-03's wording, specific to the override
  case).

**Example:**
```rust
// Source: new pure fn following substitute_previous's testability pattern
fn apply_inline_overrides(
    mut base: AgentConfig,
    is_builtin: bool,
    role: Option<&str>,
    tools: Option<&[String]>,
    model: Option<&str>,
) -> AgentConfig {
    if let Some(r) = role {
        base.system_prompt = if is_builtin {
            format!("{}\n\n{r}", base.system_prompt) // D-01: append
        } else {
            r.to_string() // D-03: override
        };
    }
    if let Some(t) = tools {
        base.tools = Some(t.to_vec()); // D-03: override
    }
    if let Some(m) = model {
        base.model = Some(m.to_string()); // D-03: override
    }
    base
}
```

This keeps `AgentConfig` as the single carrier type all the way to
`run_single`, so `run_single`'s existing `BriefSpec` construction
(`task.rs:745-765`) needs **zero changes** — it already reads
`agent.tools`, `agent.system_prompt`/`agent.description`, and
`agent.model`.

### Pattern 3: Pre-spawn validation (D-04, D-05)

**What:** Validate `tools` and `model` on the parent side, before
`run_single` is ever called, so a bad request never reaches
`build_child_args`/spawn. This is a deliberate shift from today's
behavior, where an invalid `--tools` name passed to the child would
currently cause the **child** to fail at its own CLI-arg stage
(`ToolRegistry::standard_with_allowlist` returning `Err` inside
`print.rs`, surfacing as a crashed child / "exit code 1" / possibly even
an unparseable-output soft error) rather than a clean, immediate,
specific message.

**tools validation — D-04:**
```rust
// Source: composes existing ToolRegistry::standard().canonical_name
fn validate_tools(tools: &[String]) -> Result<(), String> {
    let registry = crate::tool::ToolRegistry::standard();
    let mut unknown = Vec::new();
    for t in tools {
        // D-05/RT-05 deny-list: "agent" itself is always denied to a
        // dispatched child (a child never gets to re-dispatch). This is
        // the Phase-1-mentioned "deny-list [that] always wins" — in the
        // current codebase this deny-list is enforced structurally by
        // print.rs's `registry.remove("agent")` in agent_mode, not by a
        // separate named constant. Phase 3 should validate it here too,
        // at dispatch time, so the error is immediate rather than a
        // silently-stripped tool the child never had.
        if t.eq_ignore_ascii_case("agent") {
            unknown.push(t.clone());
            continue;
        }
        if registry.canonical_name(t).is_none() {
            unknown.push(t.clone());
        }
    }
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unknown or denied tool(s) {unknown:?}; allowed tools: {}",
            registry.names().join(", ")
        ))
    }
}
```

**model validation — D-05 (see also Open Questions / A1 below):**
```rust
// Source: composes existing models::context_window + vendor::pick_vendor
fn validate_model(model: &str, parent_vendor: &str) -> Result<(), String> {
    match crate::models::context_window(model) {
        Some(_) => Ok(()), // known id in the catalogue
        None => Err(format!(
            "unknown model {model:?}; not in nanopi's model registry \
             and no custom provider is configured for it"
        )),
    }
    // NOTE: see Open Question A1 — the "cross-vendor allowed only if a
    // provider is configured for that vendor" half of D-05 cannot be
    // implemented as specified today because nanopi's config.rs is
    // single-provider (one model/base_url/api_kind/vendor at a time),
    // not a provider-per-vendor map. A real implementation of the
    // cross-vendor clause needs a decision first (see A1).
}
```

### Anti-Patterns to Avoid

- **Validating tools/model inside the child instead of the parent.**
  This is what happens today implicitly via `--tools`/`--model` CLI
  parsing, and it produces an opaque child failure (exit code / signal /
  unparseable output) instead of D-04's "in-band error that lists the
  allowed tools". Validate before spawn.
- **Treating `role`'s append-vs-override split as one rule.** D-01 and
  D-03 use different verbs for different cases (see Pattern 2); flattening
  them to always-override or always-append contradicts one of the two
  locked decisions.
- **Reusing the `agent_scope` trust gate for the built-in default.**
  The built-in general-purpose agent is neither user- nor project-file
  sourced; do not accidentally route it through the `AgentSource::Project`
  trust check (it would either wrongly require trust or need a new,
  untested bypass branch). Mapping it to `AgentSource::User` sidesteps
  this for free.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Tool-name validation / canonicalization | A second name-matching table for the `agent` tool's inline `tools` field | `ToolRegistry::canonical_name` / `ToolRegistry::standard()` (`src/tool/mod.rs:504,568`) | Already handles `bash`/`Read_tool`/`GREP` case-insensitivity and the `_tool` suffix; a second implementation would drift |
| Model-id matching | A new model lookup / fuzzy matcher | `models::context_window()` (`src/models.rs:34`, prefix + case-insensitive) | Already the single source of truth driving the `/model` picker; reusing it keeps DYN-04's validation consistent with what a human sees in `/model` |
| Vendor/provider resolution | A new "which vendor serves this model" heuristic | `vendor::pick_vendor()` (`src/vendor/mod.rs:122`) | Already encodes base_url-domain-sniff + model-prefix + explicit-override precedence; re-deriving this for D-05's cross-vendor check would diverge from the provider the rest of nanopi actually uses |

**Key insight:** every piece of machinery DYN-01..05 needs (mode
dispatch, child spawn, brief/report, registry, tool allowlisting, model
catalogue, vendor sniffing) already exists and is already unit-tested.
This phase's entire job is to widen three structs (`AgentItem`'s parsed
shape, the tool's JSON schema, `AgentConfig` resolution) and add two
small pure validation functions — not to design new subsystems.

## Common Pitfalls

### Pitfall 1: Breaking DYN-03 while adding DYN-01

**What goes wrong:** Changing `resolve_agent`'s signature from
`&str -> Result<AgentConfig, ToolOutput>` to
`Option<&str> -> Result<AgentConfig, ToolOutput>` touches every call
site (`run_item`, both `run_parallel`'s and `run_chain`'s item loops,
plus ~6 existing tests that call `resolve_agent`/`run_item` directly
with a hardcoded agent name).

**Why it happens:** The existing test suite
(`parallel_item_unknown_agent_is_soft_error`,
`chain_stops_at_unknown_agent`, `unknown_agent_is_a_soft_error_...`,
`brief_and_dir_exist_before_spawn`, etc.) all construct `AgentItem` or
call `run_item` with a non-optional `agent: String` field. A signature
change ripples.

**How to avoid:** Keep `AgentItem.agent` as `Option<String>` from the
start (not `String`), so `parse_items` just does
`it.get("agent").and_then(|v| v.as_str()).map(String::from)` instead of
requiring it. The single-mode path in `execute()` similarly drops its
`.ok_or_else(...)` on the `agent` field. This is a minimal, additive
diff; **run the full existing `tool::agent::` test module after the
change and expect it to still compile and pass without modification**
(the acceptance gate for DYN-03).

**Warning signs:** If making `agent` optional requires editing more
than ~3 of the existing test bodies, the refactor has drifted from
"additive" into "restructuring", which risks silently changing
behavior the existing tests were pinning.

### Pitfall 2: D-06's 8 KB cap vs. the existing 64 KB `REPORT_CAP`

**What goes wrong:** `src/tool/agent.rs` already defines
`const REPORT_CAP: usize = 64 * 1024;` (Phase 1) and truncates
`report.md` to that size before returning it to the model. D-06 asks
for "about 8 KB" with "a pointer to report.md" on truncation. Naively
adding a *second* truncation step downstream of the existing one is
redundant and easy to get subtly wrong (e.g. double-counting UTF-8
boundary truncation, which the existing code already handles carefully
at `tool::agent.rs:819-825`).

**Why it happens:** Phase 1's 64 KB cap was written for its own research
scope (transcript/stdout bounding, not final-answer bounding) before
D-06 existed.

**How to avoid:** Just change the constant's value (and ideally give it
a name that reflects whose decision it encodes, e.g.
`const PARENT_REPORT_CAP: usize = 8 * 1024;`) and verify the pointer
message is already present — it is: the existing truncation appends
`"\n…(report truncated)"`, but D-06 additionally wants a path pointer.
Extend that string to include `report.md`'s path (already in scope as
`report` in that function) rather than adding a new truncation path.

### Pitfall 3: `description` field is cosmetic-only in this phase

**What goes wrong:** D-01 lists `description` as "a short label of 3–6
words for the strip" — but "the strip" is Phase 5's UI feature, which
does not exist yet. A plan for this phase might either (a) silently
drop the field, breaking forward-compatibility with Phase 5's schema
expectations, or (b) over-build strip-related plumbing that belongs in
Phase 5.

**Why it happens:** The schema is being added now so later phases don't
need a breaking schema change, but there's nothing to *display* it
yet.

**How to avoid:** Accept and store `description` on `AgentItem` /
propagate it into the registry's `AgentEntry` (or at minimum into
`brief.md`'s front matter next to `role`/`model`/`tools`) so Phase 4/5
can read it later, but do not build any TUI or strip rendering in this
phase. A test asserting the field round-trips into the brief's front
matter is sufficient acceptance evidence.

## Code Examples

### Extending the tool JSON schema (D-01, D-07)

```rust
// Source: pattern extension of src/tool/agent.rs:388-436 (existing spec())
"properties": {
    "task": { "type": "string", "description": "..." },
    "agent": { "type": "string", "description": "optional: name of a predefined agent file. Omit to use a general-purpose agent." },
    "role": { "type": "string", "description": "optional: inline role/system prompt for this dispatch." },
    "tools": { "type": "array", "items": {"type": "string"}, "description": "optional: restrict this agent to exactly these tools." },
    "model": { "type": "string", "description": "optional: model id for this agent (defaults to your own model)." },
    "description": { "type": "string", "description": "optional: 3-6 word label for this dispatch." },
    // tasks/chain items gain the same 4 optional fields (D-01)
}
```

### D-07 tool description guidance (discretion — suggested wording)

> "Use this to delegate independent or exploratory work, or any read
> that would flood your own context (e.g. scanning many files). For a
> sequence of dependent steps, prefer one agent working through them
> over multiple short-lived dispatches."

This directly encodes D-07's two bullet points and can be inserted into
the existing `description:` `concat!(...)` block (`src/tool/agent.rs:371-387`)
without restructuring it.

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|---------------|--------|
| `agent` field required, lookup-or-fail | `agent` field optional, lookup-or-built-in-default | This phase (DYN-01) | `AgentItem.agent: String` → `Option<String>`; `resolve_agent` → `resolve_agent_config` |
| Tool allowlist validated only at child CLI-parse time | Validated at parent dispatch time, pre-spawn | This phase (D-04) | New pre-spawn `validate_tools` fn; child-side validation remains as defense-in-depth, unchanged |
| `REPORT_CAP = 64 KiB` | Cap reduced to ~8 KiB per D-06 | This phase | One constant change plus an updated truncation message with a path pointer |

**Deprecated/outdated:** None — this phase adds to, rather than
replaces, Phase 1/2 machinery.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | D-05's "cross-vendor is allowed only if a provider is configured for that vendor" assumes nanopi can have *multiple* vendor configurations active simultaneously, so a model from vendor B can be validated against "is vendor B configured". `[ASSUMED]` — **this does not match the current codebase**: `src/config.rs`'s `Config` struct carries exactly one `model`/`base_url`/`api_kind`/`vendor` at a time (single active provider), confirmed by reading `config.rs:37-80` and `vendor::pick_vendor`'s single-provider signature. There is no provider-per-vendor map today. | D-05 validation (Pattern 3, Open Questions §1) | If this ambiguity isn't resolved before planning, the planner may either (a) silently implement "same-vendor-as-parent only" (weakening DYN-04's "choose a model per agent" to same-vendor-only), or (b) build unrequested multi-provider config plumbing far outside this phase's scope. Needs an owner decision: is cross-vendor dispatch in scope for v1, and if so, how does nanopi learn credentials for a second vendor? |
| A2 | The built-in general-purpose agent's trust level should be treated identically to a user-level agent file (`AgentSource::User`), not requiring the `AgentSource::Project` trust gate. `[ASSUMED]` — reasonable by analogy (a built-in prompt ships with the binary, trusted by definition, same tier as a user's own `~/.nanopi/agents/*.md`), but not explicitly stated in CONTEXT.md. | Pattern 1 | Low risk: worst case is a one-line `match` arm correction if the planner/owner disagrees; does not block implementation since the alternative (a new `AgentSource::BuiltIn` variant, always trusted) is a trivial swap. |
| A3 | D-04's "deny-list from Phase 1 D-10" is assumed to refer to the *structural* fact that `agent`/`subagent` is always excluded in agent-mode children (`src/mode/print.rs:230` `registry.remove("agent")` when `child.agent_mode`), since no literal `SUBAGENT_DENIED_TOOLS`/`AGENT_DENIED_TOOLS` constant exists in the current (child-process) codebase — that constant belonged to the **superseded in-process design** (`phases/01-child-process-runtime/superseded-inprocess/`) and was not carried forward when the runtime was rewritten as child-process-based. `[ASSUMED]` | Pattern 3 (validate_tools) | If wrong, the planner might look for a `DENIED_TOOLS` constant that no longer exists and either reintroduce dead code or miss that the deny-list is now enforced via `registry.remove("agent")` plus (this phase's new) parent-side rejection of `"agent"` in an inline `tools` list. |

## Open Questions

1. **D-05 cross-vendor validation: what does "a provider is configured
   for that vendor" mean in a single-provider config system?**
   - What we know: `models::context_window(model_id)` tells you which
     vendor a *known* model belongs to (`ModelInfo.vendor`).
     `vendor::pick_vendor()` tells you which vendor the *parent's own*
     config currently resolves to. `config.rs` has exactly one
     model/base_url/api_kind/vendor slot — there is no "list of
     configured providers" to check a second vendor against.
   - What's unclear: whether D-05 means (a) cross-vendor dispatch is
     effectively impossible today and the clause is forward-looking /
     dead until a future multi-provider feature ships, or (b) the
     intended check is narrower than it sounds — e.g. "the requested
     model's vendor equals `vendor::pick_vendor()`'s result" (same-vendor
     only, in which case "cross-vendor... only if configured" really
     means "cross-vendor is never allowed today, full stop" and the
     wording is scaffolding for a future phase).
   - Recommendation: surface this to the user during `/gsd:discuss-phase`
     or plan review before committing to an implementation. A safe
     default that satisfies D-05's letter without inventing new config
     surface: reject any model whose `ModelInfo.vendor` differs from
     `vendor::pick_vendor()`'s current result, with an in-band error
     explaining that only same-vendor models are resolvable today (since
     nanopi has one active provider). This keeps DYN-04 functional
     ("choose a model per agent" — any model of the current vendor) while
     being honest that true cross-vendor isn't buildable without new
     config plumbing this phase doesn't own.

2. **Does the built-in general-purpose agent need a distinct name for
   logging/brief front-matter (e.g. `"general-purpose"`) even though it
   has no backing file?**
   - What we know: `brief.md`'s front matter already has a `role` field
     (via `fm_value`) and `AgentConfig.name` is used in
     `format_parallel`/`format_chain`'s `### [{agent}]` section headers
     and in registry snapshots for diagnostics.
   - What's unclear: whether leaving `AgentConfig.name` empty or using a
     placeholder like `"general-purpose"` is expected; CONTEXT.md doesn't
     specify.
   - Recommendation: use a fixed literal (`"general-purpose"`) as
     `AgentConfig.name` for the built-in default — it's user-visible in
     parallel/chain output sections and in registry diagnostics, so an
     empty string there would look like a bug.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` / `#[tokio::test]` via `cargo test` |
| Config file | none (plain `Cargo.toml`, no custom test harness) |
| Quick run command | `cargo test --lib tool::agent::` |
| Full suite command | `cargo test --lib` (917 passed, 1 ignored at time of writing; `cargo test --lib --features wasm` for the wasm-gated suite) |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| DYN-01 | `{task}` only dispatches a general-purpose agent and returns a result | unit (pure resolution) + integration (`sh` fake binary, following `brief_and_dir_exist_before_spawn`'s pattern) | `cargo test --lib tool::agent::tests::` | ❌ Wave 0 — new test, e.g. `no_agent_name_uses_general_purpose_default` |
| DYN-02 | Inline `role`+`tools`+`model` dispatch runs with exactly those, validated against allowlist | unit (`validate_tools`/`apply_inline_overrides` pure fns) + integration (asserts child argv/brief reflect overrides, following `build_child_args_contract`'s pattern) | `cargo test --lib tool::agent::tests::` | ❌ Wave 0 — new tests for `apply_inline_overrides`, `validate_tools` rejection message shape |
| DYN-03 | Existing predefined-agent + single/parallel/chain modes unchanged | regression — **run the existing suite unmodified** | `cargo test --lib tool::agent::` | ✅ Existing (`resolve_agent`, `run_item`, `run_parallel`, `run_chain`, `select_mode`, `parse_items` tests already present, lines 1188–1911 of `src/tool/agent.rs`) |
| DYN-04 | Per-agent model selection, validated and resolvable | unit (`validate_model` pure fn, incl. unknown-id rejection and the A1 same-vendor boundary once resolved) | `cargo test --lib tool::agent::tests::` or `cargo test --lib models::` | ❌ Wave 0 — depends on resolving Open Question 1 first |
| DYN-05 | Parent receives capped summary, never full transcript | regression + one updated assertion (cap value, pointer message) | `cargo test --lib tool::agent::tests::` | ✅ Existing cap mechanism (`REPORT_CAP`); ❌ Wave 0 — update/add a test asserting the new ~8 KB cap and the `report.md` path in the truncation message |

### Sampling Rate
- **Per task commit:** `cargo test --lib tool::agent::`
- **Per wave merge:** `cargo test --lib` (full suite; add `--features wasm` once per phase if any plugin-adjacent code is touched — it is not expected to be, since this phase stays inside `src/tool/agent.rs` and `src/agent/agents.rs`)
- **Phase gate:** Full suite green (`cargo test --lib`, currently 917 passed / 1 ignored baseline) before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] No test file gaps — `src/tool/agent.rs`'s existing `#[cfg(test)] mod tests` block is the right home for every new test; no new test file needed.
- [ ] New tests needed (see table above): `no_agent_name_uses_general_purpose_default`, `apply_inline_overrides_appends_role_for_builtin_overrides_for_named`, `validate_tools_rejects_unknown_and_agent_itself`, `validate_model_rejects_unknown_id`, `report_cap_is_8kb_with_path_pointer` (name an existing `REPORT_CAP`-asserting test if one exists to update it, else add a new one).
- [ ] Framework install: none — no new dependency.

## Common Pitfalls
(see §"Common Pitfalls" above — kept as the canonical heading per the house template; not duplicated here.)

## Security Domain

`security_enforcement` is not set in `.planning/config.json` to `false`,
so this section is included.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-------------------|
| V2 Authentication | No | No new auth surface; API key handling is unchanged from Phase 1 (`OPENAI_API_KEY` env only, never argv — `build_child_env`) |
| V3 Session Management | No | Child session files are Phase 1/2 territory, untouched |
| V4 Access Control | Yes | The inline `tools` list is exactly an access-control list for what the dispatched child may do; D-04's validation **is** V4 enforcement — see Pattern 3 |
| V5 Input Validation | Yes | `role`/`tools`/`model`/`description` are all model-controlled free-form input reaching a child process's argv/env/brief file; existing `fm_value` sanitizer (brief.rs) already neutralizes front-matter injection for `role`'s text — no new sanitizer needed for the new fields since they flow through the same `BriefSpec`/`render_brief_with_meta` path |
| V6 Cryptography | No | No new crypto surface |

### Known Threat Patterns for this stack

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|----------------------|
| A model-crafted `tools` list naming `"agent"` to attempt recursive self-dispatch | Elevation of Privilege | Reject `"agent"`/`"subagent"` explicitly in `validate_tools` (Pattern 3) in addition to the existing structural removal in `print.rs`'s agent-mode branch — defense in depth, since this phase's validation runs *before* the child-mode removal would even apply |
| A model-crafted `role` string containing front-matter-breaking syntax (e.g. a fake `---` block or a fake `## Amendment N` heading) to corrupt `brief.md` parsing | Tampering | Already mitigated by the existing `fm_value` (120-char cap, newline-collapse) and `escape_body`'s `## Amendment ` / marker escaping in `src/agent/brief.rs` — confirm the new fields flow through `BriefSpec`/`render_brief_with_meta`, not a bypassing path |
| A model requesting an oversized `tools` array or deeply nested `tasks`/`chain` payload to cause resource exhaustion before validation | Denial of Service | Already bounded: `MAX_TASKS = 8` (parallel fan-out cap, unchanged); no equivalent cap exists today on an individual item's `tools` array length — consider capping it (e.g. to the total number of registered tools, which `ToolRegistry::standard().names().len()` already bounds structurally since duplicates/unknowns are rejected by `validate_tools` anyway) |

## Sources

### Primary (HIGH confidence — direct codebase read, this session)
- `src/tool/agent.rs` (full file, 1911 lines) — current `AgentTool`, `select_mode`, `parse_items`, `resolve_agent`, `run_single`, `build_child_args`, existing test suite
- `src/agent_registry.rs` (full file) — `AgentRegistry`, `AgentState`, `ChildGuard`
- `src/agent/agents.rs` (partial) — `AgentConfig`, `AgentSource`, `AgentScope`, `discover_agents`, frontmatter parsing
- `src/agent/brief.rs` (partial) — `BriefSpec`, `BriefMeta`, `render_brief_with_meta`, `fm_value`, front-matter parse/rewrite
- `src/tool/mod.rs` (partial) — `ToolRegistry::standard()`, `canonical_name`, `standard_with_allowlist`, `split_tool_allowlist`
- `src/models.rs` (partial) — `context_window`, `models_for_vendor`, `ModelInfo`
- `src/vendor/mod.rs` (symbol list) — `pick_vendor`
- `src/config.rs` (partial) — single-provider `Config` struct, confirming the A1 ambiguity
- `src/mode/print.rs` (partial) — confirms `registry.remove("agent")` in `child.agent_mode`, the actual current deny mechanism
- `.planning/phases/03-dynamic-subagents/03-CONTEXT.md` — locked decisions D-01..D-07
- `.planning/ROADMAP.md`, `.planning/REQUIREMENTS.md`, `.planning/STATE.md` — phase goal, requirement text, prior-phase completion status
- `.planning/phases/01-child-process-runtime/*SUMMARY*.md`, `.planning/phases/02-archive-lifecycle/*SUMMARY*.md` — confirms current runtime is child-process-based (not the superseded in-process design also present under `01-child-process-runtime/superseded-inprocess/`)
- `cargo test --lib` run in this session — 917 passed, 1 ignored (current baseline)

### Secondary (MEDIUM confidence)
None needed — this phase required no external library research.

### Tertiary (LOW confidence)
None.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — no new dependencies, pure extension of existing tested code
- Architecture: HIGH — every pattern cited is read directly from the current source, not inferred
- Pitfalls: HIGH for Pitfalls 1–3 (derived from direct inspection of the existing test suite's shape); MEDIUM for the D-05 cross-vendor question (A1), which is a genuine specification gap rather than a research gap

**Research date:** 2026-10-04
**Valid until:** No external dependency; valid until `src/tool/agent.rs`, `src/config.rs`, or `src/models.rs` are next modified by an unrelated change (effectively, until Phase 3 starts implementation — re-check `cargo test --lib` baseline count if more than a few days pass before planning).

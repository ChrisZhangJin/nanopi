# Phase 3: Dynamic agents - Context

**Gathered:** 2026-10-03
**Status:** Ready for planning

<domain>
## Phase Boundary

The model can start an agent by describing the task, optionally with
a role prompt, a toolset and a model, without any predefined agent
file. Existing agent files and single / parallel / chain modes keep
working. Covers DYN-01..05.

</domain>

<decisions>
## Implementation Decisions

### Tool schema
- **D-01:** The tool is named `agent` (user decision 2026-10-04; supersedes the earlier "keep the tool name `subagent`"). Its fields are:
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

### Validation
- **D-04:** `tools` is checked against the registered tools, and the
  deny-list from Phase 1 D-10 always wins. Unknown or denied names fail
  the dispatch with an in-band error that lists the allowed tools.
- **D-05:** `model` must be one nanopi can resolve: a model id from the
  `models.rs` registry or the config. An unknown model is an in-band
  error. Cross-vendor is allowed only if a provider is configured for
  that vendor.

### Result to parent
- **D-06:** The parent receives the agent's final report text, capped
  at about 8 KB. If it is longer, it is truncated with a pointer to
  `report.md`. The parent never receives the transcript.
- **D-07:** The tool description tells the model when to delegate:
  independent or exploratory work, or large reads that would flood the
  context. It also says to prefer one agent for sequential work.

### Claude's Discretion
- Exact wording of the general-purpose prompt and the tool description.

</decisions>

<specifics>
## Specific Ideas

Reference: Claude Code's general-purpose agent and AgentTool schema
(`claude-code-haha-main/src/tools/AgentTool/`,
`built-in/generalPurposeAgent.ts`).

</specifics>

<canonical_refs>
## Canonical References

- `.planning/REQUIREMENTS.md` — DYN-01..05
- `.planning/research/FEATURES.md`
- `src/tool/agent.rs` — the current schema and the `select_mode` /
  `parse_items` helpers
- `src/models.rs` — model registry

</canonical_refs>

<code_context>
## Existing Code Insights

The `select_mode` and `parse_items` pure helpers and their tests can be
extended rather than rewritten.

</code_context>

## Addendum (2026-10-04, planning)

- D-05 interpretation: nanopi has a single active provider, so inline `model` is validated against that provider's vendor only; other-vendor models are rejected with a clear error. Custom endpoints (vendor "fallback") skip the vendor check. Flagged to the user for override.
- Built-in default agent name: `"general-purpose"`.

---
phase: 03-dynamic-subagents
reviewed: 2026-10-04T08:38:01Z
depth: standard
files_reviewed: 7
files_reviewed_list:
  - src/agent/agents.rs
  - src/models.rs
  - src/agent/brief.rs
  - src/archive.rs
  - src/agent_registry.rs
  - src/tool/agent.rs
  - src/main.rs
findings:
  critical: 1
  warning: 2
  info: 2
  total: 5
status: issues_found
---

# Phase 03: Code Review Report

**Reviewed:** 2026-10-04T08:38:01Z
**Depth:** standard
**Files Reviewed:** 7
**Status:** issues_found

## Summary

Reviewed the diff since `a77a547` implementing optional/dynamic agent dispatch: built-in `general-purpose` agent, inline `role`/`tools`/`model`/`description` overrides, pre-spawn validation (`validate_tools`/`validate_model`), the 8 KiB parent report cap, and the updated tool schema/description. The brief-label, model-vendor, and report-cap additions are small, pure, well-tested units with no defects found. The one critical finding is a privilege-escalation bypass in the new `tools` deny-list enforcement: the explicit `"agent"`/`"subagent"` block (D-04, "deny-list always wins") can be trivially sidestepped with a `_tool`-suffixed or differently-cased variant that the tool registry's own name-mangling tolerance (`canonical_name`) resolves back to the literal `agent` tool, which **is** registered in `ToolRegistry::standard()`. This lets a dispatched agent re-acquire the `agent` tool despite the deny-list, defeating the one guard that prevents unbounded recursive agent spawning. Two warnings and two info items round out the findings; none of the Plan 01/03 additions (brief label, model vendor lookup, report cap, schema) have bugs.

## Critical Issues

### CR-01: Inline `tools` deny-list can be bypassed via tool-name mangling, re-granting the `agent` tool

**File:** `src/tool/agent.rs:596-627` (`validate_tools`)

**Issue:** `validate_tools` checks the raw requested name against `DENIED_TOOLS` (`"agent"`, `"subagent"`, case-insensitive exact match) *before* canonicalizing it. But if the raw name isn't an exact (case-insensitive) match, the code falls through to `registry.canonical_name(t)`, which lowercases the name and strips a trailing `_tool` suffix (`src/tool/mod.rs:504-514`) before looking it up. `ToolRegistry::standard()` registers the `agent` tool itself under the key `"agent"` (`src/tool/mod.rs:577`). So an inline override of `"tools": ["agent_tool"]` (or `"AGENT_TOOL"`, `"Agent_Tool"`, etc.):

1. Fails the raw deny-list check — `"agent_tool"` is not case-insensitive-equal to `"agent"` or `"subagent"`.
2. Reaches `registry.canonical_name("agent_tool")` → lowercases to `"agent_tool"` → strips `_tool` suffix → `"agent"` → `registry.tools.contains_key("agent")` is `true` → returns `Some("agent".to_string())`.
3. `"agent"` is pushed into the validated, canonicalized `out` list and returned as an *allowed* tool — the deny-list is never re-checked against the canonicalized name.

A dispatched child agent can therefore end up with the `agent` tool in its allowlist despite D-04's explicit guarantee ("the deny-list from Phase 1 D-10 always wins"). This defeats the one mechanism that bounds how deep agent-spawns-agent recursion can go, and is a privilege-escalation / resource-exhaustion vector (every recursive layer can itself spawn more agents, bypassing the intended one-level delegation model the spec describes).

The existing test `validate_tools_denies_agent_and_subagent` only exercises the literal strings `"agent"`, `"subagent"`, `"AGENT"` — it never exercises the `_tool`-suffixed mangled form that `canonical_name` itself documents as a supported input shape, so the gap wasn't caught.

**Fix:** Check the deny-list against the *canonicalized* name too (or canonicalize first, then deny-check once):

```rust
fn validate_tools(tools: &[String]) -> Result<Vec<String>, String> {
    let registry = crate::tool::ToolRegistry::standard();
    let allowed: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|n| !DENIED_TOOLS.iter().any(|d| d.eq_ignore_ascii_case(n)))
        .collect();
    let mut bad: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for t in tools {
        if DENIED_TOOLS.iter().any(|d| d.eq_ignore_ascii_case(t)) {
            bad.push(t.clone());
            continue;
        }
        match registry.canonical_name(t) {
            // Re-check the deny-list on the *canonical* name — canonicalization
            // can fold a mangled name (e.g. "agent_tool") back onto a denied
            // literal ("agent"), and that must still be denied.
            Some(canonical) if DENIED_TOOLS.iter().any(|d| d.eq_ignore_ascii_case(&canonical)) => {
                bad.push(t.clone());
            }
            Some(canonical) => {
                if !out.contains(&canonical) {
                    out.push(canonical);
                }
            }
            None => bad.push(t.clone()),
        }
    }
    if !bad.is_empty() {
        return Err(format!(
            "unknown or denied tool(s): {}. Allowed tools: {}",
            bad.join(", "),
            allowed.join(", ")
        ));
    }
    Ok(out)
}
```

Add a regression test such as:
```rust
#[test]
fn validate_tools_denies_mangled_agent_names() {
    assert!(validate_tools(&["agent_tool".into()]).is_err());
    assert!(validate_tools(&["AGENT_TOOL".into()]).is_err());
    assert!(validate_tools(&["Agent_Tool".into()]).is_err());
}
```

## Warnings

### WR-01: `label`/`description` have no length or content guidance enforced beyond the generic 120-char/single-line sanitizer

**File:** `src/agent/brief.rs:92-96`, `src/tool/agent.rs:467-469`

**Issue:** The schema documents `description` as "optional: 3-6 word label", but nothing enforces or even loosely bounds word count — the model can pass an arbitrary string up to the generic `fm_value` 120-char cap, which will render as a long `label:` line in `brief.md` and in listings that assume a short label. This is cosmetic (no injection, no crash — `fm_value` still collapses newlines), but the "listing" UX this field exists for (per the plan summary, "shown in listings") will degrade silently for any dispatch where the model ignores the word-count guidance.

**Fix:** Either enforce a stricter cap (e.g. ~40 chars) in `opt_str_field`/at the `description` call site, or soften the schema wording to avoid promising a guarantee the code doesn't keep. Low priority; does not block.

### WR-02: `validate_model`'s "unknown model" error suggests only the parent's vendor's models, even when the active vendor is `fallback`/custom

**File:** `src/tool/agent.rs:649-661`

**Issue:** When `model_vendor(model)` returns `None` (the id isn't in the static catalogue at all), the error message always lists `models_for_vendor(parent_vendor)` — but if `parent_vendor` is `None` or `Some("fallback")` (custom endpoint), `models_for_vendor("fallback")` returns an empty list (no catalogue entries have vendor `"fallback"`), so the error becomes `"unknown model \"x\". Models available from the active provider (fallback): "` — an empty, unhelpful suggestion list even though the whole point of the `fallback`/no-vendor branch (per the code's own doc comment) is "any registry-known model is accepted since there is no catalogue to check against." The error message for the *unknown-to-registry* case doesn't honor that same leniency message-wise — it still implies a vendor-restricted catalogue exists.

**Fix:** When `parent_vendor` is `None` or `"fallback"`, phrase the error without implying a vendor-scoped list, e.g. `"unknown model {model:?}; it is not in nanopi's model catalogue"` instead of naming an empty vendor list. Minor UX issue, not a correctness bug (the actual validation logic for *known* models is correct — see D-05 safe-default handling at line 662-668).

## Info

### IN-01: `cap_report`'s truncation note hardcodes "8 KB" instead of referencing `PARENT_REPORT_CAP`

**File:** `src/tool/agent.rs:770-773`

**Issue:** The literal string `"...(report truncated at 8 KB; full report: {})"` will silently go stale if `PARENT_REPORT_CAP` is ever changed (e.g. to 16 KiB), since nothing ties the string to the constant. Also technically `PARENT_REPORT_CAP = 8 * 1024` is 8 KiB, not 8 KB (1000 bytes) — a nit, but worth noting since D-06 is specifically about byte-accurate capping.

**Fix:** Derive the number in the message from the constant, e.g. `format!("\n…(report truncated at {} KiB; full report: {})", PARENT_REPORT_CAP / 1024, report_path.display())`.

### IN-02: `DENIED_TOOLS` comment claims parity with "the deny applied to a spawned child's own registry in `mode::print`" without the two lists being shared

**File:** `src/tool/agent.rs:586-590`

**Issue:** The comment says this list "mirrors the deny applied to a spawned child's own registry in `mode::print`," but `DENIED_TOOLS` is a separate `const` from whatever list `mode::print` uses — if one is ever updated without the other, they silently drift apart with no compiler or test signal tying them together. This is a maintainability smell rather than a current bug (not verified to have already drifted), but combined with CR-01 it's worth tightening: a single shared source of truth for "tools an agent may never hold" would make the deny-list genuinely authoritative.

**Fix:** Extract a single `pub(crate) const AGENT_DENY_LIST` (or function) used by both `mode::print`'s child-registry construction and `validate_tools`, so the two can't diverge.

---

_Reviewed: 2026-10-04T08:38:01Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_

---
phase: 03-dynamic-subagents
fixed_at: 2026-10-04T00:00:00Z
review_path: .planning/phases/03-dynamic-subagents/03-REVIEW.md
iteration: 1
findings_in_scope: 4
fixed: 4
skipped: 0
status: all_fixed
---

# Phase 03: Code Review Fix Report

**Fixed at:** 2026-10-04
**Source review:** .planning/phases/03-dynamic-subagents/03-REVIEW.md
**Iteration:** 1

**Summary:**
- Findings in scope: 4 (CR-01, WR-01, WR-02, IN-01)
- Fixed: 4
- Skipped: 0

## Fixed Issues

### CR-01: Inline `tools` deny-list can be bypassed via tool-name mangling, re-granting the `agent` tool

**Files modified:** `src/tool/agent.rs`
**Commit:** a7a3ce1
**Applied fix:** In `validate_tools`, after calling `registry.canonical_name(t)`, the canonicalized name is now re-checked against `DENIED_TOOLS` before being accepted. A mangled name like `"agent_tool"` or `"AGENT_TOOL"` that canonicalizes back to `"agent"` is now rejected, closing the privilege-escalation path. Added regression test `validate_tools_denies_mangled_agent_names` covering `"agent_tool"`, `"AGENT_TOOL"`, and `"Agent_Tool"`.

### WR-01: `label`/`description` have no length or content guidance enforced beyond the generic 120-char/single-line sanitizer

**Files modified:** `src/tool/agent.rs`
**Commit:** df42d5c
**Applied fix:** Chose the lower-risk option from the review's two suggestions: softened the tool schema wording for the `description` field (all three occurrences — single/parallel/chain modes) from "optional: 3-6 word label" to "optional: short label (aim for 3-6 words; may be truncated if longer), shown in listings," so the schema no longer promises an enforcement guarantee the code doesn't keep. Did not add new truncation/validation logic, to avoid changing runtime behavior for a cosmetic, low-priority issue.

### WR-02: `validate_model`'s "unknown model" error suggests only the parent's vendor's models, even when the active vendor is `fallback`/custom

**Files modified:** `src/tool/agent.rs`
**Commit:** 9af8808
**Applied fix:** In `validate_model`, when the model id isn't in the catalogue at all (`model_vendor` returns `None`), the error now branches: for `parent_vendor` of `None` or `Some("fallback")`, it returns `"unknown model {model:?}; it is not in nanopi's model catalogue"` instead of naming an empty vendor-scoped list; for any other known vendor, it keeps the original vendor-scoped suggestion list behavior.

### IN-01: `cap_report`'s truncation note hardcodes "8 KB" instead of referencing `PARENT_REPORT_CAP`

**Files modified:** `src/tool/agent.rs`
**Commit:** 8a740c4
**Applied fix:** The truncation message in `cap_report` now derives the size from `PARENT_REPORT_CAP / 1024` and uses the byte-accurate unit "KiB" instead of the hardcoded, incorrect "8 KB" literal.

## Skipped Issues

None — all in-scope findings were fixed.

---

_Fixed: 2026-10-04_
_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_

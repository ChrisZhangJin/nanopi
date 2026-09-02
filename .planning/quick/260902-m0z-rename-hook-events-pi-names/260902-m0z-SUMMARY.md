---
phase: quick
plan: 260902-m0z
subsystem: hooks
tags: [hooks, config, breaking-change, docs]
requires: []
provides: [pi-hook-event-names, honest-session-payloads, retired-key-hard-error]
affects: [src/agent/hook.rs, src/agent/loop_.rs, src/mode/tui.rs, src/mode/print.rs, src/config.rs, src/settings.rs]
tech-stack:
  added: []
  patterns: [serde-deny-unknown-fields, fire-before-swap-seam]
key-files:
  created:
    - .release-notes-v0.12.0.md
    - .planning/quick/260902-m0z-rename-hook-events-pi-names/deferred-items.md
  modified:
    - src/agent/hook.rs
    - src/agent/loop_.rs
    - src/mode/tui.rs
    - src/mode/print.rs
    - src/config.rs
    - src/settings.rs
    - config.toml.example
    - README.md
    - README_zh.md
    - docs/v0.5-research.md
decisions:
  - Retired hook keys are a hard config-load error with no alias/compat period, per plan
  - session_shutdown/session_start now fire on every agent swap (/new, /resume, /fork, /import), not just process exit
  - session_id is always the real session id; the reason for any session-lifecycle event lives exclusively in arguments.reason
metrics:
  duration: "~2.5h (across the full session, this segment: Task 4 only)"
  completed: 2026-09-02
---

# Phase quick Plan 260902-m0z: Rename hook events to PI's names Summary

Renamed the four Claude-Code-named hook events to PI's names across config
keys and the `NANOPI_EVENT` env var, made the four retired keys a hard
config-load error with no alias, fixed session-lifecycle payloads to carry
an honest `session_id` and a real `arguments.reason`, and added a
fire-before-swap seam so `session_shutdown`/`session_start` bracket every
agent swap (`/new`, `/resume`, `/fork`, `/import`).

## Tasks Completed

1. **Rename the four events** (code, doc comments, tests) — commit `7a15138`
2. **Honest session payloads** — separate `subject`/`session_id` params on
   `run_session_hooks`, `reason` moved into `arguments` — commits `f329217`
   (feat) + `a92793f` (docs: deferred-items.md tracking pre-existing clippy
   failures)
3. **Fire-before-swap seam** — `swap_agent_with_reason` in `src/mode/tui.rs`
   fires `session_shutdown` on the outgoing agent, swaps, then fires
   `session_start` on the incoming agent; wired into all four swap call
   sites (`/new`, `/resume`, `/fork`, `/import`) — commit `93a4994`
4. **Retired keys hard error + docs** — `#[serde(deny_unknown_fields)]` on
   `HooksSection`, `retired_hook_key_error()` in `src/agent/hook.rs` wired
   into both `src/config.rs::load_one` and `src/settings.rs::load_one`,
   `config.toml.example` rename table + session payload contract,
   `README.md`/`README_zh.md` hooks sections rewritten (real Chinese
   translation, not stub), one-line banner on `docs/v0.5-research.md` §6,
   `.release-notes-v0.12.0.md` written absorbing v0.11.0's notes,
   `ead885a`'s six fixes, and this plan's three breaking changes under
   `## Compatibility` — commit `9caa4c5`

All four tasks executed in order, exactly as planned. No checkpoints hit,
no auth gates encountered.

## Verification

- `cargo test --features wasm`: 575 unit + 6 doc + 22 integration, all
  passing (0 failed, 1 ignored) — matches the plan's expected baseline
- `cargo clippy --features wasm --all-targets -- -D warnings`: 55 errors,
  all pre-existing (documented in `deferred-items.md`; baseline was 52 at
  plan start, grew to 55 after Task 2's edits touched files with existing
  clippy issues — no new clippy errors were introduced by this plan's
  changes)
- `cargo build --release` (no wasm feature): compiles clean
- All Task 4 verify-block greps pass: `deny_unknown_fields` present,
  `tool_execution_start` present in README.md/README_zh.md/
  config.toml.example/.release-notes-v0.12.0.md, `arguments.reason`
  present in release notes + example config, release notes mention
  "steer", "slash command", "url_allowlist", "/tools", v0.5-research.md §6
  banner references `v0.12-events`, release notes have a `## Compatibility`
  header, no `serde(alias` anywhere in settings.rs/config.rs, VERSION
  untouched
- Overall plan verification: no stray `pre_tool_use`/`post_tool_use`/
  `user_prompt_submit` outside the retired-key table (doc comment) and
  loader tests (which intentionally construct configs using retired keys
  to test the hard-error path); no `fire_session_end`/`HookEvent::SessionEnd`
  remnants anywhere; `git status --porcelain` clean (no untracked PNGs
  present in this worktree); `git diff HEAD~4 --name-only` does not include
  VERSION; `git tag --points-at HEAD` is empty

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] docs/v0.5-research.md banner grep window too narrow**
- **Found during:** Task 4 verification
- **Issue:** Plan's verify grep is `grep -A2 '^## 6' docs/v0.5-research.md
  | grep -q 'v0.12-events'` — a 2-line window. My first draft of the banner
  put the `docs/v0.12-events.md` reference on the fourth line after the
  heading, so the grep failed.
- **Fix:** Restructured the banner's first sentence so `docs/v0.12-events.md`
  appears in the first line of blockquote text, within the 2-line window.
  Section body and historical framing unchanged, per plan instruction not
  to rewrite the section.
- **Files modified:** `docs/v0.5-research.md`
- **Commit:** `9caa4c5`

**2. [Rule 3 - Blocking] `.release-notes-v0.12.0.md` matches gitignore pattern**
- **Found during:** Task 4 staging
- **Issue:** `/.release-notes-*.md` is gitignored (scratch notes for `gh
  release create --notes-file`), so a plain `git add` silently no-ops on
  this file.
- **Fix:** Used `git add -f .release-notes-v0.12.0.md` as the plan
  explicitly requires this file to be a committed artifact.
- **Files modified:** n/a (staging only)
- **Commit:** `9caa4c5`

Both deviations were minor and within Rules 1/3 scope — no architectural
changes, no user decision required.

### Pre-existing clippy findings (documented, not fixed — out of scope)

See `.planning/quick/260902-m0z-rename-hook-events-pi-names/deferred-items.md`
for the full list of 55 pre-existing clippy failures unrelated to this
plan's changes.

## Known Stubs

None — no stub patterns introduced by this plan.

## Threat Flags

None — no new network endpoints, auth paths, file-access patterns, or
schema changes at trust boundaries were introduced. The `deny_unknown_fields`
addition tightens config parsing (a hardening change, already covered by the
plan's own threat model) rather than opening new surface.

## Self-Check: PASSED

Verified created/modified files exist and all five commit hashes are
present in `git log`:

- `7a15138` — FOUND
- `f329217` — FOUND
- `a92793f` — FOUND
- `93a4994` — FOUND
- `9caa4c5` — FOUND

Files verified present: `.release-notes-v0.12.0.md`,
`.planning/quick/260902-m0z-rename-hook-events-pi-names/deferred-items.md`,
`src/agent/hook.rs`, `src/config.rs`, `src/settings.rs`,
`config.toml.example`, `README.md`, `README_zh.md`, `docs/v0.5-research.md`.

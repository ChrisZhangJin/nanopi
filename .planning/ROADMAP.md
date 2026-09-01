---
project: nanopi
created: 2026-08-19
status: active
---

# nanopi Roadmap

nanopi is a tiny Rust port of the Pi coding-agent CLI — a ~4 MB static binary that runs on old / low-resource Linux boxes. Development is well underway (v0.9.3 shipped); GSD tracking is being retrofitted onto an existing codebase and this roadmap captures ongoing work rather than a greenfield plan.

## Milestones

### M1 · UX Polish (in progress)

Improve first-time and everyday console UX so nanopi is usable without hand-editing TOML.

**Quick tasks (see STATE.md for the running log):**
- First-run wizard for config bootstrap.
- (future work — added as it comes up)

### M2 · Extensions (v0.11.0) — complete

Pi-parity extension capabilities. See `docs/pi-vs-nanopi.md` for the
comparison that scoped this. All four phases shipped on `v0.11.0`.

- **P0** ✅ Shell-hook event coverage extended to `before_agent_start`,
  `turn_start`, `turn_end`, `message_end` (`410d12c`). Later joined by
  `session_before_compact` / `session_compact` (`675cd02`).
- **P1** ✅ `post_tool_use` can transform the tool result, not just
  observe it (`a2e3994`) — enables redaction / scrubbing hooks.
- **P2** ✅ WASM plugin system behind `--features wasm`
  (`667bf30`, `d4aff4b`, `82d36d7`). Components declared in
  `[[extensions]]` are compiled, instantiated, and their exported
  tools registered alongside the built-ins.
- **P3** ✅ `steer` / `follow-up` injection (`1767238`), wired to the
  TUI so mid-stream typing steers the running turn (`d45bb42`) and
  queued follow-ups auto-start the next one (`7064b10`).

Also landed in this milestone:

- `tool_exec_mode` (parallel vs sequential tool execution). Pi has a
  per-tool override too, which is deferred.
- **Capability-gated host functions** — `host-fs-read` (`788705c`,
  read-only, cwd-confined, symlink-aware) and `host-http-get`
  (`83bbe68`…`28c2e75`, gated on `allow_network` then a deny-by-default
  host-matching `url_allowlist`; 10 s timeout, 1 MiB cap, redirects not
  followed). Both return in-band `error: ` strings rather than
  trapping. Declared at `wit/nanopi-extension.wit:36,57`, implemented
  at `src/wasm/loader.rs:447,478`.
  *(This was listed as deferred until 2026-09-01; it had in fact
  shipped on 2026-08-28.)*
- A hardening pass over the whole milestone — ~20 `fix(...)` commits
  covering plugin epoch deadlines, trap isolation, allowlist bypass,
  cwd-guard escapes, cancel-safety of parallel tool batches, and
  session-file corruption on cancel.
- VERSION centralization (`VERSION` + `make bump` + a tag-vs-VERSION
  gate in `release.yml`), per `.planning/PLAN-VERSION.md` — that plan
  is **done**, not pending.

**Deferred to a later milestone** (from the parity review):
- **Plugin slash-command registration.** The WIT world exports only
  `list-tools` / `execute-tool`; a plugin cannot register a command.
  Upstream Pi's dispatch model is traced in
  `.planning/reference/pi-slash-commands.md` — note its trust model
  does not transfer, since nanopi plugins are sandboxed and
  capability-gated where Pi's are unsandboxed in-process imports.
  This is the largest remaining plugin capability.
- Per-tool `executionMode` override.
- Provider registration from plugins; session-management hooks beyond
  compaction; richer session metadata (thinking-level changes, labels,
  custom entries).

### M3 · Bugfix Line (v0.10.1) — shipped 2026-09-01

Patch line on top of v0.10.0. Lives on the `v0.10.1` branch; nothing
new is developed there. **Released:** tag `v0.10.1` == `d19d3c2`, also
fast-forwarded into `main`, four platform assets published.

Contents:
- `fix(config)` — the Windows first run died with `cannot read
  api_key_file ~/.nanopi/api_key`. The wizard now writes an absolute
  `api_key_file`, and `paths::expand_home` is the single expansion
  point shared by `main` and `agent::hook` (falls back to
  `dirs::home_dir()` when `$HOME` is unset, accepts `\`, honors
  `NANOPI_HOME`).
- `fix(vendor)` — MiniMax default base_url → `api.minimaxi.com`.
- `fix(provider)` — gateway HTML error pages flattened to one line
  (`retry::flatten_error_body`) instead of being shredded by the TUI
  redraw.

All three were cherry-picked onto `v0.11.0` on 2026-09-01
(`e9425b8`, `777eb8a`, `642f696`) — different SHAs, same patches, so
expect patch-id dedup when v0.11.0 eventually merges to `main`.

## Notes

This project uses `/gsd:quick` for the majority of ongoing work. New planned phases (if any) will be added as milestones above.

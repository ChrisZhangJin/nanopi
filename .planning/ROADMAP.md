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

Also landed in this milestone: `tool_exec_mode` (parallel vs
sequential tool execution) — Pi has a per-tool override too, which is
deferred.

**Deferred to a later milestone** (from the parity review):
- Capability-gated host functions for plugins (`host-http-get`,
  `host-fs-read`). The config fields exist and are plumbed; the
  functions themselves are not implemented, so plugins currently have
  no I/O beyond `host-log`.
- Per-tool `executionMode` override.
- Provider registration from plugins; session-management hooks beyond
  compaction; richer session metadata (thinking-level changes, labels,
  custom entries).

### M3 · Bugfix Line (v0.10.1) — patch on top of v0.10.0

Backports from `main` (and from `v0.11.0` once stable). Lives on the
`v0.10.1` branch. Anything landed in `main` is cherry-picked here as
needed; nothing new is developed here.

## Notes

This project uses `/gsd:quick` for the majority of ongoing work. New planned phases (if any) will be added as milestones above.

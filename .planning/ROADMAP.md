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

### M2 · Extensions (v0.11.0) — planned

Add Pi-parity extension capabilities to nanopi in three phases. See
`docs/pi-vs-nanopi.md` for the full comparison; this milestone turns
phase-1 and phase-2 there into shippable code on the `v0.11.0` branch.

- **P0** Expand shell-hook event coverage to: `before_agent_start`,
  `turn_start`, `turn_end`, `message_end`. Mirrors Pi's first-class
  event hooks; pure code change in `src/agent/`, no new dependency.
- **P1** Make `post_tool_use` hook support result transformation
  (today it is read-only) — enables log-scrubbing / secret-redaction
  plugins.
- **P2** WASM plugin system (wasmtime ~2 MB): dynamic tool and
  slash-command registration from `.wasm` files declared in
  `config.toml` `[[extensions]]`. Capability-based networking (Host
  Functions, not raw sockets).
- **P3** `steer` / `follow-up` message injection into a running turn.

### M3 · Bugfix Line (v0.10.1) — patch on top of v0.10.0

Backports from `main` (and from `v0.11.0` once stable). Lives on the
`v0.10.1` branch. Anything landed in `main` is cherry-picked here as
needed; nothing new is developed here.

## Notes

This project uses `/gsd:quick` for the majority of ongoing work. New planned phases (if any) will be added as milestones above.

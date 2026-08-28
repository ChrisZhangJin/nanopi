# Quick Task 260828-l4d: Finish gated host-http-get capability for WASM plugins - Context

**Gathered:** 2026-08-28
**Status:** Ready for planning

<domain>
## Task Boundary

Finish the gated `host-http-get` capability for WASM plugins, resuming the work
paused at commit `e47b380`. The full implementation brief lives in
`.continue-here.md` at the repo root — it is the authoritative spec for this
task and mirrors the already-shipped `host-fs-read` capability (`788705c`).

Out of scope: plugin slash-command registration (explicitly deferred in the
handoff), and the adjacent non-canonicalized path guard in
`src/tool/write.rs` / `src/tool/edit.rs` (handoff says separate commit).

</domain>

<decisions>
## Implementation Decisions

### Sync/async bridge for the host function — LOCKED

`wasmtime`'s `func_wrap` is synchronous; `reqwest` is async and its `blocking`
feature is not enabled in `Cargo.toml`. The handoff laid out three options.

**Decision: option 3 — dedicated thread + channel.**

The host function hands the request to a worker thread that owns its own
single-thread tokio runtime, and blocks on a `std::sync::mpsc` reply.

Consequences that are part of the decision:
- `ComponentBridge` stays sync behind its existing `Mutex` — do not touch it.
- The `Store` / `Config` stay non-async — do NOT set `Config::async_support(true)`.
- Every existing `TypedFunc::call` into the guest stays as-is — no `call_async`.
- No new `reqwest` feature flags; do not enable `reqwest/blocking`.
- Accepted cost: one extra thread per plugin-with-network.

Rejected: `func_wrap_async` + async `Store` (option 2) — the "correct" long-term
shape, but it drags the whole bridge async in a session that should stay narrow.
Rejected: `reqwest` `blocking` + `block_in_place` (option 1) — adds compile time
and binary size, and needs feature scoping to avoid hitting default builds.

### Everything else — follow `.continue-here.md` exactly

The handoff is prescriptive and proven; treat its "What to implement" section as
the spec. In particular these are non-negotiable:
- Gate order: deny on `!allow_network` first (message names the config knob),
  then allowlist, then fetch.
- `url_allowed(url, allowlist)` must match on **host**, not substring —
  `evil.com/?x=api.github.com` must not pass.
- **Empty allowlist denies everything** (matches what `config.toml.example`
  already promises).
- 10s request timeout so a plugin cannot hang a turn.
- In-band error convention: return `error: <reason>`, do not trap.
- Tests must be hermetic — spin up a local server in-test, never hit the network.
- Keep both test suites green: 450 default / 469 with `--features wasm`, 0 warnings.

### Claude's Discretion

Exact worker-thread lifecycle (per-call spawn vs. lazily-started shared worker),
error message wording, and how the local test server is stood up.

</decisions>

<specifics>
## Specific Ideas

Shape sketched when the decision was made:

```rust
linker.root().func_wrap("host-http-get", |caller, (url,)| {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap();
        tx.send(rt.block_on(fetch(url)));
    });
    Ok((rx.recv()?,))
})
```

Illustrative only — it omits the gating, the timeout, and the in-band error
convention, all of which are required.

</specifics>

<canonical_refs>
## Canonical References

- `.continue-here.md` (repo root, committed in `e47b380`) — the implementation
  brief. Read this first and in full.
- `788705c` — the `host-fs-read` commit. This is the template to mirror.
- Environment notes in the handoff matter on this box: `wasm32-wasip1` std was
  hand-installed and does NOT appear in `rustup target list --installed`;
  `wasm-tools` is at `~/.cargo/bin/wasm-tools`; rebuild the example with
  `--manifest-path` from the repo root, never by `cd examples/wasm-plugin`.

</canonical_refs>

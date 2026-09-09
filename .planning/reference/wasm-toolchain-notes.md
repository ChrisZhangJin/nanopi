# WASM toolchain — environment notes

Box-specific gotchas for building the example plugin and regenerating the test
fixture. These are *environment* facts, not project documentation — the
canonical build commands live in `examples/wasm-plugin/README.md` and the header
of `tests/wasm_plugin_integration.rs`, and those are correct as written.

Captured from the `host-http-get` handoff (`e47b380`) before that file was
retired, and re-confirmed during quick task `260828-l4d` on 2026-08-28.

## `rustup target add wasm32-wasip1` does not work here

The README says to run it. On this container it **404s** — the aliyun rustup
mirror does not carry the `wasm32-wasip1` std component.

It was installed **by hand** instead. Consequences:

- It does **not** appear in `rustup target list --installed`, because there is
  no manifest entry for it.
- It **is** present in `$(rustc --print sysroot)/lib/rustlib/wasm32-wasip1`.
- `cargo build --target wasm32-wasip1` works regardless.

**Its absence from the rustup list is expected — do not "fix" it.** Re-running
`rustup target add` burns time against a mirror that will not serve it. Check
the sysroot instead:

```bash
ls "$(rustc --print sysroot)/lib/rustlib/" | grep wasm
```

## `wasm-tools`

Installed at `~/.cargo/bin/wasm-tools` (1.258.0 as of 2026-08-28). The
`cargo install wasm-tools --locked` in the README took several retries
originally — crates.io downloads were timing out through the GFW. If it needs
reinstalling, expect to retry rather than assume it is broken.

## Build the example from the repo root, never from inside it

Use `--manifest-path` from the repo root:

```bash
cargo build --manifest-path examples/wasm-plugin/Cargo.toml \
  --target wasm32-wasip1 --release
```

**Do not `cd examples/wasm-plugin` first.** The subsequent
`wasm-tools component embed wit/ ...` step resolves `wit/` relative to the repo
root, and mixing the two silently embeds a *stale* module — you get a component
that builds and loads but is missing the import you just added, with no error
pointing at the cause.

After regenerating, confirm the new import actually landed:

```bash
wasm-tools component wit tests/fixtures/example-plugin.component.wasm | grep host-
```

If the import is missing the host will fail to satisfy the world at load time.

## Tests must stay hermetic

The gate tests spin up loopback `TcpListener` servers rather than reaching the
network. Keep it that way — this box is behind the GFW, so a test that
accidentally hits a real host does not fail fast, it **hangs** until the 10s
client timeout (or longer, at connect). Verified green with all proxy env vars
unset:

```bash
env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
    -u all_proxy -u ALL_PROXY cargo test --features wasm
```

## Test suites are flaky in parallel

Unrelated to WASM, but it will bite you while verifying: several lib tests
mutate process-global env and race on the default thread pool, failing roughly
1 run in 3. Use `-- --test-threads=1` for a deterministic count. See
Blockers/Concerns in `.planning/STATE.md`.

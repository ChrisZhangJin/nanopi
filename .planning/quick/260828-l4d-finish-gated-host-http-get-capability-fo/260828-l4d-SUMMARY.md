---
phase: quick
plan: 260828-l4d
subsystem: wasm-extensions
tags: [wasm, plugins, capability-gate, network, security]
requires:
  - "host-fs-read capability (788705c) as the structural template"
  - "wasmtime component model host linking (src/wasm/loader.rs)"
provides:
  - "host-http-get WIT import, gated on allow_network + url_allowlist"
  - "url_allowed host-matching helper (public contract: empty = deny all)"
  - "fetch_url sync->async bridge (worker thread + mpsc), no async Store"
  - "fetch tool in the example plugin + regenerated fixture"
affects:
  - "src/wasm/loader.rs (PluginEngine::load signature grew one bool)"
  - "src/wasm/mod.rs (load_all forwards cfg.allow_network)"
  - "tests/fixtures/example-plugin.component.wasm (regenerated)"
tech-stack:
  added: []
  patterns:
    - "sync host fn -> async client via dedicated thread owning a private current-thread tokio runtime, caller blocks on std::sync::mpsc"
    - "capability gates return in-band `error: `-prefixed strings, never traps"
    - "hermetic network tests via std::net::TcpListener on an ephemeral loopback port"
key-files:
  created: []
  modified:
    - wit/nanopi-extension.wit
    - src/wasm/loader.rs
    - src/wasm/mod.rs
    - src/config.rs
    - examples/wasm-plugin/src/lib.rs
    - examples/wasm-plugin/README.md
    - tests/fixtures/example-plugin.component.wasm
    - tests/wasm_plugin_integration.rs
    - README.md
    - README_zh.md
    - config.toml.example
decisions:
  - "Sync/async bridge is a per-call worker thread + mpsc (LOCKED by CONTEXT.md option 3); Store/Config/ComponentBridge stay synchronous"
  - "url_allowed matches on the parsed host with `.`-boundary suffix semantics, not substring"
  - "Empty url_allowlist denies everything — the documented contract, not an oversight"
  - "redirect::Policy::none() — a followed 3xx defeats the allowlist"
  - "Bodies over 1 MiB refused, sized to the example guest's ARENA_SIZE"
metrics:
  duration: ~50 min
  completed: 2026-08-28
  tasks: 3
  commits: 3
  tests-added: 15
---

# Quick Task 260828-l4d: Gated `host-http-get` for WASM Plugins Summary

Plugins can now reach the network through `host-http-get`, behind two
independent gates (`allow_network`, then `url_allowlist`), with host-based
allowlist matching that resists the query-string, userinfo, suffix, and
redirect bypasses — and without making the wasmtime `Store` async or adding a
single dependency.

## What shipped

`host-http-get(url: string) -> string` is the third host import and the second
gated capability, completing the two in scope for M2. It mirrors `host-fs-read`
(`788705c`) in shape, error convention, and test structure.

| Task | Commit | What |
|---|---|---|
| 1 | `83bbe68` | WIT import, `PluginState.allow_network`, `url_allowed`, `fetch_url`, `func_wrap` registration, `load` signature + call sites |
| 2 | `2a71cb2` | `fetch` tool in the example plugin, regenerated fixture |
| 3 | `e5757a7` | 4 hermetic gate tests, 5 docs files corrected |

## The `url_allowed` matching rule, as implemented

Recorded here so the next reader does not have to re-derive the subdomain
semantics from the code:

1. **Empty allowlist → `false`, always.** Deny-by-default is the documented
   contract (`config.toml.example`). `allow_network = true` on its own reaches
   nothing.
2. **Scheme must be `http://` or `https://`** (case-insensitive). Anything
   else — including a URL with no scheme — is refused. `file://` would
   otherwise turn the network capability into a filesystem read, sidestepping
   the separate `allow_fs` gate.
3. **Authority** = everything after the scheme up to the first `/`, `?`, or `#`.
4. **Host** = the authority after stripping userinfo (everything up to and
   including the **last** `@`) and the `:port` suffix. An authority starting
   with `[` is an IPv6 literal, so the host is the text up to the matching `]`
   and the port strip looks after the bracket rather than from the left.
5. **Comparison** is ASCII-lowercase on both sides. An allowlist entry matches
   when `host == entry` **or** `host.ends_with(".{entry}")`.

That leading dot in rule 5 is the whole trick: `github.com` covers
`api.github.com` but not `evilgithub.com` (no dot boundary) and not
`github.com.evil.com` (entry is a prefix, not a suffix). Ports are absent from
the host, so a bare `127.0.0.1` entry matches any port — which is what lets the
hermetic tests allowlist loopback without knowing the ephemeral port up front.

Config entries are normalized the same way a host is, so a user who writes
`https://api.github.com/` where a hostname was wanted still gets what they
meant.

**Why not `url.contains(entry)`:** the URL is plugin-supplied and therefore
fully attacker-controlled. Three URLs pass a `contains` test against
`["api.github.com"]` while pointing elsewhere entirely — `?x=api.github.com`
(query), `api.github.com@evil.com` (userinfo), `api.github.com.evil.com`
(attacker subdomain). Same class of bug as comparing raw paths for containment,
which is why `resolve_readable` canonicalizes.

## Gate order (load-bearing)

1. `!allow_network` → `error: network access denied (set allow_network = true ...)`
2. `!url_allowed(...)` → `error: url_allowlist does not permit <url> ...`
3. only then `fetch_url`

Deliberately not reordered for tidiness. Checking the allowlist first would
tell a plugin author their allowlist is wrong when the real problem is that the
capability is off entirely. The denial test allowlists `127.0.0.1` precisely so
it cannot pass for the wrong reason — with an empty allowlist it would still be
green with gate 1 deleted.

## The sync/async bridge

`func_wrap` host functions are synchronous and this `reqwest` has no `blocking`
feature. `fetch_url` hands the request to a thread owning a private
`new_current_thread` runtime and blocks on an `mpsc` reply. Consequences, all
verified:

- `Config::async_support` is never set; no `func_wrap_async`; no `call_async`.
  `grep -rn 'async_support\|func_wrap_async\|call_async' src/` returns nothing.
- `ComponentBridge` untouched — still sync behind its existing `Mutex`.
- **`Cargo.toml` did not change.** `git diff --quiet Cargo.toml` passes; no new
  crate, no reqwest feature flag, no dev-dependency (the test server is
  `std::net`).

Failure paths never `unwrap`: a runtime that fails to build and a panicked
worker (`RecvError`) both come back as `Err`, so a bad fetch cannot take down
the turn. One thread per call is the accepted cost of the locked design.

## Threat mitigations applied

| Threat ID | Mitigation as shipped |
|---|---|
| T-l4d-01 | `!allow_network` checked first, before any client construction. Test asserts refusal with a *permitting* allowlist. |
| T-l4d-02 | Host matching with userinfo stripping and `.`-boundary suffixes. 3 bypass unit tests. |
| T-l4d-03 | `redirect::Policy::none()`; a 3xx surfaces as `error: HTTP 30x`. Pinned by `http_get_does_not_follow_redirect_off_the_allowlist` (added post-hoc — see Orchestrator Addendum). |
| T-l4d-04 | `.timeout(Duration::from_secs(10))`. |
| T-l4d-05 | Bodies over `1 << 20` refused, sized to the guest's `ARENA_SIZE`. |
| T-l4d-06 | Runtime-build failure and `RecvError` both return `Err`. |
| T-l4d-08 | Tests use a loopback `TcpListener`; verified green with all proxy env vars unset. |

T-l4d-07 (SSRF to loopback / metadata endpoints) remains **accepted**, per the
plan: the allowlist is the control and it is deny-by-default, so reaching
`169.254.169.254` requires the user to have written it into `url_allowlist`.

## Test counts

Baseline was **450 default / 469 with `--features wasm`**. Where those numbers
come from: the "450" is `unittests src/lib.rs`; the "469" is that target's 460
under `--features wasm` plus the 9 `wasm_plugin_integration` tests. (Both runs
also include 6 `skills_integration` tests, which the handoff's figures exclude.)

| Suite | Baseline | Now |
|---|---|---|
| default (`lib.rs` unittests) | 450 | **450** — unchanged |
| `--features wasm` (lib 460→471, integration 9→13) | 469 | **484** |

**+15 tests:** 11 `url_allowed` unit tests (empty allowlist, exact match, the
three bypasses, subdomain, sibling prefix, port, case, two non-http schemes,
no-scheme) and 4 integration gate tests.

The default count staying at exactly 450 is the signal that nothing leaked out
of the `wasm` feature gate.

**0 warnings** on `cargo build --all-targets` and
`cargo build --all-targets --features wasm`. The `#[allow(dead_code)]` removed
from `url_allowlist` did not resurface as a warning, which is the confirmation
the field is genuinely read now.

## Verification performed

- `wasm-tools component wit tests/fixtures/example-plugin.component.wasm` lists
  `import host-http-get: func(url: string) -> string` — the check that the
  embed step picked up the current module rather than a stale one.
- `list-tools` reports four tools; the assertion was updated to
  `["fetch", "readfile", "rot13", "wordcount"]` *before* rebuilding the
  fixture, as the plan directed.
- Gate tests re-run with `env -u http_proxy -u https_proxy -u HTTP_PROXY -u
  HTTPS_PROXY -u all_proxy -u ALL_PROXY` — still green, so nothing depends on
  network reachability.
- `grep -rn allow_network` across the five docs files: every hit describes
  working behavior; no "reserved", "inert", "not implemented yet", or "尚未实现".
- Out of scope and confirmed untouched: `src/tool/write.rs`, `src/tool/edit.rs`
  (`git diff 324a7d8 --name-only` on both is empty), and plugin slash-command
  registration.

## Deviations / Blockers

### 1. `bc` is not installed on this box — verify command substituted

The plan's Task 3 second verify step is
`test "$(cargo test 2>&1 | grep -oP '\d+(?= passed)' | paste -sd+ | bc)" = 450`.
`bc` does not exist here (`/bin/bash: bc: command not found`, exit 127). I used
`awk '{s+=$1} END {print s}'` for the arithmetic instead.

Note the plan's formula would not have yielded 450 regardless: it sums *every*
`N passed` line across all five test targets, so the baseline value is 456
(450 lib + 6 `skills_integration`), not 450. I verified the intended invariant
directly against the per-target breakdown instead — `unittests src/lib.rs` is
exactly 450 under the default feature set, as shown in the table above.

### 2. TDD gates squashed into one commit per task

Task 1 was `tdd="true"` and the loop was followed in-session: the 11
`url_allowed` tests were written first and observed failing (13 compile errors,
`cannot find function url_allowed in this scope`) before any implementation
existed. They were not committed separately, because the orchestrator's
instruction was one atomic commit per task and a standalone RED commit would
put a non-compiling tree in `v0.11.0`'s history. So there is a `feat(...)`
commit for Task 1 but no preceding `test(...)` commit for the same code.

### 3. Pre-existing test flakiness under parallel execution (out of scope)

Discovered while establishing the baseline, **before any code was written**:
`session::tests::roundtrip_all_entry_types` failed on the very first
`cargo test` at the base commit `324a7d8`. Repeated runs fail a different
subset each time — `agent::prompt_override::tests::*` (10, as a cluster),
`paths::tests::nanopi_home_honors_env`,
`provider::openai::tests::resumed_session_outgoing_request_has_tools`,
`agent::hook::tests::run_hook_exit_2_means_block`. Roughly 1 run in 3.

These tests mutate process-global environment (`HOME`, `NANOPI_HOME`) and race
each other on the default thread pool. It reproduces in the **default** suite,
which contains none of this task's code — that is what identifies it as
pre-existing rather than a regression. None of the affected tests are in files
this task touched.

Both suites are deterministically green with `-- --test-threads=1`, which is
how the final counts above were taken.

Logged to `deferred-items.md` rather than fixed, per the scope boundary
(pre-existing failures in unrelated files). The fix — an env mutex or
injecting paths instead of reading globals — is its own commit.

## Known Stubs

None. No placeholder values, no `TODO`/`FIXME`, no component wired to an empty
data source. `fetch` is end-to-end: guest tool → import → host gates → real
socket → body back into guest memory, exercised by
`http_get_allowed_host_reaches_server`.

## Threat Flags

None. The only new surface is `host-http-get` itself, which is what the plan's
threat model covers. `PluginEngine::load` grew one `bool` parameter; no new
endpoint, auth path, or schema change.

## Self-Check: PASSED

All 11 modified files exist on disk. All 3 commit hashes (`83bbe68`, `2a71cb2`,
`e5757a7`) resolve in `git log --all`. `src/wasm/loader.rs` is 688 lines
(artifact spec required ≥480) and contains `fn url_allowed`;
`examples/wasm-plugin/src/lib.rs` contains `host_http_get_raw`;
`tests/wasm_plugin_integration.rs` contains `TcpListener`; `src/wasm/mod.rs`
contains `cfg.allow_network`. No file deletions in any of the three commits.

## Orchestrator Addendum — `28c2e75`

Added by the orchestrator during verification, after the three task commits.

**Gap found:** `redirect::Policy::none()` shipped in `83bbe68` and the threat
register marked T-l4d-03 mitigated, but **nothing tested it**. T-l4d-02 cites
three bypass unit tests; T-l4d-03 cited only the implementation. The summary
above also claimed the allowlist "resists the query-string, userinfo, suffix,
and redirect bypasses" — true of the first three by test, true of the fourth
only by inspection. An untested guard is one refactor from silent regression,
and this one is load-bearing: the allowlist is checked once against the URL the
guest supplied, so *not following the hop* IS the control.

**Fix:** `http_get_does_not_follow_redirect_off_the_allowlist` in
`tests/wasm_plugin_integration.rs`, plus a `spawn_redirect_server` helper
mirroring `spawn_test_server`.

The test's one non-obvious choice: the redirect target is `localhost` while the
allowlist holds `127.0.0.1`. Same loopback interface, different host string —
so the target is genuinely outside the allowlist while staying entirely
in-process. That asymmetry is what makes a regression fail *loudly and fast*
(the client follows the hop, reaches the second server, and the distinctive body
reaches the guest) instead of hanging on an unroutable address until the 10s
timeout.

**Mutation-checked:** swapping `Policy::none()` for `Policy::limited(10)` fails
the test in 0.72s on the `an unfollowed 3xx must surface as an error`
assertion. `loader.rs` was restored and confirmed byte-identical to `HEAD`
afterwards. A guard test never observed failing is not evidence of anything.

**Revised counts** (deterministic, `-- --test-threads=1`):

| Suite | Baseline | Now |
|---|---|---|
| default (`lib.rs` unittests) | 450 | **450** — unchanged, nothing leaked the feature gate |
| `--features wasm` (lib 471 + integration 14) | 469 | **485** |

**+16 tests** total for this task (15 from the executor, 1 here). 0 warnings on
`cargo build --all-targets` both with and without `--features wasm`.

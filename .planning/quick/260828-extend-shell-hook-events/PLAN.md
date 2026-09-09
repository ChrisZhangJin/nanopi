---
gsd_quick: true
task: "Extend shell-hook event coverage to before_agent_start, turn_start, turn_end, message_end"
branch: v0.11.0
created: 2026-08-28
status: in-progress
---

## Goal

Align nanopi's shell-hook event surface with Pi's lifecycle hooks
(`before_agent_start`, `turn_start`, `turn_end`, `message_end`) so
third-party audit / policy scripts can observe the full agent loop
without touching the binary.

No new dependencies. Pure additive: 4 new `HookEvent` variants,
4 new `HooksConfig` fields, 4 new `run_hooks()` / `run_session_hooks()`
call sites in `loop_.rs`, corresponding `settings.toml` / config loader
support, and tests.

## Scope

### What changes

| File | Change |
|------|--------|
| `src/agent/hook.rs` | Add 4 new `HookEvent` variants + `env_var` arms + serde round-trip tests |
| `src/agent/loop_.rs` | Add 4 `run_hooks()` call sites; `HooksConfig` struct gains 4 new `Vec<HookConfig>` fields |
| `src/config.rs` | `settings.toml` parsing already deserializes all `HookConfig` vecs via serde `rename_all`; just need to add the 4 new fields to `HooksConfig` |

### What does NOT change

- Provider code (`src/provider/`)
- Tool code (`src/tool/`)
- CLI args (`src/main.rs`) — hooks are config-driven, not CLI-driven
- Binary size / dependency set

## Implementation

### Step 1 — hook.rs: new HookEvent variants

Add to `HookEvent` enum:

```
BeforeAgentStart   // fired once at the top of run_turn, BEFORE user msg is pushed to context
TurnStart          // fired at the top of each for-loop iteration in run_turn
TurnEnd            // fired at the bottom of each for-loop iteration in run_turn
MessageEnd         // fired once after the for-loop completes (after all tool rounds)
```

Wire `env_var()`:
```
BeforeAgentStart → "BeforeAgentStart"
TurnStart        → "TurnStart"
TurnEnd          → "TurnEnd"
MessageEnd       → "MessageEnd"
```

Add 4 tests: one round-trip serialize/deserialize + one env_var check per variant.

### Step 2 — HooksConfig: 4 new fields

```rust
pub struct HooksConfig {
    pub pre_tool_use: Vec<HookConfig>,
    pub post_tool_use: Vec<HookConfig>,
    pub user_prompt_submit: Vec<HookConfig>,
    pub session_start: Vec<HookConfig>,
    pub session_end: Vec<HookConfig>,
    // NEW:
    pub before_agent_start: Vec<HookConfig>,
    pub turn_start: Vec<HookConfig>,
    pub turn_end: Vec<HookConfig>,
    pub message_end: Vec<HookConfig>,
}
```

`Default` impl: all `vec![]` (backward-compatible — existing configs with
no new keys produce empty vecs via serde's `default`).

### Step 3 — loop_.rs: 4 new run_hooks() call sites

**BeforeAgentStart** — after `maybe_compact()` / before `effective_msg` logic:
- matcher applied to turn_count (as string)
- payload: `{ "event": "before_agent_start", "turn_count": N, "prompt": user_msg }`
- Allow = continue; Block = return early (like UserPromptSubmit block)
- Transform = rewrite `effective_msg`

**TurnStart** — at the top of `for _ in 0..MAX_ITERATIONS`:
- matcher applied to turn_count
- payload: `{ "event": "turn_start", "turn_count": N, "iteration": i }`
- Advisory only (Block logged, does not abort the iteration)

**TurnEnd** — at the bottom of each iteration, after tool calls processed:
- matcher applied to turn_count
- payload: `{ "event": "turn_end", "turn_count": N, "iteration": i, "had_tool_calls": bool }`
- Advisory only

**MessageEnd** — after the for-loop, just before the post-turn compaction:
- matcher applied to turn_count
- payload: `{ "event": "message_end", "turn_count": N, "response_length": final_text.len() }`
- Advisory only

All call sites gated on `self.permission.hooks_active()`.

### Step 4 — config loader integration

`src/config.rs`'s `HooksConfig` already deserializes from TOML with
serde `rename_all = "snake_case"`. Adding the 4 new fields to the struct
is sufficient — any `[[hooks.before_agent_start]]` entries in
`settings.toml` / `.nanopi/settings.toml` will be picked up automatically.

No parser changes needed.

### Step 5 — tests

1. **hook.rs**: 4 round-trip + 4 env_var tests (8 new tests)
2. **hook.rs**: 1 integration test per new event: marker file side-channel
3. **loop_.rs**: 1 test for BeforeAgentStart Block (verify early return)
4. **loop_.rs**: 1 test for TurnStart/TurnEnd advisory fire (marker file)

## Acceptance

- `cargo test --all-targets -- --test-threads=1` passes
- `cargo clippy --all-targets -- -D warnings` clean
- No new dependencies in Cargo.toml
- New events appear in `settings.toml` parser output
- README.md docs updated with new event names in Hooks section

## Out of scope

- `post_tool_use` transform support (P1 — separate quick task)
- WASM plugin system (P2 — lives on v0.11.0 branch, separate phase)

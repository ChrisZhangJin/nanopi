# nanopi v0.5 — Implementation Plan

> File-level task breakdown for v0.5. Read top-down; tasks are ordered to keep the codebase compileable at every step.
> 
> Source of truth for design decisions: `docs/v0.5-research.md` (the research notes). This plan adds no new design — only sequence and acceptance.
>
> Each task ends with **ACCEPTANCE** criteria that must pass before moving to the next.

---

## Build Order (high level)

```
0.  Cargo.toml + module skeleton       → empty modules, no logic
1.  util/uuid                         → UUID v7
2.  util/iso8601                      → timestamps
3.  config + settings                 → load config.toml + settings.toml
4.  session                           → JSONL v2 read/write
5.  provider/sse                      → SSE parser (refactored from v0.1)
6.  provider/openai                   → OpenAI-compatible provider
7.  event + context                   → AgentEvent + Context types
8.  tool/{read,write,edit,bash}       → 4 built-in tools
9.  tool/mod (registry)               → ToolRegistry
10. hook                              → Claude Code-style hook runtime
11. permission                        → PermissionGate (yolo, hooks, trust)
12. agent/loop                        → the actual turn loop
13. render/{stdout,tui}               → output rendering
14. mode/{interactive,print}           → TUI mode + -p mode
15. trust + resources                 → trust model + skills/prompts loader
16. main.rs                           → clap wiring + dispatch
17. smoke tests + binary size check   → verify v0.5 acceptance criteria
```

After 17, the v0.5 release is done.

---

## Pre-flight (do before any code)

### Task 0a: Set up musl target & mirror

**Where**: host shell, not the repo.

```bash
# Already done in v0.1 — verify
rustup target list --installed | grep musl
test -f ~/.cargo/config.toml
```

**ACCEPTANCE**: both present, no action needed.

### Task 0b: Update Cargo.toml for v0.5 deps

**Where**: `Cargo.toml` at repo root.

Add the new deps on top of v0.1's:

```toml
[dependencies]
# (v0.1 deps unchanged)
clap           = { ... }
reqwest        = { ... }
futures-util   = { ... }
tokio          = { ... }
serde          = { ... }
serde_json     = "1.0"
anyhow         = "1.0"
dirs           = "5.0"

# v0.5 NEW
crossterm      = { version = "0.28", default-features = false, features = ["events", "terminal", "cursor", "style", "screen"] }
uuid           = { version = "1.10", features = ["v7"] }
serde_yaml     = "0.9"
toml           = "0.8"
async-trait    = "0.1"
regex          = "1.10"
chrono         = { version = "0.4", default-features = false, features = ["clock", "serde"] }   # roll our own? tbd in task 0c
tracing        = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

**ACCEPTANCE**: `cargo check --target x86_64-unknown-linux-musl` succeeds (with empty main.rs).

### Task 0c: Resolve chrono vs hand-rolled

**Decision**: use `chrono` with `clock` feature only. Roll-your-own ISO 8601 was estimated 30 lines but introduces date math bugs (leap years, DST, calendar edge cases). `chrono` adds ~100 KB but is the right call. Document the decision in the task log.

**ACCEPTANCE**: chrono in `Cargo.toml`, no `util/iso8601.rs` module needed.

### Task 0d: Module skeleton

**Where**: `src/` directory.

Create empty `mod` files:

```
src/
├── main.rs                  # 1-line `fn main() { println!("nanopi v0.5") }`
├── lib.rs                   # (new) declare all submodules
├── config.rs                # mod declaration only
├── session.rs               # mod declaration only
├── trust.rs                 # mod declaration only
├── resources.rs             # mod declaration only
├── settings.rs              # mod declaration only
├── agent/mod.rs             # mod declaration only
├── provider/mod.rs          # mod declaration only
├── tool/mod.rs              # mod declaration only
├── render/mod.rs            # mod declaration only
├── mode/mod.rs              # mod declaration only
└── util/mod.rs              # mod declaration only
```

**ACCEPTANCE**: `cargo build --release --target x86_64-unknown-linux-musl` succeeds with empty modules; binary prints "nanopi v0.5".

---

## Phase 1: Foundation utilities

### Task 1: util/uuid.rs

**Where**: `src/util/uuid.rs` + re-export from `util/mod.rs`.

**What**: Thin wrapper around `uuid` crate to produce UUID v7 (time-ordered).

```rust
// src/util/uuid.rs
pub fn v7() -> Uuid { Uuid::now_v7() }
pub fn parse(s: &str) -> Result<Uuid, uuid::Error> { Uuid::parse_str(s) }
```

**ACCEPTANCE**: `cargo test util::uuid` passes; produces a sortable id like `0190abcdef...`.

### Task 2: util/time.rs (replaces planned util/iso8601.rs)

**Where**: `src/util/time.rs` (decision: use chrono internally).

```rust
// src/util/time.rs
use chrono::Utc;

pub fn now_utc() -> chrono::DateTime<Utc> { Utc::now() }
pub fn to_iso8601(t: &chrono::DateTime<Utc>) -> String { t.to_rfc3339_opts(SecondsFormat::Secs, true) }
```

**ACCEPTANCE**: `now_utc().to_rfc3339()` returns "2026-08-05T04:52:02Z".

### Task 3: config.rs

**Where**: `src/config.rs` + `src/config.rs` types.

**What**:
- `Config` struct with fields from §3.4 of research
- `load_config(cwd: &Path) -> Result<Config, ConfigError>` — global + local + flag merge
- `default_config()` for tests

```rust
pub struct Config {
    pub default_model: Option<String>,
    pub default_base_url: Option<String>,
    pub api_key_source: ApiKeySource,
    pub api_key_file: Option<PathBuf>,
    pub tools: ToolsConfig,
    pub trust: TrustConfig,
    pub logging: LoggingConfig,
}

pub fn load_config(cwd: &Path) -> Result<Config, ConfigError>;
```

**Files**:
- `src/config.rs` — implementation
- `src/config.rs` — tests at bottom (one test per merge scenario)

**ACCEPTANCE**:
- `cargo test config` passes
- `load_config` deep-merges global + local correctly
- Missing config file → defaults (no error)
- Invalid TOML → error with file path

---

## Phase 2: I/O subsystems

### Task 4: session.rs

**Where**: `src/session.rs`.

**What**:
- `SessionEntry` enum (§3.2 of research)
- `SessionReader` for read-once iteration
- `SessionWriter` for append
- `active_session(cwd) -> Option<PathBuf>` and `set_active_session(cwd, &Path)` using `~/.nanopi/sessions/active` text file

```rust
pub struct Session {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub cwd: PathBuf,
    pub model: String,
    pub base_url: String,
}

pub fn new_session(cwd: &Path, model: &str, base_url: &str) -> Result<(Session, PathBuf)>;
pub fn open_session(path: &Path) -> Result<SessionReader>;
pub fn write_entry(writer: &mut SessionWriter, entry: &SessionEntry) -> Result<()>;
```

**ACCEPTANCE**:
- `cargo test session` passes
- Writing 100 entries produces 100 lines of valid JSONL
- Reading back roundtrips all 5 entry types
- `active_session` works (writes/reads `~/.nanopi/sessions/active`)

### Task 5: provider/sse.rs (refactor from v0.1)

**Where**: `src/provider/sse.rs`.

**What**: Generalize the v0.1 SSE parser. Take `Stream<Item = Result<Bytes, E>>` and yield `SseEvent { data: String }`. No provider-specific knowledge.

```rust
pub struct SseEvent { pub data: String, pub event_type: Option<String> }

pub fn parse_sse<S, E>(stream: S) -> impl Stream<Item = Result<SseEvent, SseError>>
where S: Stream<Item = Result<Bytes, E>>, E: Display;
```

**ACCEPTANCE**:
- `cargo test provider::sse` passes
- Correctly splits on `\n\n` boundaries
- Tolerates chunks that don't end on a line boundary
- Skips non-`data:` lines (event types, comments)
- Recognizes `data: [DONE]` as a sentinel

---

## Phase 3: Provider layer

### Task 6: event.rs

**Where**: `src/agent/event.rs` (or just `src/event.rs` at top level — pick top-level for now since it's referenced everywhere).

**What**: The unified `AgentEvent` enum from §1.3 of research.

**ACCEPTANCE**:
- All 6 variants defined
- `serde::Serialize + Deserialize` works (for future session persistence)
- Unit tests for each variant

### Task 7: context.rs

**Where**: `src/agent/context.rs`.

**What**: `Context`, `ContextMessage`, `ContentBlock`, `AssistantBlock`, `ToolSpec` types from §1.5 of research.

**ACCEPTANCE**:
- All types defined
- Serde roundtrip works
- `to_openai_messages()` method returns OpenAI-compatible JSON

### Task 8: provider/openai.rs

**Where**: `src/provider/openai.rs`.

**What**: The `OpenAiProvider` implementing `Provider` trait for OpenAI-compatible APIs. v0.1's main.rs logic moved here.

```rust
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn stream(&self, ctx: &Context, opts: &RequestOptions, tx: mpsc::Sender<AgentEvent>) -> Result<Usage, AgentError>;
    fn id(&self) -> &'static str { "openai" }
    fn model(&self) -> &ModelId { &self.model }
}
```

**ACCEPTANCE**:
- Streams `AgentEvent`s correctly
- Handles `[DONE]` sentinel
- Handles `tool_calls` deltas (accumulates into single `ToolCall` event with parsed JSON)
- Handles `finish_reason` mapping
- Unit test with mock HTTP server (use `httpmock` or `wiremock`)

---

## Phase 4: Tools

### Task 9: tool/mod.rs (Tool trait + registry)

**Where**: `src/tool/mod.rs`.

**What**:
- `Tool` trait (§2.3 of research)
- `ToolSpec` struct (id, name, description, parameters JSON Schema)
- `ToolContext` (cwd, session Arc)
- `ToolOutput` struct
- `ToolRegistry` with `register`, `get`, `all` methods

**ACCEPTANCE**:
- Trait compiles
- Registry: register 4 tools, get 1, list all
- Test: dispatch a fake tool, verify it's called

### Task 10: tool/read.rs

**Where**: `src/tool/read.rs`.

**What**: Reads file with offset/limit, handles images.

```rust
pub struct ReadTool;
#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> &ToolSpec { /* name: "read", params: {path, offset?, limit?} */ }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}
```

**ACCEPTANCE**:
- Reads `Cargo.toml` and returns its content
- `offset=10, limit=5` returns 5 lines starting at line 10
- Returns `is_error: true` for missing file
- Image detection: returns image content for .png/.jpg

### Task 11: tool/write.rs

**Where**: `src/tool/write.rs`.

**What**: Write file (overwrite, create). No fancy diff.

**ACCEPTANCE**:
- Creates new file
- Overwrites existing
- Returns `is_error: true` for permission denied
- Refuses to write outside cwd (security: relative paths only, or absolute within cwd)

### Task 12: tool/edit.rs

**Where**: `src/tool/edit.rs`.

**What**: oldText/newText replacement. Computes unified diff for display.

**ACCEPTANCE**:
- Replaces exact match
- Errors if oldText not found
- Errors if oldText matches multiple locations (ambiguous)
- Returns diff metadata in `ToolOutput::metadata`

### Task 13: tool/bash.rs

**Where**: `src/tool/bash.rs`.

**What**: Spawn user's shell via `bash -c "command"`. Truncate output at 30KB / 2000 lines. Timeout 30s.

```rust
pub struct BashTool {
    timeout: Duration,
    max_bytes: usize,
    max_lines: usize,
}
```

**ACCEPTANCE**:
- Runs `echo hello`, returns "hello\n"
- Truncates output of `seq 1 10000` to 2000 lines
- Kills long-running command after timeout
- Inherits user's `$PATH` and env vars
- Returns non-zero exit as `is_error: true` with stderr in content
- 30KB byte cap; overflow path recorded in `metadata`

---

## Phase 5: Hooks

### Task 14: hook.rs

**Where**: `src/agent/hook.rs`.

**What**: Claude Code-style hook runtime (§6 of research).

```rust
pub enum HookEvent { PreToolUse, PostToolUse, UserPromptSubmit /* v0.6+ */ }
pub struct HookConfig { pub matcher: String, pub command: String, pub timeout_ms: u64 }
pub struct HookInput { /* JSON-serializable, see §6.3 */ }
pub enum HookOutput { Allow, Block { reason: String }, Transform { new_args: Value } }

pub async fn run_hook(hook: &HookConfig, input: &HookInput) -> Result<HookOutput, HookError>;
pub fn expand_command_path(cmd: &str) -> PathBuf;  // ~, ${HOME}, $HOME expansion
```

**ACCEPTANCE**:
- `cargo test agent::hook` passes
- `run_hook` spawns bash, writes JSON to stdin with trailing `\n`, reads stdout + exit code
- Exit 2 + stderr → `Block { reason: stderr }`
- JSON `{"decision":"block"}` on stdout → `Block { reason: ... }`
- Timeout → child killed, returns `Block { reason: "timeout" }`
- `expand_command_path` resolves `~/.nanopi/hooks/x.sh`, `${HOME}/x.sh`, absolute paths

---

## Phase 6: Permissions and Agent Loop

### Task 15: permission.rs

**Where**: `src/agent/permission.rs`.

**What**: `PermissionGate` (§4.4 of research).

```rust
pub struct PermissionGate {
    pub yolo: bool,
    pub hooks_enabled: bool,
    pub project_trust: TrustLevel,
}
pub enum TrustLevel { Trusted, Distrusted, Ask }

impl PermissionGate {
    pub fn new(args: &Args, cwd: &Path) -> Self;
    pub fn should_run_hook(&self, kind: HookKind, decision: HookDecision) -> bool;
}
```

**ACCEPTANCE**:
- `--yolo` → yolo = true
- `--no-hooks` → hooks_enabled = false
- `--approve` → trust = Trusted
- `--no-approve` → trust = Distrusted
- Default → trust = Ask, will prompt (separate concern)

### Task 16: agent/loop.rs (the heart)

**Where**: `src/agent/loop.rs`.

**What**: Implements the actual turn loop (§2.1 of research).

```rust
pub struct Agent {
    provider: Box<dyn Provider>,
    registry: ToolRegistry,
    session: Arc<Session>,
    permission: PermissionGate,
    renderer: Box<dyn Renderer>,
}

impl Agent {
    pub async fn run_turn(&mut self, user_msg: UserMessage) -> Result<AssistantMessage, AgentError>;
}
```

**Steps in `run_turn`**:
1. Append user message to context + session
2. Loop:
   a. `provider.stream(...)` → spawn task, listen on `rx`
   b. Persist events to session, render to TUI
   c. Collect buffered `ToolCall`s
   d. On `Done`:
      - if `ToolCalls`:
        - for each call (sequential in v0.5):
          - run `PreToolUse` hooks → if any Block, append `ToolResult { is_error: true, content: reason }`
          - else: `tool.execute(...)` → append `ToolResult`
          - run `PostToolUse` hooks (transform result if requested)
        - loop back to (a)
      - else: return final message

**ACCEPTANCE**:
- `cargo test agent::loop` (with mock provider)
- Runs single tool call, gets result, continues to next LLM turn
- Handles `Block` from hook, treats as error result
- Handles empty tool_calls array (don't loop)
- Handles LLM error, doesn't crash, returns error to caller

### Task 17: render/

**Where**: `src/render/stdout.rs` and `src/render/tui.rs`.

**What**:
- `Renderer` trait (§2.3 of research)
- `StdoutRenderer` — bare ANSI escapes (like v0.1), no TUI
- `TuiRenderer` — `crossterm` based, enters alt-screen, manages layout

**TuiRenderer specifics**:
- One row per user/assistant message
- One row per tool call (with output rendered below, possibly collapsed)
- Footer: model + token estimate + status
- `q` or `Ctrl-C` to interrupt

**ACCEPTANCE**:
- `StdoutRenderer` produces identical output to v0.1 for non-tool messages
- `TuiRenderer` enters/exits alt-screen cleanly
- TuiRenderer renders streamed text without flickering (test with a fast synthetic stream)
- Both implement `Renderer` trait

---

## Phase 7: Modes

### Task 18: mode/print.rs (`-p`)

**Where**: `src/mode/print.rs`.

**What**: Non-interactive print mode (§5 of research).

```rust
pub async fn run_print_mode(args: &Args, config: &Config) -> Result<i32, AgentError>;
```

**Behavior**:
- No alt-screen
- Read message from `--message` (or `-` for stdin)
- Run `Agent::run_turn`
- Render to stdout (final assistant message) or JSON envelope
- Return exit code 0/1/2

**ACCEPTANCE**:
- `./nanopi -p "2+2?"` returns "4" to stdout, exit 0
- `./nanopi -p --output json "2+2?"` returns valid JSON envelope
- `./nanopi -p -` reads stdin, runs, exits
- Tool calls during -p mode show progress to stderr, final to stdout
- Invalid model: exit code 1, error to stderr

### Task 19: mode/interactive.rs (TUI)

**Where**: `src/mode/interactive.rs`.

**What**: TUI mode that wraps `Agent` + `TuiRenderer`.

**Behavior**:
- Enter alt-screen
- Mount `TuiRenderer`
- Run event loop: read key events, dispatch
- On `Enter`, append user message, run turn
- On `q` / `Ctrl-C`, gracefully shutdown (persist partial session)

**ACCEPTANCE**:
- Enters alt-screen on start
- Single question gets answered, TUI shows response
- Multi-turn conversation works
- `Ctrl-C` during streaming aborts cleanly
- Session file has full conversation

### Task 20: trust.rs + resources.rs

**Where**: `src/trust.rs` and `src/resources.rs`.

**What**:
- `trust.rs`: read/write `~/.nanopi/trust/<encoded-cwd>=trusted|denied`; prompt logic for TUI
- `resources.rs`: walk `~/.nanopi/skills/`, parse frontmatter, build `Vec<Skill>`; same for `prompts/`

**ACCEPTANCE**:
- First run in dir with `.nanopi/` → TUI shows trust prompt
- Selecting "Trust" writes file
- Skills loaded from global + project, system prompt contains them
- Prompt templates register as `/<name>` slash commands

---

## Phase 8: Main entry

### Task 21: main.rs

**Where**: `src/main.rs` (replaces v0.1's, which we keep as `src/v0.1_main.rs` for reference).

**What**:
- `Args` struct with all v0.5 flags (§7 of research)
- Dispatch:
  - if `--print` → `mode::print::run_print_mode(...)`
  - else → `mode::interactive::run_interactive_mode(...)`

**ACCEPTANCE**:
- All v0.5 flags parsed correctly (`--help` shows them)
- v0.1 smoke test still passes (regression check)

---

## Phase 9: Verification

### Task 22: Smoke tests

**Where**: `tests/smoke.sh` (new) — runs v0.5 acceptance scenarios.

```bash
#!/usr/bin/env bash
set -e

BIN=./target/x86_64-unknown-linux-musl/release/nanopi
KEY=$OPENAI_API_KEY
BASE=$OPENAI_BASE_URL
MODEL=$OPENAI_TEST_MODEL

# 1. Tool: read
$BIN -p --yolo --model $MODEL --base-url $BASE --api-key $KEY \
  "read /etc/hostname and tell me what you see" | tee /tmp/test1.json
jq -e '.finish_reason == "stop"' /tmp/test1.json
jq -e 'any(.messages[]; .role == "tool")' /tmp/test1.json
# verify hostname appears in final message

# 2. Tool: write
$BIN -p --yolo --model $MODEL --base-url $BASE --api-key $KEY \
  "create /tmp/nanopi-test-foo.txt with the text hello world"
test -f /tmp/nanopi-test-foo.txt
grep -q "hello world" /tmp/nanopi-test-foo.txt
rm /tmp/nanopi-test-foo.txt

# 3. -p mode + JSON output
$BIN -p --output json --model $MODEL --base-url $BASE --api-key $KEY \
  "what is 2+2" > /tmp/test3.json
jq -e '.finish_reason == "stop"' /tmp/test3.json
jq -e '.messages[-1].content | contains("4")' /tmp/test3.json

# 4. Skill injection
mkdir -p ~/.nanopi/skills
cat > ~/.nanopi/skills/test-skill.md <<'EOF'
---
name: test-skill
description: Always answer in haiku
---
Respond only in 5-7-5 haiku.
EOF
$BIN -p --model $MODEL --base-url $BASE --api-key $KEY "hi"
# Verify response is haiku-like (rough check)

# 5. PreToolUse hook blocks
cat > /tmp/check-rm-rf.sh <<'EOF'
#!/usr/bin/env bash
input=$(cat)
cmd=$(echo "$input" | jq -r '.arguments.command')
if echo "$cmd" | grep -qE 'rm\s+-rf?\s+/'; then
  echo '{"decision":"block","reason":"rm -rf refused"}'
  exit 0
fi
echo '{"decision":"allow"}'
EOF
chmod +x /tmp/check-rm-rf.sh

mkdir -p /tmp/nanopi-settings
cat > /tmp/nanopi-settings/settings.toml <<EOF
[[hooks.PreToolUse]]
matcher = "bash"
type = "command"
command = "/tmp/check-rm-rf.sh"
timeout = 5000
EOF

# (note: project-local settings.toml needs trust flow; v0.5 will test this with --approve)
# ...

# 6. yolo skips trust
# (Implicit: tests 1-2 use --yolo)

# 7. yolo makes hook block ineffective
# (TBD: detailed test)

# 8. Ctrl-C abort
# (Interactive test, hard to script)

# 9. Binary size
SIZE=$(stat -c%s $BIN)
test $SIZE -lt 5242880  # 5 MB
```

**ACCEPTANCE**: all 11 criteria from research §8.5 pass.

### Task 23: Binary size verification

**Where**: shell command.

```bash
ls -lh target/x86_64-unknown-linux-musl/release/nanopi
file target/x86_64-unknown-linux-musl/release/nanopi
ldd target/x86_64-unknown-linux-musl/release/nanopi
# expected: "statically linked"
# expected size: < 5 MB
```

**ACCEPTANCE**: binary < 5 MB, statically linked.

---

## Risk-driven tasks (insert as needed)

These aren't in the build order but should be added **as they're discovered**:

### Risk T1: `matcher` regex validation

When loading settings.toml, compile each matcher regex once. Cache the compiled regex. If compile fails, return error with line number.

### Risk T2: Workspace trust integration

When loading project-local settings.toml, the trust check must come **before** any hook can be registered. Test: `--no-approve` should prevent project-local hooks from running.

### Risk T3: Hook subprocess leak

Use `tokio::process::Command` (not std). On timeout, `child.kill().await`. Track child PIDs in a struct field, kill all on Agent drop. Test: kill nanopi with Ctrl-C during a hung hook, verify no zombie.

### Risk T4: Session file append atomicity

Use `O_APPEND` semantics. Each `write_entry` does `write_all + flush`. On crash mid-write, the JSONL may have a partial last line, but no other lines are lost.

### Risk T5: stdin handling in `-p` mode

If `--message -` and stdin is a TTY → error "no message provided". If stdin is a pipe, read until EOF. Cap at 1 MB to prevent OOM.

### Risk T6: Provider error retry

If the LLM returns 5xx or rate limit, retry with exponential backoff (capped at 3 attempts, 2-4-8s). On 4xx, no retry, return error to caller.

---

## Post-v0.5 backlog (not in this plan)

These are explicitly **deferred** to v0.6+:

- Anthropic-compatible provider
- `grep`, `find`, `ls` tools
- `rustyline` line editing
- `UserPromptSubmit` hook
- `SessionStart` / `SessionEnd` hooks
- `--continue` / `--fork` / `--resume` flags (only `--message` + session auto-save in v0.5)
- Context compaction
- `ratatui` full TUI

---

## Done criteria (the whole v0.5)

v0.5 is done when:
- [ ] All 23 tasks completed
- [ ] All acceptance criteria pass
- [ ] `cargo build --release --target x86_64-unknown-linux-musl` produces binary < 5 MB, statically linked
- [ ] All 11 v0.5 acceptance criteria from research §8.5 pass via `tests/smoke.sh`
- [ ] README updated to v0.5
- [ ] Git tag `v0.5.0` created

After that, we plan v0.6 (parallel tool execution, Anthropic provider, the rest of the backlog).
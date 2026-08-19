---
phase: quick
plan: 01
type: execute
wave: 1
depends_on: []
files_modified:
  - src/wizard.rs
  - src/lib.rs
  - src/main.rs
autonomous: true
requirements: [wizard-first-run, wizard-init-subcommand]

must_haves:
  truths:
    - "Running `nanopi` with no config, no env vars, and no CLI creds launches the interactive wizard instead of erroring."
    - "Running `nanopi init` launches the wizard and refuses to overwrite an existing ~/.nanopi/config.toml unless the user confirms."
    - "Provider pick-list offers OpenAI, DeepSeek, Anthropic direct, Gemini via gateway, Ollama, Custom — each preset autofills base_url + api_kind + default model."
    - "Wizard writes ~/.nanopi/api_key with mode 0600 and ~/.nanopi/config.toml with api_key_file pointing at it (never inline api_key)."
    - "For localhost/Ollama base URLs the api key prompt is skipped and an empty key file is not written."
    - "Wizard sends a small probe request BEFORE writing config.toml; on probe failure no config.toml is written and the user is offered retry or abort."
    - "Any pre-existing ~/.nanopi/config.toml is left untouched until probe succeeds."
  artifacts:
    - path: "src/wizard.rs"
      provides: "ProviderPreset table + run_wizard() interactive flow + probe_config()"
      contains: "PROVIDER_PRESETS"
    - path: "src/lib.rs"
      provides: "pub mod wizard; exposure"
      contains: "pub mod wizard"
    - path: "src/main.rs"
      provides: "`init` subcommand + no-config fallthrough to wizard"
      contains: "wizard::run_wizard"
  key_links:
    - from: "src/main.rs"
      to: "src/wizard.rs"
      via: "wizard::run_wizard(cwd) called when model+key+base_url are all unresolved, and for the `init` subcommand"
      pattern: "wizard::run_wizard"
    - from: "src/wizard.rs"
      to: "src/paths.rs"
      via: "paths::nanopi_home() for ~/.nanopi resolution (respects NANOPI_HOME for tests)"
      pattern: "paths::nanopi_home"
---

<objective>
Add a first-run wizard so nanopi bootstraps itself when a user runs it with no config, no env vars, and no CLI credentials. Same wizard is reachable explicitly via `nanopi init`.

Purpose: Reduce cold-start friction. Today `main.rs` bails with "error: no --model / OPENAI_MODEL / model in ~/.nanopi/config.toml" — a first-time user has nothing to do about that without reading docs. A wizard turns cold start into a 60-second guided flow.

Output:
- New `src/wizard.rs` module with a static provider preset table, stdin-driven prompts, a validation probe, and file writers for `~/.nanopi/api_key` (mode 0600) + `~/.nanopi/config.toml`.
- `src/main.rs` intercepts the "no usable config" case and calls the wizard, and adds `init` as an explicit subcommand.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/STATE.md
@src/config.rs
@src/main.rs
@src/paths.rs
@src/trust.rs
@src/provider/openai.rs
@src/provider/anthropic.rs
@Cargo.toml

<interfaces>
<!-- Key existing APIs the wizard must integrate with. Extracted from codebase. -->

From src/paths.rs:
```rust
pub fn nanopi_home() -> Option<PathBuf>;         // ~/.nanopi (honors NANOPI_HOME)
pub fn global_config_path() -> Option<PathBuf>;  // ~/.nanopi/config.toml
```

From src/config.rs:
```rust
pub struct Config {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_file: Option<PathBuf>,
    pub api_key: Option<String>,
    pub api_kind: Option<String>,
    // ...trust/logging/hooks/skills/tools/provider
}
pub fn load_config(cwd: &Path) -> Result<Config, ConfigError>;
```

Cargo.toml already provides: reqwest (rustls-tls, json), tokio, serde, serde_json, anyhow, toml, dirs. No new crates required.

Precedent for stdin prompts: `src/trust.rs` uses `std::io` — do NOT add dialoguer.
</interfaces>
</context>

<tasks>

<task type="auto" tdd="true">
  <name>Task 1: Create wizard module (presets + prompts + probe + file writers)</name>
  <files>src/wizard.rs, src/lib.rs</files>
  <behavior>
    - `PROVIDER_PRESETS: &[ProviderPreset]` static array exposes exactly six entries in this order: OpenAI, DeepSeek, Anthropic direct, Gemini via gateway, Ollama, Custom. Each non-Custom entry has non-empty `base_url`, `api_kind` in {"openai","anthropic"}, and a suggested `default_model`.
    - `is_localhost(url)` returns true for base_urls whose host is `localhost` / `127.0.0.1` / `0.0.0.0` and false otherwise; used to skip the api-key prompt for Ollama/local.
    - `probe_config(base_url, api_kind, model, api_key)` (async) sends ONE tiny non-streaming request (`max_tokens=1` on OpenAI `/chat/completions`, or `max_tokens=1` on Anthropic `/v1/messages`) with a single "hi" user message and returns `Result<(), String>` — Ok on any 2xx, Err with the status + body snippet otherwise. Reuse the existing `reqwest::Client::new()` pattern from `src/provider/openai.rs` / `anthropic.rs`; do NOT stream, do NOT depend on the full provider adapters (a hand-rolled JSON POST is fine and keeps the probe cheap).
    - `write_key_file(path, key)` writes the api key with Unix mode 0o600 via `std::os::unix::fs::OpenOptionsExt::mode` (guarded with `#[cfg(unix)]`; on non-unix fall back to plain write). Parent dir is created if missing.
    - `write_config_toml(path, model, base_url, api_kind, api_key_file)` emits a minimal TOML doc containing exactly those four keys plus a leading `# generated by \`nanopi init\`` comment. `api_key_file` is written as `"~/.nanopi/api_key"` (tilde-form string, matching the existing loader's `expand_tilde` handling in main.rs).
    - Public entry `pub async fn run_wizard(force_overwrite_prompt: bool) -> anyhow::Result<()>`:
        1. Resolve `nanopi_home()`; error if unresolvable.
        2. If `force_overwrite_prompt` is true and config.toml already exists, prompt `"~/.nanopi/config.toml already exists. Overwrite? [y/N]: "`; abort on anything but `y`/`yes`.
        3. Print numbered provider list; read a line from stdin; re-prompt on invalid selection.
        4. Fill (base_url, api_kind, model) from preset — for Custom, prompt each field. Model line offers the preset's default as bracketed hint; empty input accepts default.
        5. If `is_localhost(base_url)` → set api_key to `""` and skip key prompt AND skip writing the key file (write no `api_key_file` line and no `api_key` line in config.toml for this case). Otherwise prompt for the key (single line; do NOT echo-hide — matches trust.rs simplicity; a comment in code notes this is intentional for v1).
        6. Call `probe_config(...)`. On error: print `probe failed: {err}`, prompt `retry / edit / abort? [r/e/a]`; `r` loops the probe, `e` restarts from step 3, `a` returns Ok(()) without writing anything.
        7. On success: write `~/.nanopi/api_key` (only if non-localhost), then write `~/.nanopi/config.toml`. Print the resolved paths.
    - Add `pub mod wizard;` to `src/lib.rs` in the same block as `pub mod trust;`.
    - Unit tests in `src/wizard.rs` (behind `#[cfg(test)]`) covering: preset count/shape (6 entries, Custom last), `is_localhost` recognizes 127.0.0.1/localhost and rejects api.openai.com, `write_key_file` produces a file whose Unix mode is `0o600` (gated on `#[cfg(unix)]`), and `write_config_toml` produces TOML that `toml::from_str::<config::Config>` parses back with the same model/base_url/api_kind and `api_key_file == "~/.nanopi/api_key"`.
  </behavior>
  <action>Create `src/wizard.rs` implementing the behaviors above. Use only crates already in Cargo.toml (`reqwest`, `serde_json`, `anyhow`, `toml`, `tokio`, `std`). Follow `src/trust.rs` style for stdin prompts (`use std::io::{self, Write}; io::stdout().flush(); io::stdin().read_line`). Do NOT depend on `crate::provider::{OpenAiProvider, AnthropicProvider}` for the probe — hand-roll a minimal POST so the wizard stays independent of the streaming machinery. Register the module by adding `pub mod wizard;` to `src/lib.rs` next to the other top-level modules (after `pub mod trust;`).</action>
  <verify>
    <automated>cd /root/workspace/nanopi && cargo test --lib wizard:: 2>&1 | tail -30 && cargo check --lib 2>&1 | tail -5</automated>
  </verify>
  <done>`src/wizard.rs` compiles, `cargo test wizard::` passes all new tests, `cargo check --lib` shows no errors, and `pub mod wizard;` appears in `src/lib.rs`.</done>
</task>

<task type="auto" tdd="false">
  <name>Task 2: Wire wizard into main.rs (init subcommand + no-config fallthrough)</name>
  <files>src/main.rs</files>
  <action>
    Modify `src/main.rs` to invoke the wizard in two situations.

    (a) Explicit `nanopi init` subcommand. Add a clap `#[command(subcommand)]` variant OR — simpler and consistent with the existing flat `Args` shape — recognize `init` as the positional message value: if `args.positional_message.as_deref() == Some("init")` AND no `-p`, `-m`, `--session`, `--fork`, `--continue`, `--model`, `--base-url`, `--api-key` are set, treat it as the init subcommand. Call `wizard::run_wizard(true)` (force_overwrite_prompt=true so existing config is protected) and return the resulting `ExitCode`. Do this BEFORE `load_config` so a broken config.toml doesn't block reconfiguration — but DO still call `load_config` afterward is unnecessary; simply exit after the wizard completes.

    (b) Implicit first-run. In the current resolution ladder for `model`, `base_url`, `api_key`, if ALL of the following hold, launch the wizard:
      - `args.model` is None AND `OPENAI_MODEL` env var is unset AND `cfg.model` is None
      - `args.api_key` is None AND `OPENAI_API_KEY` env var is unset AND `cfg.api_key` is None AND `cfg.api_key_file` is None
      - `args.base_url` is None AND `OPENAI_BASE_URL` env var is unset AND `cfg.base_url` is None
    Print a short banner (`"nanopi: no config found — launching first-run wizard (Ctrl-C to abort)"`), call `wizard::run_wizard(false)` (no overwrite prompt needed; there's no config to protect). On success, RE-run `config::load_config(&cwd)` and continue with the normal resolution ladder using the new values (do not exit — the user expects their original invocation to proceed). On wizard abort/error, exit with code 2.

    Do NOT touch the existing resolution logic — only add the pre-checks and the re-load path. Do not change the error messages that fire when e.g. model is missing but api_key is set (that user has partial config and needs a targeted fix, not a full wizard).

    Note: subcommand detection happens at the top of `main` (after `Args::parse`) so it works even before `load_config`.
  </action>
  <verify>
    <automated>cd /root/workspace/nanopi && cargo build --bin nanopi 2>&1 | tail -10 && NANOPI_HOME=$(mktemp -d) HOME=$(mktemp -d) OPENAI_API_KEY= OPENAI_MODEL= OPENAI_BASE_URL= timeout 3 ./target/debug/nanopi 2>&1 | head -5 | grep -E "wizard|Select|Provider" && NANOPI_HOME=$(mktemp -d) HOME=$(mktemp -d) timeout 3 ./target/debug/nanopi init 2>&1 | head -5 | grep -E "Select|Provider"</automated>
  </verify>
  <done>`cargo build` succeeds; running `nanopi` with an empty NANOPI_HOME and no env creds prints the wizard banner and provider list; running `nanopi init` with an empty NANOPI_HOME also prints the provider list; running `nanopi init` with an existing `~/.nanopi/config.toml` prompts to overwrite.</done>
</task>

</tasks>

<verification>
- `cargo test --lib` green (existing tests unchanged, new wizard tests pass).
- `cargo build --release` still produces a static musl binary within existing size budget (adding a small module + no new deps should be a negligible size delta).
- Manual smoke: `NANOPI_HOME=/tmp/np-fresh nanopi` walks the wizard end to end with a real OpenAI key and lands on a working chat.
- Manual smoke: `nanopi init` a second time refuses to overwrite unless confirmed.
- Manual smoke: selecting Ollama with `base_url=http://localhost:11434/v1` skips the key prompt, probe hits the local server, config.toml written without `api_key_file`.
</verification>

<success_criteria>
- Zero new crates in Cargo.toml.
- Wizard module isolated in `src/wizard.rs`; `main.rs` diff is limited to (i) init-subcommand branch and (ii) no-config fallthrough + reload.
- `~/.nanopi/api_key` is chmod 0600 on unix.
- `~/.nanopi/config.toml` is only ever written after a successful probe.
- `nanopi init` refuses to clobber existing config without explicit `y` confirmation.
</success_criteria>

<output>
Create `.planning/quick/260819-ayr-add-a-first-run-wizard-to-nanopi-console/260819-ayr-SUMMARY.md` when done.
</output>

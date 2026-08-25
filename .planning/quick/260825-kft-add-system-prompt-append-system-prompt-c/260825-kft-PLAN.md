---
phase: quick
plan: 01
type: execute
wave: 1
depends_on: []
files_modified:
  - src/agent/prompt_override.rs
  - src/agent/mod.rs
  - src/agent/build.rs
  - src/agent/loop_.rs
  - src/provider/openai.rs
  - tests/skills_integration.rs
  - src/main.rs
  - src/mode/print.rs
  - src/mode/tui.rs
  - README.md
autonomous: true
requirements: [sysprompt-cli-flags, sysprompt-file-discovery, sysprompt-composition, sysprompt-both-modes]

must_haves:
  truths:
    - "`nanopi --system-prompt 'You are Bob'` replaces the built-in identity/guidelines prompt; project context files, skills and the cwd line are still appended."
    - "`nanopi --append-system-prompt X --append-system-prompt Y` appends both, joined by a blank line, after the base prompt."
    - "Both flags accept EITHER literal text OR a path to an existing file; an existing-but-unreadable file warns on stderr and falls back to using the raw string as text."
    - "With no `--system-prompt`, `<cwd>/.nanopi/SYSTEM.md` (trusted projects only) then `~/.nanopi/SYSTEM.md` is discovered; project beats global."
    - "With no `--append-system-prompt`, `<cwd>/.nanopi/APPEND_SYSTEM.md` (trusted projects only) then `~/.nanopi/APPEND_SYSTEM.md` is discovered; project beats global."
    - "A CLI flag suppresses the corresponding file discovery entirely (flag wins, no merge)."
    - "An untrusted project's `.nanopi/SYSTEM.md` / `.nanopi/APPEND_SYSTEM.md` is NEVER read; the global file is read without a trust gate."
    - "Both startup paths honor the values: `-p` print mode and the TUI, on fresh sessions AND on `/new`, `/fork`, `/model`, `/resume` rebuilds and `/reload` prompt recomposition."
    - "With neither flag nor file present, the composed system prompt is byte-for-byte identical to today's."
    - "`cargo build --all-targets` emits zero warnings; `cargo test -- --test-threads=1` passes."
  artifacts:
    - path: "src/agent/prompt_override.rs"
      provides: "PromptOverrides policy struct + ResolvedPrompt + resolve_prompt_input() + SYSTEM.md/APPEND_SYSTEM.md discovery with trust gate"
      contains: "pub struct PromptOverrides"
      min_lines: 120
    - path: "src/agent/mod.rs"
      provides: "module registration"
      contains: "pub mod prompt_override"
    - path: "src/agent/build.rs"
      provides: "compose_system_prompt() honoring custom/append, AgentBuildInputs.prompt_overrides, hydrate_resumed param"
      contains: "prompt_overrides"
    - path: "src/agent/loop_.rs"
      provides: "Agent.prompt_overrides field so /reload and TUI rebuilds keep the policy"
      contains: "pub prompt_overrides"
    - path: "src/main.rs"
      provides: "--system-prompt / --append-system-prompt clap flags + PromptOverrides::from_cli wiring"
      contains: "append_system_prompt"
    - path: "README.md"
      provides: "CLI table rows + Custom system prompt section documenting the two files and the trust gate"
      contains: "APPEND_SYSTEM.md"
  key_links:
    - from: "src/main.rs"
      to: "src/mode/print.rs and src/mode/tui.rs"
      via: "one PromptOverrides value passed to BOTH run_print_mode and run_tui_mode"
      pattern: "prompt_overrides"
    - from: "src/agent/build.rs"
      to: "src/agent/prompt_override.rs"
      via: "compose_system_prompt calls overrides.resolve(cwd) before building the base prompt"
      pattern: "\\.resolve\\("
    - from: "src/agent/prompt_override.rs"
      to: "src/paths.rs"
      via: "paths::nanopi_home() for the global SYSTEM.md location (honors NANOPI_HOME in tests)"
      pattern: "nanopi_home"
    - from: "src/mode/tui.rs"
      to: "src/agent/loop_.rs"
      via: "handle_reload recomposes the prompt from a.prompt_overrides, so an edited SYSTEM.md is picked up by /reload"
      pattern: "a\\.prompt_overrides"
---

<objective>
Port PI's `--system-prompt` / `--append-system-prompt` flags and its
`SYSTEM.md` / `APPEND_SYSTEM.md` file discovery to nanopi, using
nanopi-native paths (`<cwd>/.nanopi/` and `~/.nanopi/`).

Purpose: today `src/agent/system_prompt.rs:29` is the only possible
system prompt. Users cannot re-role the agent or bolt on standing
instructions without recompiling. PI solves this with two flags plus two
discoverable files; nanopi has every piece needed (a single composition
seam at `build.rs:213`, a trust gate at `trust.rs:22`, a global-dir
helper at `paths.rs:19`) and just lacks the wiring.

Output: a new deep module `src/agent/prompt_override.rs` that owns
resolution + discovery behind a small policy struct, threaded through
`compose_system_prompt` and stored on `Agent` (exactly like the existing
`SkillLoadPolicy` / `no_context_files` pattern) so TUI rebuilds and
`/reload` keep honoring it.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/STATE.md

Project skills that apply (read before starting):
@.claude/skills/codebase-design/SKILL.md — the new module must be DEEP:
much behavior (path-vs-text resolution, two-scope discovery, trust
gating, join semantics) behind a tiny interface (`from_cli` + `resolve`).
@.claude/skills/tdd/SKILL.md — tests assert behavior (which file won,
what the composed prompt contains), never smoke-test.

Reference implementation (PI, TypeScript — read only if a detail below
is ambiguous):
- `/root/workspace/pi/packages/coding-agent/src/cli/args.ts:95-99` — flag defs.
- `/root/workspace/pi/packages/coding-agent/src/core/resource-loader.ts:53-68` — `resolvePromptInput` (path-or-text).
- `/root/workspace/pi/packages/coding-agent/src/core/resource-loader.ts:1022-1048` — file discovery.
- `/root/workspace/pi/packages/coding-agent/src/core/resource-loader.ts:525` — flag suppresses discovery (`?? discoverSystemPromptFile()`).
- `/root/workspace/pi/packages/coding-agent/src/core/system-prompt.ts:44-71` — composition order.

<interfaces>
<!-- Everything below was read during planning. Do NOT re-explore; these
     are the exact contracts to build against. -->

src/agent/context_files.rs — the module to MIRROR in shape and doc style
(candidate discovery, project-vs-global precedence, format helper,
behavior-asserting unit tests using a `tmpdir()` helper):
  pub struct ContextFile { pub path: PathBuf, pub content: String }
  pub fn load_project_context_files(cwd: &Path, agent_dir: Option<&Path>) -> Vec<ContextFile>
  pub fn format_context_files(files: &[ContextFile]) -> String

src/paths.rs:
  pub fn nanopi_home() -> Option<PathBuf>            // NANOPI_HOME, else ~/.nanopi
  pub fn project_skills_dir(cwd: &Path) -> PathBuf   // <cwd>/.nanopi/skills

src/trust.rs:
  pub enum TrustStatus { AlreadyTrusted, AlreadyDistrusted, NeedsPrompt, NoProjectResources }
  pub fn check_trust_status(cwd: &Path) -> TrustStatus

src/agent/system_prompt.rs:
  pub fn build(cwd: &Path, tool_names: &[String]) -> String
  // Ends with the literal line: "Current working directory: {cwd}"

src/agent/build.rs (the seam):
  pub struct SkillLoadPolicy { user_dir, project_dir, cli_paths, no_discovery, disabled }
    // Clone + Default; ::from_cli(cwd, cli_paths, no_skills, project_trusted, disabled)
    // ^ COPY THIS SHAPE for PromptOverrides.
  pub struct AgentBuildInputs { cwd, registry, provider, session_path, session_id,
                                permission, hooks, model, base_url, api_key,
                                skill_load, no_context_files }
  impl Agent { pub fn build_fresh(inputs: AgentBuildInputs) -> (Self, Vec<SkillDiagnostic>) }
  impl Agent { pub fn hydrate_resumed(&mut self, provider, registry, permission, hooks,
                                      model, base_url, api_key, skill_load,
                                      no_context_files) -> Vec<SkillDiagnostic> }
  pub fn compose_system_prompt(cwd: &Path, tool_names: &[String], skills: &[Skill],
                               no_context_files: bool) -> String

src/agent/loop_.rs:63-101 — `pub struct Agent { …, pub skills: Vec<Skill>,
  pub no_context_files: bool }`. Agent has no Default (holds Box<dyn Provider>),
  so every struct literal must gain the new field explicitly.

Agent struct literals / hydrate_resumed calls that will need the new field
(compiler will point at each; this is the exhaustive list found by grep):
  src/agent/loop_.rs: 226, 1097, 1191, 1254, 1329, 1426, 1568, 1654, 2069, 2132, 2248
  src/provider/openai.rs: 978 (hydrate_resumed call in a test)
  tests/skills_integration.rs: 47 (stub_agent_inputs helper)
  src/mode/print.rs: 131 (hydrate), 145 (build_fresh)
  src/mode/tui.rs: 343 (hydrate), 357 (build_fresh), 1605 (build_fresh via app),
                   1721, 2169, 2640 (hydrate via app), 2793 (compose in handle_reload)

src/mode/tui.rs App: field `no_context_files: bool` at 669, `App::new` param at
686, initializer at 727, call site at 394-402. The new value follows the same
three-touchpoint path.
</interfaces>
</context>

<tasks>

<task type="auto" tdd="true">
  <name>Task 1: New deep module — prompt override resolution + file discovery</name>
  <files>src/agent/prompt_override.rs, src/agent/mod.rs</files>
  <behavior>
    - `resolve_prompt_input("You are Bob")` → `"You are Bob"` (no such path exists → literal text).
    - `resolve_prompt_input("<tmp>/p.md")` where the file holds "from file" → `"from file"`.
    - An existing path that fails to read (e.g. a DIRECTORY named `p.md`) → returns the raw string unchanged and warns on stderr.
    - `PromptOverrides::default().resolve(cwd)` with an empty NANOPI_HOME and no project files → `ResolvedPrompt { custom: None, append: None }`.
    - Global-only: `~/.nanopi/SYSTEM.md` = "global sys" → `custom == Some("global sys")`.
    - Project beats global: project `.nanopi/SYSTEM.md` = "proj sys" + global = "global sys", `project_trusted = true` → `custom == Some("proj sys")`.
    - Trust gate: same layout with `project_trusted = false` → `custom == Some("global sys")` (project file NOT read).
    - Global file needs no trust gate: `project_trusted = false` with only a global file still resolves it.
    - Append discovery is the same two-scope rule over `APPEND_SYSTEM.md`.
    - CLI wins over files: `from_cli(Some("flag text"), vec![], true).resolve(cwd)` with a project SYSTEM.md present → `custom == Some("flag text")` and the file is not consulted.
    - Repeatable append joins with a blank line: `from_cli(None, vec!["A", "B"], _)` → `append == Some("A\n\nB")`.
    - A CLI `--append-system-prompt` suppresses APPEND_SYSTEM.md discovery entirely (no merge).
    - Empty/whitespace-only resolved text is treated as absent (`None`) so a stray empty SYSTEM.md cannot silently blank the prompt.
  </behavior>
  <action>
Create `src/agent/prompt_override.rs`. Mirror `src/agent/context_files.rs`
in structure and doc style: a dense module doc that cites the PI sources
(`resource-loader.ts:53-68`, `:1022-1048`, `:525`, `system-prompt.ts:44-71`)
and states the nanopi-specific divergences and WHY.

Public interface (keep it this small — the module is deep, callers are not):

  /// Unresolved policy: what the CLI asked for plus whether project-local
  /// files may be read. Cheap, Clone + Default, stored on Agent/App exactly
  /// like SkillLoadPolicy.
  #[derive(Debug, Clone, Default)]
  pub struct PromptOverrides { /* private fields */ }

  impl PromptOverrides {
      pub fn from_cli(system_prompt: Option<String>,
                      append_system_prompt: Vec<String>,
                      project_trusted: bool) -> Self;
      /// Resolve against the filesystem NOW (reads flag paths and, when a
      /// flag is absent, discovers the corresponding file).
      pub fn resolve(&self, cwd: &Path) -> ResolvedPrompt;
  }

  /// Resolved TEXT, ready to compose.
  #[derive(Debug, Clone, Default)]
  pub struct ResolvedPrompt { pub custom: Option<String>, pub append: Option<String> }

Private helpers: `resolve_prompt_input(&str) -> String` (path-or-text, per
PI) and a discovery fn over the two candidate scopes. Constants for the
filenames: `SYSTEM_FILE = "SYSTEM.md"`, `APPEND_FILE = "APPEND_SYSTEM.md"`.

Behavioral rules to encode, each with a WHY comment:
1. Path-or-text: if `std::fs::metadata(input)` says a file exists, read it;
   an existing-but-unreadable path warns on stderr and falls back to the raw
   string (PI does exactly this — a prompt that happens to look like a path
   must still work).
2. Discovery order: project `<cwd>/.nanopi/<FILE>` first, then global
   `<nanopi_home>/<FILE>`; first hit wins. Project beats global because it
   is the more specific scope — same rule as config.toml layering.
3. The PROJECT file is gated on `project_trusted`; the GLOBAL file is not.
   Comment WHY: a project-local SYSTEM.md is arbitrary influence over the
   agent shipped inside a cloned repo — the same threat model as project
   skills (`SkillLoadPolicy::from_cli`) and PI's own `isProjectTrusted()`
   gate. The global file is the user's own machine-wide config.
4. A CLI flag SUPPRESSES the matching discovery (PI `resource-loader.ts:525`
   uses `??`, not a merge). Do not concatenate flag text with file text.
5. Multiple `--append-system-prompt` values join with `"\n\n"` (PI's blank-line
   join), after each value is independently path-or-text resolved.
6. Resolved text that is empty after `trim()` becomes `None`.

Use `crate::paths::nanopi_home()` for the global dir so `NANOPI_HOME` keeps
working for test isolation. Do NOT add a `paths.rs` helper for the project
dir unless it reads better — `cwd.join(".nanopi")` inline is fine and keeps
this module self-contained; if you do add helpers, name them
`user_prompt_file` / `project_prompt_file` and put them next to
`project_skills_dir`.

DELIBERATE NON-GOAL, document it in the module doc: no `config.toml` fields
for these. Justification to write down — the two discoverable files ARE the
config surface, and they already reproduce the `api_kind` precedence ladder
(CLI flag > project `.nanopi/SYSTEM.md` > global `~/.nanopi/SYSTEM.md`), one
tier per location. A TOML string field would add a fourth tier whose only
distinguishing feature is worse ergonomics for the multi-line, markdown-ish
text a system prompt actually is. PI ships no config field either.

Tests: `#[cfg(test)] mod tests` covering every bullet in <behavior>. Copy the
`tmpdir()` helper style from `context_files.rs` (temp dir + `util::uuid::v7()`,
`remove_dir_all` at the end). Any test that touches the global scope must take
`crate::TEST_LOCK` and set/restore `NANOPI_HOME` to a fresh empty temp dir —
see `build.rs::compose_injects_cwd_context_file` for the exact pattern —
otherwise it reads the developer's real `~/.nanopi/SYSTEM.md`.

Register the module: add `pub mod prompt_override;` to `src/agent/mod.rs`
in alphabetical position (after `permission`).
  </action>
  <verify>
    <automated>cargo test -- --test-threads=1 prompt_override && ! cargo build --all-targets 2>&1 | grep -E '^(warning|error)'</automated>
  </verify>
  <done>`src/agent/prompt_override.rs` exists, is registered in `src/agent/mod.rs`, all its unit tests pass under `--test-threads=1`, and the build is warning-free. No other file has changed behavior yet.</done>
</task>

<task type="auto" tdd="true">
  <name>Task 2: Thread PromptOverrides through the composition seam and Agent</name>
  <files>src/agent/build.rs, src/agent/loop_.rs, src/provider/openai.rs, tests/skills_integration.rs, src/mode/print.rs, src/mode/tui.rs</files>
  <behavior>
    - `compose_system_prompt` with `PromptOverrides::default()` and no files present produces exactly the same string as today (regression guard: assert it starts with "You are nanopi" and contains the guidelines block).
    - With a custom prompt: output starts with the custom text, does NOT contain "You are nanopi", still ends the base section with "Current working directory: <cwd>", and still contains the `<project_context>` block and the skills block.
    - With append text only: output contains the default base AND the append text after it, separated by a blank line, with `<project_context>` appearing AFTER the append text.
    - With custom + append: custom text, then the cwd line, then the append text, then context files.
    - `--no-context-files` still suppresses `<project_context>` when a custom prompt is in play.
    - `Agent.prompt_overrides` survives `hydrate_resumed` (assert the field equals what was passed).
  </behavior>
  <action>
Add the policy to the agent layer. Order of edits so the compiler guides you:

1. `src/agent/loop_.rs`: add `pub prompt_overrides: crate::agent::prompt_override::PromptOverrides`
   to `struct Agent` (after `no_context_files`), with a doc comment saying WHY
   it is stashed here — `/reload` and the TUI's `/new`/`/fork`/`/model`/`/resume`
   rebuilds recompose the system prompt and must reuse the same policy; storing
   the UNRESOLVED policy (not the resolved text) means `/reload` re-reads an
   edited `SYSTEM.md` from disk, which is the whole point of `/reload`.
   Then add `prompt_overrides: PromptOverrides::default(),` to every Agent
   struct literal the compiler flags (loop_.rs 226 + the ~10 test literals).

2. `src/agent/build.rs`:
   - `compose_system_prompt(cwd, tool_names, skills, no_context_files,
     overrides: &PromptOverrides)`. Body: `let resolved = overrides.resolve(cwd);`
     then build the base as `match resolved.custom { Some(text) =>
     format!("{text}\n\nCurrent working directory: {}", cwd.display()),
     None => system_prompt::build(cwd, tool_names) }`, then push
     `"\n\n" + append` when present, then the existing context-files and skills
     blocks unchanged.
     Document the two invariants in the fn doc: (a) a custom prompt replaces
     ONLY the identity/tools/guidelines section — context files, skills and the
     cwd line still apply, matching PI `system-prompt.ts:44-71`; (b) the base
     section always ends with the cwd line whether it came from
     `system_prompt::build` or from a custom prompt, so the append/context/skills
     tail is byte-identical across both branches.
     Also note the consequence a user must know: a custom prompt drops the
     "Available tools: …" line, and some models skip tool calls without it (see
     the note at the top of `system_prompt.rs`).
   - `AgentBuildInputs`: add `pub prompt_overrides: PromptOverrides`, destructure
     it in `build_fresh`, pass it to `compose_system_prompt`, and store it on the
     constructed `Agent`.
   - `hydrate_resumed`: add a `prompt_overrides: PromptOverrides` parameter after
     `no_context_files`, assign `self.prompt_overrides = prompt_overrides;` next
     to the existing `self.no_context_files = …`, and pass `&self.prompt_overrides`
     to the `compose_system_prompt` call in the `context.system.is_none()` branch.
     It already carries `#[allow(clippy::too_many_arguments)]`.

3. Fix the remaining call sites to compile, passing `PromptOverrides::default()`
   for now (real wiring is Task 3): `src/provider/openai.rs:978`,
   `tests/skills_integration.rs:47` (add the field to `stub_agent_inputs`),
   `src/mode/print.rs:131,145`, `src/mode/tui.rs:343,357,1605,1721,2169,2640`,
   and `src/mode/tui.rs:2793` in `handle_reload` — that one takes
   `&a.prompt_overrides` (NOT default) since the Agent already owns the policy.

4. Extend `src/agent/build.rs`'s `#[cfg(test)] mod tests` with the cases in
   <behavior>. Follow the existing `compose_injects_cwd_context_file` pattern:
   take `crate::TEST_LOCK`, point `NANOPI_HOME` at an empty temp dir, restore it
   at the end. Keep the existing tests passing with the new argument.
  </action>
  <verify>
    <automated>cargo test -- --test-threads=1 && ! cargo build --all-targets 2>&1 | grep -E '^(warning|error)'</automated>
  </verify>
  <done>`compose_system_prompt` honors custom + append text, `Agent` carries the policy through `build_fresh` / `hydrate_resumed` / `/reload`, the whole suite passes single-threaded, and the build is warning-free. The two CLI flags do not exist yet, so behavior is unchanged for users.</done>
</task>

<task type="auto">
  <name>Task 3: CLI flags, wiring into BOTH startup paths, and docs</name>
  <files>src/main.rs, src/mode/print.rs, src/mode/tui.rs, README.md</files>
  <action>
1. `src/main.rs` — add to `struct Args`, next to `no_context_files`, with
   doc comments in the surrounding style (they become `--help` text and must
   say that text OR a file path is accepted):

     /// Replace the built-in system prompt. Accepts literal text or a path
     /// to a file. Suppresses `.nanopi/SYSTEM.md` discovery.
     #[arg(long = "system-prompt", value_name = "TEXT_OR_PATH")]
     system_prompt: Option<String>,

     /// Append to the system prompt (repeatable; values joined by a blank
     /// line). Accepts literal text or a path. Suppresses
     /// `.nanopi/APPEND_SYSTEM.md` discovery.
     #[arg(long = "append-system-prompt", value_name = "TEXT_OR_PATH")]
     append_system_prompt: Vec<String>,

   Build the policy immediately after the existing `project_trusted` /
   `skill_load` block (it needs the same `project_trusted` value):

     let prompt_overrides = nanopi::agent::prompt_override::PromptOverrides::from_cli(
         args.system_prompt.clone(),
         args.append_system_prompt.clone(),
         project_trusted,
     );

   Pass `prompt_overrides.clone()` as the final argument to BOTH
   `print::run_print_mode(...)` and `tui::run_tui_mode(...)`. Wiring only one
   of the two is the exact bug class that bit `cfg.provider` — both call sites
   must change in this task.

   Also add `system_prompt`/`append_system_prompt` to the guard list in
   `is_init_subcommand` (either flag present means the user wants a real chat
   turn, not the wizard).

2. `src/mode/print.rs` — add a `prompt_overrides: crate::agent::prompt_override::PromptOverrides`
   parameter after `no_context_files`; pass it to `AgentBuildInputs` and to
   `hydrate_resumed`, replacing the Task-2 placeholders.

3. `src/mode/tui.rs`:
   - `run_tui_mode`: same new trailing parameter. Clone it for rebuilds the way
     `skill_load_for_rebuilds` is cloned, pass it to the startup
     `hydrate_resumed` (343) and `AgentBuildInputs` (357).
   - `App`: new field `prompt_overrides: crate::agent::prompt_override::PromptOverrides`
     next to `no_context_files` (669), matching doc comment (`/new`, `/fork`,
     `/resume`, `/model` rebuild with the same prompt policy), new `App::new`
     parameter (686) + initializer (727), and pass it at the `App::new` call
     site (394-402).
   - Replace the Task-2 `PromptOverrides::default()` placeholders at 1605, 1721,
     2169, 2640 with `app.prompt_overrides.clone()`.

4. Grep-gate your own work: no `PromptOverrides::default()` may remain in
   `src/main.rs` or `src/mode/` (it is still correct inside `src/agent/loop_.rs`
   literals, `src/provider/openai.rs` and `tests/skills_integration.rs`).

5. `README.md`:
   - Two rows in the `## CLI` table (after `-S`, `--no-skills`):
     `| --system-prompt <text|path> | — | Replace the built-in system prompt |`
     `| --append-system-prompt <text|path> | — | Append to the system prompt (repeatable) |`
     Add the missing `-C`, `--no-context-files` row too while you are there.
   - A short `## Custom system prompt` section after `## Skills`, in the same
     terse style: the two flags (text or path, append repeatable and blank-line
     joined), the discovered files with their precedence
     (`<cwd>/.nanopi/SYSTEM.md` only when trusted via `-a` or a persisted
     decision, then `~/.nanopi/SYSTEM.md`), the note that a flag suppresses
     discovery, and the caveat that a replaced prompt loses the auto-generated
     "Available tools: …" line so you should mention the tools yourself.
  </action>
  <verify>
    <automated>cargo build --all-targets 2>&1 | grep -E '^(warning|error)' ; cargo test -- --test-threads=1 && cargo run -q -- --help | grep -e '--system-prompt' -e '--append-system-prompt' && ! grep -rn 'PromptOverrides::default()' src/main.rs src/mode/</automated>
  </verify>
  <done>`nanopi --help` lists both flags; the resolved policy reaches print mode, TUI startup, every TUI rebuild path and `/reload`; no placeholder defaults remain under `src/main.rs`/`src/mode/`; suite green single-threaded; build warning-free; README documents flags, files, precedence and the trust gate.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| cloned repo → agent system prompt | `<cwd>/.nanopi/SYSTEM.md` and `APPEND_SYSTEM.md` are attacker-authored content if the repo came from elsewhere, and they land in the highest-authority part of the request. |
| CLI arg → filesystem read | `--system-prompt <value>` triggers a read of whatever path the value names. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-kft-01 | Elevation of Privilege | project `.nanopi/SYSTEM.md` / `APPEND_SYSTEM.md` | mitigate | Project-scope files read only when `project_trusted` (from `trust::check_trust_status` / `-a`), enforced inside `PromptOverrides::resolve`; unit test asserts an untrusted project file is never read. |
| T-kft-02 | Tampering | prompt replacement drops the tools line | accept | Documented in the fn doc and README rather than blocked — replacing the prompt is the user's explicit request; the failure mode is a model that stops calling tools, which is visible immediately. |
| T-kft-03 | Information Disclosure | `--system-prompt <path>` reads an arbitrary file into the prompt | accept | Same trust level as `-m "$(cat f)"`; the user chose the path on their own machine. No new capability. |
| T-kft-04 | Denial of Service | empty or whitespace-only `SYSTEM.md` blanks the agent's instructions | mitigate | Empty-after-`trim()` resolves to `None`, falling back to the built-in prompt (Task 1 behavior list). |
| T-kft-SC | Tampering | npm/pip/cargo installs | mitigate | No new dependencies are added by this plan; nothing to audit. |
</threat_model>

<verification>
- `! cargo build --all-targets 2>&1 | grep -E '^(warning|error)'` — zero warnings preserved.
- `cargo test -- --test-threads=1` — full suite green (parallelism races on the env-var tests).
- `cargo run -q -- --help` shows both new flags with text-or-path wording.
- `! grep -rn 'PromptOverrides::default()' src/main.rs src/mode/` — both startup paths really wired.
- No-override regression: the `compose_system_prompt` default-policy test asserts the built-in prompt is unchanged.
</verification>

<success_criteria>
- `--system-prompt` (text or file) replaces the identity/guidelines section while context files, skills and the cwd line still apply.
- `--append-system-prompt` is repeatable and joins with a blank line.
- `SYSTEM.md` / `APPEND_SYSTEM.md` are discovered project-then-global, project gated on trust, and are suppressed by the corresponding flag.
- Print mode, TUI startup, TUI rebuilds (`/new`, `/fork`, `/model`, `/resume`) and `/reload` all honor the policy; `/reload` re-reads the files from disk.
- Zero new config.toml fields, with the rationale recorded in the module doc.
- Build warning-free; suite green under `--test-threads=1`.
</success_criteria>

<output>
Create `.planning/quick/260825-kft-add-system-prompt-append-system-prompt-c/260825-kft-SUMMARY.md` when done
</output>

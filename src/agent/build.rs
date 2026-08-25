//! Agent factory — the single choke-point for constructing an Agent
//! with skills loaded and injected into the system prompt.
//!
//! Modeled after PI's `DefaultResourceLoader` +
//! `buildSystemPrompt(skills: providedSkills)`
//! (`pi/packages/coding-agent/src/core/resource-loader.ts` and
//! `.../core/system-prompt.ts`). PI's rule is: skills are appended to
//! the system prompt via `formatSkillsForPrompt` only when the `read`
//! tool is available — otherwise the model has no way to load their
//! full content on demand.

use std::path::{Path, PathBuf};

use crate::agent::context::Context;
use crate::agent::loop_::{Agent, HooksConfig, Provider};
use crate::agent::permission::PermissionGate;
use crate::agent::prompt_override::PromptOverrides;
use crate::event::Usage;
use crate::resources::{
    format_skills_for_prompt, load_skills, LoadSkillsOptions, LoadSkillsResult, Skill,
    SkillDiagnostic,
};
use crate::tool::ToolRegistry;

/// Where skills should be loaded from for a given run. Callers derive
/// this from CLI flags + trust decision and hand it to the factory.
#[derive(Debug, Clone, Default)]
pub struct SkillLoadPolicy {
    pub user_dir: Option<PathBuf>,
    pub project_dir: Option<PathBuf>,
    pub cli_paths: Vec<PathBuf>,
    pub no_discovery: bool,
    pub disabled: Vec<String>,
}

impl SkillLoadPolicy {
    /// Derive the policy from CLI args + trust status.
    /// - `project_trusted` false → project skills skipped.
    /// - `no_skills` true → user+project discovery skipped; `--skill` still
    ///   loads (matches PI `--no-skills` semantics).
    pub fn from_cli(
        cwd: &Path,
        cli_paths: Vec<PathBuf>,
        no_skills: bool,
        project_trusted: bool,
        disabled: Vec<String>,
    ) -> Self {
        Self {
            user_dir: crate::paths::user_skills_dir(),
            project_dir: if project_trusted {
                Some(crate::paths::project_skills_dir(cwd))
            } else {
                None
            },
            cli_paths,
            no_discovery: no_skills,
            disabled,
        }
    }

    pub fn into_options(self) -> LoadSkillsOptions {
        LoadSkillsOptions {
            user_dir: self.user_dir,
            project_dir: self.project_dir,
            cli_paths: self.cli_paths,
            no_discovery: self.no_discovery,
            disabled: self.disabled,
        }
    }
}

/// All the pieces the factory needs. Grouped to keep call sites tidy.
pub struct AgentBuildInputs {
    pub cwd: PathBuf,
    pub registry: ToolRegistry,
    pub provider: Box<dyn Provider>,
    pub session_path: PathBuf,
    pub session_id: String,
    pub permission: PermissionGate,
    pub hooks: HooksConfig,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub skill_load: SkillLoadPolicy,
    /// CLI `--no-context-files` — skip AGENTS.md / CLAUDE.md discovery.
    pub no_context_files: bool,
    /// `--system-prompt` / `--append-system-prompt` policy.
    pub prompt_overrides: PromptOverrides,
}

impl Agent {
    /// Fresh agent path: builds the system prompt (with `<available_skills>`
    /// appended when `read` is available) and stashes the loaded skill
    /// list on the agent so `/skill:` expansion can look them up. Returns
    /// the agent along with any diagnostics from skill loading.
    pub fn build_fresh(inputs: AgentBuildInputs) -> (Self, Vec<SkillDiagnostic>) {
        let AgentBuildInputs {
            cwd,
            registry,
            provider,
            session_path,
            session_id,
            permission,
            hooks,
            model,
            base_url,
            api_key,
            skill_load,
            no_context_files,
            prompt_overrides,
        } = inputs;

        let tool_names = registry.names();
        let LoadSkillsResult {
            skills,
            diagnostics,
        } = load_skills(skill_load.into_options());
        let prompt = compose_system_prompt(
            &cwd,
            &tool_names,
            &skills,
            no_context_files,
            &prompt_overrides,
        );

        let agent = Agent {
            context: Context {
                system: Some(prompt),
                messages: Vec::new(),
                tools: registry.all_specs(),
                thinking: None,
            },
            provider,
            registry,
            session_path,
            session_id,
            cwd,
            permission,
            hooks,
            model,
            base_url,
            api_key,
            usage_total: Usage::default(),
            turn_count: 0,
            skills,
            no_context_files,
            prompt_overrides,
        };
        (agent, diagnostics)
    }

    /// Resumed-agent path: caller already has an `Agent` back from
    /// `Agent::load_session`; this hydrates the missing runtime fields
    /// (provider, permission, hooks, model, keys), (re)loads skills,
    /// and — only if the persisted context has no system prompt yet —
    /// composes a fresh one with skills injected.
    ///
    /// Matches PI's behavior of preserving the persisted system prompt
    /// for a resumed session (see `agent-session.ts` — it does not
    /// overwrite `state.systemPrompt` unless the caller explicitly
    /// asks for it).
    #[allow(clippy::too_many_arguments)]
    pub fn hydrate_resumed(
        &mut self,
        provider: Box<dyn Provider>,
        registry: ToolRegistry,
        permission: PermissionGate,
        hooks: HooksConfig,
        model: String,
        base_url: String,
        api_key: String,
        skill_load: SkillLoadPolicy,
        no_context_files: bool,
        prompt_overrides: PromptOverrides,
    ) -> Vec<SkillDiagnostic> {
        self.provider = provider;
        self.registry = registry;
        self.permission = permission;
        self.hooks = hooks;
        self.model = model;
        self.base_url = base_url;
        self.api_key = api_key;
        self.no_context_files = no_context_files;
        self.prompt_overrides = prompt_overrides;

        // load_session rebuilds Context from JSONL messages only — it
        // never repopulates `tools`. Without this line, the first turn
        // after `--continue` goes out with `tools: []`. Models that
        // rely on the request's tool declarations to route into their
        // native tool-call channel (e.g. minimax-M3, whose sentinel
        // tokens `<]minimax[>` leak into `content` as scrambled text
        // when tools are missing) then narrate tool calls instead of
        // invoking them. TUI resume paths were setting this manually;
        // interactive/print `--continue` was not.
        self.context.tools = self.registry.all_specs();

        let LoadSkillsResult {
            skills,
            diagnostics,
        } = load_skills(skill_load.into_options());
        self.skills = skills;

        if self.context.system.is_none() {
            let tool_names = self.registry.names();
            self.context.system = Some(compose_system_prompt(
                &self.cwd,
                &tool_names,
                &self.skills,
                self.no_context_files,
                &self.prompt_overrides,
            ));
        }

        diagnostics
    }
}

/// System prompt with the `<project_context>` block (AGENTS.md /
/// CLAUDE.md) appended, then the skills block when the `read` tool is
/// present. Ordering — base prompt, then context files, then skills —
/// mirrors PI's `buildSystemPrompt` (`system-prompt.ts:144-157`), where
/// project context precedes the skills section.
///
/// `no_context_files` skips context-file discovery entirely (CLI
/// `--no-context-files`). Context files, unlike skills, are injected
/// regardless of which tools are available — PI does the same.
///
/// `overrides` resolves `--system-prompt` / `--append-system-prompt`
/// (flags or discovered `SYSTEM.md` / `APPEND_SYSTEM.md`) against `cwd`.
/// Two invariants hold regardless of whether a custom prompt is in
/// play:
/// (a) a custom prompt replaces ONLY the identity/tools/guidelines
///     section produced by `system_prompt::build` — context files,
///     skills and the cwd line still apply, matching PI's
///     `system-prompt.ts:44-71`.
/// (b) the base section always ends with the
///     "Current working directory: …" line, whether it came from
///     `system_prompt::build` or from a custom prompt, so the
///     append/context/skills tail is byte-identical across both
///     branches.
/// Consequence the user must know: a custom prompt drops the
/// "Available tools: …" line that `system_prompt::build` generates, and
/// some models skip tool calls without it (see the note at the top of
/// `system_prompt.rs`) — worth mentioning tools explicitly in a custom
/// prompt.
pub fn compose_system_prompt(
    cwd: &Path,
    tool_names: &[String],
    skills: &[Skill],
    no_context_files: bool,
    overrides: &PromptOverrides,
) -> String {
    let resolved = overrides.resolve(cwd);

    let mut prompt = match resolved.custom {
        Some(text) => format!("{text}\n\nCurrent working directory: {}", cwd.display()),
        None => crate::agent::system_prompt::build(cwd, tool_names),
    };

    if let Some(append) = resolved.append {
        prompt.push_str("\n\n");
        prompt.push_str(&append);
    }

    if !no_context_files {
        let files = crate::agent::context_files::load_project_context_files(
            cwd,
            crate::paths::nanopi_home().as_deref(),
        );
        prompt.push_str(&crate::agent::context_files::format_context_files(&files));
    }

    let has_read = tool_names.iter().any(|n| n == "read");
    if has_read && !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }
    prompt
}

/// Print skill-loading diagnostics to stderr. Kept out of the factory
/// so tests can inspect diagnostics without noise.
pub fn print_skill_diagnostics(diags: &[SkillDiagnostic]) {
    use crate::resources::DiagnosticLevel;
    for d in diags {
        let label = match d.level {
            DiagnosticLevel::Warning => "warning",
            DiagnosticLevel::Collision => "collision",
        };
        eprintln!("skill {}: {} ({})", label, d.message, d.path.display());
    }
}

// Prevent unused-import lints if a future refactor drops fields.

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-build-{tag}-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn tools() -> Vec<String> {
        vec!["read".into(), "bash".into()]
    }

    /// A cwd-level AGENTS.md is injected as a <project_context> block.
    /// NANOPI_HOME is pointed at an empty dir so the global scope stays
    /// clean and the test doesn't pick up the developer's real ~/.nanopi.
    #[test]
    fn compose_injects_cwd_context_file() {
        let _g = crate::TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NANOPI_HOME");
        let home = tmpdir("home");
        std::env::set_var("NANOPI_HOME", &home);

        let cwd = tmpdir("cwd");
        std::fs::write(cwd.join("AGENTS.md"), "PROJECT RULES HERE").unwrap();

        let prompt =
            compose_system_prompt(&cwd, &tools(), &[], false, &PromptOverrides::default());
        assert!(
            prompt.contains("<project_context>"),
            "missing block: {prompt}"
        );
        assert!(prompt.contains("PROJECT RULES HERE"));
        assert!(prompt.contains("AGENTS.md"));

        // --no-context-files suppresses it entirely.
        let bare =
            compose_system_prompt(&cwd, &tools(), &[], true, &PromptOverrides::default());
        assert!(!bare.contains("<project_context>"));
        assert!(!bare.contains("PROJECT RULES HERE"));

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    /// Regression: `Agent::load_session` returns a Context with empty
    /// `tools`, so `hydrate_resumed` must repopulate it from the
    /// registry. If it doesn't, the first request after `--continue`
    /// goes out with `tools: []` and models like minimax-M3 emit their
    /// native tool-call sentinel tokens as visible text instead of
    /// invoking tools.
    ///
    /// This test drives the full resume pipeline through the wire-format
    /// builder — load_session → hydrate_resumed → build_request — and
    /// asserts the outgoing JSON body carries a non-empty `tools` array,
    /// which is what the buggy path was omitting.
    #[test]
    fn hydrate_resumed_repopulates_tools() {
        use crate::agent::loop_::HooksConfig;
        use crate::agent::permission::PermissionGate;
        use crate::provider::openai::OpenAiProvider;

        let _g = crate::TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NANOPI_HOME");
        let home = tmpdir("home");
        std::env::set_var("NANOPI_HOME", &home);

        let cwd = tmpdir("cwd");
        let (path, _hdr) =
            crate::session::new_session(&cwd, "m", "http://x").expect("new session");
        let mut agent = Agent::load_session(&path, &cwd).expect("load");
        assert!(
            agent.context.tools.is_empty(),
            "load_session should leave tools empty — hydrate is what repopulates them"
        );

        let registry = ToolRegistry::standard();
        let expected = registry.all_specs().len();
        let _ = agent.hydrate_resumed(
            Box::new(OpenAiProvider::new("", "", "")),
            registry,
            PermissionGate::from_cli(false, None),
            HooksConfig::default(),
            "m".into(),
            "http://x".into(),
            "".into(),
            SkillLoadPolicy::default(),
            true,
            PromptOverrides::default(),
        );

        assert_eq!(
            agent.context.tools.len(),
            expected,
            "hydrate_resumed must repopulate ctx.tools from the registry"
        );

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&cwd).ok();
    }

    fn one_skill() -> Skill {
        Skill {
            name: "demo".into(),
            description: "a demo skill".into(),
            file_path: PathBuf::from("/tmp/demo/SKILL.md"),
            base_dir: PathBuf::from("/tmp/demo"),
            source: crate::resources::SkillSource::User,
            disable_model_invocation: false,
        }
    }

    /// Run `f` with `NANOPI_HOME` pointed at a fresh empty temp dir,
    /// restoring the previous value afterward. Mirrors the pattern used
    /// throughout this module's existing tests.
    fn with_empty_global_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _g = crate::TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NANOPI_HOME");
        let home = tmpdir("home");
        std::env::set_var("NANOPI_HOME", &home);

        let result = f(&home);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        std::fs::remove_dir_all(&home).ok();
        result
    }

    /// Regression guard: with no override policy and no discoverable
    /// files, the composed prompt is byte-for-byte identical to today's
    /// (must-have: "with neither flag nor file present ... identical to
    /// today's").
    #[test]
    fn compose_default_policy_is_unchanged_from_todays_prompt() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir("cwd");
            let prompt = compose_system_prompt(
                &cwd,
                &tools(),
                &[],
                false,
                &PromptOverrides::default(),
            );
            assert!(prompt.starts_with("You are nanopi"), "{prompt}");
            assert!(prompt.contains("Guidelines:"), "{prompt}");
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    /// A custom prompt replaces only the identity/guidelines section:
    /// context files and the skills block still apply, and the base
    /// section still ends with the cwd line.
    #[test]
    fn compose_custom_prompt_keeps_context_files_and_skills() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir("cwd");
            std::fs::write(cwd.join("AGENTS.md"), "PROJECT RULES").unwrap();

            let overrides =
                PromptOverrides::from_cli(Some("You are Bob".to_string()), vec![], true);
            let prompt = compose_system_prompt(&cwd, &tools(), &[one_skill()], false, &overrides);

            assert!(prompt.starts_with("You are Bob"), "{prompt}");
            assert!(!prompt.contains("You are nanopi"), "{prompt}");
            assert!(
                prompt.contains(&format!("Current working directory: {}", cwd.display())),
                "{prompt}"
            );
            assert!(prompt.contains("<project_context>"), "{prompt}");
            assert!(prompt.contains("PROJECT RULES"), "{prompt}");
            assert!(prompt.contains("<available_skills>"), "{prompt}");

            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    /// Append-only: default base, then the append text (blank-line
    /// separated), then `<project_context>` after it.
    #[test]
    fn compose_append_only_lands_after_base_before_context() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir("cwd");
            std::fs::write(cwd.join("AGENTS.md"), "PROJECT RULES").unwrap();

            let overrides = PromptOverrides::from_cli(None, vec!["EXTRA TEXT".to_string()], true);
            let prompt = compose_system_prompt(&cwd, &tools(), &[], false, &overrides);

            assert!(prompt.contains("You are nanopi"), "{prompt}");
            let cwd_line = format!("Current working directory: {}", cwd.display());
            let base_end = prompt.find(&cwd_line).expect("cwd line present");
            let append_pos = prompt.find("EXTRA TEXT").expect("append text present");
            let context_pos = prompt
                .find("<project_context>")
                .expect("project context present");
            assert!(base_end < append_pos, "{prompt}");
            assert!(append_pos < context_pos, "{prompt}");
            assert!(
                prompt.contains(&format!("{cwd_line}\n\nEXTRA TEXT")),
                "append must be blank-line separated from the base: {prompt}"
            );

            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    /// Custom + append: custom text, then the cwd line, then append
    /// text, then context files.
    #[test]
    fn compose_custom_and_append_order() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir("cwd");
            std::fs::write(cwd.join("AGENTS.md"), "PROJECT RULES").unwrap();

            let overrides = PromptOverrides::from_cli(
                Some("You are Bob".to_string()),
                vec!["EXTRA TEXT".to_string()],
                true,
            );
            let prompt = compose_system_prompt(&cwd, &tools(), &[], false, &overrides);

            let custom_pos = prompt.find("You are Bob").expect("custom text present");
            let cwd_pos = prompt
                .find(&format!("Current working directory: {}", cwd.display()))
                .expect("cwd line present");
            let append_pos = prompt.find("EXTRA TEXT").expect("append text present");
            let context_pos = prompt
                .find("<project_context>")
                .expect("project context present");

            assert!(custom_pos < cwd_pos, "{prompt}");
            assert!(cwd_pos < append_pos, "{prompt}");
            assert!(append_pos < context_pos, "{prompt}");

            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    /// `--no-context-files` still suppresses `<project_context>` when a
    /// custom prompt is also in play.
    #[test]
    fn compose_no_context_files_suppressed_with_custom_prompt() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir("cwd");
            std::fs::write(cwd.join("AGENTS.md"), "PROJECT RULES").unwrap();

            let overrides =
                PromptOverrides::from_cli(Some("You are Bob".to_string()), vec![], true);
            let prompt = compose_system_prompt(&cwd, &tools(), &[], true, &overrides);

            assert!(prompt.starts_with("You are Bob"), "{prompt}");
            assert!(!prompt.contains("<project_context>"), "{prompt}");
            assert!(!prompt.contains("PROJECT RULES"), "{prompt}");

            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    /// `Agent.prompt_overrides` survives `hydrate_resumed` — the exact
    /// value passed in is what ends up stored on the agent, so `/reload`
    /// recomposes from the same policy.
    #[test]
    fn hydrate_resumed_stores_prompt_overrides_on_agent() {
        use crate::agent::loop_::HooksConfig;
        use crate::agent::permission::PermissionGate;
        use crate::provider::openai::OpenAiProvider;

        with_empty_global_home(|_home| {
            let cwd = tmpdir("cwd");
            let (path, _hdr) =
                crate::session::new_session(&cwd, "m", "http://x").expect("new session");
            let mut agent = Agent::load_session(&path, &cwd).expect("load");

            let overrides =
                PromptOverrides::from_cli(Some("You are Bob".to_string()), vec![], true);

            let _ = agent.hydrate_resumed(
                Box::new(OpenAiProvider::new("", "", "")),
                ToolRegistry::standard(),
                PermissionGate::from_cli(false, None),
                HooksConfig::default(),
                "m".into(),
                "http://x".into(),
                "".into(),
                SkillLoadPolicy::default(),
                false,
                overrides.clone(),
            );

            assert_eq!(
                agent.prompt_overrides, overrides,
                "hydrate_resumed must store the passed-in prompt_overrides on the agent"
            );

            std::fs::remove_dir_all(&cwd).ok();
        });
    }
}

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
use crate::agent::hook::HookConfig;
use crate::agent::loop_::{Agent, AgentError, HooksConfig, Provider};
use crate::agent::permission::PermissionGate;
use crate::event::Usage;
use crate::resources::{
    LoadSkillsOptions, LoadSkillsResult, Skill, SkillDiagnostic, format_skills_for_prompt,
    load_skills,
};
use crate::tool::ToolRegistry;
use crate::util::uuid;

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
    pub session_id: uuid::Uuid,
    pub permission: PermissionGate,
    pub hooks: HooksConfig,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub skill_load: SkillLoadPolicy,
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
        } = inputs;

        let tool_names = registry.names();
        let LoadSkillsResult { skills, diagnostics } = load_skills(skill_load.into_options());
        let prompt = compose_system_prompt(&cwd, &tool_names, &skills);

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
    ) -> Vec<SkillDiagnostic> {
        self.provider = provider;
        self.registry = registry;
        self.permission = permission;
        self.hooks = hooks;
        self.model = model;
        self.base_url = base_url;
        self.api_key = api_key;

        let LoadSkillsResult { skills, diagnostics } = load_skills(skill_load.into_options());
        self.skills = skills;

        if self.context.system.is_none() {
            let tool_names = self.registry.names();
            self.context.system = Some(compose_system_prompt(&self.cwd, &tool_names, &self.skills));
        }

        diagnostics
    }
}

/// System prompt with skills block appended when the `read` tool is
/// present. This mirrors PI's `buildSystemPrompt` end-of-function
/// (`system-prompt.ts:154-157`).
pub fn compose_system_prompt(cwd: &Path, tool_names: &[String], skills: &[Skill]) -> String {
    let mut prompt = crate::agent::system_prompt::build(cwd, tool_names);
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
#[allow(dead_code)]
fn _keep_types_in_scope(_: HookConfig) {}
#[allow(dead_code)]
fn _keep_error_in_scope(e: AgentError) -> AgentError { e }

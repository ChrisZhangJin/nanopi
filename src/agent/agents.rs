//! Agent definition discovery — mirrors PI's `agents.ts`.
//!
//! An *agent definition* is a markdown file with YAML frontmatter:
//!
//! ```text
//! ---
//! name: scout
//! description: Fast codebase recon
//! tools: read, grep, find, ls, bash   # comma string OR yaml array [read, grep]
//! model: some-model-id                # optional
//! ---
//! System prompt body goes here (everything after frontmatter).
//! ```
//!
//! Discovery locations mirror skills:
//! - user-level `~/.nanopi/agents/*.md` (via [`crate::paths::user_agents_dir`])
//! - project-level: nearest `.nanopi/agents/` walking up from cwd
//!
//! Frontmatter is parsed by reusing the skill loader's
//! [`crate::resources::split_frontmatter`] and
//! [`crate::resources::parse_flat_frontmatter`] — no new YAML crate.

use std::path::{Path, PathBuf};

use crate::resources::{parse_flat_frontmatter, split_frontmatter};

/// Where an agent definition was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    User,
    Project,
}

/// Which discovery locations to consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    /// Only the user dir (`~/.nanopi/agents`).
    User,
    /// Only the nearest project dir (`.nanopi/agents`).
    Project,
    /// User then project; project overrides user on name collision.
    Both,
}

/// A parsed agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    /// Allowed tools. `None` when unspecified/empty.
    pub tools: Option<Vec<String>>,
    /// Model id override. `None` when unspecified.
    pub model: Option<String>,
    /// Everything after the frontmatter.
    pub system_prompt: String,
    pub source: AgentSource,
    pub file_path: PathBuf,
}

/// Result of [`discover_agents`].
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub agents: Vec<AgentConfig>,
    /// The project agents dir that was used (if any was found).
    pub project_agents_dir: Option<PathBuf>,
}

/// Walk up from `cwd` to the filesystem root looking for the nearest
/// `.nanopi/agents` directory. Returns the first one that exists.
pub fn find_nearest_project_agents_dir(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let candidate = d.join(".nanopi").join("agents");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Parse a single agent `.md` file. Returns `None` if it lacks a valid
/// `name` AND `description`, or cannot be read. A malformed file never
/// panics — it is simply skipped.
fn parse_agent_file(path: &Path, source: AgentSource) -> Option<AgentConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm_text, body) = split_frontmatter(&content);
    let fm = parse_flat_frontmatter(&fm_text);

    let name = fm.get("name").map(|s| s.trim()).filter(|s| !s.is_empty())?;
    let description = fm
        .get("description")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())?;

    Some(AgentConfig {
        name: name.to_string(),
        description: description.to_string(),
        tools: fm.get("tools").and_then(|s| normalize_tools(s)),
        model: fm
            .get("model")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        system_prompt: body,
        source,
        file_path: path.to_path_buf(),
    })
}

/// Normalize a `tools` value that may be either a comma-separated string
/// or a YAML-style array (`[read, grep]`). Trims entries, drops empties.
/// Returns `None` when nothing remains.
fn normalize_tools(raw: &str) -> Option<Vec<String>> {
    let inner = raw.trim();
    // Strip surrounding `[...]` if present (yaml inline array).
    let inner = inner
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(inner);

    let tools: Vec<String> = inner
        .split(',')
        .map(|t| t.trim().trim_matches(|c| c == '"' || c == '\'').trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    if tools.is_empty() {
        None
    } else {
        Some(tools)
    }
}

/// Load every valid `*.md` agent definition from `dir`. Missing dir →
/// empty vec. Malformed files are skipped, not fatal.
fn load_agents_from_dir(dir: &Path, source: AgentSource) -> Vec<AgentConfig> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .map(|e| e.eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
        })
        .collect();
    // Deterministic order.
    paths.sort();
    for path in paths {
        if let Some(cfg) = parse_agent_file(&path, source) {
            out.push(cfg);
        }
    }
    out
}

/// Discover agent definitions for the given `scope`.
///
/// - [`AgentScope::User`] loads only `~/.nanopi/agents`.
/// - [`AgentScope::Project`] loads only the nearest `.nanopi/agents`.
/// - [`AgentScope::Both`] loads user then project; a project agent
///   overrides a user agent with the same `name`.
pub fn discover_agents(cwd: &Path, scope: AgentScope) -> DiscoveryResult {
    let want_user = matches!(scope, AgentScope::User | AgentScope::Both);
    let want_project = matches!(scope, AgentScope::Project | AgentScope::Both);

    let mut agents: Vec<AgentConfig> = Vec::new();

    if want_user {
        if let Some(dir) = crate::paths::user_agents_dir() {
            agents.extend(load_agents_from_dir(&dir, AgentSource::User));
        }
    }

    let mut project_agents_dir = None;
    if want_project {
        if let Some(dir) = find_nearest_project_agents_dir(cwd) {
            let project = load_agents_from_dir(&dir, AgentSource::Project);
            project_agents_dir = Some(dir);
            // Project overrides user on name collision.
            for cfg in project {
                if let Some(existing) = agents.iter_mut().find(|a| a.name == cfg.name) {
                    *existing = cfg;
                } else {
                    agents.push(cfg);
                }
            }
        }
    }

    DiscoveryResult {
        agents,
        project_agents_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-agents-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_agent(dir: &Path, file: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(file);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn parses_valid_agent() {
        let dir = tmp();
        let p = write_agent(
            &dir,
            "scout.md",
            "---\nname: scout\ndescription: Fast recon\ntools: read, grep\nmodel: m1\n---\nYou are scout.\n",
        );
        let cfg = parse_agent_file(&p, AgentSource::User).unwrap();
        assert_eq!(cfg.name, "scout");
        assert_eq!(cfg.description, "Fast recon");
        assert_eq!(cfg.tools, Some(vec!["read".into(), "grep".into()]));
        assert_eq!(cfg.model, Some("m1".into()));
        assert_eq!(cfg.system_prompt, "You are scout.");
        assert_eq!(cfg.source, AgentSource::User);
    }

    #[test]
    fn tools_string_vs_array() {
        let s = normalize_tools("read, grep, find").unwrap();
        assert_eq!(s, vec!["read", "grep", "find"]);
        let a = normalize_tools("[read, grep]").unwrap();
        assert_eq!(a, vec!["read", "grep"]);
        // quotes + empties dropped
        let q = normalize_tools("[\"read\", '', grep, ]").unwrap();
        assert_eq!(q, vec!["read", "grep"]);
        assert_eq!(normalize_tools("   "), None);
    }

    #[test]
    fn missing_name_or_description_skipped() {
        let dir = tmp();
        let no_name = write_agent(&dir, "a.md", "---\ndescription: x\n---\nbody\n");
        assert!(parse_agent_file(&no_name, AgentSource::User).is_none());
        let no_desc = write_agent(&dir, "b.md", "---\nname: a\n---\nbody\n");
        assert!(parse_agent_file(&no_desc, AgentSource::User).is_none());
    }

    #[test]
    fn malformed_file_does_not_abort_discovery() {
        let dir = tmp();
        // one good, one malformed (no frontmatter at all)
        write_agent(
            &dir,
            "good.md",
            "---\nname: good\ndescription: ok\n---\nbody\n",
        );
        write_agent(&dir, "bad.md", "not frontmatter, just text\n");
        let agents = load_agents_from_dir(&dir, AgentSource::Project);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "good");
    }

    #[test]
    fn project_overrides_user_in_both_scope() {
        let root = tmp();
        // set NANOPI_HOME so user dir is isolated
        let home = root.join("home");
        std::env::set_var("NANOPI_HOME", &home);
        let user_dir = home.join("agents");
        write_agent(
            &user_dir,
            "scout.md",
            "---\nname: scout\ndescription: user version\n---\nuser body\n",
        );
        write_agent(
            &user_dir,
            "solo.md",
            "---\nname: solo\ndescription: only user\n---\nb\n",
        );

        let cwd = root.join("proj");
        let proj_dir = cwd.join(".nanopi").join("agents");
        write_agent(
            &proj_dir,
            "scout.md",
            "---\nname: scout\ndescription: project version\n---\nproject body\n",
        );

        let result = discover_agents(&cwd, AgentScope::Both);
        std::env::remove_var("NANOPI_HOME");

        let scout = result
            .agents
            .iter()
            .find(|a| a.name == "scout")
            .expect("scout present");
        assert_eq!(scout.description, "project version");
        assert_eq!(scout.source, AgentSource::Project);
        // user-only agent survives
        assert!(result.agents.iter().any(|a| a.name == "solo"));
        assert_eq!(result.project_agents_dir, Some(proj_dir));
    }
}

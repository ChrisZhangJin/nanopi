//! `--system-prompt` / `--append-system-prompt` resolution and
//! `SYSTEM.md` / `APPEND_SYSTEM.md` file discovery.
//!
//! Ported from PI's flag definitions
//! (`pi/packages/coding-agent/src/cli/args.ts:95-99`), its path-or-text
//! resolver `resolvePromptInput`
//! (`.../core/resource-loader.ts:53-68`), its two-scope file discovery
//! (`.../core/resource-loader.ts:1022-1048`) where a CLI flag suppresses
//! discovery entirely rather than merging with it (`??` at `:525`), and
//! the composition order in `.../core/system-prompt.ts:44-71` (custom
//! prompt still gets context files, skills, and the cwd line appended
//! after it — see `build.rs::compose_system_prompt` for that seam).
//!
//! nanopi-specific divergences from PI:
//! - Paths are nanopi-native: project scope is `<cwd>/.nanopi/SYSTEM.md`
//!   / `<cwd>/.nanopi/APPEND_SYSTEM.md`; global scope is
//!   `<nanopi_home>/SYSTEM.md` / `APPEND_SYSTEM.md` (via
//!   [`crate::paths::nanopi_home`], which honors `NANOPI_HOME` for test
//!   isolation). PI uses its own directory layout.
//! - The PROJECT file is gated on `project_trusted` — same threat model
//!   as project skills ([`crate::agent::build::SkillLoadPolicy::from_cli`])
//!   and PI's own `isProjectTrusted()` gate: a project-local `SYSTEM.md`
//!   shipped inside a cloned repo is arbitrary influence over the agent's
//!   highest-authority instructions, so it only applies once the user has
//!   trusted the project (via `-a` or a persisted decision). The GLOBAL
//!   file carries no such gate — it is the user's own machine-wide
//!   config, equivalent to editing `~/.nanopi/config.toml` by hand.
//!
//! Deliberate non-goal, and why: this module adds NO `config.toml`
//! fields. The two discoverable files already reproduce nanopi's
//! `api_kind`-style precedence ladder — CLI flag beats project
//! `.nanopi/SYSTEM.md` beats global `~/.nanopi/SYSTEM.md` — one tier per
//! filesystem location. A TOML string field would add a fourth tier
//! whose only distinguishing feature is worse ergonomics for the
//! multi-line, markdown-ish text a system prompt actually is. PI ships
//! no config field for this either.

use std::path::Path;

/// Project-scope filename for the replacement prompt.
const SYSTEM_FILE: &str = "SYSTEM.md";
/// Project-scope filename for the appended text.
const APPEND_FILE: &str = "APPEND_SYSTEM.md";

/// Unresolved policy: what `--system-prompt` / `--append-system-prompt`
/// asked for, plus whether project-local files may be read at all.
/// Cheap, `Clone + Default`, stored on `Agent` / `App` exactly like
/// [`crate::agent::build::SkillLoadPolicy`] so `/reload` and TUI
/// rebuilds (`/new`, `/fork`, `/model`, `/resume`) recompose the prompt
/// from the same policy rather than a stale resolved string — storing
/// the UNRESOLVED policy is what lets `/reload` pick up edits to an
/// on-disk `SYSTEM.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptOverrides {
    system_prompt: Option<String>,
    append_system_prompt: Vec<String>,
    project_trusted: bool,
}

impl PromptOverrides {
    /// Build the policy from CLI args + the trust decision for `cwd`.
    /// Resolution against the filesystem is deferred to [`Self::resolve`].
    pub fn from_cli(
        system_prompt: Option<String>,
        append_system_prompt: Vec<String>,
        project_trusted: bool,
    ) -> Self {
        Self {
            system_prompt,
            append_system_prompt,
            project_trusted,
        }
    }

    /// Resolve against the filesystem NOW: reads flag values as
    /// path-or-text, and — only for whichever of `custom`/`append` has
    /// no flag value — discovers the matching file. A flag present
    /// SUPPRESSES discovery for that slot entirely (no merge), matching
    /// PI's `?? discoverSystemPromptFile()`.
    pub fn resolve(&self, cwd: &Path) -> ResolvedPrompt {
        let custom = match &self.system_prompt {
            Some(raw) => Some(resolve_prompt_input(raw)),
            None => discover_file(cwd, SYSTEM_FILE, self.project_trusted),
        };

        let append = if self.append_system_prompt.is_empty() {
            discover_file(cwd, APPEND_FILE, self.project_trusted)
        } else {
            // Each value is independently path-or-text resolved, then
            // joined with a blank line (PI's join in resource-loader.ts).
            let parts: Vec<String> = self
                .append_system_prompt
                .iter()
                .map(|s| resolve_prompt_input(s))
                .collect();
            Some(parts.join("\n\n"))
        };

        ResolvedPrompt {
            custom: non_empty(custom),
            append: non_empty(append),
        }
    }
}

/// Resolved TEXT, ready for `compose_system_prompt` to splice in.
#[derive(Debug, Clone, Default)]
pub struct ResolvedPrompt {
    pub custom: Option<String>,
    pub append: Option<String>,
}

/// Empty/whitespace-only text becomes `None` so a stray empty
/// `SYSTEM.md` (or an `--append-system-prompt ""`) can't silently blank
/// out part of the prompt (threat T-kft-04: DoS via empty override).
fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|t| !t.trim().is_empty())
}

/// Path-or-text resolution: if `input` names an existing filesystem
/// entry, read it as text; otherwise treat `input` itself as the
/// literal prompt text. An existing entry that can't be read as text
/// (e.g. a directory, or a permissions error) warns on stderr and
/// falls back to using `input` verbatim — a prompt that happens to look
/// like a path must still work, exactly as PI's `resolvePromptInput`
/// does.
fn resolve_prompt_input(input: &str) -> String {
    let path = Path::new(input);
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(content) => return content,
            Err(e) => {
                eprintln!(
                    "warning: '{input}' exists but could not be read as text ({e}); \
                     using it as literal prompt text instead"
                );
                return input.to_string();
            }
        }
    }
    input.to_string()
}

/// Discover `filename` in project-then-global scope. Project scope
/// (`<cwd>/.nanopi/<filename>`) is gated on `project_trusted`; global
/// scope (`<nanopi_home>/<filename>`) is not — see the module doc for
/// why. First hit wins; missing/unreadable files are simply skipped
/// (not an error — absence is the common case).
fn discover_file(cwd: &Path, filename: &str, project_trusted: bool) -> Option<String> {
    if project_trusted {
        let project_path = cwd.join(".nanopi").join(filename);
        if let Ok(content) = std::fs::read_to_string(&project_path) {
            return Some(content);
        }
    }
    if let Some(home) = crate::paths::nanopi_home() {
        let global_path = home.join(filename);
        if let Ok(content) = std::fs::read_to_string(&global_path) {
            return Some(content);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-promptoverride-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Point `NANOPI_HOME` at a fresh empty temp dir for the duration of
    /// `f`, restoring the previous value afterward. Takes `TEST_LOCK` so
    /// concurrent tests don't race on the shared env var — mirrors
    /// `build.rs::compose_injects_cwd_context_file`.
    fn with_empty_global_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _g = crate::TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NANOPI_HOME");
        let home = tmpdir();
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

    #[test]
    fn resolve_prompt_input_literal_text_when_no_such_path() {
        assert_eq!(resolve_prompt_input("You are Bob"), "You are Bob");
    }

    #[test]
    fn resolve_prompt_input_reads_existing_file() {
        let dir = tmpdir();
        let p = dir.join("p.md");
        std::fs::write(&p, "from file").unwrap();
        assert_eq!(resolve_prompt_input(p.to_str().unwrap()), "from file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_prompt_input_falls_back_when_existing_path_unreadable_as_text() {
        // A directory named p.md exists but can't be read_to_string'd.
        let dir = tmpdir();
        let p = dir.join("p.md");
        std::fs::create_dir_all(&p).unwrap();
        let raw = p.to_str().unwrap();
        assert_eq!(resolve_prompt_input(raw), raw);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_resolve_with_empty_home_and_no_project_files_is_none() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir();
            let resolved = PromptOverrides::default().resolve(&cwd);
            assert!(resolved.custom.is_none());
            assert!(resolved.append.is_none());
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn global_only_system_md_is_discovered() {
        with_empty_global_home(|home| {
            std::fs::write(home.join(SYSTEM_FILE), "global sys").unwrap();
            let cwd = tmpdir();
            let resolved = PromptOverrides::default().resolve(&cwd);
            assert_eq!(resolved.custom, Some("global sys".to_string()));
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn project_system_md_beats_global_when_trusted() {
        with_empty_global_home(|home| {
            std::fs::write(home.join(SYSTEM_FILE), "global sys").unwrap();
            let cwd = tmpdir();
            let project_dir = cwd.join(".nanopi");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::write(project_dir.join(SYSTEM_FILE), "proj sys").unwrap();

            let overrides = PromptOverrides::from_cli(None, vec![], true);
            let resolved = overrides.resolve(&cwd);
            assert_eq!(resolved.custom, Some("proj sys".to_string()));
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn untrusted_project_system_md_is_never_read_falls_back_to_global() {
        with_empty_global_home(|home| {
            std::fs::write(home.join(SYSTEM_FILE), "global sys").unwrap();
            let cwd = tmpdir();
            let project_dir = cwd.join(".nanopi");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::write(project_dir.join(SYSTEM_FILE), "proj sys").unwrap();

            let overrides = PromptOverrides::from_cli(None, vec![], false);
            let resolved = overrides.resolve(&cwd);
            assert_eq!(
                resolved.custom,
                Some("global sys".to_string()),
                "untrusted project file must never be read"
            );
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn global_file_needs_no_trust_gate() {
        with_empty_global_home(|home| {
            std::fs::write(home.join(SYSTEM_FILE), "global sys").unwrap();
            let cwd = tmpdir();
            // No project .nanopi dir at all.
            let overrides = PromptOverrides::from_cli(None, vec![], false);
            let resolved = overrides.resolve(&cwd);
            assert_eq!(resolved.custom, Some("global sys".to_string()));
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn append_discovery_follows_same_two_scope_rule() {
        with_empty_global_home(|home| {
            std::fs::write(home.join(APPEND_FILE), "global append").unwrap();
            let cwd = tmpdir();
            let project_dir = cwd.join(".nanopi");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::write(project_dir.join(APPEND_FILE), "proj append").unwrap();

            let trusted = PromptOverrides::from_cli(None, vec![], true).resolve(&cwd);
            assert_eq!(trusted.append, Some("proj append".to_string()));

            let untrusted = PromptOverrides::from_cli(None, vec![], false).resolve(&cwd);
            assert_eq!(untrusted.append, Some("global append".to_string()));

            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn cli_flag_wins_over_files_and_file_not_consulted() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir();
            let project_dir = cwd.join(".nanopi");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::write(project_dir.join(SYSTEM_FILE), "proj sys").unwrap();

            let overrides = PromptOverrides::from_cli(Some("flag text".to_string()), vec![], true);
            let resolved = overrides.resolve(&cwd);
            assert_eq!(resolved.custom, Some("flag text".to_string()));
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn repeatable_append_joins_with_blank_line() {
        with_empty_global_home(|_home| {
            let cwd = tmpdir();
            let overrides =
                PromptOverrides::from_cli(None, vec!["A".to_string(), "B".to_string()], false);
            let resolved = overrides.resolve(&cwd);
            assert_eq!(resolved.append, Some("A\n\nB".to_string()));
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn cli_append_flag_suppresses_append_file_discovery() {
        with_empty_global_home(|home| {
            std::fs::write(home.join(APPEND_FILE), "global append").unwrap();
            let cwd = tmpdir();
            let overrides = PromptOverrides::from_cli(None, vec!["flag append".to_string()], false);
            let resolved = overrides.resolve(&cwd);
            assert_eq!(resolved.append, Some("flag append".to_string()));
            std::fs::remove_dir_all(&cwd).ok();
        });
    }

    #[test]
    fn empty_whitespace_only_resolved_text_becomes_none() {
        with_empty_global_home(|home| {
            std::fs::write(home.join(SYSTEM_FILE), "   \n\t  ").unwrap();
            let cwd = tmpdir();
            let resolved = PromptOverrides::default().resolve(&cwd);
            assert!(resolved.custom.is_none());
            std::fs::remove_dir_all(&cwd).ok();
        });
    }
}

//! Settings file loader — hooks configuration.
//!
//! v0.6+: hooks now primarily live in `config.toml` alongside CLI
//! defaults. `settings.toml` is still read for backward compatibility;
//! its hooks are appended to whatever `config.toml` declared.
//!
//! Load order for hooks (all are optional, hooks are cumulative):
//!   1. `~/.nanopi/config.toml`   [hooks.*] section        (new primary)
//!   2. `./.nanopi/config.toml`   [hooks.*] section        (new primary)
//!   3. `~/.nanopi/settings.toml` [[hooks.*]]              (legacy)
//!   4. `./.nanopi/settings.toml` [[hooks.*]]              (legacy)

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::agent::hook::HookConfig;
use crate::agent::loop_::HooksConfig;

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid hook matcher: {0}")]
    Matcher(String),
}

/// Top-level settings.toml schema.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(default)]
    pub hooks: HooksSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HooksSection {
    #[serde(default)]
    pub tool_execution_start: Vec<HookConfig>,
    #[serde(default)]
    pub tool_execution_end: Vec<HookConfig>,
    #[serde(default)]
    pub input: Vec<HookConfig>,
    #[serde(default)]
    pub session_start: Vec<HookConfig>,
    #[serde(default)]
    pub session_shutdown: Vec<HookConfig>,
    /// NEW v0.11.0 lifecycle hooks (see docs/pi-vs-nanopi.md §4.3).
    #[serde(default)]
    pub before_agent_start: Vec<HookConfig>,
    #[serde(default)]
    pub turn_start: Vec<HookConfig>,
    #[serde(default)]
    pub turn_end: Vec<HookConfig>,
    #[serde(default)]
    pub message_end: Vec<HookConfig>,
    /// v0.11.0: compaction lifecycle hooks.
    #[serde(default)]
    pub session_before_compact: Vec<HookConfig>,
    #[serde(default)]
    pub session_compact: Vec<HookConfig>,
}

impl From<Settings> for HooksConfig {
    fn from(s: Settings) -> Self {
        HooksConfig {
            tool_execution_start: s.hooks.tool_execution_start,
            tool_execution_end: s.hooks.tool_execution_end,
            input: s.hooks.input,
            session_start: s.hooks.session_start,
            session_shutdown: s.hooks.session_shutdown,
            before_agent_start: s.hooks.before_agent_start,
            turn_start: s.hooks.turn_start,
            turn_end: s.hooks.turn_end,
            message_end: s.hooks.message_end,
            session_before_compact: s.hooks.session_before_compact,
            session_compact: s.hooks.session_compact,
        }
    }
}

/// Load hooks from config.toml (primary, v0.6+) and settings.toml
/// (legacy, backward-compat). Hooks are cumulative — all matching hooks
/// from all four sources fire in the order they were declared.
pub fn load_settings(cwd: &Path) -> Result<HooksConfig, SettingsError> {
    let mut hooks = HooksConfig::default();

    // v0.6+: hooks in config.toml (global + local) come first.
    match crate::config::load_config(cwd) {
        Ok(cfg) => {
            hooks.tool_execution_start.extend(cfg.hooks.tool_execution_start);
            hooks.tool_execution_end.extend(cfg.hooks.tool_execution_end);
            hooks
                .input
                .extend(cfg.hooks.input);
            hooks.session_start.extend(cfg.hooks.session_start);
            hooks.session_shutdown.extend(cfg.hooks.session_shutdown);
            hooks.before_agent_start.extend(cfg.hooks.before_agent_start);
            hooks.turn_start.extend(cfg.hooks.turn_start);
            hooks.turn_end.extend(cfg.hooks.turn_end);
            hooks.message_end.extend(cfg.hooks.message_end);
            hooks
                .session_before_compact
                .extend(cfg.hooks.session_before_compact);
            hooks.session_compact.extend(cfg.hooks.session_compact);
        }
        Err(e) => {
            // config.toml parse errors surface as SettingsError so mode
            // code doesn't need a separate handler.
            return Err(SettingsError::Matcher(format!("config.toml: {e}")));
        }
    }

    // Legacy: settings.toml (global + local) hooks append.
    let global_path = global_settings_path();
    if let Some(p) = global_path {
        if p.exists() {
            let s = load_one(&p)?;
            hooks.tool_execution_start.extend(s.hooks.tool_execution_start);
            hooks.tool_execution_end.extend(s.hooks.tool_execution_end);
            hooks.input.extend(s.hooks.input);
            hooks.session_start.extend(s.hooks.session_start);
            hooks.session_shutdown.extend(s.hooks.session_shutdown);
            hooks.before_agent_start.extend(s.hooks.before_agent_start);
            hooks.turn_start.extend(s.hooks.turn_start);
            hooks.turn_end.extend(s.hooks.turn_end);
            hooks.message_end.extend(s.hooks.message_end);
            hooks
                .session_before_compact
                .extend(s.hooks.session_before_compact);
            hooks.session_compact.extend(s.hooks.session_compact);
        }
    }

    let local_path = cwd.join(".nanopi").join("settings.toml");
    if local_path.exists() {
        let s = load_one(&local_path)?;
        hooks.tool_execution_start.extend(s.hooks.tool_execution_start);
        hooks.tool_execution_end.extend(s.hooks.tool_execution_end);
        hooks.input.extend(s.hooks.input);
        hooks.session_start.extend(s.hooks.session_start);
        hooks.session_shutdown.extend(s.hooks.session_shutdown);
        hooks.before_agent_start.extend(s.hooks.before_agent_start);
        hooks.turn_start.extend(s.hooks.turn_start);
        hooks.turn_end.extend(s.hooks.turn_end);
        hooks.message_end.extend(s.hooks.message_end);
        // The compaction hooks were added to the other three sources but
        // missed here, so a project-level settings.toml declaring them
        // parsed clean and then silently never fired. Every source must
        // read every field or the omission looks like a user error.
        hooks
            .session_before_compact
            .extend(s.hooks.session_before_compact);
        hooks.session_compact.extend(s.hooks.session_compact);
    }

    // Validate regex matchers up front; surface errors at startup.
    crate::agent::hook::validate_hooks(&hooks.tool_execution_start).map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.tool_execution_end).map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.input)
        .map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.session_start).map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.session_shutdown).map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.before_agent_start)
        .map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.turn_start).map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.turn_end).map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.message_end).map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.session_before_compact)
        .map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.session_compact).map_err(SettingsError::Matcher)?;

    Ok(hooks)
}

pub fn global_settings_path() -> Option<PathBuf> {
    crate::paths::global_settings_path()
}

fn load_one(path: &Path) -> Result<Settings, SettingsError> {
    let text = std::fs::read_to_string(path).map_err(|e| SettingsError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str(&text).map_err(|e| SettingsError::Toml {
        path: path.to_path_buf(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests in this module mutate $NANOPI_HOME; acquire the process-wide
    // test lock (defined in lib.rs) so session tests don't race.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-settings-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_files_returns_empty() {
        let _guard = lock();
        // Point NANOPI_HOME at a path that doesn't exist (so global
        // settings.toml is missing) and cwd at another missing path
        // (so local settings.toml is missing too).
        let tmp_home = tmp();
        let tmp_cwd = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &tmp_home);

        let h = load_settings(&tmp_cwd).unwrap();
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&tmp_home);
        let _ = std::fs::remove_dir_all(&tmp_cwd);

        assert!(h.tool_execution_start.is_empty());
        assert!(h.tool_execution_end.is_empty());
    }

    #[test]
    fn loads_tool_execution_start() {
        let _guard = lock();
        let dir = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &dir);
        std::fs::write(
            dir.join("settings.toml"),
            r#"
[[hooks.tool_execution_start]]
matcher = "bash"
type = "command"
command = "/bin/true"
timeout = 1000

[[hooks.tool_execution_end]]
matcher = "*"
type = "command"
command = "/bin/true"
timeout = 2000
"#,
        )
        .unwrap();

        let h = load_settings(&PathBuf::from("/tmp")).unwrap();
        assert_eq!(h.tool_execution_start.len(), 1);
        assert_eq!(h.tool_execution_start[0].matcher, "bash");
        assert_eq!(h.tool_execution_end.len(), 1);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_toml_hooks_and_legacy_settings_toml_hooks_both_load() {
        let _guard = lock();
        let dir = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &dir);

        // Legacy settings.toml — one tool_execution_start hook.
        std::fs::write(
            dir.join("settings.toml"),
            r#"
[[hooks.tool_execution_start]]
matcher = "legacy"
type = "command"
command = "/bin/true"
timeout = 1000
"#,
        )
        .unwrap();
        // New config.toml — one tool_execution_start hook plus a session_start.
        std::fs::write(
            dir.join("config.toml"),
            r#"
[[hooks.tool_execution_start]]
matcher = "modern"
type = "command"
command = "/bin/true"
timeout = 1000

[[hooks.session_start]]
matcher = "*"
type = "command"
command = "/bin/true"
"#,
        )
        .unwrap();

        let h = load_settings(&PathBuf::from("/tmp")).unwrap();

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);

        // Both hooks present, config.toml first, settings.toml appended.
        assert_eq!(h.tool_execution_start.len(), 2);
        assert_eq!(h.tool_execution_start[0].matcher, "modern");
        assert_eq!(h.tool_execution_start[1].matcher, "legacy");
        assert_eq!(h.session_start.len(), 1);
    }

    /// Regression: the project-level `.nanopi/settings.toml` branch
    /// stopped extending at `message_end`, so the two compaction hooks
    /// parsed without error and then never fired. Global sources are
    /// pointed at an empty NANOPI_HOME so the only hooks that can reach
    /// `load_settings` are the project-level ones this test wrote.
    #[test]
    fn project_settings_toml_loads_compaction_hooks() {
        let _guard = lock();
        let home = tmp();
        let cwd = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);

        std::fs::create_dir_all(cwd.join(".nanopi")).unwrap();
        std::fs::write(
            cwd.join(".nanopi").join("settings.toml"),
            r#"
[[hooks.session_before_compact]]
matcher = "*"
type = "command"
command = "/bin/true"
timeout = 1000

[[hooks.session_compact]]
matcher = "*"
type = "command"
command = "/bin/true"
timeout = 1000
"#,
        )
        .unwrap();

        let h = load_settings(&cwd).unwrap();

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);

        assert_eq!(h.session_before_compact.len(), 1);
        assert_eq!(h.session_before_compact[0].command, "/bin/true");
        assert_eq!(h.session_compact.len(), 1);
        assert_eq!(h.session_compact[0].command, "/bin/true");
    }

    #[test]
    fn invalid_matcher_is_error() {
        let _guard = lock();
        let dir = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &dir);
        std::fs::write(
            dir.join("settings.toml"),
            r#"
[[hooks.tool_execution_start]]
matcher = "[bad"
type = "command"
command = "/bin/true"
"#,
        )
        .unwrap();

        let r = load_settings(&dir);
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(r, Err(SettingsError::Matcher(_))), "got {r:?}");
    }
}

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
    pub pre_tool_use: Vec<HookConfig>,
    #[serde(default)]
    pub post_tool_use: Vec<HookConfig>,
    #[serde(default)]
    pub user_prompt_submit: Vec<HookConfig>,
    #[serde(default)]
    pub session_start: Vec<HookConfig>,
    #[serde(default)]
    pub session_end: Vec<HookConfig>,
}

impl From<Settings> for HooksConfig {
    fn from(s: Settings) -> Self {
        HooksConfig {
            pre_tool_use: s.hooks.pre_tool_use,
            post_tool_use: s.hooks.post_tool_use,
            user_prompt_submit: s.hooks.user_prompt_submit,
            session_start: s.hooks.session_start,
            session_end: s.hooks.session_end,
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
            hooks.pre_tool_use.extend(cfg.hooks.pre_tool_use);
            hooks.post_tool_use.extend(cfg.hooks.post_tool_use);
            hooks.user_prompt_submit.extend(cfg.hooks.user_prompt_submit);
            hooks.session_start.extend(cfg.hooks.session_start);
            hooks.session_end.extend(cfg.hooks.session_end);
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
            hooks.pre_tool_use.extend(s.hooks.pre_tool_use);
            hooks.post_tool_use.extend(s.hooks.post_tool_use);
            hooks.user_prompt_submit.extend(s.hooks.user_prompt_submit);
            hooks.session_start.extend(s.hooks.session_start);
            hooks.session_end.extend(s.hooks.session_end);
        }
    }

    let local_path = cwd.join(".nanopi").join("settings.toml");
    if local_path.exists() {
        let s = load_one(&local_path)?;
        hooks.pre_tool_use.extend(s.hooks.pre_tool_use);
        hooks.post_tool_use.extend(s.hooks.post_tool_use);
        hooks.user_prompt_submit.extend(s.hooks.user_prompt_submit);
        hooks.session_start.extend(s.hooks.session_start);
        hooks.session_end.extend(s.hooks.session_end);
    }

    // Validate regex matchers up front; surface errors at startup.
    crate::agent::hook::validate_hooks(&hooks.pre_tool_use)
        .map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.post_tool_use)
        .map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.user_prompt_submit)
        .map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.session_start)
        .map_err(SettingsError::Matcher)?;
    crate::agent::hook::validate_hooks(&hooks.session_end)
        .map_err(SettingsError::Matcher)?;

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

        assert!(h.pre_tool_use.is_empty());
        assert!(h.post_tool_use.is_empty());
    }

    #[test]
    fn loads_pre_tool_use() {
        let _guard = lock();
        let dir = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &dir);
        std::fs::write(
            dir.join("settings.toml"),
            r#"
[[hooks.pre_tool_use]]
matcher = "bash"
type = "command"
command = "/bin/true"
timeout = 1000

[[hooks.post_tool_use]]
matcher = "*"
type = "command"
command = "/bin/true"
timeout = 2000
"#,
        )
        .unwrap();

        let h = load_settings(&PathBuf::from("/tmp")).unwrap();
        assert_eq!(h.pre_tool_use.len(), 1);
        assert_eq!(h.pre_tool_use[0].matcher, "bash");
        assert_eq!(h.post_tool_use.len(), 1);

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

        // Legacy settings.toml — one pre_tool_use hook.
        std::fs::write(
            dir.join("settings.toml"),
            r#"
[[hooks.pre_tool_use]]
matcher = "legacy"
type = "command"
command = "/bin/true"
timeout = 1000
"#,
        )
        .unwrap();
        // New config.toml — one pre_tool_use hook plus a session_start.
        std::fs::write(
            dir.join("config.toml"),
            r#"
[[hooks.pre_tool_use]]
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
        assert_eq!(h.pre_tool_use.len(), 2);
        assert_eq!(h.pre_tool_use[0].matcher, "modern");
        assert_eq!(h.pre_tool_use[1].matcher, "legacy");
        assert_eq!(h.session_start.len(), 1);
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
[[hooks.pre_tool_use]]
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
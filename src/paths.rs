//! Shared filesystem-path helpers for `~/.nanopi`.
//!
//! Consolidates the `NANOPI_HOME` / `~/.nanopi` resolution that was
//! previously reimplemented in `config`, `settings`, `trust`,
//! `agent::permission`, and `mode::interactive`. The rule everywhere:
//! `NANOPI_HOME` (if set) is the config root — treat it as
//! equivalent-to `~/.nanopi`, NOT as a home dir. Otherwise fall back
//! to `$HOME/.nanopi`.
//!
//! Skill discovery adds two more locations, mirroring PI's model:
//! - user   `<nanopi_home>/skills`
//! - project `<cwd>/.nanopi/skills`

use std::path::{Path, PathBuf};

/// The nanopi config root. `NANOPI_HOME` overrides for test isolation.
/// Returns `None` only when both `NANOPI_HOME` is unset and no home
/// dir can be resolved.
pub fn nanopi_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NANOPI_HOME") {
        return Some(PathBuf::from(p));
    }
    Some(dirs::home_dir()?.join(".nanopi"))
}

/// The global `config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("config.toml"))
}

/// The global `settings.toml`.
pub fn global_settings_path() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("settings.toml"))
}

/// Rustyline history file.
pub fn history_path() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("history.txt"))
}

/// Directory holding `<cwd-key>=trusted|denied` markers.
pub fn trust_dir() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("trust"))
}

/// User-scope skills root.
pub fn user_skills_dir() -> Option<PathBuf> {
    nanopi_home().map(|h| h.join("skills"))
}

/// Project-scope skills root inside a given cwd.
pub fn project_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".nanopi").join("skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanopi_home_honors_env() {
        let _g = crate::TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", "/tmp/nanopi-test-home");

        assert_eq!(nanopi_home(), Some(PathBuf::from("/tmp/nanopi-test-home")));
        assert_eq!(
            global_config_path(),
            Some(PathBuf::from("/tmp/nanopi-test-home/config.toml"))
        );
        assert_eq!(
            user_skills_dir(),
            Some(PathBuf::from("/tmp/nanopi-test-home/skills"))
        );

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
    }

    #[test]
    fn project_skills_is_dot_nanopi_skills() {
        let d = project_skills_dir(Path::new("/tmp/proj"));
        assert_eq!(d, PathBuf::from("/tmp/proj/.nanopi/skills"));
    }
}

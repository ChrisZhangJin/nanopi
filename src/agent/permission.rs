//! Permission gate — `--yolo`, hook enable/disable, project trust.
//!
//! See `docs/v0.5-research.md` §4.4 for design.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Project resources loaded unconditionally.
    Trusted,
    /// Project resources skipped (no `.pi/` hooks/skills/etc.).
    Distrusted,
    /// User must be prompted on first encounter.
    Ask,
}

impl Default for TrustLevel {
    fn default() -> Self {
        TrustLevel::Ask
    }
}

/// Centralized decision-making for "should this hook fire?" and "should
/// the user be asked to confirm?". Built once per session.
#[derive(Debug, Clone)]
pub struct PermissionGate {
    pub yolo: bool,
    pub hooks_enabled: bool,
    pub project_trust: TrustLevel,
}

impl PermissionGate {
    pub fn new(yolo: bool, hooks_enabled: bool, project_trust: TrustLevel) -> Self {
        Self {
            yolo,
            hooks_enabled,
            project_trust,
        }
    }

    /// Build from CLI args + cwd. Default is Ask/hooks-on/yolo-off.
    pub fn from_cli(yolo: bool, no_hooks: bool, approve: Option<bool>) -> Self {
        let trust = match approve {
            Some(true) => TrustLevel::Trusted,
            Some(false) => TrustLevel::Distrusted,
            None => TrustLevel::Ask,
        };
        Self::new(yolo, !no_hooks, trust)
    }

    /// Decide whether a PreToolUse block decision should be honored.
    /// In yolo mode, blocks are ignored (logged only).
    pub fn should_honor_pretooluse_block(&self) -> bool {
        !self.yolo
    }

    /// Should we run any hooks at all?
    pub fn hooks_active(&self) -> bool {
        self.hooks_enabled
    }

    /// Should we even prompt the user about project trust?
    pub fn should_prompt_trust(&self) -> bool {
        !self.yolo && self.project_trust == TrustLevel::Ask
    }
}

/// Result of asking the user (in TUI) about trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustChoice {
    Trust,
    Distrust,
    SessionOnly,
}

/// Persist a trust choice to `~/.nanopi/trust/<encoded-cwd>=<value>`.
/// `SessionOnly` is NOT persisted (it's an in-memory flag).
pub fn persist_trust_choice(cwd: &Path, choice: TrustChoice) -> std::io::Result<()> {
    let home = match std::env::var_os("NANOPI_HOME").or_else(|| std::env::var_os("HOME")) {
        Some(h) => std::path::PathBuf::from(h),
        None => return Ok(()),
    };
    let dir = home.join("trust");
    std::fs::create_dir_all(&dir)?;
    let key = encode_cwd_key(cwd);
    match choice {
        TrustChoice::SessionOnly => Ok(()),
        TrustChoice::Trust => std::fs::write(dir.join(format!("{key}=trusted")), ""),
        TrustChoice::Distrust => std::fs::write(dir.join(format!("{key}=denied")), ""),
    }
}

/// Encode a cwd as a safe filename key (replace `/` and other separators).
fn encode_cwd_key(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_ask_hooks_on() {
        let p = PermissionGate::from_cli(false, false, None);
        assert!(!p.yolo);
        assert!(p.hooks_active());
        assert_eq!(p.project_trust, TrustLevel::Ask);
        assert!(p.should_prompt_trust());
    }

    #[test]
    fn yolo_skips_trust_prompt() {
        let p = PermissionGate::from_cli(true, false, None);
        assert!(p.yolo);
        assert!(!p.should_prompt_trust());
    }

    #[test]
    fn no_hooks_disables_hooks() {
        let p = PermissionGate::from_cli(false, true, None);
        assert!(!p.hooks_active());
    }

    #[test]
    fn approve_sets_trust_to_trusted() {
        let p = PermissionGate::from_cli(false, false, Some(true));
        assert_eq!(p.project_trust, TrustLevel::Trusted);
    }

    #[test]
    fn no_approve_sets_trust_to_distrusted() {
        let p = PermissionGate::from_cli(false, false, Some(false));
        assert_eq!(p.project_trust, TrustLevel::Distrusted);
    }

    #[test]
    fn yolo_ignores_pretooluse_block() {
        let p = PermissionGate::from_cli(true, false, None);
        assert!(!p.should_honor_pretooluse_block());
    }

    #[test]
    fn default_honors_block() {
        let p = PermissionGate::from_cli(false, false, None);
        assert!(p.should_honor_pretooluse_block());
    }

    #[test]
    fn encode_cwd_key_replaces_separators() {
        let key = encode_cwd_key(&Path::new("/home/user/project"));
        assert!(!key.contains('/'));
        assert!(key.contains("home"));
        assert!(key.contains("user"));
        assert!(key.contains("project"));
    }

    #[test]
    fn persist_and_session_only_is_noop() {
        let dir = std::env::temp_dir().join(format!("nanopi-trust-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &dir);

        let cwd = Path::new("/tmp/test/cwd");
        let r = persist_trust_choice(cwd, TrustChoice::SessionOnly);
        assert!(r.is_ok());
        // SessionOnly doesn't write any file.
        let trust_dir = dir.join("trust");
        assert!(!trust_dir.exists() || std::fs::read_dir(&trust_dir).unwrap().count() == 0);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
//! Project trust model.
//!
//! When a directory has a `.nanopi/` folder, the user is prompted on
//! first encounter whether to trust it. The choice is persisted to
//! `~/.nanopi/trust/<encoded-cwd>=<value>` (see `persist_trust_choice`).
//!
//! For v0.5, the TUI prompt logic is stubbed (returns Trusted by default
//! when --approve is set). The full TUI prompt UI is v0.6.

use std::path::{Path, PathBuf};

use crate::agent::permission::TrustChoice;

/// Outcome of checking trust for a cwd.
pub enum TrustStatus {
    AlreadyTrusted,
    AlreadyDistrusted,
    NeedsPrompt,
    /// No `.nanopi/` in cwd → no trust needed.
    NoProjectResources,
}

/// Check trust status for a cwd. Does NOT prompt — returns what to do.
pub fn check_trust_status(cwd: &Path) -> TrustStatus {
    let project_nanopi = cwd.join(".nanopi");
    if !project_nanopi.exists() {
        return TrustStatus::NoProjectResources;
    }
    let key = encode_cwd_key(cwd);
    let Some(trust_dir) = trust_dir() else {
        return TrustStatus::NeedsPrompt;
    };
    if trust_dir.join(format!("{key}=trusted")).exists() {
        TrustStatus::AlreadyTrusted
    } else if trust_dir.join(format!("{key}=denied")).exists() {
        TrustStatus::AlreadyDistrusted
    } else {
        TrustStatus::NeedsPrompt
    }
}

fn trust_dir() -> Option<PathBuf> {
    let home = std::env::var_os("NANOPI_HOME")
        .or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join("trust"))
}

fn encode_cwd_key(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

/// Re-export for backward compatibility with permission.rs callers.
pub use crate::agent::permission::persist_trust_choice;

#[allow(dead_code)]
pub(crate) fn _ensure_choice_in_scope(_: TrustChoice) {}
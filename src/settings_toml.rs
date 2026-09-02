//! Interaction settings loader/saver (spec §2.1 / §5.2).
//!
//! Shares the file `~/.nanopi/settings.toml` with the legacy hooks
//! loader in `crate::settings`. Both loaders use `#[serde(default)]`,
//! so a single TOML document with `[hooks]` + our new interaction
//! keys coexists cleanly. `save` uses `toml_edit` to preserve
//! comments, key order, and unknown sections on rewrite.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::permission::TrustLevel;
use crate::agent::thinking::ThinkingLevel;
use crate::keys::{ActionId, KeyBindings, KeySpec};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SettingsFile {
    pub thinking_level: Option<ThinkingLevel>,
    pub hide_thinking: Option<bool>,
    pub auto_compact: Option<bool>,
    pub default_project_trust: Option<TrustLevelSer>,
    #[serde(default)]
    pub keybindings: HashMap<ActionId, KeySpec>,
}

/// Serializable proxy for `TrustLevel` (which lacks serde derives —
/// don't want to churn permission.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevelSer {
    Ask,
    Trusted,
    Distrusted,
}

impl From<TrustLevel> for TrustLevelSer {
    fn from(t: TrustLevel) -> Self {
        match t {
            TrustLevel::Ask => TrustLevelSer::Ask,
            TrustLevel::Trusted => TrustLevelSer::Trusted,
            TrustLevel::Distrusted => TrustLevelSer::Distrusted,
        }
    }
}
impl From<TrustLevelSer> for TrustLevel {
    fn from(s: TrustLevelSer) -> Self {
        match s {
            TrustLevelSer::Ask => TrustLevel::Ask,
            TrustLevelSer::Trusted => TrustLevel::Trusted,
            TrustLevelSer::Distrusted => TrustLevel::Distrusted,
        }
    }
}

pub fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("NANOPI_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".nanopi")))?;
    Some(base.join("settings.toml"))
}

pub fn load() -> SettingsFile {
    let Some(path) = settings_path() else {
        return SettingsFile::default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return SettingsFile::default();
        }
        Err(e) => {
            eprintln!(
                "nanopi: settings.toml unreadable ({}): {}; using defaults",
                path.display(),
                e
            );
            return SettingsFile::default();
        }
    };
    match toml::from_str::<SettingsFile>(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "nanopi: settings.toml parse error ({}): {}; using defaults",
                path.display(),
                e
            );
            SettingsFile::default()
        }
    }
}

pub fn save(f: &SettingsFile) -> std::io::Result<()> {
    let Some(path) = settings_path() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot resolve NANOPI_HOME",
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Round-trip through `toml_edit` so existing comments / [hooks] /
    // key order in the file are preserved.
    let mut doc = match std::fs::read_to_string(&path) {
        Ok(t) => t
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_else(|_| toml_edit::DocumentMut::new()),
        Err(_) => toml_edit::DocumentMut::new(),
    };

    fn set_string_opt(doc: &mut toml_edit::DocumentMut, key: &str, val: Option<String>) {
        match val {
            Some(v) => {
                doc[key] = toml_edit::value(v);
            }
            None => {
                doc.remove(key);
            }
        }
    }
    fn set_bool_opt(doc: &mut toml_edit::DocumentMut, key: &str, val: Option<bool>) {
        match val {
            Some(v) => {
                doc[key] = toml_edit::value(v);
            }
            None => {
                doc.remove(key);
            }
        }
    }

    set_string_opt(&mut doc, "thinking_level", f.thinking_level.map(|l| l.to_string()));
    set_bool_opt(&mut doc, "hide_thinking", f.hide_thinking);
    set_bool_opt(&mut doc, "auto_compact", f.auto_compact);
    set_string_opt(
        &mut doc,
        "default_project_trust",
        f.default_project_trust.map(|t| match t {
            TrustLevelSer::Ask => "ask".to_string(),
            TrustLevelSer::Trusted => "trusted".to_string(),
            TrustLevelSer::Distrusted => "distrusted".to_string(),
        }),
    );

    // Keybindings — write only if we have any; write as `[keybindings]` table.
    if f.keybindings.is_empty() {
        doc.remove("keybindings");
    } else {
        let mut tbl = toml_edit::Table::new();
        // Sort keys for stable diffs.
        let mut entries: Vec<_> = f.keybindings.iter().collect();
        entries.sort_by_key(|(a, _)| format!("{:?}", a));
        for (action, spec) in entries {
            let key = action_toml_key(*action);
            tbl.insert(key, toml_edit::value(spec.to_string()));
        }
        doc["keybindings"] = toml_edit::Item::Table(tbl);
    }

    std::fs::write(&path, doc.to_string())
}

fn action_toml_key(a: ActionId) -> &'static str {
    match a {
        ActionId::ThinkingCycle => "thinking_cycle",
        ActionId::ToolCancel => "tool_cancel",
        ActionId::ExpandLastTool => "expand_last_tool",
        ActionId::NewlineInInput => "newline_in_input",
        ActionId::OpenSlashPalette => "open_slash_palette",
        ActionId::OpenSettings => "open_settings",
    }
}

/// Build a `KeyBindings` starting from defaults, overlaying any
/// user-provided entries.
pub fn bindings_from(file: &SettingsFile) -> KeyBindings {
    if file.keybindings.is_empty() {
        return KeyBindings::default();
    }
    KeyBindings::from_overrides(file.keybindings.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn load_missing_file_returns_default() {
        let _g = _lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NANOPI_HOME", tmp.path());
        let f = load();
        assert!(f.thinking_level.is_none());
        assert!(f.hide_thinking.is_none());
        assert!(f.keybindings.is_empty());
        std::env::remove_var("NANOPI_HOME");
    }

    #[test]
    fn save_and_load_roundtrip_scalars() {
        let _g = _lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NANOPI_HOME", tmp.path());

        let mut f = SettingsFile::default();
        f.thinking_level = Some(ThinkingLevel::High);
        f.hide_thinking = Some(true);
        f.auto_compact = Some(false);
        f.default_project_trust = Some(TrustLevelSer::Trusted);
        save(&f).unwrap();

        let loaded = load();
        assert_eq!(loaded.thinking_level, Some(ThinkingLevel::High));
        assert_eq!(loaded.hide_thinking, Some(true));
        assert_eq!(loaded.auto_compact, Some(false));
        assert_eq!(loaded.default_project_trust, Some(TrustLevelSer::Trusted));

        std::env::remove_var("NANOPI_HOME");
    }

    #[test]
    fn save_preserves_user_comment_and_unknown_section() {
        let _g = _lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NANOPI_HOME", tmp.path());

        // Pre-seed the file with a comment + legacy [hooks] block that
        // the interaction loader must NOT touch.
        let path = settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "\
# User's preferred defaults — DO NOT delete this comment.
thinking_level = \"medium\"

[[hooks.tool_execution_start]]
matcher = \"Bash\"
command = \"echo hello\"
",
        )
        .unwrap();

        // Mutate a field via save().
        let mut f = load();
        f.thinking_level = Some(ThinkingLevel::High);
        save(&f).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# User's preferred defaults"),
            "comment lost:\n{}",
            after
        );
        assert!(
            after.contains("[[hooks.tool_execution_start]]"),
            "hooks section lost:\n{}",
            after
        );
        assert!(
            after.contains("thinking_level = \"high\""),
            "new value not written:\n{}",
            after
        );

        std::env::remove_var("NANOPI_HOME");
    }

    #[test]
    fn keybindings_roundtrip() {
        let _g = _lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NANOPI_HOME", tmp.path());

        let mut kb = HashMap::new();
        kb.insert(
            ActionId::OpenSettings,
            KeySpec {
                code: crossterm::event::KeyCode::Char(','),
                mods: crossterm::event::KeyModifiers::CONTROL,
            },
        );
        let f = SettingsFile {
            keybindings: kb,
            ..Default::default()
        };
        save(&f).unwrap();

        let loaded = load();
        assert_eq!(loaded.keybindings.len(), 1);
        assert_eq!(
            loaded
                .keybindings
                .get(&ActionId::OpenSettings)
                .unwrap()
                .to_string(),
            "ctrl+,"
        );

        std::env::remove_var("NANOPI_HOME");
    }

    #[test]
    fn bindings_from_uses_defaults_when_empty() {
        let bindings = bindings_from(&SettingsFile::default());
        assert!(bindings.matches(
            ActionId::ThinkingCycle,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::BackTab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
    }
}

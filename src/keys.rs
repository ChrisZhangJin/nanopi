//! User-rebindable keybindings (spec §2.3).
//!
//! Every runtime chord check in `mode::tui::interpret_key` goes
//! through `KeyBindings::matches`. Defaults match today's hardcoded
//! behaviour; the user's overrides are persisted by `settings_toml`.

use std::collections::HashMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionId {
    ThinkingCycle,
    ToolCancel,
    ExpandLastTool,
    NewlineInInput,
    OpenSlashPalette,
    OpenSettings,
}

impl ActionId {
    pub fn all() -> &'static [ActionId] {
        &[
            ActionId::ThinkingCycle,
            ActionId::ToolCancel,
            ActionId::ExpandLastTool,
            ActionId::NewlineInInput,
            ActionId::OpenSlashPalette,
            ActionId::OpenSettings,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            ActionId::ThinkingCycle => "Cycle thinking level",
            ActionId::ToolCancel => "Cancel current tool / turn",
            ActionId::ExpandLastTool => "Expand last tool output",
            ActionId::NewlineInInput => "Insert newline in input",
            ActionId::OpenSlashPalette => "Open slash-command palette",
            ActionId::OpenSettings => "Open settings menu",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeySpec {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

#[derive(Debug, thiserror::Error)]
pub enum KeySpecParseError {
    #[error("unknown key: {0}")]
    UnknownKey(String),
    #[error("empty key spec")]
    Empty,
}

impl std::str::FromStr for KeySpec {
    type Err = KeySpecParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Err(KeySpecParseError::Empty);
        }
        let mut mods = KeyModifiers::NONE;
        let mut key_part: Option<String> = None;
        for tok in s.split('+') {
            match tok {
                "ctrl" => mods |= KeyModifiers::CONTROL,
                "shift" => mods |= KeyModifiers::SHIFT,
                "alt" | "meta" | "opt" => mods |= KeyModifiers::ALT,
                "" => return Err(KeySpecParseError::UnknownKey(s.clone())),
                other => key_part = Some(other.to_string()),
            }
        }
        let kp = key_part.ok_or_else(|| KeySpecParseError::UnknownKey(s.clone()))?;
        let code = match kp.as_str() {
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "esc" | "escape" => KeyCode::Esc,
            "enter" | "return" => KeyCode::Enter,
            "space" => KeyCode::Char(' '),
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            single if single.chars().count() == 1 => {
                KeyCode::Char(single.chars().next().unwrap())
            }
            other => return Err(KeySpecParseError::UnknownKey(other.to_string())),
        };
        Ok(KeySpec { code, mods })
    }
}

impl fmt::Display for KeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if self.mods.contains(KeyModifiers::CONTROL) { parts.push("ctrl".into()); }
        if self.mods.contains(KeyModifiers::SHIFT) { parts.push("shift".into()); }
        if self.mods.contains(KeyModifiers::ALT) { parts.push("alt".into()); }
        let key_str: String = match self.code {
            KeyCode::Tab => "tab".into(),
            KeyCode::BackTab => "backtab".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Char(' ') => "space".into(),
            KeyCode::Backspace => "backspace".into(),
            KeyCode::Delete => "delete".into(),
            KeyCode::Up => "up".into(),
            KeyCode::Down => "down".into(),
            KeyCode::Left => "left".into(),
            KeyCode::Right => "right".into(),
            KeyCode::Home => "home".into(),
            KeyCode::End => "end".into(),
            KeyCode::PageUp => "pageup".into(),
            KeyCode::PageDown => "pagedown".into(),
            KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
            other => format!("{:?}", other).to_lowercase(),
        };
        parts.push(key_str);
        f.write_str(&parts.join("+"))
    }
}

impl Serialize for KeySpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeySpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone)]
pub struct KeyBindings {
    map: HashMap<ActionId, KeySpec>,
}

impl KeyBindings {
    pub fn default() -> Self {
        let mut map = HashMap::new();
        map.insert(
            ActionId::ThinkingCycle,
            KeySpec { code: KeyCode::BackTab, mods: KeyModifiers::NONE },
        );
        map.insert(
            ActionId::ToolCancel,
            KeySpec { code: KeyCode::Esc, mods: KeyModifiers::NONE },
        );
        map.insert(
            ActionId::ExpandLastTool,
            KeySpec { code: KeyCode::Char('o'), mods: KeyModifiers::CONTROL },
        );
        map.insert(
            ActionId::NewlineInInput,
            KeySpec { code: KeyCode::Char('j'), mods: KeyModifiers::CONTROL },
        );
        map.insert(
            ActionId::OpenSlashPalette,
            KeySpec { code: KeyCode::Char('/'), mods: KeyModifiers::NONE },
        );
        // OpenSettings has NO default binding — only reachable via the
        // `/settings` slash command.
        Self { map }
    }

    pub fn from_overrides(overrides: HashMap<ActionId, KeySpec>) -> Self {
        let mut me = Self::default();
        for (id, spec) in overrides {
            me.map.insert(id, spec);
        }
        me
    }

    pub fn get(&self, action: ActionId) -> Option<KeySpec> {
        self.map.get(&action).copied()
    }

    /// True if `ev` triggers `action`.
    ///
    /// Shift+Tab / BackTab equivalence: crossterm emits either form
    /// depending on the terminal / raw-mode config, so a binding
    /// stored in either shape matches events in either shape.
    pub fn matches(&self, action: ActionId, ev: KeyEvent) -> bool {
        let Some(spec) = self.map.get(&action) else { return false; };
        if spec.code == ev.code && spec.mods == ev.modifiers {
            return true;
        }
        let is_shift_tab = |c: KeyCode, m: KeyModifiers| {
            (c == KeyCode::BackTab && m == KeyModifiers::NONE)
                || (c == KeyCode::Tab && m == KeyModifiers::SHIFT)
        };
        is_shift_tab(spec.code, spec.mods) && is_shift_tab(ev.code, ev.modifiers)
    }

    /// Install a new binding. Returns the previous owner (a different
    /// ActionId that had this spec bound) if any — caller decides
    /// whether to unbind that owner or refuse.
    pub fn set(&mut self, action: ActionId, spec: KeySpec) -> Option<ActionId> {
        let prev_owner = self
            .map
            .iter()
            .find(|(a, s)| **a != action && **s == spec)
            .map(|(a, _)| *a);
        self.map.insert(action, spec);
        prev_owner
    }

    pub fn unbind(&mut self, action: ActionId) {
        self.map.remove(&action);
    }

    /// Diff against defaults — the entries `settings_toml::save`
    /// should persist.
    pub fn overrides(&self) -> HashMap<ActionId, KeySpec> {
        let defaults = Self::default();
        self.map
            .iter()
            .filter(|(a, s)| defaults.map.get(a) != Some(*s))
            .map(|(a, s)| (*a, *s))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyspec_parse_ctrl_shift_t_roundtrips() {
        let k: KeySpec = "ctrl+shift+t".parse().unwrap();
        assert_eq!(k.code, KeyCode::Char('t'));
        assert!(k.mods.contains(KeyModifiers::CONTROL));
        assert!(k.mods.contains(KeyModifiers::SHIFT));
        assert_eq!(k.to_string(), "ctrl+shift+t");
    }

    #[test]
    fn keyspec_shift_tab_equals_backtab_char() {
        let user: KeySpec = "shift+tab".parse().unwrap();
        assert_eq!(user.code, KeyCode::Tab);
        assert!(user.mods.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn keyspec_parses_esc_and_special_keys() {
        assert_eq!("esc".parse::<KeySpec>().unwrap().code, KeyCode::Esc);
        assert_eq!("enter".parse::<KeySpec>().unwrap().code, KeyCode::Enter);
        assert_eq!("space".parse::<KeySpec>().unwrap().code, KeyCode::Char(' '));
        assert_eq!("f".parse::<KeySpec>().unwrap().code, KeyCode::Char('f'));
    }

    #[test]
    fn keyspec_rejects_junk() {
        assert!("foobar".parse::<KeySpec>().is_err());
        assert!("".parse::<KeySpec>().is_err());
    }

    #[test]
    fn default_bindings_match_shifttab_for_thinking() {
        let kb = KeyBindings::default();
        assert!(kb.matches(
            ActionId::ThinkingCycle,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
        ));
        assert!(kb.matches(
            ActionId::ThinkingCycle,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ));
    }

    #[test]
    fn default_bindings_match_ctrl_j_newline() {
        let kb = KeyBindings::default();
        assert!(kb.matches(
            ActionId::NewlineInInput,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ));
        assert!(!kb.matches(
            ActionId::NewlineInInput,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        ));
    }

    #[test]
    fn set_returns_prev_owner_on_conflict() {
        let mut kb = KeyBindings::default();
        let prev = kb.set(
            ActionId::ExpandLastTool,
            KeySpec { code: KeyCode::BackTab, mods: KeyModifiers::NONE },
        );
        assert_eq!(prev, Some(ActionId::ThinkingCycle));
    }

    #[test]
    fn overrides_are_empty_for_default_bindings() {
        assert!(KeyBindings::default().overrides().is_empty());
    }

    #[test]
    fn overrides_report_only_diffs() {
        let mut kb = KeyBindings::default();
        kb.set(
            ActionId::OpenSettings,
            KeySpec { code: KeyCode::Char(','), mods: KeyModifiers::CONTROL },
        );
        let ov = kb.overrides();
        assert_eq!(ov.len(), 1);
        assert!(ov.contains_key(&ActionId::OpenSettings));
    }
}

//! Navigable menu widget used by the slash-command palette (and, in
//! the future, `/model`, `/login`, `/settings`, etc.).
//!
//! Generic over the payload `T`. The list is filtered live against a
//! user-provided query string (typically the input buffer's contents
//! after the leading `/`). Filter is case-insensitive substring on
//! `label` first, `description` second — no fuzzy scoring (keeps
//! behavior predictable, matches PI's UX).
//!
//! Rendering is done by the caller against a ratatui Rect: this
//! module owns state (items, filter, cursor), not draw code, so it's
//! easy to test.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone)]
pub struct MenuItem<T> {
    pub label: String,
    pub description: String,
    pub payload: T,
}

impl<T> MenuItem<T> {
    pub fn new(label: impl Into<String>, description: impl Into<String>, payload: T) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction<T> {
    /// Nothing to do; state may have changed (cursor moved) but the
    /// outer loop just needs to redraw.
    Nothing,
    /// User picked an item.
    Chosen(T),
    /// User pressed Esc — outer loop should close the menu.
    Cancel,
}

pub struct MenuState<T: Clone> {
    all: Vec<MenuItem<T>>,
    filter: String,
    cursor: usize, // index into filtered view
}

impl<T: Clone> MenuState<T> {
    pub fn new(items: Vec<MenuItem<T>>) -> Self {
        Self {
            all: items,
            filter: String::new(),
            cursor: 0,
        }
    }

    /// Filtered slice — recomputed on demand from `self.filter`. Slow
    /// path is fine; menu size stays small.
    pub fn visible(&self) -> Vec<&MenuItem<T>> {
        if self.filter.is_empty() {
            return self.all.iter().collect();
        }
        let f = self.filter.to_ascii_lowercase();
        self.all
            .iter()
            .filter(|it| {
                it.label.to_ascii_lowercase().contains(&f)
                    || it.description.to_ascii_lowercase().contains(&f)
            })
            .collect()
    }

    pub fn cursor(&self) -> usize {
        // Clamp against current visible list length in case items
        // shifted after a filter change.
        let len = self.visible().len();
        if len == 0 {
            0
        } else {
            self.cursor.min(len - 1)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.visible().is_empty()
    }

    /// Update the filter query. Cursor snaps back to 0.
    pub fn set_filter(&mut self, s: impl Into<String>) {
        self.filter = s.into();
        self.cursor = 0;
    }

    pub fn move_up(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        if self.cursor == 0 {
            self.cursor = len - 1; // wrap
        } else {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        self.cursor = (self.cursor + 1) % len;
    }

    pub fn selected(&self) -> Option<MenuItem<T>> {
        let vis = self.visible();
        let idx = self.cursor.min(vis.len().saturating_sub(1));
        vis.get(idx).map(|&it| it.clone())
    }

    /// Handle one key event. Returns what the outer loop should do.
    /// `Nothing` = redraw; `Chosen(T)` = fire the payload; `Cancel` =
    /// close the menu.
    pub fn handle_key(&mut self, k: KeyEvent) -> MenuAction<T> {
        // Ctrl-C in the menu → cancel
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return MenuAction::Cancel;
        }
        match k.code {
            KeyCode::Esc => MenuAction::Cancel,
            KeyCode::Up | KeyCode::Char('k') if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_up();
                MenuAction::Nothing
            }
            KeyCode::Down | KeyCode::Char('j') if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_down();
                MenuAction::Nothing
            }
            KeyCode::Up => {
                self.move_up();
                MenuAction::Nothing
            }
            KeyCode::Down => {
                self.move_down();
                MenuAction::Nothing
            }
            KeyCode::Enter | KeyCode::Tab => match self.selected() {
                Some(it) => MenuAction::Chosen(it.payload),
                None => MenuAction::Nothing,
            },
            _ => MenuAction::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<MenuItem<&'static str>> {
        vec![
            MenuItem::new("compact", "Force context compaction", "/compact"),
            MenuItem::new("exit", "Leave the session", "/exit"),
            MenuItem::new("quit", "Leave the session", "/quit"),
            MenuItem::new("model", "Change model", "/model"),
        ]
    }

    #[test]
    fn filter_by_label_substring() {
        let mut m = MenuState::new(items());
        m.set_filter("q");
        let vis = m.visible();
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].label, "quit");
    }

    #[test]
    fn filter_matches_description() {
        let mut m = MenuState::new(items());
        m.set_filter("Leave");
        // Both exit + quit have "Leave" in description
        assert_eq!(m.visible().len(), 2);
    }

    #[test]
    fn empty_filter_shows_all() {
        let m = MenuState::new(items());
        assert_eq!(m.visible().len(), 4);
    }

    #[test]
    fn nav_wraps() {
        let mut m = MenuState::new(items());
        assert_eq!(m.cursor(), 0);
        m.move_up();
        // wrapped to last
        assert_eq!(m.cursor(), 3);
        m.move_down();
        // wrapped to 0
        assert_eq!(m.cursor(), 0);
    }

    #[test]
    fn enter_selects() {
        let mut m = MenuState::new(items());
        let k = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match m.handle_key(k) {
            MenuAction::Chosen(p) => assert_eq!(p, "/compact"),
            _ => panic!(),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut m = MenuState::new(items());
        let k = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(m.handle_key(k), MenuAction::Cancel));
    }

    #[test]
    fn cursor_clamps_after_filter_shrinks() {
        let mut m = MenuState::new(items());
        m.move_down();
        m.move_down();
        m.move_down();
        assert_eq!(m.cursor(), 3);
        // Filter down to 1
        m.set_filter("compact");
        assert_eq!(m.visible().len(), 1);
        assert_eq!(m.cursor(), 0);
    }

    #[test]
    fn empty_visible_no_selection() {
        let mut m = MenuState::new(items());
        m.set_filter("zzz-nonexistent");
        assert!(m.is_empty());
        assert!(m.selected().is_none());
    }
}

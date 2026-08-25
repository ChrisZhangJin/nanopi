//! Navigable menu widget used by the slash-command palette (and, in
//! the future, `/model`, `/login`, `/settings`, etc.).
//!
//! Generic over the payload `T`. The list is filtered live against a
//! user-provided query string (typically the input buffer's contents
//! after the leading `/`). Filter is case-insensitive substring on
//! `label` first, `description` second — no fuzzy scoring (keeps
//! behavior predictable, matches PI's UX). Matches are ordered by
//! relevance tier, so an exact label hit always outranks an incidental
//! mention in some other item's description; see `MenuState::rank`.
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
    /// User pressed Enter — commit the selected payload.
    Chosen(T),
    /// User pressed Tab — fill the input buffer with `label` (with a
    /// trailing space) but do NOT commit. Caller replaces the input
    /// so the user can type args and hit Enter, matching typical
    /// shell-style tab completion (readline / zsh / fish).
    Filled(String),
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

    /// Relevance tier for one item against the lowercased query, lower
    /// being better. `None` means no match at all.
    ///
    /// Ordering by tier is what makes the palette preselect the command
    /// the user actually typed. Matching alone is not enough: with a
    /// plain "label or description contains" test, `/settings` matches
    /// both `/settings` (label) and `/reload` (whose description reads
    /// "Reload skills, config, settings"), and whichever happens to sit
    /// earlier in the item list wins the cursor.
    ///
    /// Tiers mirror what PI does for its slash palette in
    /// `packages/tui/src/fuzzy.ts` — an exact hit gets a large bonus and
    /// word-boundary hits outrank mid-string ones — but stay substring-
    /// based rather than fuzzy-subsequence, so the *set* of visible items
    /// is unchanged and only their order improves.
    fn rank(it: &MenuItem<T>, f: &str) -> Option<u8> {
        let label = it.label.to_ascii_lowercase();
        // Compare against the label with any leading sigil removed, so
        // "settings" ranks against "settings" rather than "/settings".
        let bare = label.trim_start_matches('/');
        if bare == f {
            Some(0)
        } else if bare.starts_with(f) {
            Some(1)
        } else if label.contains(f) {
            Some(2)
        } else if it.description.to_ascii_lowercase().contains(f) {
            Some(3)
        } else {
            None
        }
    }

    /// Filtered slice — recomputed on demand from `self.filter`, best
    /// match first. Slow path is fine; menu size stays small.
    pub fn visible(&self) -> Vec<&MenuItem<T>> {
        if self.filter.is_empty() {
            return self.all.iter().collect();
        }
        let f = self.filter.to_ascii_lowercase();
        let mut hits: Vec<(u8, &MenuItem<T>)> = self
            .all
            .iter()
            .filter_map(|it| Self::rank(it, &f).map(|r| (r, it)))
            .collect();
        // Stable, so items sharing a tier keep their declared order.
        hits.sort_by_key(|(r, _)| *r);
        hits.into_iter().map(|(_, it)| it).collect()
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
            KeyCode::Enter => match self.selected() {
                Some(it) => MenuAction::Chosen(it.payload),
                None => MenuAction::Nothing,
            },
            KeyCode::Tab => match self.selected() {
                Some(it) => MenuAction::Filled(it.label),
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

    /// Regression: an exact label hit must outrank another item that
    /// merely mentions the query in its description, no matter which
    /// one was declared first. This is the `/settings` bug — typing it
    /// preselected `/reload`, because "Reload skills, config, settings"
    /// contains "settings" and `/reload` is declared earlier.
    #[test]
    fn exact_label_outranks_description_mention() {
        let m = |f: &str| {
            let mut m = MenuState::new(vec![
                MenuItem::new("/reload", "Reload skills, config, settings", "r"),
                MenuItem::new("/settings", "Show interaction settings", "s"),
            ]);
            m.set_filter(f);
            m.selected().unwrap().label
        };
        assert_eq!(m("settings"), "/settings");
        // Still finds the description-only match when nothing else does.
        assert_eq!(m("config"), "/reload");
    }

    /// A prefix hit beats a mid-string hit, which beats a description
    /// hit — regardless of declaration order.
    #[test]
    fn ranks_prefix_above_substring_above_description() {
        let mut m = MenuState::new(vec![
            MenuItem::new("/unrelated", "mentions new in passing", "d"),
            MenuItem::new("/renew", "mid-string hit", "s"),
            MenuItem::new("/new", "exact hit", "e"),
            MenuItem::new("/newer", "prefix hit", "p"),
        ]);
        m.set_filter("new");
        let order: Vec<&str> = m.visible().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(order, ["/new", "/newer", "/renew", "/unrelated"]);
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
    fn tab_fills_without_committing() {
        let mut m = MenuState::new(items());
        let k = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        match m.handle_key(k) {
            MenuAction::Filled(label) => assert_eq!(label, "compact"),
            other => panic!("expected Filled, got {other:?}"),
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

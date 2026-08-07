//! Multi-line text buffer for the TUI input box.
//!
//! Emacs-style keybindings, single kill-ring slot, undo stack, and
//! a cursor tracked as `(row, col)` where col is a **byte** offset
//! into `lines[row]` (all our terminal ops are ASCII-safe with the
//! occasional multi-byte char — we grapheme-count on render, not
//! here).
//!
//! Keys (see `handle_key`):
//!   Enter                        submit (returns Submit action to caller)
//!   Shift+Enter / Alt+Enter      insert newline
//!   Backspace                    delete char before cursor
//!   Delete                       delete char at cursor
//!   ← / →                        move cursor
//!   Home / End                   line start / end
//!   Ctrl-A / Ctrl-E              line start / end (Emacs)
//!   Ctrl-K                       kill to end of line (into kill ring)
//!   Ctrl-U                       kill from cursor to start of line
//!   Ctrl-W                       kill previous word
//!   Ctrl-Y                       yank (paste kill ring)
//!   Alt-B / Alt-F                move by word
//!   Ctrl-Z                       undo
//!
//! Non-goals: multi-slot kill ring, redo, search, IME.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the outer event loop should do after handling a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Consumed; state may have changed but nothing else to do.
    Nothing,
    /// User pressed Enter without a modifier: submit the whole buffer.
    Submit(String),
    /// Ctrl-C — outer loop decides what "cancel" means in context.
    Cancel,
    /// Ctrl-D on an empty buffer — exit.
    Exit,
    /// Buffer became `/…` on the first line — outer loop may want to
    /// open a slash-command palette. Also fired on each keystroke while
    /// the first char stays `/`, so the palette can filter.
    SlashChanged(String),
}

#[derive(Debug, Clone, Default)]
pub struct TextBuffer {
    lines: Vec<String>,
    /// (row, byte column) — column is a byte offset, not a char count.
    cursor: (usize, usize),
    kill_ring: String,
    undo_stack: Vec<UndoSnapshot>,
}

#[derive(Debug, Clone)]
struct UndoSnapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            kill_ring: String::new(),
            undo_stack: Vec::new(),
        }
    }

    /// Full buffer joined with `\n`.
    pub fn as_string(&self) -> String {
        self.lines.join("\n")
    }

    /// True when there's nothing typed.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// How many display rows the buffer wants to occupy — 1 per line.
    pub fn row_count(&self) -> usize {
        self.lines.len()
    }

    fn snapshot(&mut self) {
        // Cap the stack so pathological typing doesn't eat memory.
        const MAX_UNDO: usize = 32;
        if self.undo_stack.len() >= MAX_UNDO {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(UndoSnapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
    }

    fn undo(&mut self) {
        if let Some(snap) = self.undo_stack.pop() {
            self.lines = snap.lines;
            self.cursor = snap.cursor;
        }
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        self.undo_stack.clear();
    }

    /// Insert a string at the cursor. Newlines break into new lines.
    /// Used by bracketed-paste to inject clipboard content.
    pub fn insert_str(&mut self, s: &str) {
        self.snapshot();
        for c in s.chars() {
            if c == '\n' {
                let (row, col) = self.cursor;
                let rest = self.lines[row].split_off(col);
                self.lines.insert(row + 1, rest);
                self.cursor = (row + 1, 0);
            } else {
                let (row, col) = self.cursor;
                self.lines[row].insert(col, c);
                self.cursor.1 = col + c.len_utf8();
            }
        }
    }

    // ─── Cursor motion ─────────────────────────────────────────────

    fn line(&self) -> &str {
        &self.lines[self.cursor.0]
    }

    fn line_mut(&mut self) -> &mut String {
        &mut self.lines[self.cursor.0]
    }

    fn move_left(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            // Move back one char boundary (UTF-8-safe).
            let line = self.lines[row].clone();
            let prev = line[..col]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor.1 = prev;
        } else if row > 0 {
            let prev_row = row - 1;
            let new_col = self.lines[prev_row].len();
            self.cursor = (prev_row, new_col);
        }
    }

    fn move_right(&mut self) {
        let (row, col) = self.cursor;
        let line_len = self.lines[row].len();
        if col < line_len {
            let rest = &self.lines[row][col..];
            let step = rest
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or_else(|| rest.len());
            self.cursor.1 = col + step;
        } else if row + 1 < self.lines.len() {
            self.cursor = (row + 1, 0);
        }
    }

    fn move_line_start(&mut self) {
        self.cursor.1 = 0;
    }

    fn move_line_end(&mut self) {
        self.cursor.1 = self.line().len();
    }

    fn move_word_left(&mut self) {
        // Walk left through whitespace, then through non-whitespace.
        while self.cursor.1 > 0 {
            self.move_left();
            let ch = self.line()[self.cursor.1..].chars().next();
            if ch.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
                // Continue until we hit non-word or start of line.
            } else {
                break;
            }
        }
    }

    fn move_word_right(&mut self) {
        while self.cursor.1 < self.line().len() {
            let ch = self.line()[self.cursor.1..].chars().next();
            self.move_right();
            if ch.map(|c| !(c.is_alphanumeric() || c == '_')).unwrap_or(false) {
                break;
            }
        }
    }

    // ─── Mutations ─────────────────────────────────────────────────

    fn insert_char(&mut self, c: char) {
        self.snapshot();
        let (row, col) = self.cursor;
        self.lines[row].insert(col, c);
        self.cursor.1 = col + c.len_utf8();
    }

    fn insert_newline(&mut self) {
        self.snapshot();
        let (row, col) = self.cursor;
        let rest = self.lines[row].split_off(col);
        self.lines.insert(row + 1, rest);
        self.cursor = (row + 1, 0);
    }

    fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.snapshot();
            let line = self.lines[row].clone();
            let prev = line[..col]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.lines[row].replace_range(prev..col, "");
            self.cursor.1 = prev;
        } else if row > 0 {
            self.snapshot();
            let removed = self.lines.remove(row);
            let prev_row = row - 1;
            let new_col = self.lines[prev_row].len();
            self.lines[prev_row].push_str(&removed);
            self.cursor = (prev_row, new_col);
        }
    }

    fn delete_forward(&mut self) {
        let (row, col) = self.cursor;
        let line_len = self.lines[row].len();
        if col < line_len {
            self.snapshot();
            let rest = &self.lines[row][col..];
            let step = rest
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or_else(|| rest.len());
            self.lines[row].replace_range(col..col + step, "");
        } else if row + 1 < self.lines.len() {
            self.snapshot();
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    fn kill_to_line_end(&mut self) {
        self.snapshot();
        let (row, col) = self.cursor;
        let killed = self.lines[row].split_off(col);
        self.kill_ring = killed;
    }

    fn kill_to_line_start(&mut self) {
        self.snapshot();
        let (row, col) = self.cursor;
        let killed = self.lines[row].drain(..col).collect();
        self.kill_ring = killed;
        self.cursor.1 = 0;
    }

    fn kill_prev_word(&mut self) {
        self.snapshot();
        let (row, col) = self.cursor;
        // Find start of previous word: skip trailing whitespace, then
        // skip word chars.
        let bytes = self.lines[row].as_bytes();
        let mut i = col;
        while i > 0 && !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_' {
            i -= 1;
        }
        while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i -= 1;
        }
        let killed = self.lines[row].drain(i..col).collect();
        self.kill_ring = killed;
        self.cursor.1 = i;
    }

    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        self.snapshot();
        let text = self.kill_ring.clone();
        let (row, col) = self.cursor;
        self.lines[row].insert_str(col, &text);
        self.cursor.1 = col + text.len();
    }

    // ─── Top-level key handler ─────────────────────────────────────

    /// Process one KeyEvent. Returns what the outer loop should do.
    pub fn handle_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);

        // Chord shortcuts first.
        if ctrl {
            match k.code {
                KeyCode::Char('c') => return Action::Cancel,
                KeyCode::Char('d') => {
                    if self.is_empty() {
                        return Action::Exit;
                    } else {
                        self.delete_forward();
                        return Action::Nothing;
                    }
                }
                KeyCode::Char('a') => { self.move_line_start(); return Action::Nothing; }
                KeyCode::Char('e') => { self.move_line_end();   return Action::Nothing; }
                KeyCode::Char('k') => { self.kill_to_line_end(); return Action::Nothing; }
                KeyCode::Char('u') => { self.kill_to_line_start(); return Action::Nothing; }
                KeyCode::Char('w') => { self.kill_prev_word();  return Action::Nothing; }
                KeyCode::Char('y') => { self.yank();            return self.slash_or_nothing(); }
                KeyCode::Char('z') => { self.undo();            return Action::Nothing; }
                // Some terminals send Backspace as Ctrl+H (0x08) instead
                // of DEL (0x7f). Handle that here so users don't get a
                // literal 'h' inserted when pressing Backspace.
                KeyCode::Char('h') => { self.backspace();       return self.slash_or_nothing(); }
                _ => {}
            }
        }
        if alt {
            match k.code {
                KeyCode::Char('b') => { self.move_word_left();  return Action::Nothing; }
                KeyCode::Char('f') => { self.move_word_right(); return Action::Nothing; }
                KeyCode::Enter => { self.insert_newline(); return Action::Nothing; }
                _ => {}
            }
        }

        // Base keys.
        match k.code {
            KeyCode::Enter => {
                if shift {
                    self.insert_newline();
                    return Action::Nothing;
                }
                if self.is_empty() {
                    return Action::Nothing;
                }
                let text = self.as_string();
                self.clear();
                return Action::Submit(text);
            }
            KeyCode::Backspace => { self.backspace();     return self.slash_or_nothing(); }
            KeyCode::Delete    => { self.delete_forward(); return self.slash_or_nothing(); }
            KeyCode::Left      => { self.move_left();      return Action::Nothing; }
            KeyCode::Right     => { self.move_right();     return Action::Nothing; }
            KeyCode::Home      => { self.move_line_start(); return Action::Nothing; }
            KeyCode::End       => { self.move_line_end();   return Action::Nothing; }
            KeyCode::Char(c)   => { self.insert_char(c);   return self.slash_or_nothing(); }
            _ => {}
        }
        Action::Nothing
    }

    /// After any content change, tell the caller whether the buffer
    /// currently reads as a slash command (used to drive the palette).
    fn slash_or_nothing(&self) -> Action {
        if self.lines.len() == 1 && self.lines[0].starts_with('/') {
            Action::SlashChanged(self.lines[0].clone())
        } else {
            Action::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn press_c(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }
    fn press_a(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn type_str(b: &mut TextBuffer, s: &str) {
        for c in s.chars() {
            b.handle_key(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_appends() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "hello");
        assert_eq!(b.as_string(), "hello");
        assert_eq!(b.cursor(), (0, 5));
    }

    #[test]
    fn enter_submits() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "hi");
        match b.handle_key(press(KeyCode::Enter)) {
            Action::Submit(s) => assert_eq!(s, "hi"),
            _ => panic!(),
        }
        // buffer cleared post-submit
        assert!(b.is_empty());
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "line1");
        let k = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        b.handle_key(k);
        type_str(&mut b, "line2");
        assert_eq!(b.as_string(), "line1\nline2");
    }

    #[test]
    fn backspace_deletes() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "abc");
        b.handle_key(press(KeyCode::Backspace));
        assert_eq!(b.as_string(), "ab");
    }

    /// Some terminals send Ctrl-H (0x08) as the Backspace key. We must
    /// treat that as a backspace, NOT insert an 'h'.
    #[test]
    fn ctrl_h_is_backspace() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "abc");
        b.handle_key(press_c(KeyCode::Char('h')));
        assert_eq!(b.as_string(), "ab");
    }

    #[test]
    fn ctrl_a_e() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "hello");
        b.handle_key(press_c(KeyCode::Char('a')));
        assert_eq!(b.cursor(), (0, 0));
        b.handle_key(press_c(KeyCode::Char('e')));
        assert_eq!(b.cursor(), (0, 5));
    }

    #[test]
    fn ctrl_k_kills_to_line_end() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "hello world");
        // Move to after "hello "
        for _ in 0..5 {
            b.handle_key(press(KeyCode::Left));
        }
        b.handle_key(press_c(KeyCode::Char('k')));
        assert_eq!(b.as_string(), "hello ");
        // Then yank it back at end
        b.handle_key(press_c(KeyCode::Char('e')));
        b.handle_key(press_c(KeyCode::Char('y')));
        assert_eq!(b.as_string(), "hello world");
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "hello world");
        for _ in 0..5 {
            b.handle_key(press(KeyCode::Left));
        }
        b.handle_key(press_c(KeyCode::Char('u')));
        assert_eq!(b.as_string(), "world");
    }

    #[test]
    fn ctrl_w_kills_prev_word() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "hello world");
        b.handle_key(press_c(KeyCode::Char('w')));
        assert_eq!(b.as_string(), "hello ");
    }

    #[test]
    fn alt_bf_moves_by_word() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "foo bar baz");
        b.handle_key(press_c(KeyCode::Char('a')));
        b.handle_key(press_a(KeyCode::Char('f')));
        // Should be at column 4 (start of "bar")
        assert!(b.cursor().1 >= 3, "cursor {:?}", b.cursor());
        b.handle_key(press_a(KeyCode::Char('b')));
        assert!(b.cursor().1 <= 4);
    }

    #[test]
    fn ctrl_d_on_empty_exits() {
        let mut b = TextBuffer::new();
        assert!(matches!(
            b.handle_key(press_c(KeyCode::Char('d'))),
            Action::Exit
        ));
    }

    #[test]
    fn ctrl_d_on_nonempty_deletes_forward() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "ab");
        b.handle_key(press_c(KeyCode::Char('a'))); // cursor to 0
        b.handle_key(press_c(KeyCode::Char('d')));
        assert_eq!(b.as_string(), "b");
    }

    #[test]
    fn ctrl_c_returns_cancel() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "x");
        assert!(matches!(
            b.handle_key(press_c(KeyCode::Char('c'))),
            Action::Cancel
        ));
    }

    #[test]
    fn ctrl_z_undoes() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "abc");
        b.handle_key(press_c(KeyCode::Char('z')));
        assert_eq!(b.as_string(), "ab");
        b.handle_key(press_c(KeyCode::Char('z')));
        assert_eq!(b.as_string(), "a");
    }

    #[test]
    fn slash_reports_action() {
        let mut b = TextBuffer::new();
        let a = b.handle_key(press(KeyCode::Char('/')));
        assert!(matches!(a, Action::SlashChanged(ref s) if s == "/"));
        let a = b.handle_key(press(KeyCode::Char('c')));
        assert!(matches!(a, Action::SlashChanged(ref s) if s == "/c"));
    }
}

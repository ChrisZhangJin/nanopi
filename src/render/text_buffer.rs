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
//!   Ctrl-- / Ctrl-_ / Ctrl-Z     undo (fish-style: word-coalesced)
//!
//! Undo model mirrors PI (see `packages/tui/src/components/editor.ts`):
//! consecutive word chars coalesce into one undo unit; each whitespace
//! run gets its own snapshot; every mutating non-typing op (kill, yank,
//! paste, backspace, newline) always snapshots. Cursor motion resets
//! the coalesce state without snapshotting. No redo.
//!
//! Non-goals: multi-slot kill ring, redo, search, IME.

use std::path::PathBuf;

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

/// Cap for the in-memory prompt history. PI uses 100 (see
/// `packages/tui/src/components/editor.ts:403-406`); matching keeps
/// the muscle memory identical between the two tools.
const HISTORY_CAP: usize = 100;

#[derive(Debug, Clone, Default)]
pub struct TextBuffer {
    lines: Vec<String>,
    /// (row, byte column) — column is a byte offset, not a char count.
    cursor: (usize, usize),
    kill_ring: String,
    undo_stack: Vec<UndoSnapshot>,
    /// What the previous key did — drives fish-style coalescing so
    /// runs of word chars merge into one undo unit.
    last_op: LastOp,
    /// Ring of previously-submitted prompts (oldest first, newest
    /// last). Capped at HISTORY_CAP entries.
    history: Vec<String>,
    /// When `Some(i)`, the editor is currently showing `history[i]`;
    /// Up/Down keep navigating and Enter submits that recalled text.
    /// `None` means the user is composing a fresh prompt.
    history_index: Option<usize>,
    /// Snapshot of whatever was in the editor at the moment we
    /// entered history mode, so Down past the newest entry restores
    /// it. PI calls this `historyDraft`.
    history_draft: String,
    /// When set, history is persisted to this file (one entry per
    /// line). Loaded on construction, saved after each submit.
    history_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct UndoSnapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LastOp {
    /// Fresh buffer, or the last op was a non-typing mutation / cursor
    /// motion. Next typed char will snapshot before inserting.
    #[default]
    Other,
    /// Last char was a word char in a contiguous run. Next word char
    /// does NOT snapshot; next non-word char does.
    TypingWord,
    /// Last char was whitespace/punctuation. Next word char does NOT
    /// snapshot (the space's own snapshot already covers "before this
    /// space + everything after"). Non-word chars still snapshot.
    TypingNonWord,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            kill_ring: String::new(),
            undo_stack: Vec::new(),
            last_op: LastOp::Other,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            history_path: None,
        }
    }

    /// Create a TextBuffer with per-session history persistence.
    /// Loads existing history from `path` on creation; saves after
    /// each submit.
    pub fn with_history(path: PathBuf) -> Self {
        let mut buf = Self::new();
        buf.history_path = Some(path.clone());
        if let Ok(contents) = std::fs::read_to_string(&path) {
            for line in contents.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    buf.history.push(trimmed.to_string());
                }
            }
            if buf.history.len() > HISTORY_CAP {
                let excess = buf.history.len() - HISTORY_CAP;
                buf.history.drain(0..excess);
            }
        }
        buf
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
        // Reset the coalesce state so the next typed char begins a fresh
        // undo unit; otherwise "type, undo, type" would silently merge.
        self.last_op = LastOp::Other;
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        self.undo_stack.clear();
        self.last_op = LastOp::Other;
        // history is intentionally preserved — clearing on cancel /
        // submit / palette-open must not wipe recall data.
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Push a submitted prompt onto the history ring. Blank text and
    /// exact-duplicate consecutive entries are skipped (bash-style).
    /// Older entries drop off the front once we exceed HISTORY_CAP.
    fn push_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(|s| s == trimmed).unwrap_or(false) {
            return;
        }
        self.history.push(trimmed.to_string());
        if self.history.len() > HISTORY_CAP {
            let excess = self.history.len() - HISTORY_CAP;
            self.history.drain(0..excess);
        }
        self.save_history();
    }

    /// Persist the in-memory history ring to disk (if a path is set).
    fn save_history(&self) {
        if let Some(path) = &self.history_path {
            let content = self.history.join("\n");
            let _ = std::fs::write(path, content);
        }
    }

    /// Load `text` into the editor, replacing everything and leaving
    /// the cursor at end-of-content. Used by history navigation.
    fn load_from_history(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        let last_row = self.lines.len().saturating_sub(1);
        let last_col = self.lines[last_row].len();
        self.cursor = (last_row, last_col);
        self.undo_stack.clear();
        self.last_op = LastOp::Other;
    }

    /// True when the editor is fully empty (one row, no chars).
    fn buffer_blank(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Should this key trigger history navigation rather than cursor
    /// movement? PI's rule (editor.ts:824): empty buffer, already in
    /// history mode, or cursor sitting at the very start (row 0, col 0).
    fn history_eligible(&self) -> bool {
        self.history_index.is_some() || self.buffer_blank() || self.cursor == (0, 0)
    }

    /// Called at the start of every content mutation. If the buffer
    /// was showing a recalled history entry, the user is now editing
    /// that text — exit history mode so a subsequent Down doesn't
    /// clobber their edits by re-loading the recalled snapshot.
    fn exit_history_mode(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Up-arrow: step back one entry in history (or enter it from a
    /// fresh buffer). No-op if we're already at the oldest entry.
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let target = match self.history_index {
            None => {
                self.history_draft = self.as_string();
                self.history.len() - 1
            }
            Some(0) => return, // already oldest
            Some(i) => i - 1,
        };
        self.history_index = Some(target);
        let text = self.history[target].clone();
        self.load_from_history(&text);
    }

    /// Down-arrow: step forward one entry, or restore the draft if
    /// we've moved past the newest entry.
    fn history_next(&mut self) {
        let Some(i) = self.history_index else { return };
        if i + 1 < self.history.len() {
            self.history_index = Some(i + 1);
            let text = self.history[i + 1].clone();
            self.load_from_history(&text);
        } else {
            self.history_index = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.load_from_history(&draft);
        }
    }

    /// Normalize text for buffer storage: CRLF and bare CR both become
    /// LF, and tabs expand to four spaces.
    ///
    /// Bare CR is the load-bearing case. A terminal in raw mode encodes
    /// line breaks inside a bracketed paste as CR, not LF, and crossterm
    /// hands `Event::Paste` the payload bytes verbatim — it does no
    /// newline translation. Without this, a pasted paragraph collapses
    /// into one row holding literal CR control characters: unreadable in
    /// the input box, and sent to the model with the CRs still in it.
    ///
    /// Mirrors PI's `Editor.normalizeText` in
    /// `packages/tui/src/components/editor.ts`, which applies the same
    /// three rewrites on its insertion path.
    fn normalize_pasted(s: &str) -> String {
        s.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
    }

    /// Insert a string at the cursor. Newlines break into new lines.
    /// Used by bracketed-paste to inject clipboard content — treated as
    /// an atomic op (one snapshot, does not coalesce with typing).
    pub fn insert_str(&mut self, s: &str) {
        self.exit_history_mode();
        self.snapshot();
        let s = Self::normalize_pasted(s);
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
        self.last_op = LastOp::Other;
    }

    // ─── Cursor motion ─────────────────────────────────────────────

    fn line(&self) -> &str {
        &self.lines[self.cursor.0]
    }

    /// True for chars that merge into contiguous undo runs. Matches
    /// the word-motion definition used by Ctrl-W / Alt-B / Alt-F.
    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    fn move_left(&mut self) {
        self.last_op = LastOp::Other;
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
        self.last_op = LastOp::Other;
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
        self.last_op = LastOp::Other;
        self.cursor.1 = 0;
    }

    fn move_line_end(&mut self) {
        self.last_op = LastOp::Other;
        self.cursor.1 = self.line().len();
    }

    fn move_word_left(&mut self) {
        self.last_op = LastOp::Other;
        // Walk left through whitespace, then through non-whitespace.
        while self.cursor.1 > 0 {
            self.move_left();
            let ch = self.line()[self.cursor.1..].chars().next();
            if ch.map(Self::is_word_char).unwrap_or(false) {
                // Continue until we hit non-word or start of line.
            } else {
                break;
            }
        }
    }

    fn move_word_right(&mut self) {
        self.last_op = LastOp::Other;
        while self.cursor.1 < self.line().len() {
            let ch = self.line()[self.cursor.1..].chars().next();
            self.move_right();
            if ch.map(|c| !Self::is_word_char(c)).unwrap_or(false) {
                break;
            }
        }
    }

    // ─── Mutations ─────────────────────────────────────────────────

    fn insert_char(&mut self, c: char) {
        self.exit_history_mode();
        // Fish-style coalescing:
        //   word char  → snapshot only if the previous op was Other
        //                (i.e. this starts a new typing run). Consecutive
        //                word chars merge; a preceding whitespace snap
        //                already covers "before this run".
        //   non-word   → always snapshot (each punctuation/space stands
        //                alone; the following word run coalesces with it).
        if Self::is_word_char(c) {
            if self.last_op == LastOp::Other {
                self.snapshot();
            }
            self.last_op = LastOp::TypingWord;
        } else {
            self.snapshot();
            self.last_op = LastOp::TypingNonWord;
        }
        let (row, col) = self.cursor;
        self.lines[row].insert(col, c);
        self.cursor.1 = col + c.len_utf8();
    }

    fn insert_newline(&mut self) {
        self.exit_history_mode();
        self.snapshot();
        self.last_op = LastOp::Other;
        let (row, col) = self.cursor;
        let rest = self.lines[row].split_off(col);
        self.lines.insert(row + 1, rest);
        self.cursor = (row + 1, 0);
    }

    fn backspace(&mut self) {
        self.exit_history_mode();
        let (row, col) = self.cursor;
        if col > 0 {
            self.snapshot();
            self.last_op = LastOp::Other;
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
            self.last_op = LastOp::Other;
            let removed = self.lines.remove(row);
            let prev_row = row - 1;
            let new_col = self.lines[prev_row].len();
            self.lines[prev_row].push_str(&removed);
            self.cursor = (prev_row, new_col);
        }
    }

    fn delete_forward(&mut self) {
        self.exit_history_mode();
        let (row, col) = self.cursor;
        let line_len = self.lines[row].len();
        if col < line_len {
            self.snapshot();
            self.last_op = LastOp::Other;
            let rest = &self.lines[row][col..];
            let step = rest
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or_else(|| rest.len());
            self.lines[row].replace_range(col..col + step, "");
        } else if row + 1 < self.lines.len() {
            self.snapshot();
            self.last_op = LastOp::Other;
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
    }

    fn kill_to_line_end(&mut self) {
        self.exit_history_mode();
        self.snapshot();
        self.last_op = LastOp::Other;
        let (row, col) = self.cursor;
        let killed = self.lines[row].split_off(col);
        self.kill_ring = killed;
    }

    fn kill_to_line_start(&mut self) {
        self.exit_history_mode();
        self.snapshot();
        self.last_op = LastOp::Other;
        let (row, col) = self.cursor;
        let killed = self.lines[row].drain(..col).collect();
        self.kill_ring = killed;
        self.cursor.1 = 0;
    }

    fn kill_prev_word(&mut self) {
        self.exit_history_mode();
        self.snapshot();
        self.last_op = LastOp::Other;
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
        self.exit_history_mode();
        self.snapshot();
        self.last_op = LastOp::Other;
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
                KeyCode::Char('a') => {
                    self.move_line_start();
                    return Action::Nothing;
                }
                KeyCode::Char('e') => {
                    self.move_line_end();
                    return Action::Nothing;
                }
                KeyCode::Char('k') => {
                    self.kill_to_line_end();
                    return Action::Nothing;
                }
                KeyCode::Char('u') => {
                    self.kill_to_line_start();
                    return Action::Nothing;
                }
                KeyCode::Char('w') => {
                    self.kill_prev_word();
                    return Action::Nothing;
                }
                KeyCode::Char('y') => {
                    self.yank();
                    return self.slash_or_nothing();
                }
                // Undo. PI uses Ctrl+- (Ctrl+Minus); Ctrl+_ is the
                // Emacs / GNU readline convention; Ctrl+Z is muscle
                // memory from most modern editors. Accept all three.
                KeyCode::Char('z') | KeyCode::Char('-') | KeyCode::Char('_') => {
                    self.undo();
                    return Action::Nothing;
                }
                // Some terminals send Backspace as Ctrl+H (0x08) instead
                // of DEL (0x7f). Handle that here so users don't get a
                // literal 'h' inserted when pressing Backspace.
                KeyCode::Char('h') => {
                    self.backspace();
                    return self.slash_or_nothing();
                }
                // Ctrl+J = LF (0x0A) — the portable "insert newline"
                // shortcut. Terminals that don't disambiguate
                // Shift+Enter from plain Enter (iTerm2, tmux, most
                // Linux TTYs) still emit LF for Ctrl+J, so this is the
                // only reliable multiline trigger. Matches PI (see
                // packages/tui/src/keybindings.ts:125 —
                // "tui.input.newLine": ["shift+enter", "ctrl+j"]).
                KeyCode::Char('j') => {
                    self.insert_newline();
                    return Action::Nothing;
                }
                _ => {}
            }
        }
        if alt {
            match k.code {
                KeyCode::Char('b') => {
                    self.move_word_left();
                    return Action::Nothing;
                }
                KeyCode::Char('f') => {
                    self.move_word_right();
                    return Action::Nothing;
                }
                KeyCode::Enter => {
                    self.insert_newline();
                    return Action::Nothing;
                }
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
                self.push_history(&text);
                self.clear();
                return Action::Submit(text);
            }
            KeyCode::Backspace => {
                self.backspace();
                return self.slash_or_nothing();
            }
            KeyCode::Delete => {
                self.delete_forward();
                return self.slash_or_nothing();
            }
            KeyCode::Left => {
                self.move_left();
                return Action::Nothing;
            }
            KeyCode::Right => {
                self.move_right();
                return Action::Nothing;
            }
            KeyCode::Home => {
                self.move_line_start();
                return Action::Nothing;
            }
            KeyCode::End => {
                self.move_line_end();
                return Action::Nothing;
            }
            KeyCode::Up => {
                // History nav only when eligible; else no-op (multi-
                // line row-up navigation is a future extension).
                if self.history_eligible() {
                    self.history_prev();
                }
                return Action::Nothing;
            }
            KeyCode::Down => {
                if self.history_index.is_some() {
                    self.history_next();
                }
                return Action::Nothing;
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                return self.slash_or_nothing();
            }
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
    fn ctrl_j_inserts_newline() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "line1");
        let k = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
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

    /// Fish-style: a run of word chars is one undo unit. Typing "abc"
    /// then undo goes straight back to empty.
    #[test]
    fn undo_coalesces_word_run() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "abc");
        b.handle_key(press_c(KeyCode::Char('z')));
        assert_eq!(b.as_string(), "");
    }

    /// Typing "hello world" — 2 undo units per PI's rule (each space
    /// snapshots before itself and the following word coalesces with it).
    #[test]
    fn undo_hello_world_two_units() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "hello world");
        b.handle_key(press_c(KeyCode::Char('z')));
        assert_eq!(b.as_string(), "hello");
        b.handle_key(press_c(KeyCode::Char('z')));
        assert_eq!(b.as_string(), "");
    }

    /// Ctrl+- and Ctrl+_ are aliases for undo (PI + Emacs conventions).
    #[test]
    fn undo_ctrl_minus_and_underscore() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "abc def");
        b.handle_key(press_c(KeyCode::Char('-')));
        assert_eq!(b.as_string(), "abc");
        b.handle_key(press_c(KeyCode::Char('_')));
        assert_eq!(b.as_string(), "");
    }

    /// Cursor motion resets coalescing — typing after moving should
    /// start a new undo unit, not merge with what came before.
    #[test]
    fn cursor_motion_breaks_coalesce() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "abc");
        b.handle_key(press_c(KeyCode::Char('a'))); // home
        type_str(&mut b, "X");
        // Now: "Xabc". Undo should remove just "X", not "Xabc".
        b.handle_key(press_c(KeyCode::Char('z')));
        assert_eq!(b.as_string(), "abc");
    }

    /// Backspace is always its own snapshot — undoing after backspace
    /// restores the deleted char, not the whole preceding word run.
    #[test]
    fn backspace_is_own_undo_unit() {
        let mut b = TextBuffer::new();
        type_str(&mut b, "abc");
        b.handle_key(press(KeyCode::Backspace));
        // "ab" now. Undo restores "abc".
        b.handle_key(press_c(KeyCode::Char('z')));
        assert_eq!(b.as_string(), "abc");
    }

    fn submit(b: &mut TextBuffer, s: &str) {
        type_str(b, s);
        b.handle_key(press(KeyCode::Enter));
    }

    #[test]
    fn history_up_on_empty_buffer_loads_last() {
        let mut b = TextBuffer::new();
        submit(&mut b, "hello");
        submit(&mut b, "world");
        // Buffer empty now — Up brings back "world".
        b.handle_key(press(KeyCode::Up));
        assert_eq!(b.as_string(), "world");
        // Up again → "hello"
        b.handle_key(press(KeyCode::Up));
        assert_eq!(b.as_string(), "hello");
        // Up at oldest → no-op
        b.handle_key(press(KeyCode::Up));
        assert_eq!(b.as_string(), "hello");
    }

    #[test]
    fn history_down_walks_forward_and_restores_draft() {
        let mut b = TextBuffer::new();
        submit(&mut b, "one");
        submit(&mut b, "two");
        // Start typing something, then recall + go back forward.
        type_str(&mut b, "draft");
        b.handle_key(press(KeyCode::Up)); // enter history mode from non-empty at (0,0)? no, cursor not at 0,0.
                                          // With text, cursor at (0,5), NOT eligible → no history nav.
        assert_eq!(b.as_string(), "draft");
        // Move to (0, 0) so Up becomes eligible.
        b.handle_key(press_c(KeyCode::Char('a')));
        b.handle_key(press(KeyCode::Up));
        assert_eq!(b.as_string(), "two");
        b.handle_key(press(KeyCode::Down));
        // Past the newest → draft restored.
        assert_eq!(b.as_string(), "draft");
    }

    #[test]
    fn history_dedups_consecutive_submits() {
        let mut b = TextBuffer::new();
        submit(&mut b, "same");
        submit(&mut b, "same");
        submit(&mut b, "different");
        // History should be ["same", "different"] — the duplicate dropped.
        b.handle_key(press(KeyCode::Up));
        assert_eq!(b.as_string(), "different");
        b.handle_key(press(KeyCode::Up));
        assert_eq!(b.as_string(), "same");
    }

    #[test]
    fn typing_while_in_history_exits_history_mode() {
        let mut b = TextBuffer::new();
        submit(&mut b, "first");
        submit(&mut b, "second");
        b.handle_key(press(KeyCode::Up));
        assert_eq!(b.as_string(), "second");
        // Edit the recalled text — should exit history so Down doesn't
        // wipe the edits.
        type_str(&mut b, "-edit");
        assert_eq!(b.as_string(), "second-edit");
        b.handle_key(press(KeyCode::Down));
        // No effect (history_index is None) — edits stay.
        assert_eq!(b.as_string(), "second-edit");
    }

    #[test]
    fn history_caps_at_100() {
        let mut b = TextBuffer::new();
        for i in 0..120 {
            submit(&mut b, &format!("cmd-{i}"));
        }
        // History holds the newest 100. Up 100 times reaches cmd-20.
        for _ in 0..100 {
            b.handle_key(press(KeyCode::Up));
        }
        assert_eq!(b.as_string(), "cmd-20");
    }

    #[test]
    fn slash_reports_action() {
        let mut b = TextBuffer::new();
        let a = b.handle_key(press(KeyCode::Char('/')));
        assert!(matches!(a, Action::SlashChanged(ref s) if s == "/"));
        let a = b.handle_key(press(KeyCode::Char('c')));
        assert!(matches!(a, Action::SlashChanged(ref s) if s == "/c"));
    }

    #[test]
    fn history_persists_to_disk() {
        let dir = std::env::temp_dir().join("nanopi-textbuffer-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-history.txt");
        let _ = std::fs::remove_file(&path);

        // Submit entries — should persist to disk.
        let mut b = TextBuffer::with_history(path.clone());
        submit(&mut b, "alpha");
        submit(&mut b, "beta");
        submit(&mut b, "gamma");

        // Verify the file was written.
        let contents = std::fs::read_to_string(&path).expect("history file should exist");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines, vec!["alpha", "beta", "gamma"]);

        // Load a fresh buffer from the same path — should recall history.
        let mut b2 = TextBuffer::with_history(path.clone());
        b2.handle_key(press(KeyCode::Up));
        assert_eq!(b2.as_string(), "gamma");
        b2.handle_key(press(KeyCode::Up));
        assert_eq!(b2.as_string(), "beta");
        b2.handle_key(press(KeyCode::Up));
        assert_eq!(b2.as_string(), "alpha");

        // Cleanup.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Regression: a bracketed paste arrives with the terminal's own
    /// line endings, and in raw mode that means bare CR. All three
    /// encodings must produce the same rows, with no stray control
    /// characters left behind.
    #[test]
    fn paste_normalizes_line_endings_and_tabs() {
        for src in ["a\nb\nc", "a\r\nb\r\nc", "a\rb\rc", "a\r\nb\rc"] {
            let mut b = TextBuffer::new();
            b.insert_str(src);
            assert_eq!(
                b.lines(),
                &["a".to_string(), "b".into(), "c".into()],
                "paste of {src:?} produced wrong rows"
            );
        }

        // Tabs expand to four spaces, matching PI's normalizeText.
        let mut b = TextBuffer::new();
        b.insert_str("a\tb");
        assert_eq!(b.as_string(), "a    b");

        // Cursor lands after the last inserted char, not mid-CR.
        let mut b = TextBuffer::new();
        b.insert_str("ab\rcd");
        assert_eq!(b.cursor(), (1, 2));
    }
}

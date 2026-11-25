//! Read-only live panel for one agent, shown on the right of a full-screen
//! split while the orchestrator keeps the left side and the keyboard.
//!
//! The TUI is normally inline (scrollback above a fixed dock), which has no
//! room for a side pane. While the panel is open the TUI switches to the
//! alternate screen and draws a split: the left column is the recent
//! conversation plus the usual dock, the right column is this panel. Every
//! scrollback line produced meanwhile is held back and written to the real
//! scrollback when the panel closes, so nothing is lost.
//!
//! Nothing here touches the agent: the panel only reads its transcript
//! file (bounded tail, re-read only when the file changes) and the
//! registry's state.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::agent_detail::{panel_lines, CardLine, LineKind};
use super::agents_strip::{display_width, format_elapsed, read_tail_lines, truncate_to_width};
use crate::agent_registry::{AgentEntry, AgentState};

/// Narrowest terminal that gets the split; below it the card is printed.
pub const MIN_SPLIT_WIDTH: u16 = 90;
/// Scrollback lines kept in memory for the left pane.
const RING_CAP: usize = 2000;

// ── scrollback record ────────────────────────────────────────────────

/// One scrollback line as the TUI emitted it: styled spans, plus the
/// full-row background of a "bar" line (user echo, tool output).
#[derive(Clone, Debug)]
pub struct Recorded {
    pub spans: Vec<(String, Style)>,
    pub bg: Option<Style>,
}

#[derive(Default)]
struct Ring {
    lines: std::collections::VecDeque<Recorded>,
    /// While the panel is open: lines not yet written to the terminal.
    held: Option<Vec<Recorded>>,
}

static RING: Mutex<Ring> = Mutex::new(Ring { lines: std::collections::VecDeque::new(), held: None });

fn ring() -> std::sync::MutexGuard<'static, Ring> {
    RING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record a scrollback line. Returns `true` when it must NOT be written
/// to the terminal now (the panel is open; it is held for later).
pub fn record(line: &Line<'_>, bg: Option<Style>) -> bool {
    let rec = Recorded {
        spans: line.spans.iter().map(|s| (s.content.to_string(), s.style)).collect(),
        bg,
    };
    let mut r = ring();
    r.lines.push_back(rec.clone());
    while r.lines.len() > RING_CAP {
        r.lines.pop_front();
    }
    match r.held.as_mut() {
        Some(h) => {
            h.push(rec);
            true
        }
        None => false,
    }
}

/// Start holding scrollback lines back (the panel opened).
pub fn hold() {
    let mut r = ring();
    if r.held.is_none() {
        r.held = Some(Vec::new());
    }
}

/// Stop holding; returns the lines to write to the real scrollback now.
pub fn release() -> Vec<Recorded> {
    ring().held.take().unwrap_or_default()
}

/// The last `n` recorded lines, oldest first.
pub fn recent(n: usize) -> Vec<Recorded> {
    let r = ring();
    r.lines.iter().skip(r.lines.len().saturating_sub(n)).cloned().collect()
}

// ── the panel's view state ───────────────────────────────────────────

pub struct PanelView {
    pub id: String,
    pub label: Option<String>,
    transcript: PathBuf,
    /// (length, mtime) of the transcript when last read.
    seen: Option<(u64, Option<SystemTime>)>,
    lines: Vec<CardLine>,
    /// Pages scrolled up from the bottom; 0 follows the tail.
    pages_up: std::cell::Cell<usize>,
    /// The most pages there are above the tail, as of the last draw.
    max_pages: std::cell::Cell<usize>,
    /// While scrolled up: the row count the view is anchored to, so new
    /// output from the agent does not move it.
    anchor: std::cell::Cell<Option<usize>>,
    pub state: AgentState,
    started: Instant,
    finished: Option<Instant>,
}

impl PanelView {
    pub fn new(entry: &AgentEntry, label: Option<String>) -> Self {
        let mut v = Self {
            id: entry.id.clone(),
            label,
            transcript: entry.dir.join("transcript.jsonl"),
            seen: None,
            lines: Vec::new(),
            pages_up: std::cell::Cell::new(0),
            max_pages: std::cell::Cell::new(usize::MAX),
            anchor: std::cell::Cell::new(None),
            state: entry.state,
            started: entry.started,
            finished: entry.finished,
        };
        v.refresh(Some(entry));
        v
    }

    /// Pick up the agent's latest state and transcript. The transcript is
    /// re-read only when its size or mtime changed. Returns whether
    /// anything visible changed.
    pub fn refresh(&mut self, entry: Option<&AgentEntry>) -> bool {
        let mut changed = false;
        if let Some(e) = entry {
            changed |= e.state != self.state || e.finished != self.finished;
            self.state = e.state;
            self.finished = e.finished;
        }
        let meta = std::fs::metadata(&self.transcript).ok();
        let key = meta.map(|m| (m.len(), m.modified().ok()));
        if key != self.seen {
            self.seen = key;
            let (tail, cut) = read_tail_lines(&self.transcript);
            self.lines = panel_lines(&tail, cut);
            changed = true;
        }
        changed
    }

    /// Scroll by whole pages (positive = up), never past the top.
    pub fn scroll(&mut self, pages: i32) {
        let cur = self.pages_up.get();
        let next = if pages >= 0 {
            cur.saturating_add(pages as usize)
        } else {
            cur.saturating_sub(pages.unsigned_abs() as usize)
        };
        self.pages_up.set(next.min(self.max_pages.get()));
        if self.pages_up.get() == 0 {
            self.anchor.set(None);
        }
    }

    pub fn pages_up(&self) -> usize {
        self.pages_up.get()
    }

    /// Whether the clock in the title is still moving.
    pub fn live(&self) -> bool {
        matches!(self.state, AgentState::Queued | AgentState::Running)
    }
}

// ── rendering ────────────────────────────────────────────────────────

/// Split `area` into (left, right). The panel gets ~45% of the width.
pub fn split(area: Rect) -> (Rect, Rect) {
    // In u32: `width * 45` overflows u16 above 1456 columns.
    let right_w = (u32::from(area.width) * 45 / 100) as u16;
    let right_w = right_w.max(40).min(area.width.saturating_sub(40));
    let left = Rect { width: area.width - right_w, ..area };
    let right = Rect { x: area.x + left.width, width: right_w, ..area };
    (left, right)
}

/// Wrap `text` to `width` display columns (CJK counts two).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for c in text.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    out.push(cur);
    out
}

fn style_of(kind: LineKind, state: AgentState) -> Style {
    match kind {
        LineKind::Header => Style::default().fg(Color::Cyan),
        LineKind::Section => Style::default().fg(Color::Indexed(108)),
        LineKind::Body => Style::default(),
        LineKind::Dim | LineKind::Path => Style::default().fg(Color::DarkGray),
        LineKind::Error => Style::default().fg(match state {
            AgentState::Failed => Color::Red,
            _ => Color::Yellow,
        }),
    }
}

/// The panel's title: id, label, state, elapsed, and that it is read-only.
pub fn title(v: &PanelView, now: Instant) -> String {
    let secs = v.finished.unwrap_or(now).saturating_duration_since(v.started).as_secs();
    let label = v.label.as_deref().map(|l| format!(" {l}")).unwrap_or_default();
    format!(" {}{} · {} · {} · read-only ", v.id, label, v.state.as_str(), format_elapsed(secs))
}

/// Draw the panel into `area` (border included).
pub fn draw_panel(buf: &mut Buffer, area: Rect, v: &PanelView, now: Instant) {
    let border = match v.state {
        AgentState::Running | AgentState::Queued => Color::Cyan,
        AgentState::Failed => Color::Red,
        AgentState::Completed => Color::Green,
        _ => Color::DarkGray,
    };
    let hint = if v.pages_up() > 0 {
        format!(" ↑{} page(s) · PgDn back to live ", v.pages_up())
    } else {
        " PgUp/PgDn scroll · Esc (empty prompt) closes ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            truncate_to_width(&title(v, now), area.width.saturating_sub(2) as usize),
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    let inner = block.inner(area);
    block.render(area, buf);
    let width = inner.width as usize;
    let height = inner.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut rows: Vec<Line> = Vec::new();
    for l in &v.lines {
        for piece in wrap(&l.text, width) {
            rows.push(Line::from(Span::styled(piece, style_of(l.kind, v.state))));
        }
    }
    if rows.is_empty() {
        rows.push(Line::from(Span::styled(
            "(no activity yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let total = rows.len();
    let anchored_total = v.anchor.get().unwrap_or(total).min(total);
    let max_pages = anchored_total.saturating_sub(height).div_ceil(height);
    v.max_pages.set(max_pages);
    let pages = v.pages_up.get().min(max_pages);
    v.pages_up.set(pages);
    let base = if pages == 0 {
        v.anchor.set(None);
        total
    } else {
        // Anchor on first draw after scrolling up; later output is below.
        let a = anchored_total;
        v.anchor.set(Some(a));
        a
    };
    let bottom = base.saturating_sub(pages * height).max(height.min(total));
    let top = bottom.saturating_sub(height);
    Paragraph::new(rows[top..bottom].to_vec()).render(inner, buf);
}

/// Draw the recent conversation into `area`, bottom-aligned.
pub fn draw_conversation(buf: &mut Buffer, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut rows: Vec<Line> = Vec::new();
    for rec in recent(height * 4) {
        let text: String = rec.spans.iter().map(|(t, _)| t.as_str()).collect();
        if text.is_empty() {
            rows.push(Line::from(""));
            continue;
        }
        // Keep each span's style; wrap by re-splitting per span.
        let mut cur: Vec<Span> = Vec::new();
        let mut w = 0usize;
        for (t, st) in &rec.spans {
            let style = match rec.bg {
                Some(bg) => bg.patch(*st),
                None => *st,
            };
            let mut piece = String::new();
            for c in t.chars() {
                let cw = display_width(&c.to_string());
                if w + cw > width {
                    if !piece.is_empty() {
                        cur.push(Span::styled(std::mem::take(&mut piece), style));
                    }
                    rows.push(Line::from(std::mem::take(&mut cur)));
                    w = 0;
                }
                piece.push(c);
                w += cw;
            }
            if !piece.is_empty() {
                cur.push(Span::styled(piece, style));
            }
        }
        rows.push(Line::from(cur));
    }
    let top = rows.len().saturating_sub(height);
    let shown = rows[top..].to_vec();
    let pad = height - shown.len();
    let area = Rect { y: area.y + pad as u16, height: shown.len() as u16, ..area };
    Paragraph::new(shown).render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(dir: &std::path::Path, state: AgentState) -> AgentEntry {
        AgentEntry {
            id: "a1".into(),
            pid: None,
            state,
            started: Instant::now(),
            finished: None,
            dir: dir.to_path_buf(),
        }
    }

    fn text(buf: &Buffer, area: Rect) -> String {
        let mut s = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn panel_follows_the_transcript_and_says_it_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("transcript.jsonl");
        std::fs::write(&t, "").unwrap();
        let mut v = PanelView::new(&entry(dir.path(), AgentState::Running), Some("migrate".into()));
        let area = Rect::new(0, 0, 50, 8);
        let mut buf = Buffer::empty(area);
        draw_panel(&mut buf, area, &v, Instant::now());
        let s = text(&buf, area);
        assert!(s.contains("a1 migrate") && s.contains("running") && s.contains("read-only"), "{s}");
        assert!(s.contains("no activity yet"), "{s}");

        // The agent writes more; the next refresh shows the newest lines.
        let mut body = String::new();
        for i in 0..30 {
            body.push_str(&serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"assistant","content":format!("step {i}")}).to_string());
            body.push('\n');
        }
        std::fs::write(&t, body).unwrap();
        assert!(v.refresh(None));
        assert!(!v.refresh(None), "unchanged file is not re-read");
        let mut buf = Buffer::empty(area);
        draw_panel(&mut buf, area, &v, Instant::now());
        let s = text(&buf, area);
        assert!(s.contains("step 29") && !s.contains("step 0 "), "{s}");
        // PgUp shows older lines; PgDn returns to the live tail.
        v.scroll(1);
        let mut buf = Buffer::empty(area);
        draw_panel(&mut buf, area, &v, Instant::now());
        assert!(!text(&buf, area).contains("step 29"));
        v.scroll(-5);
        assert_eq!(v.pages_up(), 0);
    }

    #[test]
    fn held_lines_are_released_once_and_recorded_for_the_left_pane() {
        let _g = crate::test_lock();
        let _ = release();
        assert!(!record(&Line::from("before"), None), "not held: written now");
        hold();
        assert!(record(&Line::from("while open"), None), "held while the panel is open");
        let out = release();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].spans[0].0, "while open");
        assert!(release().is_empty());
        let recent: Vec<String> = recent(2).into_iter().map(|r| r.spans[0].0.clone()).collect();
        assert_eq!(recent, ["before", "while open"]);
    }

    fn write_steps(t: &std::path::Path, n: usize) {
        let mut body = String::new();
        for i in 0..n {
            body.push_str(&serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"assistant","content":format!("step {i}")}).to_string());
            body.push('\n');
        }
        std::fs::write(t, body).unwrap();
    }

    /// PgUp stops at the top of the transcript, and the counter does not
    /// keep growing past it.
    #[test]
    fn scrolling_up_is_capped_at_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("transcript.jsonl");
        write_steps(&t, 30);
        let mut v = PanelView::new(&entry(dir.path(), AgentState::Running), None);
        // 8 rows minus 2 borders = 6 visible: 30 rows give 4 pages above the tail.
        let area = Rect::new(0, 0, 50, 8);
        draw_panel(&mut Buffer::empty(area), area, &v, Instant::now());
        v.scroll(50);
        assert_eq!(v.pages_up(), 4);
        let mut buf = Buffer::empty(area);
        draw_panel(&mut buf, area, &v, Instant::now());
        assert!(text(&buf, area).contains("step 0"), "the top is reachable");
        v.scroll(-1);
        assert_eq!(v.pages_up(), 3, "one PgDn moves one page");
    }

    /// While scrolled up, new output from the agent does not move the view.
    #[test]
    fn scrolled_view_stays_put_while_the_agent_writes() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("transcript.jsonl");
        write_steps(&t, 30);
        let mut v = PanelView::new(&entry(dir.path(), AgentState::Running), None);
        let area = Rect::new(0, 0, 50, 8);
        draw_panel(&mut Buffer::empty(area), area, &v, Instant::now());
        v.scroll(1);
        let mut before = Buffer::empty(area);
        draw_panel(&mut before, area, &v, Instant::now());
        write_steps(&t, 45);
        assert!(v.refresh(None));
        let mut after = Buffer::empty(area);
        draw_panel(&mut after, area, &v, Instant::now());
        let inner = Rect::new(1, 1, 48, 6);
        assert_eq!(text(&before, inner), text(&after, inner));
        // Back at the bottom it follows the tail again.
        v.scroll(-1);
        let mut live = Buffer::empty(area);
        draw_panel(&mut live, area, &v, Instant::now());
        assert!(text(&live, inner).contains("step 44"));
    }

    #[test]
    fn split_does_not_overflow_on_very_wide_terminals() {
        let (l, r) = split(Rect::new(0, 0, 2000, 10));
        assert_eq!(l.width + r.width, 2000);
        assert_eq!(r.width, 900);
    }

    #[test]
    fn split_and_wrap() {
        let (l, r) = split(Rect::new(0, 0, 120, 40));
        assert_eq!(l.width + r.width, 120);
        assert!(r.width >= 40 && l.width >= 40);
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("你好世界", 4), ["你好", "世界"]);
    }
}

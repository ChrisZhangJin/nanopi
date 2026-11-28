//! The live compaction panel drawn above the input box.
//!
//! Three rows, animated on the TUI tick:
//!
//! ```text
//!  ⠹ Compacting context ▕██████████▊░░░░░░░░░▏  54%  12.4s
//!    ✓ 186 msgs  ›  ✎ Key Decisions ●●●●●●◉○○ 7/9  ›  ○ rebuild
//!    ~103k tokens · 3.1k chars written · ⏎ 1 queued: "fix the tests"
//! ```
//!
//! The bar fills along a cyan → magenta ramp with a highlight sweeping
//! across the filled part; before the first token it shows a comet
//! bouncing along the track instead of a number. When the pass ends the
//! panel turns green and shows the saving for a moment.
//!
//! Pure: everything comes from the [`Progress`] snapshot and the clock.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::agents_strip::{display_width, truncate_to_width};
use crate::agent::compact_progress::{Phase, Progress, SECTIONS};

/// Rows the panel wants.
pub const HEIGHT: u16 = 3;

/// Cyan → blue → violet → magenta (xterm-256).
const RAMP: [u8; 8] = [51, 45, 39, 33, 63, 99, 135, 171];
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Partial cells for the leading edge, in eighths.
const EIGHTHS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

const TRACK: Color = Color::Indexed(238);
const GREEN: Color = Color::Indexed(42);
const DIM: Color = Color::DarkGray;
const QUEUE: Color = Color::Indexed(179);

fn ramp(i: usize, n: usize) -> Color {
    let k = if n <= 1 { 0 } else { i.min(n - 1) * (RAMP.len() - 1) / (n - 1) };
    Color::Indexed(RAMP[k])
}

fn spinner(ms: u128) -> &'static str {
    SPINNER[(ms / 80) as usize % SPINNER.len()]
}

pub fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

fn fmt_chars(n: usize) -> String {
    crate::models::fmt_tokens(n.min(u32::MAX as usize) as u32)
}

fn tokens(chars: usize) -> String {
    fmt_chars(chars / 4)
}

/// The bar's cells: `(symbol, style)` for `width` columns.
/// `frac == None` draws the indeterminate comet.
pub fn bar_cells(width: usize, frac: Option<f64>, done: bool, ms: u128) -> Vec<(&'static str, Style)> {
    let mut cells = vec![("░", Style::default().fg(TRACK)); width];
    if width == 0 {
        return cells;
    }
    if done {
        for c in cells.iter_mut() {
            *c = ("█", Style::default().fg(GREEN));
        }
        return cells;
    }
    let Some(frac) = frac else {
        // A comet bouncing along the track.
        let len = (width / 4).clamp(1, 10);
        let span = width - len;
        let t = (ms / 30) as usize;
        let pos = if span == 0 {
            0
        } else {
            let period = 2 * span;
            let t = t % period;
            if t <= span { t } else { period - t }
        };
        for j in 0..len {
            let sym = if j == 0 || j + 1 == len { "▒" } else if j == 1 || j + 2 == len { "▓" } else { "█" };
            cells[pos + j] = (sym, Style::default().fg(ramp(pos + j, width)));
        }
        return cells;
    };
    let units = (frac.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full = (units / 8).min(width);
    let rem = units % 8;
    // A highlight sweeps across the filled part, with a pause off the end.
    let sweep = (ms / 35) as usize % (full + 14);
    for (i, c) in cells.iter_mut().enumerate().take(full) {
        let fg = match sweep.abs_diff(i) {
            0 => Color::Indexed(231),
            1 => Color::Indexed(225),
            _ => ramp(i, width),
        };
        *c = ("█", Style::default().fg(fg));
    }
    if full < width && rem > 0 {
        cells[full] = (EIGHTHS[rem], Style::default().fg(ramp(full, width)));
    }
    cells
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| display_width(&s.content)).sum()
}

/// Draw the panel into `area` (up to [`HEIGHT`] rows are used, bottom-aligned).
pub fn draw(buf: &mut Buffer, area: Rect, p: &Progress, now: Instant, queued: &std::collections::VecDeque<String>) {
    if area.width < 10 || area.height == 0 {
        return;
    }
    let h = area.height.min(HEIGHT);
    let area = Rect { y: area.y + area.height - h, height: h, ..area };
    let width = area.width as usize;
    let ms = now.saturating_duration_since(p.started).as_millis();
    let done = p.phase == Phase::Done;
    let frac = p.fraction();

    // ── Row 1: title, bar, percent, clock ────────────────────────────
    let head = if done {
        vec![Span::styled(" ✓ ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
             Span::styled("Context compacted ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD))]
    } else {
        vec![
            Span::styled(format!(" {} ", spinner(ms)), Style::default().fg(ramp((ms / 120) as usize % RAMP.len(), RAMP.len())).add_modifier(Modifier::BOLD)),
            Span::styled("Compacting context ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]
    };
    let pct = match frac {
        Some(f) => format!(" {:>3}%", (f * 100.0).floor() as u32),
        None => format!(" {:<4}", ".".repeat((ms / 300) as usize % 4)),
    };
    let tail = vec![
        Span::styled(pct, Style::default().fg(if done { GREEN } else { Color::White }).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", fmt_elapsed(p.elapsed(now))), Style::default().fg(DIM)),
    ];
    let mut head = head;
    let mut bar_w = width.saturating_sub(spans_width(&head) + spans_width(&tail) + 2 + 1);
    if bar_w < 8 {
        // Narrow: keep the spinner, drop the title.
        head.truncate(1);
        bar_w = width.saturating_sub(spans_width(&head) + spans_width(&tail) + 2 + 1);
    }
    let edge = Style::default().fg(if done { GREEN } else { TRACK });
    let mut row1 = head;
    row1.push(Span::styled("▕", edge));
    for (sym, st) in bar_cells(bar_w, frac, done, ms) {
        row1.push(Span::styled(sym, st));
    }
    row1.push(Span::styled("▏", edge));
    row1.extend(tail);
    buf.set_line(area.x, area.y, &Line::from(row1), area.width);
    if h < 2 {
        return;
    }

    // ── Row 2: the three stages ──────────────────────────────────────
    let sep = || Span::styled("  ›  ", Style::default().fg(DIM));
    let ok = Style::default().fg(GREEN);
    let mut row2 = vec![
        Span::raw("   "),
        Span::styled(format!("✓ {} msgs", p.messages), ok),
        sep(),
    ];
    let current = p.section.unwrap_or(0);
    let dots = |row: &mut Vec<Span>, all_done: bool| {
        row.push(Span::raw(" "));
        for k in 0..SECTIONS.len() {
            let (sym, st) = if all_done || k < current {
                ("●", Style::default().fg(GREEN))
            } else if k == current && p.section.is_some() {
                let pulse = (ms / 400) % 2 == 0;
                ("◉", Style::default().fg(if pulse { Color::Indexed(213) } else { Color::Indexed(51) }))
            } else {
                ("○", Style::default().fg(TRACK))
            };
            row.push(Span::styled(sym, st));
        }
    };
    match p.phase {
        Phase::Waiting => {
            row2.push(Span::styled(format!("{} waiting for the model", spinner(ms)), Style::default().fg(Color::Cyan)));
            row2.push(sep());
            row2.push(Span::styled("○ rebuild", Style::default().fg(DIM)));
        }
        Phase::Writing => {
            let name = p.section_name().unwrap_or("Summary");
            row2.push(Span::styled(format!("✎ {name}"), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
            dots(&mut row2, false);
            row2.push(Span::styled(
                format!(" {}/{}", p.section.map_or(0, |s| s + 1), SECTIONS.len()),
                Style::default().fg(DIM),
            ));
            row2.push(sep());
            row2.push(Span::styled("○ rebuild", Style::default().fg(DIM)));
        }
        Phase::Rebuilding | Phase::Done => {
            row2.push(Span::styled("✓ summary", ok));
            dots(&mut row2, true);
            row2.push(sep());
            if done {
                row2.push(Span::styled("✓ saved", ok));
            } else {
                row2.push(Span::styled(format!("{} rebuilding", spinner(ms)), Style::default().fg(Color::Yellow)));
            }
        }
    }
    buf.set_line(area.x, area.y + 1, &Line::from(row2), area.width);
    if h < 3 {
        return;
    }

    // ── Row 3: numbers, and what waits for the pass ──────────────────
    let mut row3 = vec![Span::raw("   ")];
    let stats = match (p.phase, p.chars_after) {
        (Phase::Done, Some(after)) => {
            let saved = if p.chars_before > 0 {
                p.chars_before.saturating_sub(after) * 100 / p.chars_before
            } else {
                0
            };
            let via = if p.used_llm { "" } else { " · truncated (no summary)" };
            vec![
                Span::styled(format!("~{} → {} tokens ", tokens(p.chars_before), tokens(after)), Style::default().fg(Color::White)),
                Span::styled(format!("(−{saved}%)"), Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
                Span::styled(via.to_string(), Style::default().fg(DIM)),
            ]
        }
        _ => {
            let mut s = format!("~{} tokens", tokens(p.chars_before));
            if p.streamed > 0 {
                s.push_str(&format!(" · {} chars written", fmt_chars(p.streamed)));
            }
            if p.reason != "manual" {
                s.push_str(&format!(" · {}", p.reason));
            }
            vec![Span::styled(s, Style::default().fg(DIM))]
        }
    };
    row3.extend(stats);
    if let Some(last) = queued.back() {
        let n = queued.len();
        let what = if n == 1 { "message".to_string() } else { format!("{n} messages") };
        let verb = if done { format!("⏎ sending {what}") } else if n == 1 { "⏎ 1 queued".to_string() } else { format!("⏎ {n} queued") };
        row3.push(Span::styled("  ·  ", Style::default().fg(DIM)));
        row3.push(Span::styled(verb, Style::default().fg(QUEUE).add_modifier(Modifier::BOLD)));
        let room = width.saturating_sub(spans_width(&row3) + 4);
        if room >= 8 {
            let first_line = last.lines().next().unwrap_or("");
            row3.push(Span::styled(format!(": “{}”", truncate_to_width(&super::agents_strip::sanitize(first_line), room)), Style::default().fg(QUEUE)));
        }
    }
    buf.set_line(area.x, area.y + 2, &Line::from(row3), area.width);
}

/// Rows the Dream panel wants.
pub const DREAM_HEIGHT: u16 = 2;

/// Dream's live panel above the input, while it runs in the background:
///
/// ```text
///  ⠹ Dream · claude-opus-5-5 · 3 notes · unprocessed notes        12.4s
///    ▕██████▊░░░░░░░░░░░░░░░░░▏ asking the model · call 1 of 3 · 2 changes
/// ```
pub fn draw_dream(buf: &mut Buffer, area: Rect, p: &crate::memory::dream_progress::Progress, now: Instant) {
    use crate::memory::dream_progress::Phase;
    if area.width < 20 || area.height == 0 {
        return;
    }
    let h = area.height.min(DREAM_HEIGHT);
    let area = Rect { y: area.y + area.height - h, height: h, ..area };
    let width = area.width as usize;
    let ms = now.saturating_duration_since(p.started).as_millis();
    let dim = Style::default().fg(DIM);
    // The note count once known (row two says "reading notes" before).
    let notes = match p.notes_total {
        0 => String::new(),
        1 => " · 1 note".to_string(),
        n => format!(" · {n} notes"),
    };
    let left = vec![
        Span::styled(format!(" {} ", spinner(ms)), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled("Dream", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {}{notes} · {}", p.model, p.range), dim),
    ];
    let clock = format!("{} ", fmt_elapsed(now.saturating_duration_since(p.started)));
    let mut row1 = left;
    let used = spans_width(&row1);
    if used + clock.len() < width {
        row1.push(Span::raw(" ".repeat(width - used - clock.len())));
        row1.push(Span::styled(clock, dim));
    }
    buf.set_line(area.x, area.y, &Line::from(row1), area.width);
    if h < 2 {
        return;
    }
    let what = match p.phase {
        Phase::Preparing => "reading notes…".to_string(),
        Phase::Asking => format!("asking the model · call {} of at most {}", p.calls, p.max_calls),
        Phase::Applying => "writing changes into the library".to_string(),
    };
    let changes = match p.changes {
        0 => String::new(),
        1 => " · 1 change".to_string(),
        n => format!(" · {n} changes"),
    };
    // Determinate by notes once some are done; a comet while it waits.
    let frac = p.fraction().filter(|f| *f > 0.0);
    let bar_w = (width / 4).clamp(8, 28);
    let mut row2 = vec![Span::raw("   "), Span::styled("▕", Style::default().fg(TRACK))];
    for (sym, st) in bar_cells(bar_w, frac, false, ms) {
        row2.push(Span::styled(sym, st));
    }
    row2.push(Span::styled("▏ ", Style::default().fg(TRACK)));
    row2.push(Span::styled(what, Style::default().fg(Color::Gray)));
    row2.push(Span::styled(changes, Style::default().fg(GREEN)));
    buf.set_line(area.x, area.y + 1, &Line::from(row2), area.width);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(buf: &Buffer) -> String {
        let a = buf.area;
        let mut s = String::new();
        for y in a.y..a.y + a.height {
            for x in a.x..a.x + a.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn render(p: &Progress, now: Instant, queued: &[String]) -> String {
        let area = Rect::new(0, 0, 90, 3);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, p, now, &queued.iter().cloned().collect());
        text(&buf)
    }

    #[test]
    fn waiting_shows_a_comet_not_a_number() {
        let t0 = Instant::now();
        let p = Progress::new("manual", 186, 412_000, t0);
        let s = render(&p, t0 + Duration::from_millis(500), &[]);
        assert!(s.contains("Compacting context"), "{s}");
        assert!(s.contains("waiting for the model"), "{s}");
        assert!(!s.contains('%'), "no percent before the first token: {s}");
        assert!(s.contains("✓ 186 msgs") && s.contains("~103.0k tokens"), "{s}");
        // The comet moves.
        let later = render(&p, t0 + Duration::from_millis(900), &[]);
        assert_ne!(s.lines().next(), later.lines().next());
    }

    #[test]
    fn writing_shows_the_section_percent_and_queue() {
        let t0 = Instant::now();
        let mut p = Progress::new("threshold", 186, 412_000, t0);
        for name in &SECTIONS[..7] {
            p.push(&format!("## {name}\n- point\n"));
        }
        let queued = vec!["fix the failing test\nand more".to_string()];
        let s = render(&p, t0 + Duration::from_millis(12_400), &queued);
        assert!(s.contains("✎ Key Decisions"), "{s}");
        assert!(s.contains("●●●●●●◉○○") && s.contains("7/9"), "{s}");
        let pct = (p.fraction().unwrap() * 100.0).floor() as u32;
        assert!(s.contains(&format!("{pct}%")) && s.contains("12.4s"), "{s}");
        assert!(s.contains("⏎ 1 queued: “fix the failing test”"), "{s}");
        assert!(s.contains("threshold"), "auto passes say why: {s}");
    }

    #[test]
    fn done_turns_green_and_shows_the_saving() {
        let t0 = Instant::now();
        let mut p = Progress::new("manual", 186, 412_000, t0);
        p.push("## Goal\n");
        p.phase = Phase::Done;
        p.chars_after = Some(38_000);
        p.used_llm = true;
        p.finished = Some(t0 + Duration::from_millis(14_200));
        let s = render(&p, t0 + Duration::from_secs(15), &["next".into(), "after".into()]);
        assert!(s.contains("Context compacted") && s.contains("100%") && s.contains("14.2s"), "{s}");
        assert!(s.contains("~103.0k → 9.5k tokens (−90%)"), "{s}");
        assert!(s.contains("●●●●●●●●●") && s.contains("✓ saved"), "{s}");
        assert!(s.contains("⏎ sending 2 messages"), "{s}");
    }

    #[test]
    fn bar_fills_in_proportion_with_a_partial_edge() {
        let cells = bar_cells(40, Some(0.5), false, 0);
        assert_eq!(cells.len(), 40);
        assert_eq!(cells.iter().filter(|c| c.0 == "█").count(), 20);
        assert_eq!(cells[20].0, "░");
        let cells = bar_cells(40, Some(0.51), false, 0);
        assert!(EIGHTHS.contains(&cells[20].0) && cells[20].0 != " ", "{:?}", cells[20]);
        assert!(bar_cells(40, Some(1.0), true, 0).iter().all(|c| c.0 == "█"));
        // Indeterminate: never panics at small widths, stays in bounds.
        for w in 0..12 {
            for ms in (0..5000).step_by(37) {
                assert_eq!(bar_cells(w, None, false, ms).len(), w);
            }
        }
    }

    #[test]
    fn dream_panel_shows_phase_counts_and_motion() {
        use crate::memory::dream_progress::{Phase, Progress};
        let t0 = Instant::now();
        let mut p = Progress {
            model: "claude-opus-5-5".into(),
            range: "unprocessed notes".into(),
            started: t0,
            phase: Phase::Asking,
            notes_total: 3,
            notes_done: 0,
            calls: 1,
            max_calls: 3,
            changes: 0,
        };
        let render = |p: &Progress, ms| {
            let area = Rect::new(0, 0, 100, 2);
            let mut buf = Buffer::empty(area);
            draw_dream(&mut buf, area, p, t0 + Duration::from_millis(ms));
            text(&buf)
        };
        let s = render(&p, 400);
        assert!(s.contains("Dream · claude-opus-5-5 · 3 notes · unprocessed notes"), "{s}");
        assert!(s.contains("asking the model · call 1 of at most 3"), "{s}");
        assert_ne!(s, render(&p, 900), "the comet moves while it waits");
        p.phase = Phase::Applying;
        p.notes_done = 3;
        p.changes = 2;
        let s = render(&p, 1000);
        assert!(s.contains("writing changes into the library · 2 changes"), "{s}");
    }

    #[test]
    fn short_areas_keep_the_bar_row() {
        let t0 = Instant::now();
        let p = Progress::new("manual", 3, 4000, t0);
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &p, t0, &Default::default());
        let s = text(&buf);
        assert!(s.contains('▕') && s.contains('▏'), "{s}");
    }
}

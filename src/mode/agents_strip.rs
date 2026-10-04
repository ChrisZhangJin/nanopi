//! Agents-strip data model and pure renderer (Phase 5, D-02/D-03/D-07/D-08).
//!
//! Isolated from `tui.rs` so it is unit-testable on a bare [`Buffer`] and so
//! the render path is provably IO-free (UI-04): `refresh()` is the only
//! function that touches the filesystem; `draw_agents_strip`,
//! `collapsed_rows` and `strip_height` read only in-memory cached data.
//!
//! Display-only (UI-03): this module has no function that takes
//! `&AgentRegistry` mutably or calls `stop`/`set_state`/`reactivate`.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::agent::brief::front_matter_get;
use crate::agent_registry::{AgentEntry, AgentState};

/// Re-read the transcript tail for a `Running` agent at most this often.
const TRANSCRIPT_THROTTLE_SECS: u64 = 1;
/// Read at most the last 64 KiB of `transcript.jsonl` (T-05-02).
const TRANSCRIPT_TAIL_BYTES: u64 = 64 * 1024;
/// Keep the last N tool_call entries as the activity log.
const ACTIVITY_LINES: usize = 3;

/// Glyph table (D-03). `Interrupted`/`waiting-for-permission` has no
/// dedicated glyph since the current revision never prompts.
pub fn glyph_for(state: AgentState) -> &'static str {
    match state {
        AgentState::Running => "●",
        AgentState::Queued => "◐",
        AgentState::Completed => "✓",
        AgentState::Failed => "✗",
        AgentState::Stopped => "■",
        AgentState::LimitReached => "⏱",
        AgentState::Interrupted => "?",
    }
}

/// Strip control characters from model-influenced text before it ever
/// reaches layout (T-05-01): escape sequences in brief/report/transcript
/// text must not reach the terminal.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Width-aware truncation (CJK-safe) using display columns, not chars.
fn truncate_to_width(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    s.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum()
}

/// Format an elapsed [`std::time::Duration`] as "Ns" / "MmSSs" / "HhMMm".
fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m{s:02}s")
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h{m:02}m")
    }
}

/// Cached, display-ready state for one agent. All fields are populated by
/// [`AgentsView::refresh`]; nothing here is read from disk by draw code.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub state: AgentState,
    pub started: Instant,
    /// Set the first time `state.is_terminal()` becomes true; used for
    /// "most recently finished" ordering.
    pub finished_seen: Option<Instant>,
    pub role: Option<String>,
    pub description: String,
    pub activity: Vec<String>,
    pub turns: Option<String>,
    pub tokens: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub report_path: PathBuf,
    pub report_exists: bool,
    pub dir: PathBuf,
    last_activity_read: Option<Instant>,
    last_state_for_reread: AgentState,
}

impl AgentInfo {
    fn new(entry: &AgentEntry, now: Instant) -> Self {
        let mut info = AgentInfo {
            id: entry.id.clone(),
            state: entry.state,
            started: entry.started,
            finished_seen: if entry.state.is_terminal() { Some(now) } else { None },
            role: None,
            description: String::new(),
            activity: Vec::new(),
            turns: None,
            tokens: None,
            worktree: None,
            branch: None,
            report_path: entry.dir.join("report.md"),
            report_exists: false,
            dir: entry.dir.clone(),
            last_activity_read: None,
            last_state_for_reread: entry.state,
        };
        info.reread_brief_and_report();
        info
    }

    fn reread_brief_and_report(&mut self) {
        let brief = std::fs::read_to_string(self.dir.join("brief.md")).unwrap_or_default();
        self.role = front_matter_get(&brief, "role").map(|s| sanitize(&s));
        let label = front_matter_get(&brief, "label")
            .filter(|s| !s.trim().is_empty() && s != "(none)");
        let description = label.unwrap_or_else(|| {
            // Fall back to the first non-empty line of the brief body, then role.
            body_first_line(&brief)
                .unwrap_or_else(|| self.role.clone().unwrap_or_default())
        });
        self.description = sanitize(&description);

        self.report_path = self.dir.join("report.md");
        let report = std::fs::read_to_string(&self.report_path).ok();
        self.report_exists = report.is_some();
        match &report {
            Some(r) => {
                self.turns = front_matter_get(r, "turns");
                self.tokens = front_matter_get(r, "tokens");
                self.worktree = front_matter_get(r, "worktree").map(|s| sanitize(&s));
                self.branch = front_matter_get(r, "branch").map(|s| sanitize(&s));
            }
            None => {
                self.worktree = front_matter_get(&brief, "worktree").map(|s| sanitize(&s));
                self.branch = front_matter_get(&brief, "branch").map(|s| sanitize(&s));
            }
        }
    }

    fn reread_activity(&mut self, now: Instant) {
        self.activity = tail_activity(&self.dir.join("transcript.jsonl"));
        self.last_activity_read = Some(now);
    }
}

/// Body text after the front-matter block: the first non-blank line.
fn body_first_line(brief: &str) -> Option<String> {
    let mut lines = brief.lines();
    let first = lines.next()?;
    if first.trim_end() != "---" {
        // No front matter block; treat whole content as body.
        return brief.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from);
    }
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            break;
        }
    }
    for line in lines {
        let t = line.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

/// Read at most the last [`TRANSCRIPT_TAIL_BYTES`] of `path` and return the
/// last [`ACTIVITY_LINES`] formatted `tool_call` entries. Malformed or
/// partial trailing lines are ignored; a missing file yields no activity
/// (T-05-02: never reads more than the capped tail, never on the draw path).
fn tail_activity(path: &std::path::Path) -> Vec<String> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        // Not valid UTF-8 from this offset (likely split a multi-byte
        // char or JSON line); fall back to no activity rather than panic.
        return Vec::new();
    }

    let mut lines: Vec<&str> = buf.split('\n').collect();
    // If we didn't start at byte 0, the first "line" is a partial
    // fragment of the previous line; drop it.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    // The very last element after a trailing split is "" when the file
    // ends with \n; if not, it's a partial (unterminated) line - drop it.
    if !buf.ends_with('\n') {
        lines.pop();
    }

    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("tool_call") {
            continue;
        }
        let tool_name = v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("?");
        let tool_name = truncate_to_width(&sanitize(tool_name), 40);
        let args = v.get("arguments");
        let short_arg = args.and_then(|a| {
            for key in ["path", "command", "pattern", "query"] {
                if let Some(s) = a.get(key).and_then(|x| x.as_str()) {
                    return Some(s.to_string());
                }
            }
            None
        });
        let formatted = match short_arg {
            Some(arg) => {
                let arg = truncate_to_width(&sanitize(&arg), 40);
                format!("{tool_name} {arg}")
            }
            None => tool_name.clone(),
        };
        out.push(formatted);
    }

    let n = out.len();
    if n > ACTIVITY_LINES {
        out.split_off(n - ACTIVITY_LINES)
    } else {
        out
    }
}

/// Cache of all agents observed this run, in last-seen-snapshot order.
#[derive(Debug, Default, Clone)]
pub struct AgentsView {
    agents: Vec<AgentInfo>,
}

impl AgentsView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn agents(&self) -> &[AgentInfo] {
        &self.agents
    }

    /// Update the cache from a fresh registry snapshot (UI-03: read-only
    /// w.r.t. the registry — only ever called with a `&[AgentEntry]` that
    /// was already snapshotted elsewhere).
    ///
    /// IO (brief.md/report.md reads) happens only when an id is first seen
    /// or its state changes; the transcript tail is re-read for `Running`
    /// agents at most once per second (T-05-02).
    pub fn refresh(&mut self, snapshot: &[AgentEntry], now: Instant) {
        use std::collections::HashSet;
        let seen: HashSet<&str> = snapshot.iter().map(|e| e.id.as_str()).collect();
        self.agents.retain(|a| seen.contains(a.id.as_str()));

        for entry in snapshot {
            if let Some(info) = self.agents.iter_mut().find(|a| a.id == entry.id) {
                let state_changed = info.last_state_for_reread != entry.state;
                info.state = entry.state;
                if state_changed {
                    if entry.state.is_terminal() && info.finished_seen.is_none() {
                        info.finished_seen = Some(now);
                    }
                    info.last_state_for_reread = entry.state;
                    info.reread_brief_and_report();
                }
                if entry.state == AgentState::Running {
                    let stale = match info.last_activity_read {
                        None => true,
                        Some(t) => now.duration_since(t).as_secs() >= TRANSCRIPT_THROTTLE_SECS,
                    };
                    if stale {
                        info.reread_activity(now);
                    }
                }
            } else {
                let mut info = AgentInfo::new(entry, now);
                if entry.state == AgentState::Running {
                    info.reread_activity(now);
                }
                self.agents.push(info);
            }
        }
    }
}

/// One rendered, ordered row: either a real agent line or the collapsed
/// "+K more" summary row.
#[derive(Debug, Clone)]
pub enum StripRow<'a> {
    Agent(&'a AgentInfo),
    More { count: usize, running: usize },
}

/// Order agents failed -> running -> queued -> most-recently-finished
/// (D-02), then cap to at most 3 rows, collapsing the remainder into a
/// "+K more (R running)" row.
pub fn collapsed_rows(view: &AgentsView) -> Vec<StripRow<'_>> {
    let ordered = ordered_agents(view);
    if ordered.len() <= 3 {
        return ordered.into_iter().map(StripRow::Agent).collect();
    }
    let shown = &ordered[..2];
    let rest = &ordered[2..];
    let running = rest.iter().filter(|a| a.state == AgentState::Running).count();
    let mut rows: Vec<StripRow> = shown.iter().map(|a| StripRow::Agent(a)).collect();
    rows.push(StripRow::More { count: rest.len(), running });
    rows
}

fn rank(state: AgentState) -> u8 {
    match state {
        AgentState::Failed => 0,
        AgentState::Running => 1,
        AgentState::Queued => 2,
        // Terminal-but-not-failed states sort by recency within this bucket.
        _ => 3,
    }
}

fn ordered_agents(view: &AgentsView) -> Vec<&AgentInfo> {
    let mut v: Vec<&AgentInfo> = view.agents.iter().collect();
    v.sort_by(|a, b| {
        let ra = rank(a.state);
        let rb = rank(b.state);
        if ra != rb {
            return ra.cmp(&rb);
        }
        if ra == 3 {
            // Most recently finished first.
            let fa = a.finished_seen.unwrap_or(a.started);
            let fb = b.finished_seen.unwrap_or(b.started);
            return fb.cmp(&fa);
        }
        // Stable-ish fallback: started order.
        a.started.cmp(&b.started)
    });
    v
}

/// Strip height depends only on agent count, expanded flag (currently
/// unused, see 05-03) and terminal rows — never on text length (D-08).
pub fn strip_height(view: &AgentsView, term_rows: u16, _expanded: bool) -> u16 {
    if view.is_empty() {
        return 0;
    }
    if term_rows < 15 {
        return 1;
    }
    1 + (view.len().min(3) as u16)
}

/// Rendering options independent of the cached data itself.
#[derive(Debug, Clone)]
pub struct StripOpts {
    pub expanded: bool,
    pub term_rows: u16,
    pub toggle_key_label: String,
    pub now: Instant,
}

fn style_for_state(state: AgentState) -> Style {
    match state {
        AgentState::Running => Style::default().fg(Color::Cyan),
        AgentState::Failed => Style::default().fg(Color::Red),
        AgentState::Completed => Style::default().fg(Color::Green),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn elapsed_secs_for(info: &AgentInfo, now: Instant) -> u64 {
    let end = info.finished_seen.unwrap_or(now);
    end.saturating_duration_since(info.started).as_secs()
}

/// Build the single-line text for one agent row at `width` columns,
/// dropping activity first and then description as the width shrinks
/// below 60/30 columns (D-08). The id, glyph and elapsed time are always
/// present. Never exceeds `width` display columns (CJK-safe).
fn agent_line(info: &AgentInfo, width: usize, use_activity: bool) -> String {
    let glyph = glyph_for(info.state);
    let id = format!("#{}", info.id);
    let elapsed = format_elapsed(elapsed_secs_for(info, Instant::now()));

    let text_slot = if use_activity {
        info.activity.last().cloned().unwrap_or_default()
    } else {
        info.description.clone()
    };

    // Fixed parts: glyph, id, elapsed, and spacing.
    let fixed = format!("{glyph} {id}");
    let fixed_w = display_width(&fixed);
    let elapsed_w = display_width(&elapsed);
    // Reserve 1 space before elapsed and 1 space after fixed, when room allows.
    let reserved = fixed_w + elapsed_w + 2;

    if width <= reserved || width < 60 {
        // Narrow handling per D-08 thresholds, applied independent of text_slot choice:
        if width < 30 {
            // id, glyph, elapsed only.
            return pad_line(&fixed, &elapsed, width);
        }
        if width < 60 {
            // description kept (if use_activity is false we already have no
            // activity text); when use_activity requested but width < 60
            // activity is omitted regardless.
            let desc = if use_activity { String::new() } else { truncate_to_width(&info.description, width.saturating_sub(reserved)) };
            let middle = desc;
            return pad_line(&format!("{fixed} {middle}").trim_end().to_string(), &elapsed, width);
        }
    }

    let avail = width.saturating_sub(reserved);
    let middle = truncate_to_width(&text_slot, avail);
    let left = format!("{fixed} {middle}").trim_end().to_string();
    pad_line(&left, &elapsed, width)
}

fn pad_line(left: &str, right: &str, width: usize) -> String {
    let left = truncate_to_width(left, width.saturating_sub(display_width(right).min(width)));
    let left_w = display_width(&left);
    let right_w = display_width(right);
    if left_w + right_w >= width {
        return truncate_to_width(&format!("{left}{right}"), width);
    }
    let pad = width - left_w - right_w;
    format!("{left}{}{right}", " ".repeat(pad))
}

/// Header text: "agents (N) · Ctrl+G expand" collapsed, "... collapse"
/// expanded.
fn header_text(n: usize, opts: &StripOpts) -> String {
    let verb = if opts.expanded { "collapse" } else { "expand" };
    format!("agents ({n}) · {} {verb}", opts.toggle_key_label)
}

/// Per-agent expanded detail lines (D-04): the agent line, up to 3
/// activity lines, "turns N · tokens M" when known, "worktree <path>
/// (<branch>)" when present, "report <dir>/report.md".
pub fn expanded_detail_lines(view: &AgentsView, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for info in ordered_agents(view) {
        out.push(agent_line(info, width, false));
        for act in &info.activity {
            out.push(truncate_to_width(&format!("  {act}"), width));
        }
        if info.turns.is_some() || info.tokens.is_some() {
            let turns = info.turns.clone().unwrap_or_else(|| "?".to_string());
            let tokens = info.tokens.clone().unwrap_or_else(|| "?".to_string());
            out.push(truncate_to_width(&format!("  turns {turns} · tokens {tokens}"), width));
        }
        if let Some(w) = &info.worktree {
            let branch = info.branch.clone().unwrap_or_default();
            let line = if branch.is_empty() {
                format!("  worktree {w}")
            } else {
                format!("  worktree {w} ({branch})")
            };
            out.push(truncate_to_width(&line, width));
        }
        out.push(truncate_to_width(&format!("  report {}", info.report_path.display()), width));
    }
    out
}

/// Pure draw function: writes header + rows into `buf` at `area`. Reads
/// only in-memory cached data from `view`; never touches the filesystem
/// (UI-04). Calling twice with the same `view`/`opts` yields an
/// identical buffer.
pub fn draw_agents_strip(buf: &mut Buffer, area: Rect, view: &AgentsView, opts: &StripOpts) {
    if view.is_empty() || area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;

    let header = truncate_to_width(&header_text(view.len(), opts), width);
    buf.set_string(area.x, area.y, &header, Style::default().fg(Color::DarkGray));

    if opts.term_rows < 15 || area.height < 2 {
        return;
    }

    let rows = collapsed_rows(view);
    for (i, row) in rows.iter().enumerate() {
        let y = area.y + 1 + i as u16;
        if y >= area.y + area.height {
            break;
        }
        match row {
            StripRow::Agent(info) => {
                let line = agent_line(info, width, opts.expanded);
                buf.set_string(area.x, y, &line, style_for_state(info.state));
            }
            StripRow::More { count, running } => {
                let text = truncate_to_width(&format!("+{count} more ({running} running)"), width);
                buf.set_string(area.x, y, &text, Style::default().fg(Color::DarkGray));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::time::Duration;

    fn entry(id: &str, state: AgentState, started: Instant, dir: PathBuf) -> AgentEntry {
        AgentEntry { id: id.to_string(), pid: None, state, started, dir }
    }

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn glyph_table_covers_all_states() {
        assert_eq!(glyph_for(AgentState::Running), "●");
        assert_eq!(glyph_for(AgentState::Queued), "◐");
        assert_eq!(glyph_for(AgentState::Completed), "✓");
        assert_eq!(glyph_for(AgentState::Failed), "✗");
        assert_eq!(glyph_for(AgentState::Stopped), "■");
        assert_eq!(glyph_for(AgentState::LimitReached), "⏱");
        assert_eq!(glyph_for(AgentState::Interrupted), "?");
    }

    #[test]
    fn empty_snapshot_is_empty_and_zero_height() {
        let mut view = AgentsView::new();
        view.refresh(&[], Instant::now());
        assert!(view.is_empty());
        assert_eq!(strip_height(&view, 40, false), 0);
        assert_eq!(strip_height(&view, 40, true), 0);
    }

    #[test]
    fn ordering_failed_running_queued_finished() {
        let now = Instant::now();
        let d1 = tmp_dir();
        let d2 = tmp_dir();
        let d3 = tmp_dir();
        let d4 = tmp_dir();
        let snapshot = vec![
            entry("done1", AgentState::Completed, now, d1.path().to_path_buf()),
            entry("run2", AgentState::Running, now, d2.path().to_path_buf()),
            entry("fail3", AgentState::Failed, now, d3.path().to_path_buf()),
            entry("queued4", AgentState::Queued, now, d4.path().to_path_buf()),
        ];
        let mut view = AgentsView::new();
        view.refresh(&snapshot, now);
        let ids: Vec<&str> = ordered_agents(&view).iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["fail3", "run2", "queued4", "done1"]);
    }

    #[test]
    fn most_recently_finished_sorts_first_among_finished() {
        let t0 = Instant::now();
        let d1 = tmp_dir();
        let d2 = tmp_dir();
        let mut view = AgentsView::new();

        // Both start running.
        let snap1 = vec![
            entry("a", AgentState::Running, t0, d1.path().to_path_buf()),
            entry("b", AgentState::Running, t0, d2.path().to_path_buf()),
        ];
        view.refresh(&snap1, t0);

        // "a" finishes first.
        let t1 = t0 + Duration::from_millis(10);
        let snap2 = vec![
            entry("a", AgentState::Completed, t0, d1.path().to_path_buf()),
            entry("b", AgentState::Running, t0, d2.path().to_path_buf()),
        ];
        view.refresh(&snap2, t1);

        // "b" finishes second.
        let t2 = t1 + Duration::from_millis(10);
        let snap3 = vec![
            entry("a", AgentState::Completed, t0, d1.path().to_path_buf()),
            entry("b", AgentState::Completed, t0, d2.path().to_path_buf()),
        ];
        view.refresh(&snap3, t2);

        let rows = collapsed_rows(&view);
        let ids: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                StripRow::Agent(a) => a.id.as_str(),
                StripRow::More { .. } => "+more",
            })
            .collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn height_scales_with_count_and_caps_at_three_rows() {
        let now = Instant::now();
        let mut view = AgentsView::new();
        let d1 = tmp_dir();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        assert_eq!(strip_height(&view, 40, false), 2);

        let dirs: Vec<_> = (0..3).map(|_| tmp_dir()).collect();
        let snap: Vec<_> = dirs
            .iter()
            .enumerate()
            .map(|(i, d)| entry(&format!("a{i}"), AgentState::Running, now, d.path().to_path_buf()))
            .collect();
        let mut view3 = AgentsView::new();
        view3.refresh(&snap, now);
        assert_eq!(strip_height(&view3, 40, false), 4);

        // 5 agents -> capped to 3 rows + header = 4; the 2 shown are fail + the
        // first running agent, leaving 2 running (plus queued) collapsed into
        // "+3 more (2 running)".
        let dirs5: Vec<_> = (0..5).map(|_| tmp_dir()).collect();
        let states = [
            AgentState::Failed,
            AgentState::Running,
            AgentState::Running,
            AgentState::Running,
            AgentState::Queued,
        ];
        let snap5: Vec<_> = dirs5
            .iter()
            .zip(states.iter())
            .enumerate()
            .map(|(i, (d, s))| entry(&format!("b{i}"), *s, now, d.path().to_path_buf()))
            .collect();
        let mut view5 = AgentsView::new();
        view5.refresh(&snap5, now);
        assert_eq!(strip_height(&view5, 40, false), 4);
        let rows = collapsed_rows(&view5);
        assert_eq!(rows.len(), 3);
        match rows.last().unwrap() {
            StripRow::More { count, running } => {
                assert_eq!(*count, 3);
                assert_eq!(*running, 2);
            }
            StripRow::Agent(_) => panic!("expected More row"),
        }
    }

    #[test]
    fn header_text_collapsed_and_expanded() {
        let now = Instant::now();
        let d1 = tmp_dir();
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        let opts_collapsed = StripOpts {
            expanded: false,
            term_rows: 40,
            toggle_key_label: "Ctrl+G".to_string(),
            now,
        };
        assert_eq!(header_text(view.len(), &opts_collapsed), "agents (1) · Ctrl+G expand");
        let opts_expanded = StripOpts { expanded: true, ..opts_collapsed };
        assert_eq!(header_text(view.len(), &opts_expanded), "agents (1) · Ctrl+G collapse");
    }

    #[test]
    fn line_layout_width_80_contains_all_fields() {
        let now = Instant::now();
        let d1 = tmp_dir();
        std::fs::write(
            d1.path().join("brief.md"),
            "---\nid: a\nstate: running\nstarted: x\nparent: none\nlabel: do the thing\n---\nbody\n",
        )
        .unwrap();
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        let line = agent_line(&view.agents()[0], 80, false);
        assert!(line.contains("●"));
        assert!(line.contains("#a"));
        assert!(line.contains("do the thing"));
        assert!(display_width(&line) <= 80);
    }

    #[test]
    fn narrow_width_drops_activity_then_description() {
        let now = Instant::now();
        let d1 = tmp_dir();
        std::fs::write(
            d1.path().join("brief.md"),
            "---\nid: a\nstate: running\nstarted: x\nparent: none\nlabel: 读取 src/config and more text here\n---\nbody\n",
        )
        .unwrap();
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        let info = &view.agents()[0];

        let at_50 = agent_line(info, 50, true);
        assert!(display_width(&at_50) <= 50);

        let at_30 = agent_line(info, 30, false);
        assert!(display_width(&at_30) <= 30);
        assert!(at_30.contains("#a"));
    }

    #[test]
    fn cjk_description_never_exceeds_width() {
        let now = Instant::now();
        let d1 = tmp_dir();
        std::fs::write(
            d1.path().join("brief.md"),
            "---\nid: a\nstate: running\nstarted: x\nparent: none\nlabel: 读取 src/config 这是一段很长的中文描述用来测试宽度截断\n---\nbody\n",
        )
        .unwrap();
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        for w in [80usize, 50, 30, 20] {
            let line = agent_line(&view.agents()[0], w, false);
            assert!(display_width(&line) <= w, "width {w}: {line:?} ({})", display_width(&line));
        }
    }

    #[test]
    fn short_terminal_forces_header_only_height() {
        let now = Instant::now();
        let mut view = AgentsView::new();
        let d1 = tmp_dir();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        assert_eq!(strip_height(&view, 14, false), 1);
        assert_eq!(strip_height(&view, 14, true), 1);
    }

    fn write_transcript_lines(dir: &std::path::Path, calls: &[(&str, &str)]) {
        let mut f = std::fs::File::create(dir.join("transcript.jsonl")).unwrap();
        for (name, arg) in calls {
            let line = serde_json::json!({
                "type": "tool_call",
                "tool_name": name,
                "arguments": {"path": arg},
            });
            writeln!(f, "{line}").unwrap();
        }
    }

    #[test]
    fn activity_keeps_last_three_formatted_entries() {
        let now = Instant::now();
        let d1 = tmp_dir();
        write_transcript_lines(
            d1.path(),
            &[("read", "a.rs"), ("read", "b.rs"), ("grep", "c.rs"), ("read", "d.rs"), ("write", "e.rs")],
        );
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        let info = &view.agents()[0];
        assert_eq!(info.activity.len(), 3);
        assert_eq!(info.activity[0], "grep c.rs");
        assert_eq!(info.activity[1], "read d.rs");
        assert_eq!(info.activity[2], "write e.rs");
    }

    #[test]
    fn activity_sanitizes_and_bounds_tool_name_from_untrusted_transcript() {
        // CR-01 regression: `tool_name` is attacker/model-influenced text
        // read straight from transcript.jsonl and must be stripped of
        // control characters (T-05-01) and bounded in width just like `arg`.
        let now = Instant::now();
        let d1 = tmp_dir();
        let malicious_name = format!("evil\x1b[31m{}", "x".repeat(100));
        write_transcript_lines(d1.path(), &[(&malicious_name, "a.rs")]);
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        let info = &view.agents()[0];
        assert_eq!(info.activity.len(), 1);
        let line = &info.activity[0];
        assert!(!line.contains('\x1b'), "escape byte leaked into activity: {line:?}");
        assert!(!line.chars().any(|c| c.is_control()), "control char leaked: {line:?}");
        assert!(display_width(line) <= 40 + 1 + display_width("a.rs"));
    }

    #[test]
    fn activity_sanitizes_tool_name_with_no_arg() {
        // Same as above but exercising the `None` branch of `short_arg`,
        // where `tool_name` is used standalone.
        let now = Instant::now();
        let d1 = tmp_dir();
        let mut f = std::fs::File::create(d1.path().join("transcript.jsonl")).unwrap();
        let line = serde_json::json!({
            "type": "tool_call",
            "tool_name": format!("bad\x07name{}", "z".repeat(100)),
            "arguments": {},
        });
        writeln!(f, "{line}").unwrap();
        drop(f);
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        let info = &view.agents()[0];
        assert_eq!(info.activity.len(), 1);
        assert!(!info.activity[0].contains('\x07'));
        assert!(display_width(&info.activity[0]) <= 40);
    }

    #[test]
    fn activity_ignores_partial_trailing_line_and_missing_file() {
        let now = Instant::now();
        let d1 = tmp_dir();
        let path = d1.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"tool_call","tool_name":"read","arguments":{{"path":"a.rs"}}}}"#).unwrap();
        write!(f, r#"{{"type":"tool_call","tool_name":"read","arguments":{{"path":"b.rs""#).unwrap(); // partial
        drop(f);
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        assert_eq!(view.agents()[0].activity, vec!["read a.rs".to_string()]);

        let d2 = tmp_dir();
        let mut view2 = AgentsView::new();
        view2.refresh(&[entry("b", AgentState::Running, now, d2.path().to_path_buf())], now);
        assert!(view2.agents()[0].activity.is_empty());
    }

    #[test]
    fn throttle_skips_reread_without_state_change() {
        let now = Instant::now();
        let d1 = tmp_dir();
        std::fs::write(
            d1.path().join("brief.md"),
            "---\nid: a\nstate: running\nstarted: x\nparent: none\nlabel: original\n---\nbody\n",
        )
        .unwrap();
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        assert_eq!(view.agents()[0].description, "original");

        std::fs::remove_file(d1.path().join("brief.md")).unwrap();
        let later = now + Duration::from_millis(200);
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], later);
        assert_eq!(view.agents()[0].description, "original");
    }

    #[test]
    fn draw_is_pure_and_deterministic() {
        let now = Instant::now();
        let mut view = AgentsView::new();
        let d1 = tmp_dir();
        view.refresh(&[entry("a", AgentState::Running, now, d1.path().to_path_buf())], now);
        let opts = StripOpts {
            expanded: false,
            term_rows: 40,
            toggle_key_label: "Ctrl+G".to_string(),
            now,
        };
        let area = Rect::new(0, 0, 40, 4);
        let mut buf1 = Buffer::empty(area);
        let mut buf2 = Buffer::empty(area);
        draw_agents_strip(&mut buf1, area, &view, &opts);
        draw_agents_strip(&mut buf2, area, &view, &opts);
        assert_eq!(buf1, buf2);
    }

    #[test]
    fn draw_works_with_nonexistent_dir() {
        let now = Instant::now();
        let mut view = AgentsView::new();
        let missing = PathBuf::from("/nonexistent/path/for/test/agents_strip");
        view.refresh(&[entry("a", AgentState::Running, now, missing)], now);
        let opts = StripOpts {
            expanded: false,
            term_rows: 40,
            toggle_key_label: "Ctrl+G".to_string(),
            now,
        };
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        draw_agents_strip(&mut buf, area, &view, &opts);
    }

    #[test]
    fn expanded_detail_lines_include_report_and_worktree() {
        let now = Instant::now();
        let d1 = tmp_dir();
        std::fs::write(
            d1.path().join("brief.md"),
            "---\nid: a\nstate: done\nstarted: x\nparent: none\nlabel: task\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            d1.path().join("report.md"),
            "---\nturns: 3\ntokens: 1200\nworktree: /tmp/wt\nbranch: feature/x\n---\nsummary\n",
        )
        .unwrap();
        let mut view = AgentsView::new();
        view.refresh(&[entry("a", AgentState::Completed, now, d1.path().to_path_buf())], now);
        let lines = expanded_detail_lines(&view, 80);
        assert!(lines.iter().any(|l| l.contains("turns 3") && l.contains("tokens 1200")));
        assert!(lines.iter().any(|l| l.contains("worktree /tmp/wt (feature/x)")));
        assert!(lines.iter().any(|l| l.contains("report") && l.contains("report.md")));
    }

}

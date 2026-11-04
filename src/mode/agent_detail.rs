//! `/agents` picker rows and the per-agent detail card.
//!
//! Where [`super::agents_strip`] shows the *live* agents at a glance and
//! forgets a finished one on the next tick, this module covers every agent of
//! the run, finished ones included (the registry keeps them for the run):
//! the interactive `/agents` picker, `/agents show`, and the finished block of
//! `/agents list`.
//!
//! Split in three layers so the interesting part is testable without a
//! terminal, a registry or a disk:
//!
//! * **loaders** ([`collect_rows`], [`load_card`]) — the only functions that
//!   touch the filesystem. Every read is bounded (a head or tail window of a
//!   few KiB to a few hundred KiB, never the whole of a file the agent can
//!   grow), and a missing, partial, or non-UTF-8 file degrades to "that field
//!   is absent" rather than an error.
//! * **parsers** — pure functions from file text to [`AgentRow`] /
//!   [`CardInput`] parts.
//! * **renderers** ([`render_card`], [`picker_items`]) — pure functions over
//!   plain inputs. The card renderer is where every string that came from a
//!   model or from a file an agent can write is stripped of control
//!   characters (T-05-01) and clipped by display width (CJK-safe), so the
//!   guarantee holds no matter which loader produced the input.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use super::agents_strip::{
    display_width, format_elapsed, glyph_for, read_tail_lines, sanitize, truncate_to_width,
};
use crate::agent::brief::{front_matter_get, parse_amendments, AMENDMENTS_MARKER};
use crate::agent_registry::{AgentEntry, AgentState};

/// Task text shown on the card, in characters.
const TASK_CHARS: usize = 300;
/// Most lines the task may wrap to.
const TASK_LINES: usize = 8;
/// Transcript entries shown under "activity".
const ACTIVITY_ENTRIES: usize = 12;
/// One transcript entry's text, in characters, before layout clips it.
const ENTRY_CHARS: usize = 240;
/// Lines of `report.md` shown on the card.
const REPORT_LINES: usize = 15;
/// Hard cap on the whole card.
pub const CARD_MAX_LINES: usize = 60;
/// Amendment text shown on the card, in characters.
const AMENDMENT_CHARS: usize = 200;
/// Lines of the failure/stop reason shown.
const REASON_LINES: usize = 2;

/// How much of `brief.md` is read for a card. The task and front matter are
/// at the top and amendments at the bottom; a brief bigger than this loses
/// its latest amendments, which the card says.
const BRIEF_CAP: u64 = 256 * 1024;
/// How much of `brief.md` is read to name a picker row (front matter plus
/// the start of the task).
const ROW_BRIEF_CAP: u64 = 16 * 1024;
/// How much of `report.md` is read for the card.
const REPORT_CAP: u64 = 16 * 1024;
/// `tools.json` is a short list of names.
const TOOLS_CAP: u64 = 16 * 1024;
/// The transcript's first line (the session header, which names the model).
const HEADER_CAP: u64 = 8 * 1024;

// ─────────────────────────────────────────────────────────────────────
// Small text helpers
// ─────────────────────────────────────────────────────────────────────

/// `a` followed by digits: the shape of a registry agent id.
pub fn is_agent_id(q: &str) -> bool {
    q.strip_prefix('a')
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Numeric suffix of an agent id, for ordering (`a10` after `a9`).
fn id_number(id: &str) -> u64 {
    id.strip_prefix('a').and_then(|d| d.parse().ok()).unwrap_or(u64::MAX)
}

/// Whitespace-collapsed, control-stripped single line.
fn one_line(s: &str) -> String {
    sanitize(&s.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// `s` clipped to `width` display columns, ending in `…` when it was cut.
fn clip(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = truncate_to_width(s, width - 1);
    out.push('…');
    out
}

/// `s` cut to `max` characters with a trailing `…`.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Break `text` into lines of at most `width` display columns: at spaces
/// when there is one to break at, mid-word when there is not.
fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let w = display_width(word);
        let sep = usize::from(!cur.is_empty());
        if cur_w + sep + w <= width {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            cur_w += sep + w;
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        // A word longer than a line is broken mid-word.
        for ch in word.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > width && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += cw;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Greedily pack `parts` (joined by ` · `) into lines of at most `width`
/// columns, so a narrow terminal wraps the facts instead of cutting them.
fn pack_parts(parts: &[String], width: usize) -> Vec<String> {
    const SEP: &str = " · ";
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for p in parts {
        let p = clip(p, width);
        if cur.is_empty() {
            cur = p;
        } else if display_width(&cur) + display_width(SEP) + display_width(&p) <= width {
            cur.push_str(SEP);
            cur.push_str(&p);
        } else {
            lines.push(std::mem::replace(&mut cur, p));
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// A placeholder the writer of a brief/report used for "no value".
fn known(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !matches!(s.as_str(), "(none)" | "(unknown)" | "(inherit)" | "(default)"))
}

/// First non-blank line of `s`, trimmed, at most `max` characters.
fn first_line(s: &str, max: usize) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(max).collect())
        .unwrap_or_default()
}

// ─────────────────────────────────────────────────────────────────────
// Bounded file reads
// ─────────────────────────────────────────────────────────────────────

/// Up to `cap` bytes from the start of `path`, lossily decoded, and whether
/// the file was longer. `None` if it cannot be opened or read at all.
fn read_head(path: &Path, cap: u64) -> Option<(String, bool)> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    // One byte past the cap tells "exactly cap" from "longer".
    file.take(cap + 1).read_to_end(&mut bytes).ok()?;
    let truncated = bytes.len() as u64 > cap;
    bytes.truncate(cap as usize);
    Some((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

// ─────────────────────────────────────────────────────────────────────
// Rows (picker, list, resolution)
// ─────────────────────────────────────────────────────────────────────

/// One agent of this run, as the picker and `/agents list` show it. All text
/// is already control-stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub id: String,
    pub state: AgentState,
    /// Seconds running so far, or the total once finished.
    pub elapsed_secs: u64,
    /// Seconds since it finished; `None` while queued or running.
    pub finished_ago_secs: Option<u64>,
    /// The brief's `role:` (an agent name, or the start of an inline role).
    pub role: Option<String>,
    /// The brief's `label:` (the dispatcher's short description).
    pub label: Option<String>,
    /// First line of the task.
    pub task_line: Option<String>,
}

impl AgentRow {
    /// What to call the agent: its label, else its role, else the start of
    /// its task, else `#id`.
    pub fn name(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.role.clone())
            .or_else(|| self.task_line.clone())
            .unwrap_or_else(|| format!("#{}", self.id))
    }

    pub fn is_live(&self) -> bool {
        !self.state.is_terminal()
    }
}

/// The task text of a brief: what follows `## Task`, up to the `## Role`
/// section (or the amendments marker, or the end). A brief without that
/// heading (hand-written, or from an older version) is its body.
fn task_text(brief: &str) -> Option<String> {
    let after_fm = body_after_front_matter(brief);
    let task = match after_fm.find("## Task\n") {
        Some(i) => {
            let rest = &after_fm[i + "## Task\n".len()..];
            let end = rest
                .find("\n\n## Role\n")
                .into_iter()
                .chain(rest.find(AMENDMENTS_MARKER))
                .min()
                .unwrap_or(rest.len());
            &rest[..end]
        }
        None => after_fm,
    };
    let task = task.trim();
    (!task.is_empty()).then(|| task.to_string())
}

fn body_after_front_matter(content: &str) -> &str {
    let mut offset = 0;
    let mut lines = content.split_inclusive('\n');
    match lines.next() {
        Some(first) if first.trim_end() == "---" => offset += first.len(),
        _ => return content,
    }
    for line in lines {
        offset += line.len();
        if line.trim_end() == "---" {
            return &content[offset..];
        }
    }
    // Never closed: no usable body.
    ""
}

/// Build a row from what the registry knows and the head of the brief.
pub fn build_row(
    id: &str,
    state: AgentState,
    elapsed_secs: u64,
    finished_ago_secs: Option<u64>,
    brief: &str,
) -> AgentRow {
    let text = |v: Option<String>| known(v).map(|s| one_line(&s)).filter(|s| !s.is_empty());
    AgentRow {
        id: id.to_string(),
        state,
        elapsed_secs,
        finished_ago_secs,
        role: text(front_matter_get(brief, "role")),
        label: text(front_matter_get(brief, "label")),
        task_line: task_text(brief)
            .map(|t| one_line(&first_line(&t, 120)))
            .filter(|s| !s.is_empty()),
    }
}

/// Every entry of `snapshot` as a row, in the order the picker shows them
/// ([`order_rows`]). Reads the head of each agent's `brief.md`.
pub fn collect_rows(snapshot: &[AgentEntry], now: Instant) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = snapshot
        .iter()
        .map(|e| {
            let end = e.finished.unwrap_or(now);
            let brief = read_head(&e.dir.join("brief.md"), ROW_BRIEF_CAP)
                .map(|(t, _)| t)
                .unwrap_or_default();
            build_row(
                &e.id,
                e.state,
                end.saturating_duration_since(e.started).as_secs(),
                e.finished.map(|f| now.saturating_duration_since(f).as_secs()),
                &brief,
            )
        })
        .collect();
    order_rows(&mut rows);
    rows
}

/// Running and queued agents first (oldest first, so ids ascend), then the
/// finished ones, most recently finished first.
pub fn order_rows(rows: &mut [AgentRow]) {
    rows.sort_by(|a, b| match (a.is_live(), b.is_live()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => id_number(&a.id).cmp(&id_number(&b.id)),
        (false, false) => a
            .finished_ago_secs
            .cmp(&b.finished_ago_secs)
            .then_with(|| id_number(&b.id).cmp(&id_number(&a.id))),
    });
}

/// Does `query` (already lowercased) start the agent's label, role, or task?
fn row_name_matches(row: &AgentRow, q: &str) -> bool {
    [&row.label, &row.role, &row.task_line]
        .into_iter()
        .flatten()
        .any(|n| n.trim().to_lowercase().starts_with(q))
}

/// Resolve what `/agents show` was given to one agent id (pure). Unlike
/// `/agents stop` this ranges over every agent of the run, finished ones
/// included: showing is harmless, so an id only has to exist. A name must
/// pick exactly one; an ambiguous prefix shows nothing and lists the ids.
pub fn resolve_agent(query: &str, rows: &[AgentRow]) -> Result<String, String> {
    let q = query.trim().to_lowercase();
    if rows.is_empty() {
        return Err("no agents in this session".to_string());
    }
    if is_agent_id(&q) {
        return if rows.iter().any(|r| r.id == q) {
            Ok(q)
        } else {
            Err(format!("no agent {q} in this session (/agents lists them)"))
        };
    }
    let hits: Vec<&AgentRow> = rows.iter().filter(|r| row_name_matches(r, &q)).collect();
    match hits.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(format!("no agent matches {query:?} (/agents lists them)")),
        many => Err(format!(
            "{query:?} matches {} agents ({}): give an id",
            many.len(),
            many.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", "),
        )),
    }
}

/// `done 1m05s` / `running 12s`: the state word and the clock that goes
/// with it.
fn state_clock(row: &AgentRow) -> String {
    format!("{} {}", row.state.as_str(), format_elapsed(row.elapsed_secs))
}

/// Widest name the picker shows before clipping it.
const PICKER_NAME_COLS: usize = 36;

/// `(label, description, id)` per picker row, in display order: glyph, id
/// and name on the left, state and clock on the right.
pub fn picker_items(rows: &[AgentRow]) -> Vec<(String, String, String)> {
    rows.iter()
        .map(|r| {
            let name = clip(&r.name(), PICKER_NAME_COLS);
            (
                format!("{} {:<3} {name}", glyph_for(r.state), r.id),
                state_clock(r),
                r.id.clone(),
            )
        })
        .collect()
}

/// The finished agents as `/agents list` prints them, one line each, marked
/// by their state word: `✓ a2  name  done 1m05s`.
pub fn finished_list_lines(rows: &[AgentRow], width: usize) -> Vec<String> {
    rows.iter()
        .filter(|r| !r.is_live())
        .map(|r| {
            clip(
                &format!("{} {}  {}  {}", glyph_for(r.state), r.id, r.name(), state_clock(r)),
                width,
            )
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Card input
// ─────────────────────────────────────────────────────────────────────

/// One transcript entry, reduced to what the card shows. Text may still
/// hold control characters; [`render_card`] strips them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityEntry {
    User(String),
    Assistant(String),
    ToolCall { name: String, args: String },
    ToolResult { ok: bool, text: String },
}

/// The recent end of a transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    /// The last few entries, oldest first.
    pub shown: Vec<ActivityEntry>,
    /// Entries seen in the window but not in `shown`.
    pub earlier: usize,
    /// The file was longer than the window read, so `earlier` (and
    /// `tool_calls`) are lower bounds.
    pub window_cut: bool,
    /// Tool calls in the window.
    pub tool_calls: usize,
}

/// First lines of `report.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportHead {
    pub lines: Vec<String>,
    /// There is more of the report than `lines`.
    pub more: bool,
}

/// Everything the card shows, as plain data.
#[derive(Debug, Clone)]
pub struct CardInput {
    pub id: String,
    pub state: AgentState,
    pub label: Option<String>,
    pub role: Option<String>,
    pub elapsed_secs: u64,
    pub model: Option<String>,
    pub turns: Option<String>,
    pub tokens: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// `Some(vec![])` means the default set.
    pub tools: Option<Vec<String>>,
    pub task: Option<String>,
    pub amendments: Vec<(u32, String)>,
    /// `brief.md` was longer than was read, so later amendments may be
    /// missing.
    pub brief_truncated: bool,
    pub activity: Activity,
    pub transcript_exists: bool,
    /// Why it failed, stopped, or hit a limit (from the report summary).
    pub reason: Option<String>,
    pub report_path: PathBuf,
    /// `None` when there is no report (yet).
    pub report: Option<ReportHead>,
    pub dir: PathBuf,
    pub transcript_path: PathBuf,
}

/// `tools.json`'s `{"tools": [...]}`.
fn parse_tools_json(text: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(text).ok()?;
    v.get("tools")?
        .as_array()?
        .iter()
        .map(|t| t.as_str().map(str::to_string))
        .collect()
}

/// The brief's `tools:` line: `(all)` or a comma list, possibly cut short by
/// `fm_value`'s 120-character cap (then ending in `…`).
fn parse_tools_line(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if line == "(all)" {
        return Some(Vec::new());
    }
    let cut = line.ends_with('…');
    let mut names: Vec<String> = line
        .trim_end_matches('…')
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if cut {
        // The last name may be half-written.
        names.pop();
    }
    (!names.is_empty()).then_some(names)
}

/// Why an agent ended badly: the first lines of its report's `## Summary`.
fn report_reason(report: &str) -> Option<String> {
    let body = body_after_front_matter(report);
    let mut lines = body.lines();
    lines.by_ref().find(|l| l.trim() == "## Summary")?;
    let text: Vec<&str> = lines
        .take_while(|l| !l.starts_with("## "))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(REASON_LINES + 1)
        .collect();
    let text = text.join(" ");
    (!text.is_empty()).then_some(text)
}

/// The first [`REPORT_LINES`] non-blank lines of a report body.
fn report_head(report: &str, read_truncated: bool) -> ReportHead {
    let mut lines: Vec<String> = body_after_front_matter(report)
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let more = lines.len() > REPORT_LINES || read_truncated;
    lines.truncate(REPORT_LINES);
    ReportHead { lines, more }
}

/// One transcript argument object as `name(summary)`'s summary: the first of
/// the usual path/command/pattern/query fields, else the arguments as compact
/// JSON.
fn summarize_args(args: Option<&Value>) -> String {
    let Some(a) = args else { return String::new() };
    for key in ["path", "command", "pattern", "query", "file_path", "url"] {
        if let Some(s) = a.get(key).and_then(Value::as_str) {
            return one_line_raw(s);
        }
    }
    match a {
        Value::Null => String::new(),
        Value::Object(m) if m.is_empty() => String::new(),
        other => cap_chars(&other.to_string(), ENTRY_CHARS),
    }
}

/// Collapse to one line and bound the size, without the control-character
/// pass (that is [`render_card`]'s job).
fn one_line_raw(s: &str) -> String {
    cap_chars(&s.split_whitespace().collect::<Vec<_>>().join(" "), ENTRY_CHARS)
}

/// Is this message the agent's own `brief.md` (front matter and all)?
fn is_brief(content: &str) -> bool {
    let c = content.trim_start();
    c.starts_with("---") && c.lines().skip(1).take(12).any(|l| l.starts_with("id: "))
}

/// Reduce the lines of a transcript tail to the last `max` entries worth
/// showing. Malformed lines and entry kinds the card does not show (header,
/// model changes, compaction, ...) are skipped.
pub fn parse_activity(lines: &[String], window_cut: bool, max: usize) -> Activity {
    let mut shown: std::collections::VecDeque<ActivityEntry> = std::collections::VecDeque::new();
    let mut total = 0usize;
    let mut tool_calls = 0usize;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let str_of = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("");
        let entry = match str_of("type") {
            "message" => {
                let content = str_of("content");
                // The child's first user message is its whole brief, whose
                // first line is the front matter's `---`: say what it is.
                let text = if is_brief(content) {
                    "(the task brief)".to_string()
                } else {
                    first_line(content, ENTRY_CHARS)
                };
                if text.is_empty() {
                    continue;
                }
                match str_of("role") {
                    "assistant" => ActivityEntry::Assistant(text),
                    "user" => ActivityEntry::User(text),
                    _ => continue,
                }
            }
            "tool_call" => {
                tool_calls += 1;
                ActivityEntry::ToolCall {
                    name: cap_chars(str_of("tool_name"), 60),
                    args: summarize_args(v.get("arguments")),
                }
            }
            "tool_result" => ActivityEntry::ToolResult {
                ok: !v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                text: first_line(str_of("content"), ENTRY_CHARS),
            },
            _ => continue,
        };
        total += 1;
        shown.push_back(entry);
        if shown.len() > max {
            shown.pop_front();
        }
    }
    Activity {
        earlier: total - shown.len(),
        shown: shown.into(),
        window_cut,
        tool_calls,
    }
}

/// Read what the card needs from the agent's directory. Every read is
/// bounded; anything missing or unreadable is simply absent from the result.
pub fn load_card(entry: &AgentEntry, now: Instant) -> CardInput {
    let dir = entry.dir.clone();
    let transcript_path = dir.join("transcript.jsonl");
    let report_path = dir.join("report.md");

    let (brief, brief_truncated) = read_head(&dir.join("brief.md"), BRIEF_CAP).unwrap_or_default();
    let report = read_head(&report_path, REPORT_CAP);
    let (tail, window_cut) = read_tail_lines(&transcript_path);
    let transcript_exists = transcript_path.exists();

    // The brief's own `model:` is "(inherit)" for most agents; the session
    // header the child wrote names the model it actually ran.
    let model = known(front_matter_get(&brief, "model")).or_else(|| {
        let (head, _) = read_head(&transcript_path, HEADER_CAP)?;
        let first = head.lines().next()?;
        let v: Value = serde_json::from_str(first).ok()?;
        known(v.get("model").and_then(Value::as_str).map(str::to_string))
    });

    let tools = read_head(&dir.join("tools.json"), TOOLS_CAP)
        .and_then(|(t, _)| parse_tools_json(&t))
        .or_else(|| front_matter_get(&brief, "tools").and_then(|l| parse_tools_line(&l)));

    let report_fm = |key: &str| report.as_ref().and_then(|(r, _)| front_matter_get(r, key));
    let brief_fm = |key: &str| front_matter_get(&brief, key);
    let ended_badly = matches!(
        entry.state,
        AgentState::Failed | AgentState::Stopped | AgentState::LimitReached | AgentState::Interrupted
    );

    CardInput {
        id: entry.id.clone(),
        state: entry.state,
        label: known(brief_fm("label")),
        role: known(brief_fm("role")),
        elapsed_secs: entry
            .finished
            .unwrap_or(now)
            .saturating_duration_since(entry.started)
            .as_secs(),
        model,
        turns: known(report_fm("turns")),
        tokens: known(report_fm("tokens")),
        worktree: known(report_fm("worktree").or_else(|| brief_fm("worktree"))),
        branch: known(report_fm("branch").or_else(|| brief_fm("branch"))),
        tools,
        task: task_text(&brief),
        amendments: parse_amendments(&brief),
        brief_truncated,
        activity: parse_activity(&tail, window_cut, ACTIVITY_ENTRIES),
        transcript_exists,
        reason: if ended_badly {
            report.as_ref().and_then(|(r, _)| report_reason(r))
        } else {
            None
        },
        report_path,
        report: report.as_ref().map(|(r, cut)| report_head(r, *cut)),
        dir,
        transcript_path,
    }
}

// ─────────────────────────────────────────────────────────────────────
// Card renderer
// ─────────────────────────────────────────────────────────────────────

/// What a card line is, so the caller can colour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The agent's glyph, id and name; coloured by state.
    Header,
    /// A section heading.
    Section,
    Body,
    Dim,
    Error,
    /// A filesystem path. Never clipped: a path cut short cannot be opened.
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardLine {
    pub kind: LineKind,
    pub text: String,
}

fn line(kind: LineKind, text: impl Into<String>) -> CardLine {
    CardLine { kind, text: text.into() }
}

/// The card's activity entry as one line (before clipping).
fn entry_text(e: &ActivityEntry) -> String {
    match e {
        ActivityEntry::User(t) => format!("you  {}", one_line(t)),
        ActivityEntry::Assistant(t) => format!("◆ {}", one_line(t)),
        ActivityEntry::ToolCall { name, args } => {
            let name = one_line(name);
            let args = one_line(args);
            if args.is_empty() {
                format!("→ {name}()")
            } else {
                format!("→ {name}({})", clip(&args, 80))
            }
        }
        ActivityEntry::ToolResult { ok, text } => {
            let text = one_line(text);
            let status = if *ok { "ok" } else { "error" };
            if text.is_empty() {
                format!("← {status}")
            } else {
                format!("← {status}  {text}")
            }
        }
    }
}

/// Render the card as lines of at most `width` columns (paths excepted, see
/// [`LineKind::Path`]) and at most [`CARD_MAX_LINES`] lines. Pure; every
/// string from `c` is control-stripped here.
pub fn render_card(c: &CardInput, width: usize) -> Vec<CardLine> {
    use LineKind::*;
    let width = width.max(20);
    let inner = width - 2;
    let mut body: Vec<CardLine> = Vec::new();

    // ── header ───────────────────────────────────────────────────────
    let name = c
        .label
        .as_deref()
        .or(c.role.as_deref())
        .map(one_line)
        .filter(|n| !n.is_empty());
    let head = match name {
        Some(n) => format!("{} {}  {n}", glyph_for(c.state), one_line(&c.id)),
        None => format!("{} {}", glyph_for(c.state), one_line(&c.id)),
    };
    body.push(line(Header, clip(&head, width)));

    let mut facts = vec![
        c.state.as_str().to_string(),
        if c.state.is_terminal() {
            format!("took {}", format_elapsed(c.elapsed_secs))
        } else {
            format!("{} so far", format_elapsed(c.elapsed_secs))
        },
    ];
    if let Some(m) = c.model.as_deref().map(one_line).filter(|m| !m.is_empty()) {
        facts.push(format!("model {m}"));
    }
    if let Some(t) = c.turns.as_deref().map(one_line).filter(|t| !t.is_empty()) {
        facts.push(format!("turns {t}"));
    }
    if let Some(t) = c.tokens.as_deref().map(one_line).filter(|t| !t.is_empty()) {
        facts.push(format!("tokens {t}"));
    }
    if c.activity.tool_calls > 0 {
        let plus = if c.activity.window_cut { "+" } else { "" };
        facts.push(format!("{}{plus} tool calls", c.activity.tool_calls));
    }
    for l in pack_parts(&facts, inner) {
        body.push(line(Body, format!("  {l}")));
    }
    if let Some(w) = c.worktree.as_deref().map(one_line).filter(|w| !w.is_empty()) {
        let text = match c.branch.as_deref().map(one_line).filter(|b| !b.is_empty()) {
            Some(b) => format!("  worktree {w} ({b})"),
            None => format!("  worktree {w}"),
        };
        body.push(line(Body, clip(&text, width)));
    }

    // ── tools ────────────────────────────────────────────────────────
    if let Some(tools) = &c.tools {
        let names: Vec<String> = tools.iter().map(|t| one_line(t)).filter(|t| !t.is_empty()).collect();
        let text = if names.is_empty() { "all (default set)".to_string() } else { names.join(", ") };
        let wrapped = wrap(&text, inner.saturating_sub("tools  ".len()));
        for (i, l) in wrapped.iter().take(2).enumerate() {
            let lead = if i == 0 { "  tools  " } else { "         " };
            let more = i == 1 && wrapped.len() > 2;
            body.push(line(Body, clip(&format!("{lead}{l}{}", if more { " …" } else { "" }), width)));
        }
    }

    // ── task ─────────────────────────────────────────────────────────
    if let Some(task) = c.task.as_deref().map(one_line).filter(|t| !t.is_empty()) {
        body.push(line(Section, "task"));
        let task = cap_chars(&task, TASK_CHARS);
        let wrapped = wrap(&task, inner);
        let over = wrapped.len() > TASK_LINES;
        for (i, l) in wrapped.iter().take(TASK_LINES).enumerate() {
            let last_shown = over && i == TASK_LINES - 1;
            let text = if last_shown { clip(&format!("{l}…"), inner) } else { l.clone() };
            body.push(line(Body, format!("  {text}")));
        }
    }

    // ── amendments ───────────────────────────────────────────────────
    if let Some((n, text)) = c.amendments.last() {
        body.push(line(
            Section,
            format!("amendments ({} so far)", c.amendments.len()),
        ));
        let text = cap_chars(&one_line(text), AMENDMENT_CHARS);
        let wrapped = wrap(&format!("#{n} {text}"), inner);
        for l in wrapped.iter().take(2) {
            body.push(line(Body, format!("  {l}")));
        }
    }
    if c.brief_truncated {
        body.push(line(
            Dim,
            clip("  (brief.md is large; later amendments may not be listed)", width),
        ));
    }

    // ── activity ─────────────────────────────────────────────────────
    body.push(line(Section, "activity"));
    if c.activity.shown.is_empty() {
        let msg = if c.transcript_exists { "  (nothing recorded yet)" } else { "  (no transcript yet)" };
        body.push(line(Dim, msg));
    } else {
        if c.activity.earlier > 0 {
            let plus = if c.activity.window_cut { "+" } else { "" };
            let n = c.activity.earlier;
            body.push(line(
                Dim,
                clip(&format!("  … {n}{plus} earlier entries not shown"), width),
            ));
        } else if c.activity.window_cut {
            body.push(line(Dim, clip("  … earlier entries not shown", width)));
        }
        for e in &c.activity.shown {
            let kind = match e {
                ActivityEntry::ToolResult { ok: false, .. } => Error,
                ActivityEntry::ToolCall { .. } | ActivityEntry::ToolResult { .. } => Dim,
                _ => Body,
            };
            body.push(line(kind, clip(&format!("  {}", entry_text(e)), width)));
        }
    }

    // ── why it ended ─────────────────────────────────────────────────
    if let Some(reason) = c.reason.as_deref().map(one_line).filter(|r| !r.is_empty()) {
        let word = match c.state {
            AgentState::Failed => "error",
            AgentState::Stopped => "stopped",
            AgentState::LimitReached => "limit reached",
            _ => "ended",
        };
        // The report's summary already starts "error: " for a failure.
        let reason = match (c.state, reason.strip_prefix("error: ")) {
            (AgentState::Failed, Some(rest)) if !rest.is_empty() => rest.to_string(),
            _ => reason,
        };
        let wrapped = wrap(&reason, inner.saturating_sub(word.len() + 2));
        for (i, l) in wrapped.iter().take(REASON_LINES).enumerate() {
            let text = if i == 0 { format!("{word}  {l}") } else { format!("{}  {l}", " ".repeat(word.len())) };
            body.push(line(Error, clip(&format!("  {text}"), width)));
        }
    }

    // ── report ───────────────────────────────────────────────────────
    if let Some(r) = &c.report {
        body.push(line(Section, "report"));
        body.extend(path_lines(None, &c.report_path, width));
        for l in &r.lines {
            body.push(line(Dim, clip(&format!("  │ {}", one_line_keep_indent(l)), width)));
        }
        if r.more {
            body.push(line(Dim, "  │ …"));
        }
    }

    // ── paths ────────────────────────────────────────────────────────
    let mut paths = vec![line(Section, "paths")];
    paths.extend(path_lines(Some("dir"), &c.dir, width));
    if c.transcript_exists {
        paths.extend(path_lines(Some("transcript"), &c.transcript_path, width));
    }

    // Hard cap. The budgets above keep a card well under it; this is the
    // backstop, and it sacrifices the middle, never the paths.
    let budget = CARD_MAX_LINES.saturating_sub(paths.len());
    if body.len() > budget {
        let dropped = body.len() - (budget - 1);
        body.truncate(budget - 1);
        body.push(line(Dim, format!("  … {dropped} more lines not shown")));
    }
    body.extend(paths);

    // Defence in depth: whatever slipped through the field-level cleaning,
    // no control character leaves this function.
    for l in &mut body {
        if l.text.chars().any(char::is_control) {
            l.text = sanitize(&l.text);
        }
        if l.kind != Path {
            l.text = clip(&l.text, width);
        }
    }
    body
}

/// A path as card lines. Whole on one line when it fits (`  dir         /p`);
/// otherwise the label gets its own line and the path follows with no
/// indent, because the scrollback inserter soft-wraps at the last space and
/// an indented path that is too long would leave a blank row ahead of it.
/// A path is never clipped: one cut short cannot be opened.
fn path_lines(label: Option<&str>, path: &Path, width: usize) -> Vec<CardLine> {
    let p = sanitize(&path.display().to_string());
    let one = match label {
        Some(l) => format!("  {l:<12}{p}"),
        None => format!("  {p}"),
    };
    if display_width(&one) <= width {
        return vec![line(LineKind::Path, one)];
    }
    let mut out = Vec::new();
    if let Some(l) = label {
        out.push(line(LineKind::Dim, format!("  {l}")));
    }
    out.push(line(LineKind::Path, p));
    out
}

/// A report line without control characters, keeping its indentation (the
/// report is markdown; list nesting is worth seeing).
fn one_line_keep_indent(l: &str) -> String {
    let indent: String = l.chars().take_while(|c| *c == ' ').take(8).collect();
    format!("{indent}{}", sanitize(l.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn row(id: &str, state: AgentState, elapsed: u64, ago: Option<u64>, label: Option<&str>) -> AgentRow {
        AgentRow {
            id: id.into(),
            state,
            elapsed_secs: elapsed,
            finished_ago_secs: ago,
            role: None,
            label: label.map(String::from),
            task_line: None,
        }
    }

    fn input(state: AgentState) -> CardInput {
        CardInput {
            id: "a1".into(),
            state,
            label: Some("review auth".into()),
            role: None,
            elapsed_secs: 65,
            model: None,
            turns: None,
            tokens: None,
            worktree: None,
            branch: None,
            tools: None,
            task: None,
            amendments: Vec::new(),
            brief_truncated: false,
            activity: Activity::default(),
            transcript_exists: false,
            reason: None,
            report_path: PathBuf::from("/x/a1/report.md"),
            report: None,
            dir: PathBuf::from("/x/a1"),
            transcript_path: PathBuf::from("/x/a1/transcript.jsonl"),
        }
    }

    fn text(lines: &[CardLine]) -> String {
        lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    fn assert_clean(lines: &[CardLine], width: usize) {
        assert!(lines.len() <= CARD_MAX_LINES, "{} lines", lines.len());
        for l in lines {
            assert!(!l.text.chars().any(char::is_control), "control char in {:?}", l.text);
            if l.kind != LineKind::Path {
                assert!(display_width(&l.text) <= width, "{:?} wider than {width}", l.text);
            }
        }
    }

    // ── rows ─────────────────────────────────────────────────────────

    #[test]
    fn rows_order_live_first_then_finished_most_recent_first() {
        let mut rows = vec![
            row("a1", AgentState::Completed, 10, Some(300), None),
            row("a10", AgentState::Running, 5, None, None),
            row("a2", AgentState::Failed, 10, Some(5), None),
            row("a9", AgentState::Queued, 0, None, None),
            row("a3", AgentState::Stopped, 10, Some(5), None),
        ];
        order_rows(&mut rows);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        // Live by id (numeric, so a9 before a10), then finished: a3 and a2
        // tie on recency (higher id first), a1 is oldest.
        assert_eq!(ids, ["a9", "a10", "a3", "a2", "a1"]);
    }

    #[test]
    fn build_row_names_from_label_role_or_task() {
        let brief = "---\nid: a1\nrole: reviewer\nlabel: review auth\nstate: done\n---\n\n# Brief\n\n## Task\n\nlook at login\n\n## Role\n\nreviewer\n";
        let r = build_row("a1", AgentState::Completed, 3, Some(1), brief);
        assert_eq!(r.name(), "review auth");
        assert_eq!(r.role.as_deref(), Some("reviewer"));
        assert_eq!(r.task_line.as_deref(), Some("look at login"));

        let no_label = "---\nid: a1\nrole: (default)\nlabel: (none)\n---\n\n# Brief\n\n## Task\n\nlook at login\n\n## Role\n\n(default)\n";
        let r = build_row("a1", AgentState::Running, 3, None, no_label);
        assert_eq!(r.name(), "look at login");
        assert_eq!(build_row("a7", AgentState::Running, 0, None, "").name(), "#a7");
    }

    #[test]
    fn build_row_strips_control_characters() {
        let brief = "---\nlabel: hi\u{1b}[31mred\u{7}\n---\n";
        let r = build_row("a1", AgentState::Running, 0, None, brief);
        assert_eq!(r.label.as_deref(), Some("hi[31mred"));
    }

    #[test]
    fn picker_items_carry_glyph_id_name_and_clock() {
        let items = picker_items(&[
            row("a2", AgentState::Running, 65, None, Some("busy")),
            row("a1", AgentState::Completed, 9, Some(4), Some("done one")),
        ]);
        assert_eq!(items[0], ("● a2  busy".to_string(), "running 1m05s".to_string(), "a2".to_string()));
        assert_eq!(items[1].0, "✓ a1  done one");
        assert_eq!(items[1].1, "done 9s");
        assert_eq!(items[1].2, "a1");
    }

    #[test]
    fn picker_names_are_clipped_by_display_width() {
        let long = "日本語".repeat(30);
        let items = picker_items(&[row("a1", AgentState::Running, 0, None, Some(&long))]);
        assert!(display_width(&items[0].0) <= 3 + 4 + PICKER_NAME_COLS, "{}", items[0].0);
        assert!(items[0].0.ends_with('…'));
    }

    // ── resolution ───────────────────────────────────────────────────

    fn named(id: &str, state: AgentState, label: &str) -> AgentRow {
        row(id, state, 1, state.is_terminal().then_some(1), Some(label))
    }

    #[test]
    fn resolve_finds_finished_agents_by_id_and_name() {
        let rows = vec![
            named("a1", AgentState::Completed, "Alpha"),
            named("a2", AgentState::Running, "bravo"),
        ];
        assert_eq!(resolve_agent("a1", &rows), Ok("a1".into()));
        assert_eq!(resolve_agent(" A2 ", &rows), Ok("a2".into()));
        assert_eq!(resolve_agent("alp", &rows), Ok("a1".into()));
        assert_eq!(resolve_agent("BRA", &rows), Ok("a2".into()));
    }

    #[test]
    fn resolve_ambiguous_name_lists_candidates_and_picks_nothing() {
        let rows = vec![
            named("a1", AgentState::Completed, "build api"),
            named("a2", AgentState::Failed, "build ui"),
            named("a3", AgentState::Running, "test"),
        ];
        let err = resolve_agent("build", &rows).unwrap_err();
        assert!(err.contains("a1") && err.contains("a2") && !err.contains("a3"), "{err}");
        assert!(err.contains("2 agents"), "{err}");
    }

    #[test]
    fn resolve_unknown_is_one_line() {
        let rows = vec![named("a1", AgentState::Completed, "alpha")];
        for q in ["a9", "nope"] {
            let err = resolve_agent(q, &rows).unwrap_err();
            assert!(!err.contains('\n'), "{err}");
            assert!(err.contains("no agent"), "{err}");
        }
        assert_eq!(resolve_agent("a1", &[]), Err("no agents in this session".into()));
    }

    #[test]
    fn agent_id_shape() {
        assert!(is_agent_id("a1") && is_agent_id("a42"));
        assert!(!is_agent_id("a") && !is_agent_id("alpha") && !is_agent_id("1") && !is_agent_id("a1b"));
    }

    #[test]
    fn finished_list_lines_mark_only_finished_agents() {
        let lines = finished_list_lines(
            &[
                row("a1", AgentState::Running, 5, None, Some("live")),
                row("a2", AgentState::Failed, 70, Some(1), Some("broke")),
            ],
            80,
        );
        assert_eq!(lines, vec!["✗ a2  broke  failed 1m10s".to_string()]);
    }

    // ── parsing ──────────────────────────────────────────────────────

    #[test]
    fn task_text_stops_at_the_role_section() {
        let b = "---\nid: a1\n---\n\n# Brief\n\n## Task\n\nline one\nline two\n\n## Role\n\nr\n\n## Tools\n\n(all)\n\n## Model\n\n(inherit)\n\n<!-- nanopi:amendments -->\n";
        assert_eq!(task_text(b).as_deref(), Some("line one\nline two"));
        assert_eq!(task_text("just text").as_deref(), Some("just text"));
        assert_eq!(task_text("---\nid: a1\n---\n"), None);
    }

    #[test]
    fn tools_come_from_json_or_the_brief_line() {
        assert_eq!(parse_tools_json(r#"{"tools":["read","grep"]}"#), Some(vec!["read".into(), "grep".into()]));
        assert_eq!(parse_tools_json(r#"{"tools":[]}"#), Some(vec![]));
        assert_eq!(parse_tools_json("garbage"), None);
        assert_eq!(parse_tools_json(r#"{"tools":[1]}"#), None);
        assert_eq!(parse_tools_line("(all)"), Some(vec![]));
        assert_eq!(parse_tools_line("read, grep"), Some(vec!["read".into(), "grep".into()]));
        // A line cut short by the writer loses its half-written last name.
        assert_eq!(parse_tools_line("read, gr…"), Some(vec!["read".into()]));
    }

    #[test]
    fn report_reason_reads_the_summary_section() {
        let r = "---\nid: a1\nstate: failed\n---\n\n# Report\n\n## Summary\n\nboom: the thing broke\nmore detail\n\n## Files changed\n\n(none)\n";
        assert_eq!(report_reason(r).as_deref(), Some("boom: the thing broke more detail"));
        assert_eq!(report_reason("no sections"), None);
    }

    #[test]
    fn report_head_caps_lines_and_notes_more() {
        let body: String = (0..40).map(|i| format!("line {i}\n\n")).collect();
        let h = report_head(&format!("---\nid: a1\n---\n{body}"), false);
        assert_eq!(h.lines.len(), REPORT_LINES);
        assert_eq!(h.lines[0], "line 0");
        assert!(h.more);
        let short = report_head("---\nid: a1\n---\nonly\n", false);
        assert_eq!((short.lines.len(), short.more), (1, false));
        assert!(report_head("x\n", true).more, "a cut read means there is more");
    }

    fn jl(vs: &[Value]) -> Vec<String> {
        vs.iter().map(Value::to_string).collect()
    }

    #[test]
    fn activity_keeps_the_last_entries_and_counts_the_rest() {
        let mut lines = vec![serde_json::json!({"type":"session","version":2}).to_string()];
        for i in 0..10 {
            lines.push(serde_json::json!({"type":"tool_call","id":format!("c{i}"),"timestamp":"t","tool_name":"read","arguments":{"path":format!("f{i}.rs")}}).to_string());
            lines.push(serde_json::json!({"type":"tool_result","tool_call_id":format!("c{i}"),"timestamp":"t","content":"fine\nsecond","is_error":false}).to_string());
        }
        lines.push("{ torn".into());
        lines.push(serde_json::json!({"type":"model_change","timestamp":"t","from":"a","to":"b"}).to_string());
        let a = parse_activity(&lines, false, 12);
        assert_eq!(a.shown.len(), 12);
        assert_eq!(a.earlier, 8);
        assert_eq!(a.tool_calls, 10);
        assert_eq!(a.shown[0], ActivityEntry::ToolCall { name: "read".into(), args: "f4.rs".into() });
        assert_eq!(a.shown[1], ActivityEntry::ToolResult { ok: true, text: "fine".into() });
    }

    #[test]
    fn the_brief_message_is_named_not_quoted() {
        let brief = "---\nid: a1\nrole: x\n---\n\n# Brief\n";
        let a = parse_activity(
            &jl(&[
                serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"user","content":brief}),
                serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"user","content":"--- is a rule, not a brief"}),
            ]),
            false,
            12,
        );
        assert_eq!(
            a.shown,
            vec![
                ActivityEntry::User("(the task brief)".into()),
                ActivityEntry::User("--- is a rule, not a brief".into()),
            ]
        );
    }

    #[test]
    fn activity_shapes() {
        let a = parse_activity(
            &jl(&[
                serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"assistant","content":"\n  first line\nsecond"}),
                serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"assistant","content":"   "}),
                serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"user","content":"do it"}),
                serde_json::json!({"type":"tool_call","id":"c","timestamp":"t","tool_name":"x","arguments":{"a":1}}),
                serde_json::json!({"type":"tool_call","id":"c","timestamp":"t","tool_name":"y","arguments":{}}),
                serde_json::json!({"type":"tool_result","tool_call_id":"c","timestamp":"t","content":"","is_error":true}),
            ]),
            false,
            12,
        );
        assert_eq!(
            a.shown,
            vec![
                ActivityEntry::Assistant("first line".into()),
                ActivityEntry::User("do it".into()),
                ActivityEntry::ToolCall { name: "x".into(), args: r#"{"a":1}"#.into() },
                ActivityEntry::ToolCall { name: "y".into(), args: String::new() },
                ActivityEntry::ToolResult { ok: false, text: String::new() },
            ]
        );
    }

    // ── card ─────────────────────────────────────────────────────────

    #[test]
    fn running_card_has_header_activity_and_paths_only() {
        let mut c = input(AgentState::Running);
        c.task = Some("review the auth module".into());
        c.transcript_exists = true;
        c.activity = Activity {
            shown: vec![
                ActivityEntry::ToolCall { name: "read".into(), args: "src/auth.rs".into() },
                ActivityEntry::ToolResult { ok: true, text: "pub fn login".into() },
                ActivityEntry::Assistant("looking at login".into()),
            ],
            earlier: 0,
            window_cut: false,
            tool_calls: 1,
        };
        let out = render_card(&c, 80);
        let t = text(&out);
        assert_eq!(out[0].text, "● a1  review auth");
        assert_eq!(out[0].kind, LineKind::Header);
        assert!(t.contains("running · 1m05s so far · 1 tool calls"), "{t}");
        assert!(t.contains("→ read(src/auth.rs)") && t.contains("← ok  pub fn login") && t.contains("◆ looking at login"), "{t}");
        assert!(t.contains("dir         /x/a1") && t.contains("transcript  /x/a1/transcript.jsonl"), "{t}");
        assert!(!t.contains("report") && !t.contains("error"), "no report/reason for a live agent: {t}");
        assert!(!t.contains("model") && !t.contains("turns") && !t.contains("tokens"), "unknown fields are omitted: {t}");
        assert_clean(&out, 80);
    }

    #[test]
    fn completed_card_shows_duration_turns_tokens_model_tools_and_report() {
        let mut c = input(AgentState::Completed);
        c.model = Some("gpt-4o".into());
        c.turns = Some("3".into());
        c.tokens = Some("1200".into());
        c.tools = Some(vec!["read".into(), "grep".into()]);
        c.worktree = Some("/wt/a1".into());
        c.branch = Some("agent/a1".into());
        c.report = Some(ReportHead { lines: vec!["# Report".into(), "  all good".into()], more: true });
        let out = render_card(&c, 80);
        let t = text(&out);
        assert!(t.contains("done · took 1m05s · model gpt-4o · turns 3 · tokens 1200"), "{t}");
        assert!(t.contains("tools  read, grep") && t.contains("worktree /wt/a1 (agent/a1)"), "{t}");
        assert!(t.contains("report\n  /x/a1/report.md\n  │ # Report\n  │   all good\n  │ …"), "{t}");
        assert_clean(&out, 80);
    }

    /// A path too long for the terminal is neither cut nor indented (an
    /// indented one would wrap to a blank row first): label on its own
    /// line, the whole path under it.
    #[test]
    fn long_paths_are_kept_whole_and_unindented() {
        let long = "/very/long/".repeat(12) + "a1";
        let mut c = input(AgentState::Completed);
        c.dir = PathBuf::from(&long);
        c.report_path = PathBuf::from(format!("{long}/report.md"));
        c.report = Some(ReportHead::default());
        c.transcript_exists = true;
        c.transcript_path = PathBuf::from(format!("{long}/transcript.jsonl"));
        let out = render_card(&c, 40);
        let paths: Vec<&CardLine> = out.iter().filter(|l| l.kind == LineKind::Path).collect();
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0].text, format!("{long}/report.md"));
        assert_eq!(paths[1].text, long);
        assert_eq!(paths[2].text, format!("{long}/transcript.jsonl"));
        let t = text(&out);
        assert!(t.contains("\n  dir\n/very/long/") && t.contains("\n  transcript\n/very/long/"), "{t}");
        assert_clean(&out, 40);
    }

    #[test]
    fn a_failure_reason_does_not_repeat_the_word_error() {
        let mut c = input(AgentState::Failed);
        c.reason = Some("error: provider returned 400".into());
        let out = render_card(&c, 80);
        let l = out.iter().find(|l| l.kind == LineKind::Error).unwrap();
        assert_eq!(l.text, "  error  provider returned 400");
        // Other states keep the text as written.
        let mut c = input(AgentState::Stopped);
        c.reason = Some("error: odd".into());
        let out = render_card(&c, 80);
        assert_eq!(out.iter().find(|l| l.kind == LineKind::Error).unwrap().text, "  stopped  error: odd");
    }

    #[test]
    fn default_tool_set_is_stated() {
        let mut c = input(AgentState::Running);
        c.tools = Some(vec![]);
        assert!(text(&render_card(&c, 80)).contains("tools  all (default set)"));
    }

    #[test]
    fn failed_and_stopped_cards_show_the_reason() {
        for (state, word) in [(AgentState::Failed, "error"), (AgentState::Stopped, "stopped"), (AgentState::LimitReached, "limit reached")] {
            let mut c = input(state);
            c.reason = Some("it broke badly".into());
            let out = render_card(&c, 80);
            let l = out.iter().find(|l| l.kind == LineKind::Error).expect("reason line");
            assert_eq!(l.text, format!("  {word}  it broke badly"));
        }
        // A completed agent never prints one, even if given.
        let mut c = input(AgentState::Completed);
        c.reason = None;
        assert!(!render_card(&c, 80).iter().any(|l| l.kind == LineKind::Error));
    }

    #[test]
    fn task_is_cut_to_300_chars_with_an_ellipsis() {
        let mut c = input(AgentState::Running);
        c.task = Some("word ".repeat(200));
        let out = render_card(&c, 100);
        let task: Vec<&CardLine> = {
            let i = out.iter().position(|l| l.text == "task").unwrap();
            out[i + 1..].iter().take_while(|l| l.kind == LineKind::Body).collect()
        };
        let joined: String = task.iter().map(|l| l.text.trim()).collect::<Vec<_>>().join(" ");
        assert!(joined.ends_with('…'), "{joined}");
        assert!(joined.chars().count() <= 302, "{}", joined.chars().count());
        assert!(task.len() <= TASK_LINES);
        assert_clean(&out, 100);
    }

    #[test]
    fn amendments_show_count_and_last_text() {
        let mut c = input(AgentState::Running);
        c.amendments = vec![(1, "first".into()), (2, "also check the tests".into())];
        let t = text(&render_card(&c, 80));
        assert!(t.contains("amendments (2 so far)\n  #2 also check the tests"), "{t}");
        assert!(!t.contains("first"), "{t}");
    }

    #[test]
    fn no_transcript_says_so() {
        let c = input(AgentState::Queued);
        let t = text(&render_card(&c, 80));
        assert!(t.contains("activity\n  (no transcript yet)"), "{t}");
        assert!(!t.contains("transcript  "), "a missing transcript has no path line: {t}");
        let mut c = input(AgentState::Running);
        c.transcript_exists = true;
        assert!(text(&render_card(&c, 80)).contains("(nothing recorded yet)"));
    }

    #[test]
    fn earlier_entries_note_counts_and_marks_a_cut_window() {
        let mut c = input(AgentState::Running);
        c.transcript_exists = true;
        c.activity = Activity {
            shown: vec![ActivityEntry::Assistant("x".into())],
            earlier: 28,
            window_cut: false,
            tool_calls: 0,
        };
        assert!(text(&render_card(&c, 80)).contains("  … 28 earlier entries not shown"));
        c.activity.window_cut = true;
        assert!(text(&render_card(&c, 80)).contains("  … 28+ earlier entries not shown"));
    }

    #[test]
    fn hostile_input_never_reaches_the_output_as_control_characters() {
        let evil = "a\u{1b}[2J\u{1b}]0;pwn\u{7}b\r\nc\u{0}d\u{85}e\u{9b}f";
        let mut c = input(AgentState::Failed);
        c.id = format!("a1{evil}");
        c.label = Some(evil.into());
        c.model = Some(evil.into());
        c.turns = Some(evil.into());
        c.tokens = Some(evil.into());
        c.worktree = Some(evil.into());
        c.branch = Some(evil.into());
        c.tools = Some(vec![evil.into()]);
        c.task = Some(evil.repeat(20));
        c.amendments = vec![(1, evil.into())];
        c.reason = Some(evil.into());
        c.transcript_exists = true;
        c.activity = Activity {
            shown: vec![
                ActivityEntry::User(evil.into()),
                ActivityEntry::Assistant(evil.into()),
                ActivityEntry::ToolCall { name: evil.into(), args: evil.into() },
                ActivityEntry::ToolResult { ok: false, text: evil.into() },
            ],
            earlier: 3,
            window_cut: true,
            tool_calls: 1,
        };
        c.report = Some(ReportHead { lines: vec![evil.into(); 20], more: true });
        for width in [20, 40, 80, 200] {
            assert_clean(&render_card(&c, width), width);
        }
    }

    #[test]
    fn cjk_text_is_clipped_by_display_width() {
        let cjk = "日本語のテキスト".repeat(40);
        let mut c = input(AgentState::Running);
        c.label = Some(cjk.clone());
        c.task = Some(cjk.clone());
        c.transcript_exists = true;
        c.activity = Activity {
            shown: vec![ActivityEntry::Assistant(cjk.clone()), ActivityEntry::ToolCall { name: cjk.clone(), args: cjk.clone() }],
            earlier: 0,
            window_cut: false,
            tool_calls: 1,
        };
        c.report = Some(ReportHead { lines: vec![cjk.clone()], more: false });
        for width in [20, 33, 60] {
            let out = render_card(&c, width);
            assert_clean(&out, width);
            // Clipping lands on a character boundary, never inside a wide one.
            assert!(out.iter().any(|l| l.text.ends_with('…')), "{}", text(&out));
        }
    }

    #[test]
    fn card_size_is_capped_with_the_paths_kept() {
        let mut c = input(AgentState::Failed);
        c.task = Some("x ".repeat(500));
        c.amendments = vec![(1, "y ".repeat(500))];
        c.tools = Some((0..200).map(|i| format!("tool{i}")).collect());
        c.reason = Some("z ".repeat(500));
        c.transcript_exists = true;
        c.activity = Activity {
            shown: vec![ActivityEntry::Assistant("w ".repeat(300)); 12],
            earlier: 99,
            window_cut: true,
            tool_calls: 99,
        };
        c.report = Some(ReportHead { lines: vec!["r".repeat(500); REPORT_LINES], more: true });
        for width in [20, 30, 120] {
            let out = render_card(&c, width);
            assert_clean(&out, width);
            let t = text(&out);
            assert!(t.contains("/x/a1/transcript.jsonl"), "paths survive: {t}");
        }
        // A deliberately oversized body trips the backstop.
        let mut big = input(AgentState::Failed);
        big.report = Some(ReportHead { lines: vec!["r".into(); 500], more: false });
        // report_head caps at the loader; the renderer must cap regardless.
        let out = render_card(&big, 80);
        assert_eq!(out.len(), CARD_MAX_LINES);
        assert!(text(&out).contains("more lines not shown"));
        assert!(out.last().unwrap().text.contains("/x/a1"));
    }

    // ── loaders (real files) ─────────────────────────────────────────

    fn entry_for(dir: &Path, state: AgentState, started: Instant, finished: Option<Instant>) -> AgentEntry {
        AgentEntry { id: "a1".into(), pid: None, state, started, finished, dir: dir.to_path_buf() }
    }

    #[test]
    fn load_card_reads_what_is_on_disk() {
        let d = tempfile::tempdir().unwrap();
        let brief = "---\nid: a1\nrole: reviewer\nmodel: (inherit)\ntools: read, grep\nstate: failed\nlabel: review auth\n---\n\n# Brief\n\n## Task\n\nlook at login\n\n## Role\n\nreviewer\n\n## Tools\n\n- read\n\n## Model\n\n(inherit)\n\n<!-- nanopi:amendments -->\n\n## Amendment 1 (2026-01-01T00:00:00+00:00)\n\nalso the tests\n";
        std::fs::write(d.path().join("brief.md"), brief).unwrap();
        std::fs::write(d.path().join("tools.json"), r#"{"tools":["read"]}"#).unwrap();
        std::fs::write(
            d.path().join("report.md"),
            "---\nid: a1\nstate: failed\nturns: 4\ntokens: (unknown)\n---\n\n# Report\n\n## Summary\n\nit exploded\n",
        )
        .unwrap();
        let t = [
            serde_json::json!({"type":"session","version":2,"id":"s","timestamp":"t","cwd":"/","model":"m-1","base_url":"u"}),
            serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"assistant","content":"hello"}),
        ];
        std::fs::write(d.path().join("transcript.jsonl"), format!("{}\n{}\n", t[0], t[1])).unwrap();

        let t0 = Instant::now();
        let e = entry_for(d.path(), AgentState::Failed, t0, Some(t0 + Duration::from_secs(70)));
        let c = load_card(&e, t0 + Duration::from_secs(500));
        assert_eq!(c.elapsed_secs, 70, "a finished agent's clock stops");
        assert_eq!(c.label.as_deref(), Some("review auth"));
        assert_eq!(c.model.as_deref(), Some("m-1"), "falls back to the session header");
        assert_eq!(c.turns.as_deref(), Some("4"));
        assert_eq!(c.tokens, None);
        assert_eq!(c.tools, Some(vec!["read".to_string()]));
        assert_eq!(c.task.as_deref(), Some("look at login"));
        assert_eq!(c.amendments, vec![(1, "also the tests".to_string())]);
        assert_eq!(c.reason.as_deref(), Some("it exploded"));
        assert_eq!(c.activity.shown, vec![ActivityEntry::Assistant("hello".into())]);
        assert!(c.transcript_exists && c.report.is_some());
        assert_clean(&render_card(&c, 60), 60);
    }

    #[test]
    fn load_card_survives_an_empty_directory() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let c = load_card(&entry_for(d.path(), AgentState::Running, t0, None), t0);
        assert!(c.task.is_none() && c.report.is_none() && !c.transcript_exists);
        let out = render_card(&c, 60);
        assert!(text(&out).contains("(no transcript yet)"));
        assert_clean(&out, 60);
        // And a directory that does not exist at all.
        let c = load_card(&entry_for(&d.path().join("gone"), AgentState::Queued, t0, None), t0);
        assert_clean(&render_card(&c, 60), 60);
    }

    #[test]
    fn garbled_huge_transcript_is_read_through_a_bounded_tail() {
        let d = tempfile::tempdir().unwrap();
        let mut f: Vec<u8> = Vec::new();
        // ~1 MiB of valid entries, then invalid UTF-8, then escape-laden
        // text, then a torn last line.
        for i in 0..20_000 {
            f.extend_from_slice(
                serde_json::json!({"type":"tool_call","id":format!("c{i}"),"timestamp":"t","tool_name":"read","arguments":{"path":format!("f{i}")}})
                    .to_string()
                    .as_bytes(),
            );
            f.push(b'\n');
        }
        f.extend_from_slice(b"\xff\xfe\xc3\n");
        f.extend_from_slice(
            serde_json::json!({"type":"message","id":"m","timestamp":"t","role":"assistant","content":"x\u{1b}[31mred\u{1b}[0m"})
                .to_string()
                .as_bytes(),
        );
        f.push(b'\n');
        f.extend_from_slice(b"{\"type\":\"message\",\"id\":\"m\",\"role\":\"assist");
        assert!(f.len() > 1_000_000);
        std::fs::write(d.path().join("transcript.jsonl"), &f).unwrap();
        // A brief and report bigger than their caps too.
        std::fs::write(d.path().join("brief.md"), format!("---\nid: a1\n---\n\n## Task\n\n{}", "t".repeat(2_000_000))).unwrap();
        std::fs::write(d.path().join("report.md"), "x\n".repeat(1_000_000)).unwrap();

        let t0 = Instant::now();
        let c = load_card(&entry_for(d.path(), AgentState::Running, t0, None), t0);
        assert_eq!(c.activity.shown.len(), ACTIVITY_ENTRIES);
        assert!(c.activity.window_cut && c.activity.earlier > 0);
        assert!(c.brief_truncated);
        assert!(c.report.as_ref().unwrap().more);
        let out = render_card(&c, 70);
        assert_clean(&out, 70);
        let t = text(&out);
        assert!(t.contains("◆ x[31mred[0m"), "escapes are stripped, not executed: {t}");
        assert!(t.contains("earlier entries not shown"), "{t}");
    }

    #[test]
    fn collect_rows_orders_live_before_finished() {
        let root = tempfile::tempdir().unwrap();
        let mk = |id: &str, label: &str| {
            let dir = root.path().join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("brief.md"), format!("---\nid: {id}\nlabel: {label}\n---\n")).unwrap();
            dir
        };
        let t0 = Instant::now();
        let mut snapshot = Vec::new();
        for (id, label, state, fin) in [
            ("a1", "first", AgentState::Completed, Some(10)),
            ("a2", "second", AgentState::Running, None),
            ("a3", "third", AgentState::Failed, Some(20)),
        ] {
            let mut e = entry_for(&mk(id, label), state, t0, fin.map(|s| t0 + Duration::from_secs(s)));
            e.id = id.into();
            snapshot.push(e);
        }
        let rows = collect_rows(&snapshot, t0 + Duration::from_secs(100));
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a2", "a3", "a1"]);
        assert_eq!(rows[0].elapsed_secs, 100);
        assert_eq!(rows[1].elapsed_secs, 20);
        assert_eq!(rows[1].finished_ago_secs, Some(80));
    }
}

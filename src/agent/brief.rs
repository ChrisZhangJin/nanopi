//! Brief / amendment / report text model for subagents (RT-09, D-09..D-11).
//!
//! A brief is a markdown file the orchestrator writes for a child. Later
//! instructions are appended as `## Amendment N` sections. Amendments are
//! only recognised after [`AMENDMENTS_MARKER`], so headings inside the
//! original task text never count as amendments.

use std::io::{self, Write};
use std::path::Path;

/// Separates the rendered brief from appended amendments.
pub const AMENDMENTS_MARKER: &str = "<!-- nanopi:amendments -->";

const AMENDMENT_PREFIX: &str = "## Amendment ";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BriefSpec {
    pub task: String,
    pub role: Option<String>,
    pub tools: Vec<String>,
    pub model: Option<String>,
}

/// Neutralise lines that would be mistaken for structure on re-parse.
fn escape_body(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim() == AMENDMENTS_MARKER || l.starts_with(AMENDMENT_PREFIX) {
                format!("\\{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_brief(spec: &BriefSpec) -> String {
    let mut out = String::from("# Brief\n\n## Task\n\n");
    out.push_str(escape_body(spec.task.trim_end()).as_str());
    out.push_str("\n\n## Role\n\n");
    out.push_str(spec.role.as_deref().unwrap_or("(default)"));
    out.push_str("\n\n## Tools\n\n");
    if spec.tools.is_empty() {
        out.push_str("(all)\n");
    } else {
        for t in &spec.tools {
            out.push_str(&format!("- {t}\n"));
        }
    }
    out.push_str("\n## Model\n\n");
    out.push_str(spec.model.as_deref().unwrap_or("(inherit)"));
    out.push_str("\n\n");
    out.push_str(AMENDMENTS_MARKER);
    out.push('\n');
    out
}

/// Append `## Amendment n` with a single `write_all` on an append handle
/// (atomic w.r.t. concurrent readers for typical sizes). New files are
/// created with mode 0o600.
pub fn append_amendment(path: &Path, n: u32, text: &str) -> io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    let chunk = format!(
        "\n{AMENDMENT_PREFIX}{n}\n\n{}\n",
        escape_body(text.trim_end())
    );
    f.write_all(chunk.as_bytes())
}

/// Parse amendments, treating the content as complete.
pub fn parse_amendments(content: &str) -> Vec<(u32, String)> {
    parse_amendments_with(content, true)
}

/// Parse amendments. With `stable = false` the content may be a torn
/// read of an in-progress append, so a final section lacking its
/// terminating newline is dropped.
pub fn parse_amendments_with(content: &str, stable: bool) -> Vec<(u32, String)> {
    let Some(pos) = content.find(AMENDMENTS_MARKER) else {
        return Vec::new();
    };
    let tail = &content[pos + AMENDMENTS_MARKER.len()..];
    let mut out: Vec<(u32, String, bool)> = Vec::new();
    let mut current: Option<(u32, Vec<&str>)> = None;
    for line in tail.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if let Some(num) = bare.strip_prefix(AMENDMENT_PREFIX) {
            if let Ok(n) = num.trim().parse::<u32>() {
                if let Some((cn, lines)) = current.take() {
                    out.push((cn, join_body(&lines), true));
                }
                current = Some((n, Vec::new()));
                continue;
            }
        }
        if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    if let Some((cn, lines)) = current {
        let terminated = tail.ends_with('\n');
        out.push((cn, join_body(&lines), terminated));
    }
    out.into_iter()
        .filter(|(_, _, complete)| stable || *complete)
        .map(|(n, t, _)| (n, t))
        .collect()
}

fn join_body(lines: &[&str]) -> String {
    let joined: String = lines.concat();
    joined
        .trim()
        .lines()
        .map(|l| {
            l.strip_prefix('\\')
                .filter(|r| r.starts_with(AMENDMENT_PREFIX) || r.trim() == AMENDMENTS_MARKER)
                .unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Highest amendment number + 1 (1 when none).
pub fn next_amendment_number(content: &str) -> u32 {
    parse_amendments(content)
        .iter()
        .map(|(n, _)| *n)
        .max()
        .map_or(1, |n| n.saturating_add(1))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChecklistItem {
    pub label: String,
    pub done: bool,
    pub note: String,
}

pub fn render_report(status: &str, summary: &str, items: &[ChecklistItem]) -> String {
    let mut out = format!(
        "# Report\n\n## Status\n\n{status}\n\n## Summary\n\n{}\n\n## Checklist\n\n",
        summary.trim_end()
    );
    for it in items {
        let mark = if it.done { "x" } else { " " };
        if it.note.is_empty() {
            out.push_str(&format!("- [{mark}] {}\n", it.label));
        } else {
            out.push_str(&format!("- [{mark}] {} — {}\n", it.label, it.note));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(task: &str) -> BriefSpec {
        BriefSpec {
            task: task.into(),
            role: Some("reviewer".into()),
            tools: vec!["read".into()],
            model: None,
        }
    }

    #[test]
    fn render_brief_has_sections() {
        let b = render_brief(&spec("do it"));
        for h in [
            "# Brief",
            "## Task",
            "## Role",
            "## Tools",
            "## Model",
            AMENDMENTS_MARKER,
        ] {
            assert!(b.contains(h), "missing {h}: {b}");
        }
        assert!(parse_amendments(&b).is_empty());
    }

    #[test]
    fn parses_numbered_amendments() {
        let mut b = render_brief(&spec("task"));
        b.push_str("\n## Amendment 1\n\nfirst\n\n## Amendment 2\n\nsecond\nline\n");
        assert_eq!(
            parse_amendments(&b),
            vec![(1, "first".to_string()), (2, "second\nline".to_string())]
        );
        assert_eq!(next_amendment_number(&b), 3);
    }

    #[test]
    fn task_headings_are_not_amendments() {
        let b = render_brief(&spec("## Amendment 1\nnot really"));
        assert!(parse_amendments(&b).is_empty());
        assert_eq!(next_amendment_number(&b), 1);
    }

    #[test]
    fn torn_trailing_section_ignored_when_unstable() {
        let mut b = render_brief(&spec("t"));
        b.push_str("\n## Amendment 1\n\nok\n\n## Amendment 2\n\npart");
        assert_eq!(
            parse_amendments_with(&b, false),
            vec![(1, "ok".to_string())]
        );
        assert_eq!(parse_amendments_with(&b, true).len(), 2);
    }

    #[test]
    fn append_amendment_roundtrip_and_mode() {
        let dir = std::env::temp_dir().join(format!("nanopi-brief-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("brief.md");
        let _ = std::fs::remove_file(&p);
        append_amendment(&p, 1, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&p, render_brief(&spec("t"))).unwrap();
        append_amendment(&p, 1, "first").unwrap();
        append_amendment(&p, 2, "## Amendment 9 inside").unwrap();
        let c = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            parse_amendments_with(&c, false),
            vec![(1, "first".into()), (2, "## Amendment 9 inside".into())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_checklist() {
        let r = render_report(
            "done",
            "all good",
            &[
                ChecklistItem {
                    label: "a".into(),
                    done: true,
                    note: String::new(),
                },
                ChecklistItem {
                    label: "b".into(),
                    done: false,
                    note: "blocked".into(),
                },
            ],
        );
        assert!(r.contains("## Checklist"));
        assert!(r.contains("- [x] a\n"));
        assert!(r.contains("- [ ] b — blocked\n"));
    }
}

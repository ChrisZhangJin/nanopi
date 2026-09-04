//! Grouped startup notices, PI-style.
//!
//! Extension loading used to print one flat `nanopi: …` line per event,
//! straight to stderr as each one happened:
//!
//! ```text
//! nanopi: /home/chris/workspace/nanopi/dist/nanopi-events-plugin.component.wasm has url_allowlist = ["*"] — this plugin may fetch ANY http/https host, including link-local metadata endpoints. Narrow it to `*.example.com` or `example.com`
//! if you can.
//! nanopi: /home/chris/workspace/nanopi/dist/nanopi-events-plugin.component.wasm has both `events` and `allow_network = true` — this plugin can observe lifecycle events AND reach the network, and could exfiltrate event payloads. Grant both
//!  only if you trust the plugin.
//! ```
//!
//! Four things go wrong there, and PI's startup output (see
//! `PI_warning.png` — its `[Skill conflicts]` block) avoids all four:
//!
//! 1. **Severity is invisible.** A security warning about network
//!    exfiltration renders identically to `registered extension tool`.
//! 2. **Continuation lines are indistinguishable from new ones.** The
//!    terminal soft-wraps at column 0, so `if you can.` looks like its
//!    own notice.
//! 3. **The subject is repeated in full**, once per notice. An absolute
//!    plugin path is ~70 columns, pushing the actual message off the
//!    right of a normal terminal.
//! 4. **`nanopi: ` on every line**, which says nothing the surrounding
//!    context doesn't.
//!
//! So: group by subject, print the subject once, hang-indent the body
//! so wrapped text is visibly continuation, and color by severity.
//!
//! Message text is deliberately unchanged from the flat version —
//! `docs/v0.12-manual-test-plan.md` T5.1/T5.2/T5.6 grep for substrings
//! of it, and those greps should keep working across a presentation
//! change.

/// How loud a notice is. Ordered: `Warn` sorts before `Info` so the
/// thing you need to act on is at the top of the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Something failed outright — a plugin that did not load.
    Error,
    /// Loaded, but you should know something. Security grants,
    /// unsatisfied requests, skipped files.
    Warn,
    /// Ordinary progress. No action needed.
    Info,
}

impl Level {
    /// SGR for the marker and subject. Errors red, warnings yellow
    /// (matching PI's `[Skill conflicts]` / `Update Available`), info
    /// dim so it recedes.
    fn color(self) -> &'static str {
        match self {
            Level::Error => "\x1b[31m",
            Level::Warn => "\x1b[33m",
            Level::Info => "\x1b[2m",
        }
    }

    /// One glyph in the gutter. Cheap severity signal that survives a
    /// pipe, `NO_COLOR`, or a terminal that drops SGR — which the
    /// color alone does not.
    fn marker(self) -> &'static str {
        match self {
            Level::Error => "✗",
            Level::Warn => "!",
            Level::Info => "·",
        }
    }
}

/// One thing worth telling the user at startup.
#[derive(Debug, Clone)]
pub struct Notice {
    pub level: Level,
    /// What it is about — a plugin path, a skill file. Notices sharing
    /// a subject are grouped under one heading. `None` groups under no
    /// heading at all.
    pub subject: Option<String>,
    pub message: String,
}

impl Notice {
    pub fn warn(subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            subject: Some(subject.into()),
            message: message.into(),
        }
    }

    pub fn error(subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            subject: Some(subject.into()),
            message: message.into(),
        }
    }

    pub fn info(subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Info,
            subject: Some(subject.into()),
            message: message.into(),
        }
    }
}

/// Shorten a subject for the heading: the file name, not the path.
///
/// Full paths are kept when two different directories contribute the
/// same file name — otherwise the heading would claim two plugins are
/// one. Callers pass every subject in the block so this can be decided
/// for the block as a whole rather than per notice.
pub fn short_subjects(subjects: &[String]) -> std::collections::HashMap<String, String> {
    use std::collections::{HashMap, HashSet};
    // Count DISTINCT paths per basename. Counting notices instead made
    // a single plugin with two warnings look like two files sharing a
    // name, and printed the full path for both.
    let distinct: HashSet<&String> = subjects.iter().collect();
    let mut basename_count: HashMap<&str, usize> = HashMap::new();
    for s in &distinct {
        *basename_count.entry(basename(s)).or_insert(0) += 1;
    }
    distinct
        .into_iter()
        .map(|s| {
            let b = basename(s);
            // Ambiguous basename → keep the path so the heading still
            // identifies exactly one file.
            let display = if basename_count.get(b).copied().unwrap_or(0) > 1 {
                s.clone()
            } else {
                b.to_string()
            };
            (s.clone(), display)
        })
        .collect()
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Wrap `text` to `width` columns, breaking on spaces. Unlike the TUI's
/// `wrap_chars` this KEEPS the space at the break: these lines are
/// emitted with explicit newlines, so a dropped space would silently
/// corrupt the text on copy-paste.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let add = if cur.is_empty() {
            word.chars().count()
        } else {
            word.chars().count() + 1
        };
        if !cur.is_empty() && cur.chars().count() + add > width {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Terminal width, or 80 when it can't be determined (a pipe, a dumb
/// terminal). Capped: a 400-column window should not produce 400-column
/// paragraphs, which are harder to read than the wrap.
fn text_width() -> usize {
    let cols = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    cols.clamp(40, 100)
}

/// Render one titled block. Returns the lines rather than printing so
/// the shape is testable — `emit` does the writing.
pub fn block_lines(title: &str, notices: &[Notice], width: usize) -> Vec<String> {
    if notices.is_empty() {
        return Vec::new();
    }
    let subjects: Vec<String> = notices.iter().filter_map(|n| n.subject.clone()).collect();
    let short = short_subjects(&subjects);

    let mut lines = vec![format!("\x1b[33m[{title}]\x1b[0m")];

    // Group by subject, preserving first-appearance order so the
    // output tracks the order things actually happened.
    let mut order: Vec<Option<String>> = Vec::new();
    for n in notices {
        if !order.contains(&n.subject) {
            order.push(n.subject.clone());
        }
    }

    for subj in &order {
        let mut group: Vec<&Notice> = notices.iter().filter(|n| &n.subject == subj).collect();
        // Loudest first within a group.
        group.sort_by_key(|n| n.level);

        let body_indent = if let Some(s) = subj {
            let display = short.get(s).cloned().unwrap_or_else(|| s.clone());
            lines.push(format!("  \x1b[1m{display}\x1b[0m"));
            4
        } else {
            2
        };

        for n in group {
            // `!` in the gutter, then the message hang-indented under
            // it so wrapped text can't be mistaken for a new notice.
            let gutter = format!("{:indent$}", "", indent = body_indent);
            let avail = width.saturating_sub(body_indent + 2).max(20);
            let wrapped = wrap(&n.message, avail);
            for (i, seg) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(format!(
                        "{gutter}{}{}\x1b[0m {seg}",
                        n.level.color(),
                        n.level.marker()
                    ));
                } else {
                    // Two extra columns: aligns under the text, not
                    // under the marker.
                    lines.push(format!("{gutter}  {seg}"));
                }
            }
        }
    }
    lines
}

/// Print a titled block to stderr, raw-mode-aware.
pub fn emit(title: &str, notices: &[Notice]) {
    for line in block_lines(title, notices, text_width()) {
        crate::render::raw_tty::note(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR so assertions read as the text a user sees.
    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let mut out = String::new();
                let mut chars = l.chars();
                while let Some(c) = chars.next() {
                    if c == '\x1b' {
                        for c2 in chars.by_ref() {
                            if c2 == 'm' {
                                break;
                            }
                        }
                    } else {
                        out.push(c);
                    }
                }
                out
            })
            .collect()
    }

    #[test]
    fn an_empty_block_prints_nothing() {
        assert!(block_lines("Warnings", &[], 80).is_empty());
    }

    /// The shape: title, subject once, one gutter marker per notice.
    #[test]
    fn notices_group_under_one_subject_heading() {
        let p = "/long/path/to/events-plugin.component.wasm";
        let lines = plain(&block_lines(
            "Extensions",
            &[
                Notice::warn(p, "first warning"),
                Notice::warn(p, "second warning"),
            ],
            80,
        ));
        assert_eq!(
            lines,
            vec![
                "[Extensions]",
                // Basename only — the full path is 42 columns of noise
                // repeated per notice in the flat version.
                "  events-plugin.component.wasm",
                "    ! first warning",
                "    ! second warning",
            ]
        );
    }

    /// A wrapped message must be visibly continuation, and must not
    /// lose the space at the break — the flat version's worst failure
    /// was `if you can.` looking like its own notice.
    #[test]
    fn wrapped_text_is_indented_under_the_message_not_the_marker() {
        let lines = plain(&block_lines(
            "Warnings",
            &[Notice::warn("p.wasm", "alpha beta gamma delta epsilon")],
            26,
        ));
        assert_eq!(
            lines,
            vec![
                "[Warnings]",
                "  p.wasm",
                "    ! alpha beta gamma",
                "      delta epsilon",
            ]
        );
        // Rejoining the body reproduces the message exactly — no word
        // welded to its neighbour.
        let body: String = lines[2..]
            .iter()
            .map(|l| l.trim_start().trim_start_matches("! "))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(body, "alpha beta gamma delta epsilon");
    }

    /// Same basename from two directories keeps both full paths —
    /// otherwise the heading claims two plugins are one.
    #[test]
    fn ambiguous_basenames_keep_their_paths() {
        let lines = plain(&block_lines(
            "Extensions",
            &[
                Notice::warn("/a/plugin.wasm", "from a"),
                Notice::warn("/b/plugin.wasm", "from b"),
            ],
            80,
        ));
        assert!(lines.contains(&"  /a/plugin.wasm".to_string()), "{lines:?}");
        assert!(lines.contains(&"  /b/plugin.wasm".to_string()), "{lines:?}");
    }

    /// Errors sort above warnings above info inside a group: the thing
    /// you must act on is not below three lines you needn't read.
    #[test]
    fn louder_notices_come_first_within_a_group() {
        let lines = plain(&block_lines(
            "Extensions",
            &[
                Notice::info("p.wasm", "registered tool"),
                Notice::error("p.wasm", "failed to load"),
                Notice::warn("p.wasm", "broad allowlist"),
            ],
            80,
        ));
        assert_eq!(
            &lines[2..],
            &[
                "    ✗ failed to load",
                "    ! broad allowlist",
                "    · registered tool",
            ]
        );
    }

    /// Severity survives a pipe. Color alone does not — `NO_COLOR`, a
    /// log file, or a terminal that drops SGR all flatten it.
    #[test]
    fn the_marker_carries_severity_without_color() {
        for (n, want) in [
            (Notice::error("p", "e"), "✗"),
            (Notice::warn("p", "w"), "!"),
            (Notice::info("p", "i"), "·"),
        ] {
            let lines = plain(&block_lines("T", &[n], 80));
            assert!(lines[2].trim_start().starts_with(want), "{lines:?}");
        }
    }

    /// Subject-less notices get no heading and sit one level in.
    #[test]
    fn a_notice_without_a_subject_needs_no_heading() {
        let lines = plain(&block_lines(
            "Warnings",
            &[Notice {
                level: Level::Warn,
                subject: None,
                message: "no subject".into(),
            }],
            80,
        ));
        assert_eq!(lines, vec!["[Warnings]", "  ! no subject"]);
    }

    #[test]
    fn wrap_never_loses_or_duplicates_a_word() {
        let text = "the quick brown fox jumps over the lazy dog";
        for w in 5..=50 {
            let joined = wrap(text, w).join(" ");
            assert_eq!(joined, text, "width {w} corrupted the text");
        }
    }

    /// A word longer than the width goes on its own line rather than
    /// being dropped or silently truncated — long URLs and absolute
    /// paths appear in these messages.
    #[test]
    fn an_overlong_word_survives_intact() {
        let long = "https://example.com/a/very/long/url/that/exceeds/the/width";
        let lines = wrap(&format!("see {long} now"), 20);
        assert!(lines.iter().any(|l| l == long), "{lines:?}");
        assert_eq!(lines.join(" "), format!("see {long} now"));
    }

    #[test]
    fn text_width_is_clamped_to_a_readable_range() {
        let w = text_width();
        assert!((40..=100).contains(&w), "got {w}");
    }
}

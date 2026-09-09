//! Minimal inline-markdown → styled Spans renderer for the TUI.
//!
//! Handles the small set of things the model actually emits often
//! (see PI's approach in `packages/tui/src/components/markdown.ts`,
//! trimmed to what nanopi needs):
//!
//!   `# H1` / `## H2` / `### H3`   → colored bold, whole line
//!   `- item` / `1. item`          → keep prefix, dim the marker
//!   `> quote`                     → dim `▏` gutter + dimmed text
//!   ``` fenced code block         → different bg on all lines inside
//!   `**bold**`                    → bold
//!   `*italic*` / `_italic_`       → italic
//!   `` `code` ``                  → inline code (dim bg)
//!   `[label](url)`                → underline blue label + dim url
//!
//! Not handled (yet): tables, task lists, HTML. (Blockquotes were on
//! this list until `> quote` above shipped; nested `>>` still isn't
//! distinguished from a single level.)
//!
//! Called per-line by the TUI when flushing assistant text. Fenced
//! code blocks are a per-flush *state* — the caller passes a mutable
//! `MdState` so `in_code` persists across lines.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

#[derive(Debug, Default)]
pub struct MdState {
    /// True when we're inside a ``` fenced block. Every line rendered
    /// while true gets the code-block bg style, no inline parsing.
    pub in_code: bool,
}

/// Convert one line of markdown to a Vec<Span>. Mutates `state` when
/// the line is a fence delimiter.
pub fn render_line<'a>(line: &'a str, state: &mut MdState) -> Vec<Span<'a>> {
    // Fenced code block toggle.
    let trimmed_start = line.trim_start();
    if trimmed_start.starts_with("```") {
        state.in_code = !state.in_code;
        // Render the fence line as dim (it's a delimiter).
        return vec![Span::styled(
            line.to_string(),
            Style::default().fg(Color::DarkGray),
        )];
    }
    if state.in_code {
        return vec![Span::styled(
            line.to_string(),
            Style::default()
                .bg(Color::Indexed(236))
                .fg(Color::Indexed(252)),
        )];
    }

    // Headings.
    if let Some(rest) = trimmed_start.strip_prefix("# ") {
        return vec![Span::styled(
            format!("# {rest}"),
            Style::default()
                .fg(Color::Indexed(214))
                .add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(rest) = trimmed_start.strip_prefix("## ") {
        return vec![Span::styled(
            format!("## {rest}"),
            Style::default()
                .fg(Color::Indexed(220))
                .add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(rest) = trimmed_start.strip_prefix("### ") {
        return vec![Span::styled(
            format!("### {rest}"),
            Style::default()
                .fg(Color::Indexed(228))
                .add_modifier(Modifier::BOLD),
        )];
    }

    // Blockquotes → dim gutter bar + slightly-dimmed text.
    //
    // Deliberately NOT `DarkGray + ITALIC`: that is what the TUI uses
    // for the model's *thinking* stream, and a reply that quotes
    // something was rendering identically to the model muttering to
    // itself. These models quote tool output constantly, so the
    // collision fired on most turns — see the `> Hello, elon musk!`
    // case, where the reply and the reasoning above it were the same
    // grey italic.
    //
    // The gutter carries the "this is quoted" signal instead, which
    // is both what other renderers do and cheaper than spending
    // another color: 250 is legible enough to still read as speech,
    // where DarkGray reads as an aside.
    if let Some(rest) = trimmed_start
        .strip_prefix("> ")
        .or_else(|| (trimmed_start == ">").then_some(""))
    {
        return vec![
            Span::styled("▏ ", Style::default().fg(Color::DarkGray)),
            Span::styled(rest.to_string(), Style::default().fg(Color::Indexed(250))),
        ];
    }

    // List item marker — dim the `-` / `*` / `1.`, then parse the rest
    // as inline.
    let (marker, body) = split_list_marker(line);
    let mut out: Vec<Span> = Vec::new();
    if !marker.is_empty() {
        out.push(Span::styled(
            marker.to_string(),
            Style::default().fg(Color::Indexed(108)),
        ));
        out.extend(render_inline(body));
    } else {
        out.extend(render_inline(line));
    }
    out
}

/// If the line starts with `  - ` / `- ` / `* ` / `+ ` or `<digits>.` +
/// space, return (marker_prefix, rest). Otherwise ("", line).
fn split_list_marker(s: &str) -> (&str, &str) {
    let after_indent = s.trim_start_matches(|c: char| c == ' ' || c == '\t');
    let indent_len = s.len() - after_indent.len();

    // Bullet: -, *, + followed by a space
    for bullet in ["- ", "* ", "+ "] {
        if after_indent.starts_with(bullet) {
            let end = indent_len + bullet.len();
            return (&s[..end], &s[end..]);
        }
    }
    // Numbered: <digits>. or <digits>) then space
    let mut chars = after_indent.chars();
    let mut n = 0usize;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            n += 1;
        } else if (c == '.' || c == ')') && n > 0 {
            // must be followed by a space
            if let Some(next) = chars.next() {
                if next == ' ' {
                    let end = indent_len + n + 2; // digits + '. ' or ') '
                    return (&s[..end], &s[end..]);
                }
            }
            break;
        } else {
            break;
        }
    }
    ("", s)
}

/// Inline markdown: **bold**, *italic*, `code`, [label](url).
/// Non-recursive — bold spans can't contain italics etc. Good enough
/// for typical assistant output.
fn render_inline(s: &str) -> Vec<Span<'_>> {
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        // Try inline code `code`
        if bytes[i] == b'`' {
            if let Some(end_rel) = s[i + 1..].find('`') {
                let end = i + 1 + end_rel;
                out.push(Span::styled(
                    s[i + 1..end].to_string(),
                    Style::default()
                        .bg(Color::Indexed(236))
                        .fg(Color::Indexed(228)),
                ));
                i = end + 1;
                continue;
            }
        }
        // Bold **text**
        if i + 1 < bytes.len() && &bytes[i..i + 2] == b"**" {
            if let Some(end_rel) = s[i + 2..].find("**") {
                let end = i + 2 + end_rel;
                out.push(Span::styled(
                    s[i + 2..end].to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
        }
        // Italic *text* or _text_ (single delim, one char each side)
        if bytes[i] == b'*' || bytes[i] == b'_' {
            let delim = bytes[i] as char;
            // Not part of **
            let is_bold_start = bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'*';
            if !is_bold_start {
                if let Some(end_rel) = s[i + 1..].find(delim) {
                    let end = i + 1 + end_rel;
                    // Content should not be empty
                    if end > i + 1 {
                        out.push(Span::styled(
                            s[i + 1..end].to_string(),
                            Style::default().add_modifier(Modifier::ITALIC),
                        ));
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        // Link [label](url)
        if bytes[i] == b'[' {
            if let Some(close_rel) = s[i + 1..].find(']') {
                let close = i + 1 + close_rel;
                if close + 1 < bytes.len() && bytes[close + 1] == b'(' {
                    if let Some(rparen_rel) = s[close + 2..].find(')') {
                        let rparen = close + 2 + rparen_rel;
                        out.push(Span::styled(
                            s[i + 1..close].to_string(),
                            Style::default()
                                .fg(Color::Indexed(75))
                                .add_modifier(Modifier::UNDERLINED),
                        ));
                        out.push(Span::styled(
                            format!(" ({})", &s[close + 2..rparen]),
                            Style::default().fg(Color::DarkGray),
                        ));
                        i = rparen + 1;
                        continue;
                    }
                }
            }
        }
        // Plain text run — advance until next markdown char.
        let start = i;
        while i < bytes.len()
            && bytes[i] != b'`'
            && bytes[i] != b'*'
            && bytes[i] != b'_'
            && bytes[i] != b'['
        {
            i += 1;
        }
        if i > start {
            out.push(Span::raw(s[start..i].to_string()));
        } else {
            // No markdown match; consume one char to avoid infinite loop.
            let ch = s[i..].chars().next().unwrap();
            let end = i + ch.len_utf8();
            out.push(Span::raw(s[i..end].to_string()));
            i = end;
        }
    }
    if out.is_empty() {
        out.push(Span::raw(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concat_content(spans: &[Span]) -> String {
        spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn heading_h1() {
        let mut st = MdState::default();
        let s = render_line("# Hello", &mut st);
        assert_eq!(s.len(), 1);
        assert!(s[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(concat_content(&s).contains("Hello"));
    }

    /// A blockquote must not look like the thinking stream.
    ///
    /// The TUI renders thinking as `DarkGray + ITALIC`
    /// (`flush_thinking_buf`). Blockquotes used the same pair, so a
    /// reply quoting its tool output was indistinguishable from the
    /// model's reasoning. Asserting the *absence* of that exact
    /// combination is the point of the test — any future restyling
    /// that drifts back into it fails here.
    #[test]
    fn blockquote_is_not_styled_like_thinking() {
        let mut st = MdState::default();
        let s = render_line("> Hello, elon musk! — from my-plugin", &mut st);
        let body = s.last().expect("a body span");
        assert!(
            !body.style.add_modifier.contains(Modifier::ITALIC),
            "quoted text is italic again — that is the thinking style"
        );
        assert_ne!(
            body.style.fg,
            Some(Color::DarkGray),
            "quoted text is DarkGray again — that is the thinking color"
        );
        // The gutter replaces the raw `>`; the text itself survives.
        let rendered = concat_content(&s);
        assert!(rendered.starts_with('▏'), "no gutter: {rendered:?}");
        assert!(rendered.contains("Hello, elon musk!"), "{rendered:?}");
        assert!(!rendered.contains('>'), "raw marker left in: {rendered:?}");
    }

    /// A bare `>` is a blank quote line in markdown, and models emit
    /// it between quoted paragraphs. It must keep the gutter rather
    /// than falling through to inline parsing as literal text.
    #[test]
    fn a_bare_gt_is_still_a_quote_line() {
        let mut st = MdState::default();
        let s = render_line(">", &mut st);
        assert_eq!(concat_content(&s).trim_end(), "▏");
    }

    #[test]
    fn inline_bold_and_code() {
        let mut st = MdState::default();
        let s = render_line("run **npm test** to see `errors`", &mut st);
        // bold + code + others
        let has_bold = s
            .iter()
            .any(|sp| sp.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold);
        assert!(concat_content(&s).contains("npm test"));
        assert!(concat_content(&s).contains("errors"));
    }

    #[test]
    fn italic_asterisk() {
        let mut st = MdState::default();
        let s = render_line("this is *very* important", &mut st);
        assert!(s
            .iter()
            .any(|sp| sp.style.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn list_marker_dashes() {
        let mut st = MdState::default();
        let s = render_line("- first item", &mut st);
        // The `- ` marker should be its own span, dim
        assert!(s.len() >= 2);
        assert!(concat_content(&s).contains("first item"));
    }

    #[test]
    fn numbered_list() {
        let mut st = MdState::default();
        let s = render_line("1. hi", &mut st);
        assert!(s.len() >= 2);
        assert!(concat_content(&s).starts_with("1. "));
    }

    #[test]
    fn fenced_code_toggles() {
        let mut st = MdState::default();
        let _ = render_line("```rust", &mut st);
        assert!(st.in_code);
        let s = render_line("fn main() {}", &mut st);
        // Inside code, no markdown parsing — just one span with code bg
        assert_eq!(s.len(), 1);
        let _ = render_line("```", &mut st);
        assert!(!st.in_code);
    }

    #[test]
    fn link_extracts_label_and_url() {
        let mut st = MdState::default();
        let s = render_line("see [docs](https://example.com) for more", &mut st);
        assert!(concat_content(&s).contains("docs"));
        assert!(concat_content(&s).contains("https://example.com"));
    }

    #[test]
    fn plain_text_passthrough() {
        let mut st = MdState::default();
        let s = render_line("just plain text", &mut st);
        assert_eq!(concat_content(&s), "just plain text");
    }
}

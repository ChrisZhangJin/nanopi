//! Render a session (header + entries) to a self-contained HTML page.
//!
//! Mirrors PI's HTML export (see
//! `packages/coding-agent/src/core/export-html/`) but trimmed to the
//! shape nanopi actually persists. Output is a single file — no
//! external CSS / JS — so users can email it, paste into a browser
//! tab, or archive it without dragging along assets.
//!
//! Styling is a dark-terminal look-alike; roles are color-banded to
//! match the TUI palette (sage-green assistant, dusty-rose error,
//! muted amber tools).

use crate::session::{SessionEntry, SessionHeader};

/// Build the complete HTML document for a session.
pub fn build(header: &SessionHeader, entries: &[SessionEntry]) -> String {
    let title = html_escape(header.name.as_deref().unwrap_or_else(|| "nanopi session"));
    let id = header.id.to_string();
    let short = &id[..std::cmp::min(id.len(), 8)];
    let model = html_escape(&header.model);
    let cwd = html_escape(header.cwd.to_string_lossy().as_ref());

    let mut body_html = String::with_capacity(entries.len() * 256);
    for e in entries {
        match e {
            SessionEntry::Header { .. } => {}
            SessionEntry::Message {
                role,
                content,
                timestamp,
                ..
            } => {
                let (class, label) = match role.as_str() {
                    "user" => ("user", "user"),
                    "assistant" => ("assistant", "assistant"),
                    other => ("other", other),
                };
                body_html.push_str(&format_turn(
                    class,
                    label,
                    &html_escape(content),
                    Some(timestamp),
                ));
            }
            SessionEntry::ToolCall {
                tool_name,
                arguments,
                timestamp,
                ..
            } => {
                let preview = match tool_name.as_str() {
                    "bash" => arguments
                        .get("command")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| arguments.to_string()),
                    _ => arguments.to_string(),
                };
                body_html.push_str(&format_turn(
                    "tool-call",
                    &format!("[{}]", tool_name),
                    &html_escape(&preview),
                    Some(timestamp),
                ));
            }
            SessionEntry::ToolResult {
                content,
                is_error,
                timestamp,
                ..
            } => {
                let class = if *is_error {
                    "tool-error"
                } else {
                    "tool-result"
                };
                let label = if *is_error { "tool ✗" } else { "tool →" };
                body_html.push_str(&format_turn(
                    class,
                    label,
                    &html_escape(content),
                    Some(timestamp),
                ));
            }
            SessionEntry::ModelChange {
                from,
                to,
                timestamp,
            } => {
                body_html.push_str(&format_turn(
                    "meta",
                    "model change",
                    &html_escape(&format!("{from} → {to}")),
                    Some(timestamp),
                ));
            }
            SessionEntry::Compaction {
                summary,
                replaced_count,
                timestamp,
            } => {
                body_html.push_str(&format_turn(
                    "meta",
                    &format!("compaction ({replaced_count} msgs)"),
                    &html_escape(summary),
                    Some(timestamp),
                ));
            }
            SessionEntry::BranchSummary { summary, timestamp } => {
                body_html.push_str(&format_turn(
                    "meta",
                    "branch summary",
                    &html_escape(summary),
                    Some(timestamp),
                ));
            }
            SessionEntry::SkillInvocation {
                name,
                body,
                user_message,
                timestamp,
                ..
            } => {
                let mut text = format!("# {name}\n\n{body}");
                if let Some(u) = user_message {
                    text.push_str("\n\n---\n\n");
                    text.push_str(u);
                }
                body_html.push_str(&format_turn(
                    "meta",
                    &format!("[skill] {name}"),
                    &html_escape(&text),
                    Some(timestamp),
                ));
            }
        }
    }

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title} · {short}</title>
<style>
  html, body {{ margin: 0; padding: 0; }}
  body {{
    background: #1a1a1a;
    color: #d0d0d0;
    font: 14px/1.5 -apple-system, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
    max-width: 900px;
    margin: 0 auto;
    padding: 2em 1.5em;
  }}
  h1 {{ color: #87af87; margin: 0 0 .5em 0; font-size: 1.3em; }}
  .meta {{ color: #808080; font-size: .85em; margin-bottom: 2em; }}
  .meta div {{ margin: .15em 0; }}
  .turn {{
    margin: .75em 0;
    padding: .6em .85em;
    border-radius: 3px;
    background: #232323;
    border-left: 3px solid #555;
  }}
  .role {{
    color: #7f7f7f;
    font-size: .7em;
    text-transform: uppercase;
    letter-spacing: .05em;
    margin-bottom: .3em;
    display: flex;
    justify-content: space-between;
  }}
  .role .ts {{ font-weight: normal; opacity: .7; }}
  .body {{ white-space: pre-wrap; word-wrap: break-word; }}
  .user      {{ border-left-color: #6d8fb8; }}
  .assistant {{ border-left-color: #87af87; }}
  .tool-call {{ border-left-color: #af8f5f; color: #b8b8b8; font-size: .92em; }}
  .tool-result {{ border-left-color: #7f9f7f; color: #b8b8b8; font-size: .9em; }}
  .tool-error  {{ border-left-color: #af5f5f; color: #d0a0a0; font-size: .9em; }}
  .meta.turn   {{ border-left-color: #808080; color: #a0a0a0; font-size: .88em; font-style: italic; }}
  .other       {{ border-left-color: #808080; }}
</style>
</head>
<body>
<h1>{title}</h1>
<div class="meta">
  <div>id: {id}</div>
  <div>model: {model}</div>
  <div>cwd: {cwd}</div>
</div>
{body_html}</body>
</html>
"#
    )
}

fn format_turn(class: &str, role: &str, body_escaped: &str, ts: Option<&str>) -> String {
    let ts_span = ts
        .map(|t| format!("<span class=\"ts\">{}</span>", html_escape(t)))
        .unwrap_or_default();
    format!(
        "  <div class=\"turn {class}\">\n    <div class=\"role\"><span>{role}</span>{ts_span}</div>\n    <div class=\"body\">{body_escaped}</div>\n  </div>\n"
    )
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionHeader;

    fn hdr() -> SessionHeader {
        SessionHeader {
            id: crate::util::uuid::v7(),
            parent_id: None,
            cwd: std::path::PathBuf::from("/tmp/proj"),
            model: "claude-opus-4-7".into(),
            base_url: "https://x".into(),
            name: Some("test-session".into()),
        }
    }

    #[test]
    fn build_returns_well_formed_html() {
        let entries = vec![
            SessionEntry::Message {
                id: "1".into(),
                timestamp: "2026-08-08T10:00:00Z".into(),
                role: "user".into(),
                content: "hi".into(),
            },
            SessionEntry::Message {
                id: "2".into(),
                timestamp: "2026-08-08T10:00:05Z".into(),
                role: "assistant".into(),
                content: "hello there".into(),
            },
        ];
        let html = build(&hdr(), &entries);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>test-session"));
        assert!(html.contains("claude-opus-4-7"));
        assert!(html.contains(">hi<"));
        assert!(html.contains(">hello there<"));
        assert!(html.ends_with("</html>\n"));
    }

    #[test]
    fn build_escapes_html_content() {
        let entries = vec![SessionEntry::Message {
            id: "1".into(),
            timestamp: "".into(),
            role: "user".into(),
            content: "<script>alert('x')</script>".into(),
        }];
        let html = build(&hdr(), &entries);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
    }

    #[test]
    fn build_renders_all_entry_kinds() {
        let entries = vec![
            SessionEntry::Message {
                id: "1".into(),
                timestamp: "".into(),
                role: "user".into(),
                content: "u".into(),
            },
            SessionEntry::ToolCall {
                id: "2".into(),
                timestamp: "".into(),
                tool_name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
            SessionEntry::ToolResult {
                tool_call_id: "2".into(),
                timestamp: "".into(),
                content: "file.txt".into(),
                is_error: false,
                images: vec![],
            },
            SessionEntry::Compaction {
                timestamp: "".into(),
                summary: "we did stuff".into(),
                replaced_count: 3,
            },
            SessionEntry::BranchSummary {
                timestamp: "".into(),
                summary: "abandoned branch: tried X".into(),
            },
        ];
        let html = build(&hdr(), &entries);
        assert!(html.contains("[bash]"));
        assert!(html.contains("ls"));
        assert!(html.contains("tool →"));
        assert!(html.contains("file.txt"));
        assert!(html.contains("compaction"));
        assert!(html.contains("branch summary"));
    }
}

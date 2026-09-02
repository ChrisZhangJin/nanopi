//! Plain stdout renderer — collects text deltas and prints to stdout
//! with green color for assistant text. No TUI, no alt-screen.

use std::io::{self, Write};

use crate::event::AgentEvent;

/// How much of a tool call's arguments to echo on the `[tool_call: …]`
/// line. Enough for a full-ish shell command, short enough to stay one
/// line in a log.
const MAX_ARG_PREVIEW: usize = 160;

/// How much of a FAILED tool's output to echo. Only errors get this
/// treatment: a `[bash ✗ 46 bytes]` marker tells you something broke
/// but not what, which is useless in a captured log — the failure text
/// is the whole point. Successful output stays a byte count because the
/// model already acted on it.
const MAX_ERROR_ECHO: usize = 1200;

/// Hex chars of a `call_xxxxxxxx…` id kept in the log. Long enough to
/// pair a result with its call and to `grep` the full id out of the
/// session JSONL, short enough not to dominate the line — the raw ids
/// are 24 hex chars and appeared on two lines each.
const CALL_ID_HEX: usize = 8;

/// Abbreviate a tool-call id for display, preserving any `call_` prefix
/// so the shortened form is still recognizable (and greppable) as one.
fn short_call_id(id: &str) -> String {
    let (prefix, hex) = match id.strip_prefix("call_") {
        Some(rest) => ("call_", rest),
        None => ("", id),
    };
    let head: String = hex.chars().take(CALL_ID_HEX).collect();
    format!("{prefix}{head}")
}

/// Strip the wrapper prefixes the tool layer stacks onto every failure
/// (`tool error: ` from the agent loop, `execution failed: ` from
/// `ToolError::Execution`). Both are pure boilerplate next to a `✗`,
/// and together they cost 30 columns before the real message starts.
/// The *informative* variants — `invalid arguments:`, `io error:`,
/// `tool not found:` — are deliberately left intact.
fn strip_error_boilerplate(content: &str) -> &str {
    let s = content
        .strip_prefix("tool error: ")
        .unwrap_or(content)
        .trim_start();
    s.strip_prefix("execution failed: ").unwrap_or(s)
}

/// One-line summary of a tool call's arguments — the bit you need when
/// a call fails: which command, which file, which pattern. Falls back
/// to compact JSON for tools without a well-known "subject" field.
fn arg_preview(tool_name: &str, args: &serde_json::Value) -> String {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str());
    let raw = match tool_name {
        "bash" => s("command").map(str::to_owned),
        "read" | "write" | "edit" | "ls" => s("path").map(str::to_owned),
        "grep" | "find" => s("pattern").map(|p| match s("path") {
            Some(base) => format!("{p}  in {base}"),
            None => p.to_owned(),
        }),
        _ => None,
    }
    .unwrap_or_else(|| args.to_string());
    collapse_ws_truncate(&raw, MAX_ARG_PREVIEW)
}

/// Flatten to one line and cap at `max_chars`, marking the cut with `…`
/// so a truncated preview is never mistaken for the whole value.
fn collapse_ws_truncate(s: &str, max_chars: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let head: String = one_line.chars().take(max_chars).collect();
    format!("{head}…")
}

/// Render a failed tool's output.
///
/// A single-line error rides on the marker line itself — the common case
/// (`No such file or directory`) shouldn't cost two lines. Anything
/// multi-line gets a `↳` gutter block instead, because compiler errors
/// and stack traces are unreadable flattened. Returns
/// `(inline_suffix, block)`; exactly one is ever non-empty.
fn error_body(content: &str) -> (String, String) {
    let body = strip_error_boilerplate(content).trim_end();
    let truncated: String = body.chars().take(MAX_ERROR_ECHO).collect();
    let dropped = body.len().saturating_sub(truncated.len());

    let mut lines = truncated.lines();
    let first = lines.next().unwrap_or_default();
    if dropped == 0 && lines.next().is_none() {
        return (format!(" {first}"), String::new());
    }

    let mut block = String::new();
    for line in truncated.lines() {
        block.push_str("  ↳ ");
        block.push_str(line);
        block.push('\n');
    }
    if dropped > 0 {
        block.push_str(&format!("  ↳ …({dropped} more bytes truncated)\n"));
    }
    (String::new(), block)
}

pub struct StdoutRenderer {
    buffer: String,
    /// True when the last thing written was a thinking chunk.
    ///
    /// Reasoning arrives as deltas with no trailing newline, so the
    /// first token of the actual answer landed on the same line:
    /// `That's a simple greeting.Hello, Tom! — from my-plugin`. The
    /// two are different kinds of text — one is the model musing, one
    /// is its reply — and in `-p` mode dim styling is the only thing
    /// separating them, which is nothing at all once the output is
    /// piped or the terminal drops SGR.
    after_thinking: bool,
}

impl StdoutRenderer {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            after_thinking: false,
        }
    }

    /// Render one AgentEvent to stdout.
    pub fn render(&mut self, event: &AgentEvent) -> io::Result<()> {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        match event {
            AgentEvent::Start { .. } => {
                // Begin green text.
                write!(out, "\x1b[1;32m")?;
            }
            AgentEvent::TextDelta { text, .. } => {
                // Break the line the reasoning left open. Only on the
                // first chunk after thinking — inside the answer,
                // deltas must concatenate exactly as they arrive, or
                // every token boundary becomes a line break.
                if std::mem::take(&mut self.after_thinking) {
                    writeln!(out)?;
                }
                self.buffer.push_str(text);
                write!(out, "{}", text)?;
                out.flush()?;
            }
            AgentEvent::ThinkingDelta { text, .. } => {
                // Subtle gray, dim.
                write!(out, "\x1b[2m{}\x1b[0m", text)?;
                self.after_thinking = true;
                out.flush()?;
            }
            AgentEvent::ToolCall { call, .. } => {
                // The arg preview is what makes a later failure legible:
                // `[tool_call: bash call_92c4…]` alone never said WHICH
                // command was about to run.
                // The marker's own leading `\n` already closes any
                // open thinking line, so drop the flag rather than
                // letting it add a blank one after the result.
                self.after_thinking = false;
                let preview = arg_preview(&call.name, &call.arguments);
                write!(
                    out,
                    "\x1b[33m\n[{} {}]{}\x1b[0m\n",
                    call.name,
                    short_call_id(&call.id),
                    if preview.is_empty() {
                        String::new()
                    } else {
                        format!(" {preview}")
                    }
                )?;
                out.flush()?;
            }
            AgentEvent::ToolResult {
                call_id,
                tool_name,
                content,
                is_error,
                elapsed_ms,
            } => {
                // `[bash → call_92c4bda9  794 bytes  32ms]` on success,
                // `[bash ✗ call_92c4bda9  32ms] <why>` on failure —
                // colored by outcome. The id pairs the result with its
                // call: the model batches calls, so marker order alone
                // doesn't say which one this answers.
                let (sep, color) = if *is_error {
                    ("✗", "\x1b[31m")
                } else {
                    ("→", "\x1b[2m")
                };
                let took = if *elapsed_ms < 1000 {
                    format!("{}ms", elapsed_ms)
                } else {
                    format!("{:.1}s", *elapsed_ms as f64 / 1000.0)
                };
                // A byte count is meaningful for output the model
                // consumed and we don't show; on a failure the message
                // itself follows, so the count is just noise.
                if *is_error {
                    let (inline, block) = error_body(content);
                    write!(
                        out,
                        "{color}[{} {} {}  {}]{}\x1b[0m\n{}",
                        tool_name,
                        sep,
                        short_call_id(call_id),
                        took,
                        inline,
                        block,
                    )?;
                } else {
                    write!(
                        out,
                        "{color}[{} {} {}  {} bytes  {}]\x1b[0m\n",
                        tool_name,
                        sep,
                        short_call_id(call_id),
                        content.len(),
                        took
                    )?;
                }
                out.flush()?;
            }
            AgentEvent::Done { .. } => {
                // Trailing newline ensures the next terminal write (e.g. a
                // `> ` prompt) starts on a fresh row instead of
                // overwriting the last chunk of assistant text.
                write!(out, "\x1b[0m\n")?;
                out.flush()?;
            }
            AgentEvent::Error { error } => {
                write!(out, "\x1b[31m\n[error: {}]\x1b[0m\n", error)?;
                out.flush()?;
            }
            AgentEvent::CompactionStart { reason } => {
                write!(out, "\x1b[2m\n[compacting context ({})…]\x1b[0m\n", reason)?;
                out.flush()?;
            }
            AgentEvent::CompactionEnd {
                replaced_count,
                used_llm,
            } => {
                let via = if *used_llm { "summary" } else { "truncation" };
                write!(
                    out,
                    "\x1b[2m[compacted {} messages via {}]\x1b[0m\n",
                    replaced_count, via
                )?;
                out.flush()?;
            }
            AgentEvent::SkillInvocation { name, .. } => {
                write!(out, "\x1b[2m[skill: {}]\x1b[0m\n", name)?;
                out.flush()?;
            }
        }
        Ok(())
    }
}

impl Default for StdoutRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FinishReason, Usage};
    use serde_json::json;

    #[test]
    fn render_text_delta_does_not_panic() {
        let mut r = StdoutRenderer::new();
        r.render(&AgentEvent::TextDelta {
            content_index: 0,
            text: "hi".into(),
        })
        .unwrap();
        r.render(&AgentEvent::Done {
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        })
        .unwrap();
        assert_eq!(r.buffer, "hi");
    }

    /// Reasoning must not run into the answer.
    ///
    /// `-p` produced `That's a simple greeting.Hello, Tom! — from
    /// my-plugin`: thinking deltas carry no trailing newline, so the
    /// reply's first token continued the line. Dim SGR was the only
    /// thing distinguishing them, and that survives neither a pipe nor
    /// a terminal that ignores it.
    ///
    /// Asserts the state machine, not the bytes: `render` writes to
    /// the process's real stdout, which a unit test can't capture
    /// without threading a writer through the whole renderer.
    #[test]
    fn a_reply_after_thinking_starts_on_its_own_line() {
        let mut r = StdoutRenderer::new();
        r.render(&AgentEvent::ThinkingDelta {
            content_index: 0,
            text: "That's a simple greeting.".into(),
        })
        .unwrap();
        assert!(r.after_thinking, "thinking left the line open");

        r.render(&AgentEvent::TextDelta {
            content_index: 0,
            text: "Hello, Tom!".into(),
        })
        .unwrap();
        assert!(!r.after_thinking, "flag must clear on the first chunk");

        // Mid-answer deltas must concatenate untouched — a newline per
        // delta would break the reply at every token boundary.
        r.render(&AgentEvent::TextDelta {
            content_index: 0,
            text: " — from my-plugin".into(),
        })
        .unwrap();
        // The separator is display-only: `buffer` feeds `--output json`
        // and the saved session, neither of which should gain a
        // newline the model never emitted.
        assert_eq!(r.buffer, "Hello, Tom! — from my-plugin");
    }

    /// A tool marker already opens with `\n`, so the flag must not
    /// also insert one after the result — that would leave a blank
    /// line between the tool card and the reply.
    #[test]
    fn a_tool_call_consumes_the_pending_separator() {
        let mut r = StdoutRenderer::new();
        r.render(&AgentEvent::ThinkingDelta {
            content_index: 0,
            text: "I should call greet.".into(),
        })
        .unwrap();
        r.render(&AgentEvent::ToolCall {
            content_index: 0,
            call: crate::event::ToolCall {
                id: "call_1".into(),
                name: "greet".into(),
                arguments: json!({"name": "Tom"}),
            },
        })
        .unwrap();
        assert!(!r.after_thinking);
    }

    #[test]
    fn arg_preview_names_the_subject_of_each_tool() {
        assert_eq!(
            arg_preview("bash", &json!({"command": "cargo test --lib"})),
            "cargo test --lib"
        );
        assert_eq!(arg_preview("read", &json!({"path": "src/main.rs"})), "src/main.rs");
        assert_eq!(
            arg_preview("edit", &json!({"path": "a.rs", "oldText": "x", "newText": "y"})),
            "a.rs"
        );
        assert_eq!(
            arg_preview("grep", &json!({"pattern": "TODO", "path": "src"})),
            "TODO in src"
        );
        assert_eq!(arg_preview("find", &json!({"pattern": "\\.rs$"})), "\\.rs$");
        // Multi-line commands collapse to one log line.
        assert_eq!(
            arg_preview("bash", &json!({"command": "set -e\nmake  build\n"})),
            "set -e make build"
        );
        // Unknown tool → compact JSON rather than nothing.
        assert_eq!(arg_preview("mystery", &json!({"k": 1})), "{\"k\":1}");
        // Missing expected field → falls back instead of printing empty.
        assert_eq!(arg_preview("bash", &json!({})), "{}");
    }

    #[test]
    fn arg_preview_truncation_is_marked() {
        let long = "x".repeat(MAX_ARG_PREVIEW + 50);
        let p = arg_preview("bash", &json!({"command": long}));
        assert_eq!(p.chars().count(), MAX_ARG_PREVIEW + 1, "capped plus the ellipsis");
        assert!(p.ends_with('…'), "a cut preview must announce itself");
        // Exactly at the cap: no ellipsis.
        let exact = "y".repeat(MAX_ARG_PREVIEW);
        let p = arg_preview("bash", &json!({"command": exact.clone()}));
        assert_eq!(p, exact);
    }

    #[test]
    fn short_call_id_keeps_the_prefix_and_enough_hex_to_grep() {
        assert_eq!(
            short_call_id("call_4b4e9e407f8f4ad09074227d"),
            "call_4b4e9e40"
        );
        // Already short, or no `call_` prefix at all: pass through.
        assert_eq!(short_call_id("call_abc"), "call_abc");
        assert_eq!(short_call_id("toolu_0123456789"), "toolu_01");
        assert_eq!(short_call_id(""), "");
        // The abbreviation must be a genuine prefix of the original, or
        // grepping the session JSONL for it would find nothing.
        let full = "call_4b4e9e407f8f4ad09074227d";
        assert!(full.starts_with(&short_call_id(full)));
    }

    #[test]
    fn strip_error_boilerplate_drops_wrappers_but_keeps_real_categories() {
        assert_eq!(
            strip_error_boilerplate("tool error: execution failed: cannot read /x"),
            "cannot read /x"
        );
        assert_eq!(strip_error_boilerplate("execution failed: boom"), "boom");
        // Informative variants survive — they say something the ✗ doesn't.
        assert_eq!(
            strip_error_boilerplate("tool error: invalid arguments: missing `path`"),
            "invalid arguments: missing `path`"
        );
        assert_eq!(
            strip_error_boilerplate("tool error: io error: broken pipe"),
            "io error: broken pipe"
        );
        assert_eq!(
            strip_error_boilerplate("tool error: tool not found: frobnicate"),
            "tool not found: frobnicate"
        );
        // Unwrapped content is untouched.
        assert_eq!(strip_error_boilerplate("plain"), "plain");
    }

    #[test]
    fn error_body_inlines_one_liners_and_blocks_the_rest() {
        // Single line → rides on the marker line, no gutter.
        let (inline, block) = error_body("tool error: execution failed: no such file");
        assert_eq!(inline, " no such file");
        assert!(block.is_empty());

        // Trailing newline still counts as one line.
        let (inline, block) = error_body("only line\n");
        assert_eq!(inline, " only line");
        assert!(block.is_empty());

        // Multi-line → gutter block, marker line stays clean.
        let (inline, block) = error_body("ls: cannot access 'a'\nls: cannot access 'b'");
        assert!(inline.is_empty());
        assert_eq!(block, "  ↳ ls: cannot access 'a'\n  ↳ ls: cannot access 'b'\n");

        // Oversized bodies report exactly how much was dropped, and are
        // always blocked (never inlined) so the notice has somewhere to go.
        let huge = "z".repeat(MAX_ERROR_ECHO + 42);
        let (inline, block) = error_body(&huge);
        assert!(inline.is_empty());
        assert!(block.contains("…(42 more bytes truncated)"));
        assert!(!block.contains("truncated\n  ↳ z"), "notice goes last");
    }

    #[test]
    fn render_failed_tool_result_does_not_panic() {
        let mut r = StdoutRenderer::new();
        r.render(&AgentEvent::ToolResult {
            call_id: "call_1".into(),
            tool_name: "bash".into(),
            content: "bash: cargo: command not found".into(),
            is_error: true,
            elapsed_ms: 9,
        })
        .unwrap();
    }

    #[test]
    fn render_tool_call_does_not_panic() {
        let mut r = StdoutRenderer::new();
        r.render(&AgentEvent::ToolCall {
            content_index: 0,
            call: crate::event::ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            },
        })
        .unwrap();
    }
}

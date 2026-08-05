//! Plain stdout renderer — collects text deltas and prints to stdout
//! with green color for assistant text. No TUI, no alt-screen.

use std::io::{self, Write};

use crate::event::AgentEvent;

pub struct StdoutRenderer {
    buffer: String,
}

impl StdoutRenderer {
    pub fn new() -> Self {
        Self { buffer: String::new() }
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
                self.buffer.push_str(text);
                write!(out, "{}", text)?;
                out.flush()?;
            }
            AgentEvent::ThinkingDelta { text, .. } => {
                // Subtle gray, dim.
                write!(out, "\x1b[2m{}\x1b[0m", text)?;
                out.flush()?;
            }
            AgentEvent::ToolCall { call, .. } => {
                write!(out, "\x1b[33m\n[tool_call: {} {}]\x1b[0m\n", call.name, call.id)?;
                out.flush()?;
            }
            AgentEvent::Done { .. } => {
                write!(out, "\x1b[0m")?;
                out.flush()?;
            }
            AgentEvent::Error { error } => {
                write!(out, "\x1b[31m\n[error: {}]\x1b[0m\n", error)?;
                out.flush()?;
            }
        }
        Ok(())
    }

    /// Drain buffered text (the assistant's full message).
    pub fn finalize(&mut self) -> String {
        let s = std::mem::take(&mut self.buffer);
        s
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

    #[test]
    fn finalize_returns_buffer_and_clears() {
        let mut r = StdoutRenderer::new();
        let _ = r.render(&AgentEvent::TextDelta {
            content_index: 0,
            text: "abc".into(),
        });
        assert_eq!(r.finalize(), "abc");
        assert!(r.buffer.is_empty());
    }
}
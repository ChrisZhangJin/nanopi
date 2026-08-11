//! Tool fold panel: renders a single tool call as a single collapsible
//! line in the TUI. State machine: pending → running → done/error.
//!
//! For v0.6 we keep it simple: one ToolPanel = one tool call's worth
//! of output. The TUI loop maintains a `Vec<ToolPanel>` and renders
//! them in order. Expansion (Enter to show output) is a follow-up.

use std::io::{self, Write};

use crate::event::{AgentEvent, FinishReason, ToolCall};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelState {
    Pending,
    Running,
    Done { output_chars: usize },
    Errored { output_chars: usize },
}

#[derive(Debug, Clone)]
pub struct ToolPanel {
    pub call_id: String,
    pub tool_name: String,
    pub arguments_preview: String,
    state: PanelState,
    /// One-line summary shown when collapsed. Built from tool name + args.
    summary: String,
}

impl ToolPanel {
    /// Create a panel for a new tool call.
    pub fn new(call: &ToolCall) -> Self {
        let args_str = call.arguments.to_string();
        let preview = if args_str.len() > 40 {
            format!("{}…", &args_str[..40])
        } else {
            args_str.clone()
        };
        let summary = format!("{} {}", call.name, preview);
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments_preview: preview,
            state: PanelState::Pending,
            summary,
        }
    }

    /// Apply an event from the agent event stream. Only events for this
    /// panel's call_id are consumed; others are ignored.
    pub fn feed(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::Start { .. }
            | AgentEvent::TextDelta { .. }
            | AgentEvent::ThinkingDelta { .. } => {
                // Run began.
                if self.state == PanelState::Pending {
                    self.state = PanelState::Running;
                }
            }
            AgentEvent::ToolCall { call, .. } if call.id == self.call_id => {
                // Self-feed (idempotent).
            }
            AgentEvent::Done { finish_reason, .. } => {
                let chars = self.state.output_chars_ref().unwrap_or(0);
                self.state = match finish_reason {
                    FinishReason::Stop | FinishReason::Length | FinishReason::ToolCalls => {
                        PanelState::Done {
                            output_chars: chars,
                        }
                    }
                    FinishReason::Refusal | FinishReason::Unknown => PanelState::Errored {
                        output_chars: chars,
                    },
                };
            }
            _ => {}
        }
    }

    /// Render the one-line collapsed view to the writer.
    pub fn render<W: Write>(&self, out: &mut W) -> io::Result<()> {
        let tag = match &self.state {
            PanelState::Pending => "[…]",
            PanelState::Running => "[...]",
            PanelState::Done { .. } => "[done]",
            PanelState::Errored { .. } => "[err ]",
        };
        writeln!(out, "{} {}", tag, self.summary)?;
        Ok(())
    }
}

// Helper extension to peek at current output_chars.
trait PanelStateExt {
    fn output_chars_ref(&self) -> Option<usize>;
}
impl PanelStateExt for PanelState {
    fn output_chars_ref(&self) -> Option<usize> {
        match self {
            PanelState::Done { output_chars } | PanelState::Errored { output_chars } => {
                Some(*output_chars)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Usage;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn new_panel_starts_pending() {
        let p = ToolPanel::new(&call("bash", json!({"command": "ls"})));
        assert_eq!(p.state, PanelState::Pending);
        assert!(p.summary.contains("bash"));
        assert!(p.summary.contains("ls"));
    }

    #[test]
    fn running_then_done_produces_done_state() {
        let mut p = ToolPanel::new(&call("read", json!({"path": "/etc/hostname"})));
        p.feed(&AgentEvent::Start {
            message_id: "m".into(),
        });
        assert_eq!(p.state, PanelState::Running);
        p.feed(&AgentEvent::Done {
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        });
        assert!(matches!(p.state, PanelState::Done { .. }));
    }

    #[test]
    fn render_emits_one_line_with_state_tag() {
        let mut p = ToolPanel::new(&call("edit", json!({"path": "a.txt"})));
        p.feed(&AgentEvent::Start {
            message_id: "m".into(),
        });
        let mut buf: Vec<u8> = Vec::new();
        p.render(&mut buf).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.contains("[..."), "want running tag, got: {line:?}");
        assert!(line.contains("edit"));
    }

    #[test]
    fn error_state_shows_err_tag() {
        let mut p = ToolPanel::new(&call("bash", json!({"command": "false"})));
        p.feed(&AgentEvent::Start {
            message_id: "m".into(),
        });
        p.feed(&AgentEvent::Done {
            finish_reason: FinishReason::Refusal,
            usage: Usage::default(),
        });
        let mut buf: Vec<u8> = Vec::new();
        p.render(&mut buf).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.contains("[err"), "want err tag, got: {line:?}");
    }

    #[test]
    fn long_arguments_get_truncated() {
        let long_cmd = "x".repeat(100);
        let p = ToolPanel::new(&call("bash", json!({"command": long_cmd.clone()})));
        assert!(p.arguments_preview.ends_with('…'));
        assert!(p.arguments_preview.len() < long_cmd.len());
    }
}

//! TUI renderer — v0.5 stub. Full crossterm alt-screen TUI is v0.6+.

#![allow(dead_code)]

use crate::event::AgentEvent;

pub struct TuiRenderer;

impl TuiRenderer {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&mut self, _event: &AgentEvent) {
        // v0.5: not implemented. Use StdoutRenderer instead.
    }
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new()
    }
}

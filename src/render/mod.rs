//! Output renderers — turn AgentEvent streams into terminal output.
//!
//! v0.5 ships `StdoutRenderer` (ANSI-colored, no TUI) for both
//! interactive and `-p` modes. `TuiRenderer` (crossterm alt-screen)
//! is a v0.6+ concern.

pub mod alt_screen;
pub mod export_html;
pub mod markdown;
pub mod menu;
pub mod panel;
pub mod spinner;
pub mod status_line;
pub mod stdout;
pub mod text_buffer;
pub mod tui;

//! Output renderers — turn AgentEvent streams into terminal output.
//!
//! v0.5 ships `StdoutRenderer` (ANSI-colored, no TUI) for both
//! interactive and `-p` modes. `TuiRenderer` (crossterm alt-screen)
//! is a v0.6+ concern.

pub mod stdout;
pub mod tui;
//! Output renderers — turn AgentEvent streams into terminal output.
//!
//! `StdoutRenderer` (ANSI-colored, no TUI) serves `-p` / piped mode.
//! The interactive TUI renders itself in `mode::tui` against ratatui,
//! using the widgets here (`menu`, `panel`, `text_buffer`, `markdown`).

pub mod alt_screen;
pub mod export_html;
pub mod markdown;
pub mod menu;
pub mod panel;
pub mod spinner;
pub mod status_line;
pub mod stdout;
pub mod text_buffer;

//! Output renderers — turn AgentEvent streams into terminal output.
//!
//! `StdoutRenderer` (ANSI-colored, no TUI) serves `-p` / piped mode.
//! The interactive TUI renders itself in `mode::tui` against ratatui,
//! using the widgets here (`menu`, `panel`, `text_buffer`, `markdown`).

pub mod alt_screen;

/// Human-readable one-line preview of a `subagent` tool call, shared by
/// the `-p` arg preview (`stdout::arg_preview`) and the TUI panel
/// summary (`panel::ToolPanel`) so their wording cannot drift. Returns
/// `None` for arg shapes it doesn't recognize, letting the caller fall
/// back to its generic preview. Never panics on malformed model input.
pub(crate) fn subagent_preview(args: &serde_json::Value) -> Option<String> {
    // single: `<agent>: <first ~60 chars of task>`
    if let Some(task) = args.get("task").and_then(|v| v.as_str()) {
        let agent = args.get("agent").and_then(|v| v.as_str()).unwrap_or("agent");
        return Some(format!("{agent}: {}", subagent_one_line(task, 60)));
    }
    // parallel: `parallel xN: agentA, agentB, ...`
    if let Some(tasks) = args.get("tasks").and_then(|v| v.as_array()) {
        return Some(format!(
            "parallel x{}: {}",
            tasks.len(),
            subagent_agent_names(tasks).join(", ")
        ));
    }
    // chain: `chain: agentA -> agentB -> ...`
    if let Some(chain) = args.get("chain").and_then(|v| v.as_array()) {
        return Some(format!(
            "chain: {}",
            subagent_agent_names(chain).join(" -> ")
        ));
    }
    None
}

/// Collect each item's `agent` name, standing in `?` for a missing or
/// non-string one rather than dropping the entry — the count in
/// `parallel xN` must still line up with what's listed.
fn subagent_agent_names(items: &[serde_json::Value]) -> Vec<String> {
    items
        .iter()
        .map(|it| {
            it.get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

/// Flatten to one line and cap at `max` chars, marking a cut with `…`.
fn subagent_one_line(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        return one;
    }
    let head: String = one.chars().take(max).collect();
    format!("{head}…")
}

pub mod export_html;
pub mod markdown;
pub mod menu;
pub mod notice;
pub mod panel;
pub mod raw_tty;
pub mod spinner;
pub mod status_line;
pub mod stdout;
pub mod text_buffer;

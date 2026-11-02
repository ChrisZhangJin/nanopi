//! The text of `/mcp`: one pure function from plain data to lines.
//!
//! The TUI only styles and prints what [`report`] returns, so every state
//! the command can show is testable without a terminal, a manager that
//! connects, or a clock. Nothing here can block or touch the network: the
//! input is a snapshot ([`ServerStatus`] rows) the manager already holds.
//!
//! **What may appear.** Server names, the transport, the endpoint exactly
//! as [`manager::endpoint_of`](crate::mcp::manager::endpoint_of) masked
//! it, tool names and a reason. Never an `env` value, never a header
//! name or value, never what a `${VAR}` expands to: the rows do not carry
//! them, so no rendering mistake here can print one.
//!
//! **Tool names are listed the same way in every mode** (the first
//! [`TOOLS_SHOWN`], then "… N more"), because "which tool do I ask for"
//! is the question the list answers and it is the same in solo mode and
//! in orchestrator mode. Orchestrator mode adds one line saying who uses
//! them.

use std::path::Path;

use crate::mcp::manager::{ServerState, ServerStatus};

/// How many tool names a server's row lists before "… N more". `/tools`
/// has the complete inventory with descriptions.
pub const TOOLS_SHOWN: usize = 8;

/// One line of output, tagged so the TUI can colour it without parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// The title.
    Heading(String),
    /// A single sentence of context, dim.
    Note(String),
    /// Something wrong with the file as a whole; stands out.
    Warn(String),
    /// A server's row. `summary` is everything after the state badge.
    Row { name: String, state: ServerState, summary: String },
    /// Indented under a row: endpoint or tool names, dim.
    Detail(String),
    /// Indented under a row: why it is not working.
    Reason { state: ServerState, text: String },
    Blank,
}

impl Line {
    /// The line as plain text, for tests and non-terminal output.
    pub fn plain(&self) -> String {
        match self {
            Line::Heading(t) | Line::Note(t) | Line::Warn(t) => t.clone(),
            Line::Row { name, state, summary } => {
                format!("  {name} [{}] {summary}", state.label()).trim_end().to_string()
            }
            Line::Detail(t) => format!("      {t}"),
            Line::Reason { text, .. } => format!("      {text}"),
            Line::Blank => String::new(),
        }
    }
}

/// Everything `/mcp` needs, as plain data.
pub struct Input<'a> {
    pub rows: &'a [ServerStatus],
    /// File-level problems from `mcp.json` (malformed, unreadable, a
    /// repeated name), each already carrying its path.
    pub config_notes: &'a [String],
    /// Why this session did not read `mcp.json` at all, if it did not.
    pub disabled: Option<&'a str>,
    /// Where `mcp.json` goes, for the not-configured hint.
    pub config_path: Option<&'a Path>,
    /// Orchestrator mode is on: the main agent does not call these tools.
    pub orchestrator: bool,
    /// Terminal width in columns, or 0 for "do not wrap". Lines indented
    /// under a row are wrapped here, with the indent repeated: the
    /// transcript's own wrapping would drop it and make a long tool list
    /// or reason read as if it belonged to the next row.
    pub width: usize,
}

/// Render the report. Never empty.
pub fn report(input: &Input<'_>) -> Vec<Line> {
    let mut out = Vec::new();

    if let Some(reason) = input.disabled {
        out.push(Line::Heading("MCP is off in this session".to_string()));
        push_wrapped(&mut out, input.width, &format!("{reason}."), Line::Note);
        out.push(Line::Blank);
        return out;
    }

    if input.rows.is_empty() {
        if input.config_notes.is_empty() {
            out.push(Line::Heading("No MCP servers configured".to_string()));
            let target = input
                .config_path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<nanopi home>/mcp.json".to_string());
            push_wrapped(
                &mut out,
                input.width,
                &format!(
                    "Add an \"mcpServers\" object to {target} and restart nanopi \
                     (it is read once, at startup). See docs/mcp.md."
                ),
                Line::Note,
            );
        } else {
            out.push(Line::Heading("No MCP servers are active".to_string()));
            for note in input.config_notes {
                push_wrapped(&mut out, input.width, &format!("mcp.json: {}", one_line(note)), Line::Warn);
            }
        }
        out.push(Line::Blank);
        return out;
    }

    out.push(Line::Heading(format!(
        "MCP servers ({})",
        summary_counts(input.rows)
    )));
    if input.orchestrator {
        push_wrapped(
            &mut out,
            input.width,
            "Orchestrator mode: these tools are used by the agents you dispatch, \
             not by the main agent itself.",
            Line::Note,
        );
    }
    for note in input.config_notes {
        push_wrapped(&mut out, input.width, &format!("mcp.json: {}", one_line(note)), Line::Warn);
    }

    let width = input.rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0).min(24);
    for row in input.rows {
        out.push(Line::Row {
            name: format!("{:<width$}", row.name),
            state: row.state,
            summary: row_summary(row),
        });
        if !row.endpoint.is_empty() {
            out.push(Line::Detail(row.endpoint.clone()));
        }
        if let Some(text) = tools_line(row) {
            for piece in wrap(&text, input.width) {
                out.push(Line::Detail(piece));
            }
        }
        // A connected server carries a reason only when something is
        // wrong (its tool list could not be read); say so rather than
        // show a green row that is quietly empty.
        if let Some(reason) = &row.reason_full {
            let text = match row.state {
                ServerState::Connected => format!("note: {reason}"),
                _ => reason.clone(),
            };
            for piece in wrap(&text, input.width) {
                out.push(Line::Reason { state: row.state, text: piece });
            }
        }
    }
    out.push(Line::Blank);
    out
}

/// "4: 1 connected, 1 failed, 2 skipped" — only the states present.
fn summary_counts(rows: &[ServerStatus]) -> String {
    let count = |s: ServerState| rows.iter().filter(|r| r.state == s).count();
    let mut parts = Vec::new();
    for state in [
        ServerState::Connected,
        ServerState::Connecting,
        ServerState::Failed,
        ServerState::Skipped,
    ] {
        let n = count(state);
        if n > 0 {
            parts.push(format!("{n} {}", state.label()));
        }
    }
    format!("{}: {}", rows.len(), parts.join(", "))
}

/// What follows the state badge: transport and tool count.
fn row_summary(row: &ServerStatus) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = row.transport {
        parts.push(t.label().to_string());
    }
    if row.state == ServerState::Connected {
        parts.push(match row.tool_count {
            1 => "1 tool".to_string(),
            n => format!("{n} tools"),
        });
    }
    parts.join(" · ")
}

/// The tool-name line: the first [`TOOLS_SHOWN`] registered names, then
/// "… N more". Only for a server that has tools.
fn tools_line(row: &ServerStatus) -> Option<String> {
    if row.tool_names.is_empty() {
        return None;
    }
    let shown = row.tool_names.iter().take(TOOLS_SHOWN).cloned().collect::<Vec<_>>();
    let more = row.tool_names.len().saturating_sub(shown.len());
    let mut line = shown.join(", ");
    if more > 0 {
        line.push_str(&format!(", … {more} more"));
    }
    Some(line)
}

/// Columns an indented line starts at (`Line::plain`'s six spaces).
const INDENT: usize = 6;

/// A sentence under the heading: indented two columns, and four on
/// continuation lines so a wrapped note does not read as a new row.
fn push_wrapped(out: &mut Vec<Line>, width: usize, text: &str, make: fn(String) -> Line) {
    for (i, piece) in wrap_at(text, width, 4).into_iter().enumerate() {
        out.push(make(format!("{}{piece}", if i == 0 { "  " } else { "    " })));
    }
}

/// [`wrap_at`] for lines indented under a row.
fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_at(text, width, INDENT)
}

/// Break `text` at spaces so each piece fits beside `indent` columns of
/// indent in a terminal `width` columns wide. A word longer than the
/// room keeps a line to itself (the terminal wraps it) rather than being
/// cut: a tool name or a path must stay copyable. `width == 0` means no
/// wrapping.
fn wrap_at(text: &str, width: usize, indent: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    // Leave the last column free: a line that exactly fills the row
    // makes some terminals wrap early.
    let room = width.saturating_sub(indent + 1).max(20);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let need = if cur.is_empty() { 0 } else { cur.chars().count() + 1 };
        if !cur.is_empty() && need + word.chars().count() > room {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// A note is one line even if a diagnostic grew a newline, so the
/// transcript layout never depends on what an error message contained.
fn one_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::manager::TransportKind;
    use std::path::PathBuf;

    fn row(name: &str, state: ServerState) -> ServerStatus {
        ServerStatus {
            name: name.to_string(),
            endpoint: String::new(),
            transport: Some(TransportKind::Stdio),
            state,
            connected: state == ServerState::Connected,
            tool_count: 0,
            tool_names: Vec::new(),
            reason: None,
            reason_full: None,
        }
    }

    fn connected(name: &str, tools: &[&str]) -> ServerStatus {
        ServerStatus {
            endpoint: "npx -y some-server".to_string(),
            tool_count: tools.len(),
            tool_names: tools.iter().map(|t| format!("mcp__{name}__{t}")).collect(),
            ..row(name, ServerState::Connected)
        }
    }

    fn render(rows: &[ServerStatus], orchestrator: bool) -> String {
        render_with(rows, &[], orchestrator)
    }

    fn render_with(rows: &[ServerStatus], notes: &[String], orchestrator: bool) -> String {
        let path = PathBuf::from("/home/u/.nanopi/mcp.json");
        let lines = report(&Input {
            rows,
            config_notes: notes,
            disabled: None,
            config_path: Some(&path),
            orchestrator,
            width: 0,
        });
        lines.iter().map(Line::plain).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn a_connected_server_shows_transport_endpoint_count_and_tool_names() {
        let mut r = connected("fs", &["read", "write"]);
        r.transport = Some(TransportKind::Http);
        r.endpoint = "http://127.0.0.1:8931/mcp".to_string();
        let out = render(&[r], false);
        assert!(out.contains("MCP servers (1: 1 connected)"), "{out}");
        assert!(out.contains("[connected] http · 2 tools"), "{out}");
        assert!(out.contains("http://127.0.0.1:8931/mcp"), "{out}");
        assert!(out.contains("mcp__fs__read, mcp__fs__write"), "{out}");
        assert!(!out.contains("more"), "{out}");
    }

    #[test]
    fn one_tool_is_singular_and_none_lists_no_tools_line() {
        let one = render(&[connected("a", &["x"])], false);
        assert!(one.contains("[connected] stdio · 1 tool\n"), "{one}");
        let none = render(&[connected("a", &[])], false);
        assert!(none.contains("0 tools"), "{none}");
        assert!(!none.contains("mcp__a__"), "{none}");
    }

    #[test]
    fn a_failed_server_shows_its_whole_first_line_reason() {
        let reason = "spawn \"nonexistent-cmd\": No such file or directory (os error 2) ".repeat(4);
        let reason = reason.trim().to_string();
        assert!(reason.chars().count() > 160, "must exceed the row cap to prove it is not cut");
        let mut r = row("broken", ServerState::Failed);
        r.endpoint = "nonexistent-cmd --flag".to_string();
        r.reason = Some("cut…".to_string());
        r.reason_full = Some(reason.clone());
        let out = render(&[r], false);
        assert!(out.contains("[failed] stdio"), "{out}");
        assert!(out.contains("nonexistent-cmd --flag"), "{out}");
        assert!(out.contains(&reason), "{out}");
        assert!(!out.contains("cut…"), "the capped form must not be what /mcp shows: {out}");
        assert!(!out.contains("tools"), "a failed server has no tool count: {out}");
    }

    #[test]
    fn a_skipped_server_shows_its_reason_and_no_transport() {
        let mut r = row("gateway", ServerState::Skipped);
        r.transport = None;
        r.reason_full = Some("server \"gateway\": header \"Authorization\" uses ${NOPE}, which is not set".into());
        let out = render(&[r], false);
        assert!(out.contains("MCP servers (1: 1 skipped)"), "{out}");
        assert!(out.contains("gateway [skipped]"), "{out}");
        assert!(out.contains("${NOPE}, which is not set"), "{out}");
        assert!(!out.contains("stdio") && !out.contains("http"), "{out}");
    }

    #[test]
    fn a_connecting_server_is_labelled_connecting() {
        let mut r = row("slow", ServerState::Connecting);
        r.reason_full = Some("still connecting".to_string());
        let out = render(&[r], false);
        assert!(out.contains("[connecting] stdio"), "{out}");
        assert!(out.contains("still connecting"), "{out}");
    }

    #[test]
    fn a_connected_server_whose_tool_list_failed_says_so() {
        let mut r = connected("half", &[]);
        r.reason_full = Some("connected, but listing tools failed: timeout".to_string());
        let out = render(&[r], false);
        assert!(out.contains("[connected]"), "{out}");
        assert!(out.contains("note: connected, but listing tools failed: timeout"), "{out}");
    }

    #[test]
    fn every_state_appears_once_in_config_order_with_counts() {
        let rows = [
            connected("a", &["t"]),
            row("b", ServerState::Failed),
            row("c", ServerState::Skipped),
            row("d", ServerState::Skipped),
        ];
        let out = render(&rows, false);
        assert!(out.contains("MCP servers (4: 1 connected, 1 failed, 2 skipped)"), "{out}");
        let at = |n: &str| out.find(&format!("  {n} [")).unwrap_or_else(|| panic!("{n}: {out}"));
        assert!(at("a") < at("b") && at("b") < at("c") && at("c") < at("d"), "{out}");
    }

    #[test]
    fn nothing_configured_points_at_the_file_and_is_not_an_error() {
        let out = render(&[], false);
        assert!(out.contains("No MCP servers configured"), "{out}");
        assert!(out.contains("/home/u/.nanopi/mcp.json"), "{out}");
        assert!(out.contains("mcpServers"), "{out}");
        assert!(!out.to_lowercase().contains("error"), "{out}");
        // Without a resolvable home the hint still says where it goes.
        let lines = report(&Input {
            rows: &[],
            config_notes: &[],
            disabled: None,
            config_path: None,
            orchestrator: false,
            width: 0,
        });
        assert!(lines.iter().map(Line::plain).collect::<String>().contains("mcp.json"));
    }

    #[test]
    fn a_malformed_file_shows_its_diagnostic_instead_of_the_hint() {
        let notes = vec![
            "invalid JSON at line 3 (expected `,` or `}`); leaving it untouched and starting \
             with no MCP servers (/home/u/.nanopi/mcp.json)"
                .to_string(),
        ];
        let out = render_with(&[], &notes, false);
        assert!(out.contains("No MCP servers are active"), "{out}");
        assert!(out.contains("invalid JSON at line 3"), "{out}");
        assert!(out.contains("/home/u/.nanopi/mcp.json"), "{out}");
        assert!(!out.contains("No MCP servers configured"), "{out}");
    }

    #[test]
    fn file_notes_sit_above_the_rows_when_servers_exist() {
        let notes = vec!["server \"a\" is defined more than once; using the last one (/x)".to_string()];
        let out = render_with(&[connected("a", &["t"])], &notes, false);
        assert!(out.find("defined more than once").unwrap() < out.find("a [connected]").unwrap(), "{out}");
    }

    #[test]
    fn a_session_that_never_read_the_file_says_why_not() {
        let lines = report(&Input {
            rows: &[],
            config_notes: &[],
            disabled: Some("--tools restricts the tool set, which excludes MCP tools"),
            config_path: None,
            orchestrator: false,
            width: 0,
        });
        let out = lines.iter().map(Line::plain).collect::<Vec<_>>().join("\n");
        assert!(out.contains("MCP is off in this session"), "{out}");
        assert!(out.contains("--tools restricts"), "{out}");
        assert!(!out.contains("configured"), "must not claim the file is empty: {out}");
    }

    #[test]
    fn tool_names_are_capped_with_a_count_of_the_rest() {
        let names: Vec<String> = (0..11).map(|i| format!("t{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let out = render(&[connected("big", &refs)], false);
        assert!(out.contains("11 tools"), "{out}");
        assert!(out.contains("mcp__big__t7"), "{out}");
        assert!(!out.contains("mcp__big__t8"), "{out}");
        assert!(out.contains("… 3 more"), "{out}");
        // Exactly at the cap: nothing is hidden.
        let eight: Vec<&str> = refs[..8].to_vec();
        let out = render(&[connected("exact", &eight)], false);
        assert!(out.contains("mcp__exact__t7") && !out.contains("more"), "{out}");
    }

    #[test]
    fn orchestrator_mode_says_who_uses_the_tools_and_still_lists_them() {
        let solo = render(&[connected("fs", &["read"])], false);
        let orch = render(&[connected("fs", &["read"])], true);
        assert!(!solo.contains("Orchestrator mode"), "{solo}");
        assert!(orch.contains("Orchestrator mode: these tools are used by the agents you dispatch"), "{orch}");
        assert!(orch.contains("not by the main agent itself"), "{orch}");
        assert!(orch.contains("mcp__fs__read"), "{orch}");
        // The note is one line, and is not shown when there is nothing to use.
        assert_eq!(orch.matches("Orchestrator mode").count(), 1, "{orch}");
        assert!(!render(&[], true).contains("Orchestrator mode"));
    }

    #[test]
    fn long_tool_lists_and_reasons_wrap_with_the_indent_repeated() {
        let names: Vec<String> = (0..8).map(|i| format!("tool_number_{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut r = connected("srv", &refs);
        r.state = ServerState::Failed;
        r.reason_full = Some("word ".repeat(40).trim().to_string());
        let lines = report(&Input {
            rows: &[r],
            config_notes: &[],
            disabled: None,
            config_path: None,
            orchestrator: false,
            width: 60,
        });
        let plain: Vec<String> = lines.iter().map(Line::plain).collect();
        assert!(plain.len() > 8, "{plain:#?}");
        for l in &plain {
            assert!(l.chars().count() <= 60 - 1, "{} cols: {l:?}", l.chars().count());
        }
        // Nothing is lost or reordered by wrapping.
        let tools: String = plain.iter().filter(|l| l.contains("mcp__srv__")).cloned().collect::<Vec<_>>().join(" ");
        let squashed = tools.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(squashed.starts_with("mcp__srv__tool_number_0, mcp__srv__tool_number_1,"), "{squashed}");
        assert!(squashed.ends_with("mcp__srv__tool_number_7"), "{squashed}");
        let reason_lines = plain.iter().filter(|l| l.trim().starts_with("word")).count();
        assert!(reason_lines >= 2, "the reason spans several lines: {plain:#?}");
        // A single word wider than the room is kept whole.
        assert_eq!(wrap(&"x".repeat(100), 40), vec!["x".repeat(100)]);
        assert_eq!(wrap("a b", 0), vec!["a b".to_string()]);
    }

    #[test]
    fn a_wrapped_note_keeps_a_continuation_indent() {
        let path = PathBuf::from("/home/u/.nanopi/mcp.json");
        let lines = report(&Input {
            rows: &[],
            config_notes: &[],
            disabled: None,
            config_path: Some(&path),
            orchestrator: false,
            width: 50,
        });
        let plain: Vec<String> = lines.iter().map(Line::plain).collect();
        let note: Vec<&String> = plain.iter().skip(1).filter(|l| !l.is_empty()).collect();
        assert!(note.len() >= 3, "{plain:#?}");
        assert!(note[0].starts_with("  Add") && note[1..].iter().all(|l| l.starts_with("    ")), "{plain:#?}");
        assert!(plain.iter().all(|l| l.chars().count() < 50), "{plain:#?}");
    }

    #[test]
    fn a_reason_with_newlines_cannot_add_lines() {
        let out = render_with(&[], &["first\nsecond (/p)".to_string()], false);
        assert!(out.contains("first") && !out.contains("second"), "{out}");
    }

    /// Credentials in the configuration must not reach the screen by any
    /// route: stdio `env`, an HTTP header value, a `${VAR}` expansion and
    /// a secret in the URL's query. Built the real way (a manager over
    /// real configs, through `status()`), not from hand-made rows.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_credential_from_the_config_reaches_the_output() {
        use crate::mcp::config::{self, ServerConfig, ServerSpec};
        use crate::mcp::manager::{testing::fake_server, ManagerOptions, McpManager};
        use std::collections::BTreeMap;

        const ENV_SECRET: &str = "ENVSECRET-5f3a91";
        const HEADER_SECRET: &str = "HDRSECRET-77c2b0";
        const EXPANDED_SECRET: &str = "EXPANDEDSECRET-a1e4d8";
        const QUERY_SECRET: &str = "QUERYSECRET-3b9e60";
        const SKIPPED_SECRET: &str = "SKIPSECRET-c04f12";
        const VAR: &str = "NANOPI_M7_TEST_TOKEN";
        // SAFETY: a name nothing else in the suite reads, set before any
        // thread of this test looks at it.
        unsafe { std::env::set_var(VAR, EXPANDED_SECRET) };

        // Connected stdio server, secret in env.
        let mut ok = fake_server("local", "", &["read", "write"]);
        if let ServerSpec::Stdio { env, .. } = &mut ok.spec {
            env.insert("API_KEY".to_string(), ENV_SECRET.to_string());
        }
        // Failed stdio server (cannot spawn), secret in env.
        let bad = ServerConfig::stdio(
            "nospawn",
            "/nonexistent/nanopi-m7-command",
            vec!["--flag".to_string()],
            BTreeMap::from([("TOKEN".to_string(), ENV_SECRET.to_string())]),
        );
        // Failed http server (nothing listens on port 1): a header value,
        // a header that expands `${VAR}`, and a secret in the query.
        let http = ServerConfig::http(
            "remote",
            format!("http://127.0.0.1:1/mcp?api_key={QUERY_SECRET}"),
            BTreeMap::from([
                ("X-Api-Key".to_string(), HEADER_SECRET.to_string()),
                ("Authorization".to_string(), format!("Bearer ${{{VAR}}}")),
            ]),
        );

        // Entries mcp.json rejects, with secrets in the headers beside
        // the fault.
        let dir = std::env::temp_dir().join(format!("nanopi-m7-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"mcpServers": {{
                    "unset": {{"url": "https://h.example/mcp",
                               "headers": {{"Authorization": "Bearer ${{NANOPI_M7_NOT_SET}}",
                                            "X-Other": "{SKIPPED_SECRET}"}}}},
                    "cleartext": {{"url": "http://example.com/mcp?k={SKIPPED_SECRET}",
                                   "headers": {{"Authorization": "{SKIPPED_SECRET}"}}}},
                    "legacy": {{"type": "sse", "url": "https://h.example/sse?k={SKIPPED_SECRET}"}}
                }}}}"#
            ),
        )
        .unwrap();
        let loaded = config::load(&path);
        assert_eq!(loaded.skipped.len(), 3, "{:?}", loaded.diagnostics);

        let manager = McpManager::connect(
            vec![ok, bad, http],
            ManagerOptions { startup_deadline: std::time::Duration::from_secs(10) },
        )
        .await
        .with_config_problems(loaded.skipped.clone(), loaded.file_notes());

        let rows = manager.status();
        assert_eq!(rows.len(), 6);
        for orchestrator in [false, true] {
            let lines = report(&Input {
                rows: &rows,
                config_notes: manager.config_notes(),
                disabled: None,
                config_path: Some(&path),
                orchestrator,
                width: 0,
            });
            let out = lines.iter().map(Line::plain).collect::<Vec<_>>().join("\n");
            // The test is only meaningful if every state was rendered.
            for needle in ["[connected]", "[failed]", "[skipped]", "local", "remote", "unset"] {
                assert!(out.contains(needle), "{needle} missing:\n{out}");
            }
            for secret in
                [ENV_SECRET, HEADER_SECRET, EXPANDED_SECRET, QUERY_SECRET, SKIPPED_SECRET]
            {
                assert!(!out.contains(secret), "{secret} leaked:\n{out}");
            }
        }
        manager.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

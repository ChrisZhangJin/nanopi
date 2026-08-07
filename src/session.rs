//! JSONL session read/write.
//!
//! Schema v2 (additive over v0.1). One entry per line.
//! Files live at `~/.nanopi/sessions/<uuid>.jsonl`.
//! Active-session-per-cwd is tracked in `~/.nanopi/sessions/active` (one line per cwd-encoded-id).
//!
//! Entry types:
//!   - session   (header, first line, version=2)
//!   - message   (user or assistant)
//!   - tool_call (LLM decided to call a tool)
//!   - tool_result (output of a tool call)
//!   - model_change (model switched mid-session)

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::util::{time, uuid};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("json parse error on {path} line {line}: {source}")]
    Parse {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("first line must be a session header")]
    NotASession,
    #[error("session version {found} not supported (need {expected})")]
    UnsupportedVersion { found: u32, expected: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEntry {
    #[serde(rename = "session")]
    Header {
        version: u32,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        timestamp: String,
        cwd: String,
        model: String,
        base_url: String,
    },
    #[serde(rename = "message")]
    Message {
        id: String,
        timestamp: String,
        role: String, // "user" | "assistant"
        content: String,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        timestamp: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_call_id: String,
        timestamp: String,
        content: String,
        is_error: bool,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        timestamp: String,
        from: String,
        to: String,
    },
    /// Written when the agent compacts context to save tokens. Records the
    /// generated summary and how many messages it replaced. Replay logic
    /// (Agent::load_session) treats the summary as a user message.
    #[serde(rename = "compaction")]
    Compaction {
        timestamp: String,
        summary: String,
        replaced_count: usize,
    },
    /// Written when the user forked to a new branch AND asked the agent
    /// to summarize the tail that got cut off. The summary carries
    /// what happened on the abandoned branch (files touched, decisions,
    /// open questions) so the new branch can pick up with context.
    /// Replayed as a synthetic user message on load, like Compaction.
    #[serde(rename = "branch_summary")]
    BranchSummary {
        timestamp: String,
        summary: String,
    },
}

/// Metadata extracted from the session header.
#[derive(Debug, Clone)]
pub struct SessionHeader {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub cwd: PathBuf,
    pub model: String,
    pub base_url: String,
}

/// Compute the directory where session JSONL files live.
///
/// Override via the `NANOPI_HOME` env var (used by tests and edge-case
/// deployments). Falls back to `$HOME/.nanopi/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NANOPI_HOME") {
        return Some(PathBuf::from(p).join("sessions"));
    }
    let home = dirs::home_dir()?;
    Some(home.join(".nanopi").join("sessions"))
}

/// Create a brand-new session file at `sessions_dir()/<id>.jsonl` and
/// return the path. Writes the header line.
pub fn new_session(cwd: &Path, model: &str, base_url: &str) -> Result<(PathBuf, SessionHeader), SessionError> {
    let dir = sessions_dir().ok_or_else(|| SessionError::Io {
        path: PathBuf::from("~/.nanopi/sessions"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir"),
    })?;
    std::fs::create_dir_all(&dir).map_err(|e| SessionError::Io {
        path: dir.clone(),
        source: e,
    })?;
    let id = uuid::v7();
    let path = dir.join(format!("{id}.jsonl"));
    let header = SessionHeader {
        id,
        parent_id: None,
        cwd: cwd.to_path_buf(),
        model: model.to_string(),
        base_url: base_url.to_string(),
    };
    let entry = SessionEntry::Header {
        version: 2,
        id: id.to_string(),
        parent_id: None,
        timestamp: time::now_iso8601(),
        cwd: cwd.display().to_string(),
        model: model.to_string(),
        base_url: base_url.to_string(),
    };
    append_entry(&path, &entry)?;
    Ok((path, header))
}

/// Append one entry to a session file. Each entry is one line.
pub fn append_entry(path: &Path, entry: &SessionEntry) -> Result<(), SessionError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SessionError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| SessionError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    let line = serde_json::to_string(entry).expect("serialize SessionEntry");
    writeln!(f, "{line}").map_err(|e| SessionError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    f.flush().map_err(|e| SessionError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Look up a session by id under the sessions dir. Returns the path
/// if a file matching `<id>.jsonl` exists. Used by the `--session` flag.
pub fn session_by_id(id: &str) -> Option<PathBuf> {
    let dir = sessions_dir()?;
    let candidate = dir.join(format!("{id}.jsonl"));
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Which session file to use for this invocation.
#[derive(Debug)]
pub enum SessionChoice {
    /// Brand-new session, create one.
    New,
    /// Existing session, resume from this file.
    Resume(PathBuf),
}

/// Resolve which session to use:
///   - `--fork <id>`: copy the session with that id into a new file (with
///     parent_id set to the source), then Resume the new file
///   - `--session <id>`: the session with that id (must exist)
///   - `--continue`: most recently used session for this cwd (if any)
///   - none: create a new session
///
/// Fork/session/continue are expected to be mutually exclusive at the CLI
/// layer; here we just prefer them in that order if multiple are set.
pub fn resolve_session(
    cwd: &Path,
    continue_flag: bool,
    session_id: Option<&str>,
    fork_id: Option<&str>,
) -> Result<SessionChoice, SessionError> {
    if let Some(id) = fork_id {
        let src = session_by_id(id).ok_or(SessionError::NotASession)?;
        let (new_path, _hdr) = fork_session(cwd, &src)?;
        return Ok(SessionChoice::Resume(new_path));
    }
    if let Some(id) = session_id {
        let p = session_by_id(id).ok_or(SessionError::NotASession)?;
        return Ok(SessionChoice::Resume(p));
    }
    if continue_flag {
        if let Some(p) = active_session(cwd) {
            return Ok(SessionChoice::Resume(p));
        }
        // No prior session; --continue degrades to a fresh start.
        return Ok(SessionChoice::New);
    }
    Ok(SessionChoice::New)
}

/// Fork a session: copy the header + all entries of `source` into a new
/// session file with a fresh id. The new header records `parent_id`
/// pointing back to the source, and its `cwd` is the caller's cwd
/// (which may differ from the source cwd — e.g. forking someone else's
/// session into your own project).
pub fn fork_session(cwd: &Path, source: &Path) -> Result<(PathBuf, SessionHeader), SessionError> {
    let (src_hdr, entries) = read_session(source)?;

    let dir = sessions_dir().ok_or_else(|| SessionError::Io {
        path: PathBuf::from("~/.nanopi/sessions"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir"),
    })?;
    std::fs::create_dir_all(&dir).map_err(|e| SessionError::Io {
        path: dir.clone(),
        source: e,
    })?;

    let new_id = uuid::v7();
    let path = dir.join(format!("{new_id}.jsonl"));
    let header = SessionHeader {
        id: new_id,
        parent_id: Some(src_hdr.id),
        cwd: cwd.to_path_buf(),
        model: src_hdr.model.clone(),
        base_url: src_hdr.base_url.clone(),
    };
    let header_entry = SessionEntry::Header {
        version: 2,
        id: new_id.to_string(),
        parent_id: Some(src_hdr.id.to_string()),
        timestamp: time::now_iso8601(),
        cwd: cwd.display().to_string(),
        model: src_hdr.model.clone(),
        base_url: src_hdr.base_url.clone(),
    };
    append_entry(&path, &header_entry)?;
    for e in entries {
        append_entry(&path, &e)?;
    }
    Ok((path, header))
}

/// Fork a session at a specific entry boundary. The new session
/// contains the header (with `parent_id` set to source) plus every
/// entry BEFORE `target_entry_index` in the source's entries Vec.
/// The target entry itself is NOT copied.
///
/// - If the target entry is a user Message, its text is returned as
///   `Some(text)` so the caller can pre-fill the editor. The caller
///   typically expects the user to edit + resubmit.
/// - If the target is an assistant / tool / anything else, `None` is
///   returned. The caller starts the new branch with a blank editor.
/// - `target_entry_index == 0` yields an empty branch (header only).
/// - Index beyond `entries.len()` copies everything (degenerate).
pub fn fork_session_at(
    cwd: &Path,
    source: &Path,
    target_entry_index: usize,
) -> Result<(PathBuf, SessionHeader, Option<String>), SessionError> {
    let (src_hdr, entries) = read_session(source)?;

    let copy_until = target_entry_index.min(entries.len());
    let prefill = entries.get(target_entry_index).and_then(|e| match e {
        SessionEntry::Message { role, content, .. } if role == "user" => {
            Some(content.clone())
        }
        _ => None,
    });

    let dir = sessions_dir().ok_or_else(|| SessionError::Io {
        path: PathBuf::from("~/.nanopi/sessions"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir"),
    })?;
    std::fs::create_dir_all(&dir).map_err(|e| SessionError::Io {
        path: dir.clone(),
        source: e,
    })?;

    let new_id = uuid::v7();
    let path = dir.join(format!("{new_id}.jsonl"));
    let header = SessionHeader {
        id: new_id,
        parent_id: Some(src_hdr.id),
        cwd: cwd.to_path_buf(),
        model: src_hdr.model.clone(),
        base_url: src_hdr.base_url.clone(),
    };
    let header_entry = SessionEntry::Header {
        version: 2,
        id: new_id.to_string(),
        parent_id: Some(src_hdr.id.to_string()),
        timestamp: time::now_iso8601(),
        cwd: cwd.display().to_string(),
        model: src_hdr.model.clone(),
        base_url: src_hdr.base_url.clone(),
    };
    append_entry(&path, &header_entry)?;
    for e in entries.iter().take(copy_until) {
        append_entry(&path, e)?;
    }
    Ok((path, header, prefill))
}

/// Extract user Message texts from a session, in file order. Used to
/// populate the fork picker. Returns (index-in-user-list, content).
pub fn user_messages(entries: &[SessionEntry]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut n = 0usize;
    for e in entries {
        if let SessionEntry::Message { role, content, .. } = e {
            if role == "user" {
                out.push((n, content.clone()));
                n += 1;
            }
        }
    }
    out
}

/// One displayable row in the fork tree picker. Matches PI's
/// default tree filter (see `tree-selector.ts:345-386`): show user,
/// assistant, tool calls, and compaction; hide bookkeeping (model
/// changes etc.).
///
/// `entry_index` is the position of this row inside the source
/// session's entries Vec — used by fork_session_at to slice a prefix.
/// `prefill_text` is Some(content) when the row is a user Message so
/// the editor can be pre-filled on fork; None otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub entry_index: usize,
    pub role: String,
    pub preview: String,
    pub prefill_text: Option<String>,
}

/// Extract displayable rows from a session's entries in file order.
/// Skips model_change / (future) session_info / label bookkeeping
/// entries. Tool results are folded into their preceding ToolCall's
/// row (no separate row) so the tree stays skimmable.
pub fn tree_items(entries: &[SessionEntry]) -> Vec<TreeRow> {
    let mut out = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        match e {
            SessionEntry::Message { role, content, .. } => {
                let preview = collapse_ws_truncate(content, 60);
                out.push(TreeRow {
                    entry_index: i,
                    role: role.clone(),
                    preview,
                    prefill_text: if role == "user" {
                        Some(content.clone())
                    } else {
                        None
                    },
                });
            }
            SessionEntry::ToolCall { tool_name, arguments, .. } => {
                // For bash render the command; otherwise a compact
                // JSON one-liner of the args. Users rarely care about
                // full arg blob at picker time.
                let preview = match tool_name.as_str() {
                    "bash" => arguments
                        .get("command")
                        .and_then(|v| v.as_str())
                        .map(|s| collapse_ws_truncate(s, 60))
                        .unwrap_or_default(),
                    _ => {
                        let raw = arguments.to_string();
                        collapse_ws_truncate(&raw, 60)
                    }
                };
                out.push(TreeRow {
                    entry_index: i,
                    role: format!("[{}]", tool_name),
                    preview,
                    prefill_text: None,
                });
            }
            SessionEntry::Compaction { summary, replaced_count, .. } => {
                out.push(TreeRow {
                    entry_index: i,
                    role: "[compaction]".into(),
                    preview: format!(
                        "{} msgs → {}",
                        replaced_count,
                        collapse_ws_truncate(summary, 50)
                    ),
                    prefill_text: None,
                });
            }
            SessionEntry::BranchSummary { summary, .. } => {
                out.push(TreeRow {
                    entry_index: i,
                    role: "[branch summary]".into(),
                    preview: collapse_ws_truncate(summary, 60),
                    prefill_text: None,
                });
            }
            // ToolResult is folded into its ToolCall row above; a
            // separate "[result]" row would double every tool.
            SessionEntry::ToolResult { .. } => {}
            // Header lives once at line 0 — not user-facing content.
            SessionEntry::Header { .. } => {}
            SessionEntry::ModelChange { .. } => {}
        }
    }
    out
}

fn collapse_ws_truncate(s: &str, max_chars: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(max_chars).collect()
}

/// Walk `parent_id` up from `current` toward the root. Returns
/// `[(path, header, tree_rows)]` with `current` at index 0 and the
/// root at the last index. Each session's tree_rows are its
/// full displayable entry list (user + assistant + tools + compact),
/// used by the fork picker to render a PI-style DFS tree.
pub fn walk_parent_chain(
    current: &Path,
) -> Vec<(PathBuf, SessionHeader, Vec<TreeRow>)> {
    let mut out = Vec::new();
    let mut cur = Some(current.to_path_buf());
    while let Some(p) = cur.take() {
        let (hdr, entries) = match read_session(&p) {
            Ok(t) => t,
            Err(_) => break,
        };
        let rows = tree_items(&entries);
        let parent_id_opt = hdr.parent_id.map(|id| id.to_string());
        out.push((p, hdr, rows));
        if let Some(pid) = parent_id_opt {
            cur = session_by_id(&pid);
        }
    }
    out
}

/// Longest common prefix length between two TreeRow lists — compares
/// role + preview. Used by the fork picker to skip rows a child
/// inherited from its parent so we don't show duplicates.
pub fn common_tree_row_prefix(a: &[TreeRow], b: &[TreeRow]) -> usize {
    let mut n = 0;
    while n < a.len() && n < b.len() && a[n].role == b[n].role && a[n].preview == b[n].preview {
        n += 1;
    }
    n
}

/// Iterate over entries in a session file. Returns header + body.
pub fn read_session(path: &Path) -> Result<(SessionHeader, Vec<SessionEntry>), SessionError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SessionError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let file = File::open(path).map_err(|e| SessionError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut header: Option<SessionHeader> = None;
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| SessionError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: SessionEntry = serde_json::from_str(&line).map_err(|e| SessionError::Parse {
            path: path.to_path_buf(),
            line: line_no + 1,
            source: e,
        })?;
        if line_no == 0 {
            // First non-empty line is the header. Validate and consume; do
            // NOT push into `entries` (caller wants header separately).
            match &entry {
                SessionEntry::Header { version, id, parent_id, cwd, model, base_url, .. } => {
                    if *version != 2 {
                        return Err(SessionError::UnsupportedVersion {
                            found: *version,
                            expected: 2,
                        });
                    }
                    header = Some(SessionHeader {
                        id: Uuid::parse_str(id).map_err(|_| SessionError::NotASession)?,
                        parent_id: parent_id
                            .as_ref()
                            .and_then(|s| Uuid::parse_str(s).ok()),
                        cwd: PathBuf::from(cwd),
                        model: model.clone(),
                        base_url: base_url.clone(),
                    });
                }
                _ => return Err(SessionError::NotASession),
            }
            continue;
        }
        entries.push(entry);
    }
    Ok((header.expect("header parsed"), entries))
}

/// Record the active session for a given cwd.
pub fn set_active_session(cwd: &Path, session_path: &Path) -> Result<(), SessionError> {
    let dir = sessions_dir().ok_or_else(|| SessionError::Io {
        path: PathBuf::from("~/.nanopi/sessions"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir"),
    })?;
    std::fs::create_dir_all(&dir).map_err(|e| SessionError::Io {
        path: dir.clone(),
        source: e,
    })?;
    let active_path = dir.join("active");
    // Read existing entries, replace matching cwd line, append if new.
    let cwd_key = cwd.to_string_lossy().to_string();
    let mut lines: Vec<String> = if active_path.exists() {
        std::fs::read_to_string(&active_path)
            .map_err(|e| SessionError::Io { path: active_path.clone(), source: e })?
            .lines()
            .filter(|l| !l.starts_with(&format!("{cwd_key}\t")))
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };
    lines.push(format!("{cwd_key}\t{}", session_path.display()));
    std::fs::write(&active_path, lines.join("\n") + "\n")
        .map_err(|e| SessionError::Io { path: active_path, source: e })?;
    Ok(())
}

/// Look up the active session path for a given cwd. Returns None if none.
pub fn active_session(cwd: &Path) -> Option<PathBuf> {
    let dir = sessions_dir()?;
    let active_path = dir.join("active");
    let text = std::fs::read_to_string(&active_path).ok()?;
    let cwd_key = cwd.to_string_lossy();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('\t') {
            if k == cwd_key {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-session-{}", uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn _unused_tmp() -> PathBuf {
        tmp()
    }

    #[test]
    fn roundtrip_all_entry_types() {
        let cwd = tmp();
        let (path, header) = new_session(&cwd, "test-model", "https://api.example/v1").unwrap();
        assert!(path.exists());

        append_entry(
            &path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "hi".into(),
            },
        )
        .unwrap();
        append_entry(
            &path,
            &SessionEntry::ToolCall {
                id: "call_1".into(),
                timestamp: time::now_iso8601(),
                tool_name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        )
        .unwrap();
        append_entry(
            &path,
            &SessionEntry::ToolResult {
                tool_call_id: "call_1".into(),
                timestamp: time::now_iso8601(),
                content: "file1\nfile2".into(),
                is_error: false,
            },
        )
        .unwrap();
        append_entry(
            &path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "assistant".into(),
                content: "ran ls".into(),
            },
        )
        .unwrap();
        append_entry(
            &path,
            &SessionEntry::ModelChange {
                timestamp: time::now_iso8601(),
                from: "test-model".into(),
                to: "test-model-v2".into(),
            },
        )
        .unwrap();

        let (read_header, entries) = read_session(&path).unwrap();
        assert_eq!(read_header.id, header.id);
        assert_eq!(read_header.cwd, cwd);
        assert_eq!(entries.len(), 5);

        // Each entry type roundtripped correctly.
        matches!(entries[0], SessionEntry::Message { .. });
        matches!(entries[1], SessionEntry::ToolCall { .. });
        matches!(entries[2], SessionEntry::ToolResult { .. });
        matches!(entries[3], SessionEntry::Message { .. });
        matches!(entries[4], SessionEntry::ModelChange { .. });

        // Cleanup
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn read_session_requires_header() {
        let cwd = tmp();
        let path = cwd.join("bad.jsonl");
        // Valid JSON, but not a header on line 1 — must yield NotASession.
        std::fs::write(
            &path,
            "{\"type\":\"message\",\"id\":\"x\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"role\":\"user\",\"content\":\"hi\"}\n",
        )
        .unwrap();
        let r = read_session(&path);
        assert!(matches!(r, Err(SessionError::NotASession)), "got {r:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn active_session_roundtrip() {
        let _guard = lock();
        // Use NANOPI_HOME to isolate from real ~/.nanopi.
        let tmp_home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &tmp_home);

        let cwd = tmp();
        let (path, _) = new_session(&cwd, "m", "https://api.example/v1").unwrap();
        set_active_session(&cwd, &path).unwrap();

        let got = active_session(&cwd);
        assert_eq!(got, Some(path));

        // Cleanup
        if let Some(h) = prev {
            std::env::set_var("NANOPI_HOME", h);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&tmp_home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn active_session_replaces_existing() {
        let _guard = lock();
        let tmp_home = tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &tmp_home);

        let cwd = tmp();
        let (p1, _) = new_session(&cwd, "m", "https://api.example/v1").unwrap();
        let (p2, _) = new_session(&cwd, "m", "https://api.example/v1").unwrap();
        set_active_session(&cwd, &p1).unwrap();
        set_active_session(&cwd, &p2).unwrap();

        let got = active_session(&cwd);
        assert_eq!(got, Some(p2));

        // Verify only one entry in active file.
        let active_text = std::fs::read_to_string(tmp_home.join("sessions/active")).unwrap();
        let count = active_text.lines().filter(|l| l.contains(&cwd.to_string_lossy().to_string())).count();
        assert_eq!(count, 1);

        if let Some(h) = prev {
            std::env::set_var("NANOPI_HOME", h);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&tmp_home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // ─────── v0.6+: resolve_session / session_by_id ───────

    // ─────── v0.6+: resolve_session / session_by_id ───────

    fn home_tmp() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-sess-resolve-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// No --continue / --session → New.
    #[test]
    fn resolve_session_default_is_new() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let choice = resolve_session(&cwd, false, None, None).expect("resolve");
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&cwd);
        matches!(choice, SessionChoice::New);
    }

    /// --continue with no prior session falls back to New (not error).
    #[test]
    fn resolve_session_continue_without_history_falls_back_to_new() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let choice = resolve_session(&cwd, true, None, None).expect("resolve");
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&cwd);
        matches!(choice, SessionChoice::New);
    }

    /// --continue with a recorded active session returns Resume that path.
    #[test]
    fn resolve_session_continue_returns_active_path() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let (path, _h) = new_session(&cwd, "m", "http://x").unwrap();
        set_active_session(&cwd, &path).unwrap();

        let choice = resolve_session(&cwd, true, None, None).expect("resolve");
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&cwd);
        match choice {
            SessionChoice::Resume(p) => assert_eq!(p, path),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    /// --session <id> resolves to that session file.
    #[test]
    fn resolve_session_by_id_returns_path() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let (path, header) = new_session(&cwd, "m", "http://x").unwrap();

        let choice = resolve_session(&cwd, false, Some(&header.id.to_string()), None).expect("resolve");
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&cwd);
        match choice {
            SessionChoice::Resume(p) => assert_eq!(p, path),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    /// fork_session copies body entries and sets parent_id on the new header.
    #[test]
    fn fork_session_copies_history() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        // Build a source session with two messages.
        let (src_path, src_hdr) = new_session(&cwd, "m", "http://x").unwrap();
        append_entry(
            &src_path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "hi".into(),
            },
        )
        .unwrap();
        append_entry(
            &src_path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "assistant".into(),
                content: "hello".into(),
            },
        )
        .unwrap();

        // Fork it.
        let (fork_path, fork_hdr) = fork_session(&cwd, &src_path).unwrap();
        assert_ne!(fork_hdr.id, src_hdr.id);
        assert_eq!(fork_hdr.parent_id, Some(src_hdr.id));
        // Body should be identical.
        let (_h, entries) = read_session(&fork_path).unwrap();
        assert_eq!(entries.len(), 2);
        matches!(&entries[0], SessionEntry::Message { .. });
        matches!(&entries[1], SessionEntry::Message { .. });

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// fork_session_at truncates the prefix at the Nth user message
    /// (0-based). Selected message text is returned alongside so the
    /// TUI can pre-fill the editor. parent_id links back to source.
    #[test]
    fn fork_session_at_truncates_prefix_and_returns_selected_text() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let (src_path, src_hdr) = new_session(&cwd, "m", "http://x").unwrap();
        // Build 3 user messages interleaved with assistant replies.
        for (role, text) in [
            ("user", "u1"),
            ("assistant", "a1"),
            ("user", "u2"),
            ("assistant", "a2"),
            ("user", "u3"),
        ] {
            append_entry(
                &src_path,
                &SessionEntry::Message {
                    id: uuid::v7().to_string(),
                    timestamp: time::now_iso8601(),
                    role: role.into(),
                    content: text.into(),
                },
            )
            .unwrap();
        }

        // Fork at ENTRY index 2 (which is "u2" — indices 0/1/2/3/4 =
        // u1/a1/u2/a2/u3). Prefix should include entries[0..2] = u1+a1
        // (2 entries). Selected text = u2 (a user Message → prefill).
        let (fork_path, fork_hdr, prefill) = fork_session_at(&cwd, &src_path, 2).unwrap();
        assert_eq!(prefill.as_deref(), Some("u2"));
        assert_eq!(fork_hdr.parent_id, Some(src_hdr.id));
        let (_h, entries) = read_session(&fork_path).unwrap();
        assert_eq!(entries.len(), 2, "expected u1 + a1 to be copied");

        // Fork at ENTRY index 1 (an assistant message "a1"). Prefix
        // is entries[0..1] = [u1]. Prefill = None (not a user Message).
        let (asst_path, _, prefill) = fork_session_at(&cwd, &src_path, 1).unwrap();
        assert_eq!(prefill, None, "assistant fork target ⇒ no prefill");
        let (_h, entries) = read_session(&asst_path).unwrap();
        assert_eq!(entries.len(), 1);

        // Fork at index 0 → no messages, prefill is u1 (target is user).
        let (empty_path, _, prefill) = fork_session_at(&cwd, &src_path, 0).unwrap();
        let (_h, entries) = read_session(&empty_path).unwrap();
        assert_eq!(entries.len(), 0);
        assert_eq!(prefill.as_deref(), Some("u1"));

        // Fork past the end → everything copies (degenerate but safe),
        // no prefill (index out of range).
        let (all_path, _, prefill) = fork_session_at(&cwd, &src_path, 99).unwrap();
        let (_h, entries) = read_session(&all_path).unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(prefill, None);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn walk_parent_chain_returns_current_to_root() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        // Root session with one user message.
        let (root_path, _root_hdr) = new_session(&cwd, "m", "http://x").unwrap();
        append_entry(
            &root_path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "root-1".into(),
            },
        )
        .unwrap();
        append_entry(
            &root_path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "root-2".into(),
            },
        )
        .unwrap();

        // Fork at message 1 (keeps only "root-1").
        let (child_path, _child_hdr, _text) =
            fork_session_at(&cwd, &root_path, 1).unwrap();
        // Add one more user message to the child.
        append_entry(
            &child_path,
            &SessionEntry::Message {
                id: uuid::v7().to_string(),
                timestamp: time::now_iso8601(),
                role: "user".into(),
                content: "child-1".into(),
            },
        )
        .unwrap();

        let chain = walk_parent_chain(&child_path);
        assert_eq!(chain.len(), 2, "current + root");
        assert_eq!(chain[0].0, child_path, "current at index 0");
        assert_eq!(chain[1].0, root_path, "root at last");
        // Child's tree rows: root-1 (inherited via fork prefix) + child-1
        let child_previews: Vec<&str> = chain[0].2.iter().map(|r| r.preview.as_str()).collect();
        assert_eq!(child_previews, vec!["root-1", "child-1"]);
        // Root's: root-1, root-2
        let root_previews: Vec<&str> = chain[1].2.iter().map(|r| r.preview.as_str()).collect();
        assert_eq!(root_previews, vec!["root-1", "root-2"]);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn common_tree_row_prefix_finds_divergence() {
        fn row(role: &str, preview: &str) -> TreeRow {
            TreeRow {
                entry_index: 0,
                role: role.into(),
                preview: preview.into(),
                prefill_text: None,
            }
        }
        let a = vec![row("user", "a"), row("assistant", "b"), row("user", "c")];
        let b_same = vec![row("user", "a"), row("assistant", "b"), row("user", "c")];
        assert_eq!(common_tree_row_prefix(&a, &b_same), 3);
        let b_extended = vec![row("user", "a"), row("assistant", "b"), row("user", "c"), row("user", "d")];
        assert_eq!(common_tree_row_prefix(&a, &b_extended), 3);
        let b_diverge = vec![row("user", "a"), row("assistant", "b'")];
        assert_eq!(common_tree_row_prefix(&a, &b_diverge), 1);
        let b_role_mismatch = vec![row("assistant", "a")];
        assert_eq!(common_tree_row_prefix(&a, &b_role_mismatch), 0);
    }

    #[test]
    fn tree_items_shows_all_display_types_with_role_labels() {
        let entries = vec![
            SessionEntry::Message {
                id: "1".into(), timestamp: "".into(),
                role: "user".into(), content: "hi   there  \n friend".into(),
            },
            SessionEntry::Message {
                id: "2".into(), timestamp: "".into(),
                role: "assistant".into(), content: "hello".into(),
            },
            SessionEntry::ToolCall {
                id: "3".into(), timestamp: "".into(),
                tool_name: "bash".into(),
                arguments: serde_json::json!({"command": "ls -la"}),
            },
            SessionEntry::ToolResult {
                tool_call_id: "3".into(), timestamp: "".into(),
                content: "output".into(), is_error: false,
            },
            SessionEntry::ModelChange {
                timestamp: "".into(), from: "a".into(), to: "b".into(),
            },
            SessionEntry::Compaction {
                timestamp: "".into(), summary: "we discussed X".into(), replaced_count: 12,
            },
        ];
        let rows = tree_items(&entries);
        // user, assistant, bash tool call, compaction — 4 rows.
        // ToolResult folded, ModelChange skipped.
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].role, "user");
        assert_eq!(rows[0].preview, "hi there friend"); // whitespace collapsed
        assert_eq!(rows[0].prefill_text.as_deref(), Some("hi   there  \n friend"));
        assert_eq!(rows[0].entry_index, 0);
        assert_eq!(rows[1].role, "assistant");
        assert!(rows[1].prefill_text.is_none());
        assert_eq!(rows[2].role, "[bash]");
        assert_eq!(rows[2].preview, "ls -la");
        assert_eq!(rows[2].entry_index, 2);
        assert_eq!(rows[3].role, "[compaction]");
        assert!(rows[3].preview.contains("12 msgs"));
        assert_eq!(rows[3].entry_index, 5);
    }

    #[test]
    fn user_messages_picks_only_user_role() {
        let entries = vec![
            SessionEntry::Message {
                id: "1".into(), timestamp: "".into(), role: "user".into(), content: "hi".into(),
            },
            SessionEntry::Message {
                id: "2".into(), timestamp: "".into(), role: "assistant".into(), content: "hello".into(),
            },
            SessionEntry::Message {
                id: "3".into(), timestamp: "".into(), role: "user".into(), content: "how are you".into(),
            },
        ];
        let out = user_messages(&entries);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], (0, "hi".into()));
        assert_eq!(out[1], (1, "how are you".into()));
    }

    /// resolve_session with --fork returns Resume(new_path) and the new file
    /// has parent_id pointing at the source.
    #[test]
    fn resolve_session_fork_creates_child() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let (_src_path, src_hdr) = new_session(&cwd, "m", "http://x").unwrap();
        let choice = resolve_session(&cwd, false, None, Some(&src_hdr.id.to_string()))
            .expect("resolve fork");
        let new_path = match choice {
            SessionChoice::Resume(p) => p,
            other => {
                if let Some(p) = prev {
                    std::env::set_var("NANOPI_HOME", p);
                } else {
                    std::env::remove_var("NANOPI_HOME");
                }
                panic!("expected Resume, got {other:?}");
            }
        };
        let (new_hdr, _) = read_session(&new_path).unwrap();
        assert_eq!(new_hdr.parent_id, Some(src_hdr.id));
        assert_ne!(new_hdr.id, src_hdr.id);

        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// --fork with a missing source id returns error.
    #[test]
    fn resolve_session_fork_missing_source_errors() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let r = resolve_session(&cwd, false, None, Some("does-not-exist"));
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
        assert!(r.is_err());
    }

    /// SessionEntry::Compaction serializes as `type = "compaction"` and
    /// round-trips its summary + replaced_count.
    #[test]
    fn compaction_entry_serde_roundtrips() {
        let entry = SessionEntry::Compaction {
            timestamp: "2026-08-06T00:00:00Z".into(),
            summary: "hello world".into(),
            replaced_count: 12,
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(s.contains("\"type\":\"compaction\""), "got {s}");
        assert!(s.contains("\"replaced_count\":12"), "got {s}");
        let back: SessionEntry = serde_json::from_str(&s).unwrap();
        matches!(back, SessionEntry::Compaction { .. });
    }

    /// BranchSummary is the fork-time counterpart of Compaction —
    /// serializes with type "branch_summary" and roundtrips its text.
    #[test]
    fn branch_summary_entry_serde_roundtrips() {
        let entry = SessionEntry::BranchSummary {
            timestamp: "2026-08-07T18:00:00Z".into(),
            summary: "abandoned line: user tried fix X, got Y".into(),
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(s.contains("\"type\":\"branch_summary\""), "got {s}");
        assert!(s.contains("abandoned line"), "got {s}");
        let back: SessionEntry = serde_json::from_str(&s).unwrap();
        matches!(back, SessionEntry::BranchSummary { .. });
    }

    /// tree_items renders BranchSummary as a "[branch summary]" row so
    /// users can see it in the fork picker.
    #[test]
    fn tree_items_renders_branch_summary() {
        let entries = vec![SessionEntry::BranchSummary {
            timestamp: "".into(),
            summary: "we deleted three files and reverted an edit".into(),
        }];
        let rows = tree_items(&entries);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, "[branch summary]");
        assert!(rows[0].preview.contains("deleted three files"));
        assert!(rows[0].prefill_text.is_none());
    }

    /// --session <id> with a missing id returns error (not New).
    #[test]
    fn resolve_session_by_id_missing_returns_error() {
        let _guard = lock();
        let home = home_tmp();
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", &home);
        let cwd = home_tmp();

        let r = resolve_session(&cwd, false, Some("does-not-exist"), None);
        if let Some(p) = prev {
            std::env::set_var("NANOPI_HOME", p);
        } else {
            std::env::remove_var("NANOPI_HOME");
        }
        let _ = std::fs::remove_dir_all(&cwd);
        assert!(r.is_err(), "expected error for missing id, got {r:?}");
    }
}

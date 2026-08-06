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

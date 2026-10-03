//! Per-agent file-state tracking + process-wide per-path write lock
//! (ISO-03, D-15).
//!
//! Each agent records the mtime and content hash of a file when it
//! reads it. An edit or write against a path with no record passes
//! (preserves today's main-agent behaviour for unread files); a path
//! WITH a record is refused if the current hash differs — the file
//! changed since this agent last read it, whether from another agent's
//! write or an external edit.
//!
//! `path_lock` extends `mutation_key`'s existing intra-batch
//! serialization to a process-wide lock keyed on canonical path, so two
//! agents writing the same file are serialized rather than racing.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use crate::tool::resolve_in_cwd;

/// Recorded mtime + content hash for one file, as observed at the last
/// `record()` call for that path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    pub mtime: Option<SystemTime>,
    pub hash: u64,
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Per-agent record of files read, used to refuse a stale write/edit
/// (ISO-03). `Default`-constructible so `ToolContext::new` can always
/// supply one without an `Option`.
#[derive(Debug, Default)]
pub struct FileStateTracker {
    records: Mutex<HashMap<PathBuf, FileFingerprint>>,
}

impl FileStateTracker {
    /// Record the fingerprint of `bytes` just read from `canonical`.
    pub fn record(&self, canonical: &Path, bytes: &[u8]) {
        let mtime = std::fs::metadata(canonical).and_then(|m| m.modified()).ok();
        let fp = FileFingerprint {
            mtime,
            hash: hash_bytes(bytes),
        };
        if let Ok(mut map) = self.records.lock() {
            map.insert(canonical.to_path_buf(), fp);
        }
    }

    /// Refuse a write/edit if a record exists for `canonical` and the
    /// file's current content hash no longer matches it. A path never
    /// recorded passes (unread files behave exactly as before this
    /// feature existed). A deleted file WITH a record is a refusal —
    /// the content plainly changed (it's gone).
    pub fn check(&self, canonical: &Path) -> Result<(), String> {
        let recorded = {
            let map = self
                .records
                .lock()
                .map_err(|_| "file state tracker lock poisoned".to_string())?;
            match map.get(canonical) {
                Some(fp) => *fp,
                None => return Ok(()),
            }
        };

        let current_hash = match std::fs::read(canonical) {
            Ok(bytes) => hash_bytes(&bytes),
            Err(_) => {
                // Deleted (or unreadable) since the record was made —
                // treat as changed, not as "nothing to compare".
                return Err("file changed since you read it — re-read first".to_string());
            }
        };

        if current_hash != recorded.hash {
            return Err("file changed since you read it — re-read first".to_string());
        }
        Ok(())
    }

    /// Update the record after this agent's own write/edit, so a
    /// subsequent check against its own change passes.
    pub fn update_after_write(&self, canonical: &Path, bytes: &[u8]) {
        self.record(canonical, bytes);
    }
}

/// Resolve a model-supplied path string to a canonical key, the same
/// way `mutation_key` does — reusing `resolve_in_cwd` rather than
/// duplicating its cwd-escape / symlink handling.
pub fn canonical_key(cwd: &Path, path_str: &str) -> Option<PathBuf> {
    let resolved = resolve_in_cwd(cwd, path_str).ok()?;
    match std::fs::canonicalize(&resolved) {
        Ok(real) => Some(real),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Some(resolved)
        }
        Err(_) => Some(resolved),
    }
}

type PathLockMap = Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>;

static PATH_LOCKS: OnceLock<PathLockMap> = OnceLock::new();

/// A process-wide lock for `canonical`, shared by every
/// `FileStateTracker` (and hence every agent) in the process. Extends
/// `mutation_key`'s intra-batch serialization to across-agent
/// serialization (D-15).
pub fn path_lock(canonical: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let map = PATH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match map.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    Arc::clone(
        guard
            .entry(canonical.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "nanopi-file-state-test-{}-{}",
                std::process::id(),
                crate::util::uuid::v7()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn record_then_check_unchanged_is_ok() {
        let dir = TempDir::new();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        let tracker = FileStateTracker::default();
        tracker.record(&file, b"hello");
        assert!(tracker.check(&file).is_ok());
    }

    #[test]
    fn check_after_another_writer_changes_bytes_is_err() {
        let dir = TempDir::new();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        let tracker = FileStateTracker::default();
        tracker.record(&file, b"hello");

        // Simulate another agent's write.
        std::fs::write(&file, "goodbye").unwrap();

        let err = tracker.check(&file).unwrap_err();
        assert!(err.contains("file changed since you read it — re-read first"));
    }

    #[test]
    fn check_on_never_recorded_path_is_ok() {
        let dir = TempDir::new();
        let file = dir.path().join("never-read.txt");
        std::fs::write(&file, "x").unwrap();

        let tracker = FileStateTracker::default();
        assert!(tracker.check(&file).is_ok());
    }

    #[test]
    fn update_after_own_write_makes_subsequent_check_ok() {
        let dir = TempDir::new();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        let tracker = FileStateTracker::default();
        tracker.record(&file, b"hello");

        std::fs::write(&file, "updated").unwrap();
        tracker.update_after_write(&file, b"updated");

        assert!(tracker.check(&file).is_ok());
    }

    #[test]
    fn two_trackers_are_independent() {
        let dir = TempDir::new();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        let tracker_a = FileStateTracker::default();
        let tracker_b = FileStateTracker::default();

        tracker_a.record(&file, b"hello");
        // tracker_b never read it, so its check still passes even
        // after a change, because it has no record to violate.
        std::fs::write(&file, "goodbye").unwrap();
        assert!(tracker_b.check(&file).is_ok());
        assert!(tracker_a.check(&file).is_err());
    }

    #[tokio::test]
    async fn path_lock_is_shared_across_trackers_for_same_path() {
        let dir = TempDir::new();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();

        let lock_a = path_lock(&canonical);
        let lock_b = path_lock(&canonical);
        assert!(Arc::ptr_eq(&lock_a, &lock_b));
    }
}

//! Per-process stale-write guard (ISO-03, D-13).
//!
//! Every nanopi process (main agent or `nanopi -p` child) remembers the
//! on-disk state of each file it read. Before `edit` / `write` mutate a
//! file, the stamp is re-checked; if another process changed the file
//! since, the mutation is refused and the model is told to re-read.
//! Files never read by this process are not tracked and may be written
//! freely, so the main agent behaves as before.
//!
//! The content hash is always recomputed: mtime has 1s granularity on
//! some filesystems, so a same-length rewrite within the same second
//! would slip past an mtime/len-only check.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: u64,
    hash: u64,
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn stamp_of(path: &Path) -> std::io::Result<FileStamp> {
    let meta = std::fs::metadata(path)?;
    let bytes = std::fs::read(path)?;
    Ok(FileStamp {
        mtime: meta.modified().ok(),
        len: bytes.len() as u64,
        hash: hash_bytes(&bytes),
    })
}

pub struct FileStateTracker {
    map: Mutex<HashMap<PathBuf, FileStamp>>,
}

impl Default for FileStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FileStateTracker {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Remember the current on-disk state of `path` (full file bytes).
    pub fn record(&self, path: &Path) -> std::io::Result<()> {
        let stamp = stamp_of(path)?;
        self.map.lock().unwrap().insert(key(path), stamp);
        Ok(())
    }

    /// Record using bytes the caller already read, avoiding a second read
    /// that could observe a different version than the one returned.
    pub fn record_bytes(&self, path: &Path, bytes: &[u8]) {
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        self.map.lock().unwrap().insert(
            key(path),
            FileStamp {
                mtime,
                len: bytes.len() as u64,
                hash: hash_bytes(bytes),
            },
        );
    }

    /// Ok if untracked or unchanged since the last record/update.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let k = key(path);
        let expected = match self.map.lock().unwrap().get(&k) {
            Some(s) => s.clone(),
            None => return Ok(()),
        };
        let changed = || {
            format!(
                "file changed since you read it — re-read first: {}",
                path.display()
            )
        };
        match stamp_of(path) {
            // mtime is informative only; len + content hash decide.
            Ok(now) if now.len == expected.len && now.hash == expected.hash => Ok(()),
            _ => Err(changed()),
        }
    }

    /// Refresh the stamp after this process's own write.
    pub fn update(&self, path: &Path) {
        let _ = self.record(path);
    }
}

/// The process-global tracker. A child agent is its own process, so this
/// is exactly the D-13 scope.
pub fn global() -> &'static FileStateTracker {
    static G: OnceLock<FileStateTracker> = OnceLock::new();
    G.get_or_init(FileStateTracker::new)
}

/// Write `bytes` to `path` atomically: temp file in the same directory,
/// fsync, copy permissions of the existing target, rename over it. The
/// temp file is removed on any error.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{name}.nanopi-{}-{nanos}.tmp", std::process::id()));

    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Shared safe-write path for `edit` and `write`: stale check, refuse a
/// symlink (rename would replace it rather than follow it, but a symlink
/// here means something swapped it in) and a multiply-linked target, then
/// write atomically and refresh the stamp.
pub fn guarded_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    global().check(path)?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to write {}: target is a symlink",
                path.display()
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() > 1 {
                return Err(format!(
                    "cannot write {}: refusing to write a file with multiple hard links: \
                     the same inode is reachable from outside the working directory",
                    path.display()
                ));
            }
        }
    }
    atomic_write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    global().update(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-fstate-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn unchanged_file_passes() {
        let d = tmp();
        let f = d.join("a.txt");
        std::fs::write(&f, "hello").unwrap();
        let t = FileStateTracker::new();
        t.record(&f).unwrap();
        assert!(t.check(&f).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn same_length_rewrite_is_detected() {
        let d = tmp();
        let f = d.join("a.txt");
        std::fs::write(&f, "hello").unwrap();
        let t = FileStateTracker::new();
        t.record(&f).unwrap();
        std::fs::write(&f, "jello").unwrap();
        let e = t.check(&f).unwrap_err();
        assert!(e.contains("file changed since you read it — re-read first"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn untracked_path_passes() {
        let d = tmp();
        let t = FileStateTracker::new();
        assert!(t.check(&d.join("never.txt")).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deleted_file_is_changed() {
        let d = tmp();
        let f = d.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let t = FileStateTracker::new();
        t.record(&f).unwrap();
        std::fs::remove_file(&f).unwrap();
        assert!(t.check(&f).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn update_after_own_write_passes() {
        let d = tmp();
        let f = d.join("a.txt");
        std::fs::write(&f, "one").unwrap();
        let t = FileStateTracker::new();
        t.record(&f).unwrap();
        std::fs::write(&f, "two two").unwrap();
        t.update(&f);
        assert!(t.check(&f).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn atomic_write_leaves_no_tmp() {
        let d = tmp();
        let f = d.join("a.txt");
        std::fs::write(&f, "old").unwrap();
        atomic_write(&f, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
        let left: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty());
        // Failure path: target dir does not exist -> error, nothing left.
        assert!(atomic_write(&d.join("nope/x.txt"), b"x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let f = d.join("s.sh");
        std::fs::write(&f, "a").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o750)).unwrap();
        atomic_write(&f, b"b").unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
        let _ = std::fs::remove_dir_all(&d);
    }
}

//! Host-side keyed persistence for WASM plugins (`host-store-get` /
//! `host-store-set`, gated on `allow_store`).
//!
//! **Keys, not paths — and that is why there is no containment check.**
//! `resolve_readable` in `loader.rs` has to defend against `../`,
//! symlinks pointing outward, FIFOs and character devices, because
//! `host-fs-read` lets the plugin *name a filesystem location*. Here it
//! does not: the plugin supplies a JSON map KEY, and the host alone
//! decides which file that lands in. The only filesystem name derived
//! from anything plugin-adjacent is the `.wasm` file stem, which comes
//! out of the user's own `[[extensions]]` config, not out of the guest.
//! So a key of `../../etc/passwd` is stored as an ordinary map key with
//! those characters in it, and creates nothing anywhere. Do not add a
//! path-containment check here looking for parity with `host-fs-read` —
//! there is no path to contain.
//!
//! **Durability, stated at what it actually delivers.** Each `set` is a
//! read-modify-write of the whole JSON object, committed by writing a
//! temp file in the same directory and `rename`ing it over
//! `store.json`. A same-directory rename is atomic, so a crash leaves
//! either the whole old file or the whole new one — never a torn one
//! (`docs/plugin-capabilities.md` §4). There is deliberately **no
//! `fsync`**: this survives a process crash, not a power cut. The
//! distinction is the point of `docs/claims-and-races.md` §1 — the WIT
//! doc for `host-store-set` promises "the bytes reached the
//! filesystem", which is what a rename gives, and does not promise they
//! reached the platter, which it does not.
//!
//! The store is host-side rather than in guest memory because a guest
//! trap resets the instance (`loader.rs`, `ComponentBridge::reset`), so
//! anything kept guest-side is one trap away from gone
//! (`plugin-capabilities.md` invariant 14).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Longest key accepted, in chars. INCLUSIVE — a 128-char key is fine,
/// 129 is refused (`plugin-capabilities.md` §2.1 says `key ≤ 128`).
pub const MAX_KEY_CHARS: usize = 128;
/// Most keys one plugin may hold.
pub const MAX_KEYS: usize = 1000;
/// Ceiling on the serialized `store.json`, measured on the object that
/// WOULD be written — so a replacement that shrinks the file is never
/// refused for the size it used to be.
pub const MAX_STORE_BYTES: usize = 1 << 20;

/// The exact quota message from `plugin-capabilities.md` §2.1. The
/// caller in `loader.rs` prefixes it with `error: `.
const QUOTA_MSG: &str = "store quota exceeded (1 MiB)";

/// One plugin's keyed store, rooted at `<root>/<stem>/store.json`.
///
/// `root` is a parameter rather than a hardcoded `~/.nanopi/extensions`
/// precisely so tests can point it at a temp directory; production
/// callers get the real one from [`PluginStore::default_root`].
pub struct PluginStore {
    dir: PathBuf,
}

impl PluginStore {
    pub fn new(root: PathBuf, stem: &str) -> Self {
        Self {
            dir: root.join(stem),
        }
    }

    /// `~/.nanopi/extensions`, the production root. Falls back to a
    /// relative path when `HOME` is unset, matching how
    /// `crate::wasm::expand_path` degrades rather than panicking.
    pub fn default_root() -> PathBuf {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::new()
                .join(home)
                .join(".nanopi")
                .join("extensions"),
            None => PathBuf::from(".nanopi").join("extensions"),
        }
    }

    /// Where the JSON object lives. Public so tests and the startup
    /// notice can name it.
    pub fn file(&self) -> PathBuf {
        self.dir.join("store.json")
    }

    /// This plugin's value for `key`, or `""`.
    ///
    /// Absent, unreadable, empty and malformed all read as `""`. A
    /// hand-edited or torn `store.json` must not brick the plugin, and
    /// `""` is unambiguous as "nothing stored" because `""` is not
    /// valid JSON — a plugin storing JSON can always tell the two
    /// apart (`plugin-capabilities.md` §2.1).
    pub fn get(&self, key: &str) -> String {
        self.read_map()
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace this plugin's value at `key`.
    ///
    /// `Ok(())` means the bytes are on the filesystem — there is no
    /// write-behind buffer to flush (invariant 8). The `Err` payload is
    /// the message body WITHOUT the `error: ` prefix; the caller adds
    /// it, matching `resolve_readable`.
    ///
    /// Every bound is checked BEFORE anything is written, and a refusal
    /// writes nothing at all: the plugin is told it failed and the
    /// store is exactly as it was.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        if key.chars().count() > MAX_KEY_CHARS {
            return Err(format!(
                "store key too long ({} chars, limit {MAX_KEY_CHARS})",
                key.chars().count()
            ));
        }
        let mut map = self.read_map();
        // A replacement is not a new key, so it does not count against
        // the key cap — otherwise a plugin at the cap could never
        // update what it already stored.
        if !map.contains_key(key) && map.len() >= MAX_KEYS {
            return Err(format!(
                "store key limit exceeded ({MAX_KEYS} keys)"
            ));
        }
        map.insert(key.to_string(), value.to_string());
        let serialized = serde_json::to_string(&map)
            .map_err(|e| format!("store serialize failed: {e}"))?;
        if serialized.len() > MAX_STORE_BYTES {
            return Err(QUOTA_MSG.to_string());
        }
        self.commit(&serialized)
    }

    /// Read the whole object, degrading to empty on every failure. See
    /// `get` for why malformed is not an error.
    fn read_map(&self) -> BTreeMap<String, String> {
        let Ok(text) = std::fs::read_to_string(self.file()) else {
            return BTreeMap::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Temp file + rename, in the same directory so the rename is
    /// atomic. A different directory would make it a copy on some
    /// filesystems and could fail across mount points.
    fn commit(&self, serialized: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            format!("cannot create store dir {}: {e}", self.dir.display())
        })?;
        let tmp = self.dir.join(format!(
            "store.json.tmp.{}-{}",
            std::process::id(),
            crate::util::uuid::v7()
        ));
        write_then_rename(&tmp, &self.file(), serialized)
    }
}

/// Split out so the atomicity test has something to name, and so the
/// error paths can clean up the temp file on the way out — a failed
/// commit must not leave litter beside `store.json`.
fn write_then_rename(tmp: &Path, final_path: &Path, serialized: &str) -> Result<(), String> {
    if let Err(e) = std::fs::write(tmp, serialized) {
        let _ = std::fs::remove_file(tmp);
        return Err(format!("cannot write store: {e}"));
    }
    if let Err(e) = std::fs::rename(tmp, final_path) {
        let _ = std::fs::remove_file(tmp);
        return Err(format!("cannot commit store: {e}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nanopi-store-test-{}-{}",
            std::process::id(),
            crate::util::uuid::v7()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn absent_key_reads_empty() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        assert_eq!(s.get("nope"), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_then_get_round_trips_and_replaces() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        s.set("k", "v1").unwrap();
        assert_eq!(s.get("k"), "v1");
        s.set("k", "v2").unwrap();
        assert_eq!(s.get("k"), "v2", "a second set replaces, never appends");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Invariant 8: `Ok(())` from `set` means the bytes reached the
    /// filesystem. Read the file with plain `std::fs` and NO further
    /// flush call — this is the test that forbids a write-behind cache.
    #[test]
    fn set_returning_ok_means_the_bytes_are_on_disk() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        s.set("prefers", "rust").unwrap();
        let raw = std::fs::read_to_string(root.join("memory").join("store.json"))
            .expect("store.json must exist immediately after a successful set");
        assert!(
            raw.contains("prefers") && raw.contains("rust"),
            "the value must be in the file, not in a buffer: {raw:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_plugins_do_not_share_keys() {
        let root = tmp_root();
        let a = PluginStore::new(root.clone(), "alpha");
        let b = PluginStore::new(root.clone(), "beta");
        a.set("secret", "alpha-only").unwrap();
        assert_eq!(
            b.get("secret"),
            "",
            "one plugin must not read another's keys"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn over_byte_quota_is_reported_and_stores_nothing() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        s.set("keep", "me").unwrap();
        let huge = "x".repeat(MAX_STORE_BYTES + 1);
        let err = s.set("big", &huge).expect_err("over quota must be refused");
        // The spec's literal, not a recomputation of the code's own
        // formatting.
        assert_eq!(err, "store quota exceeded (1 MiB)");
        assert_eq!(s.get("big"), "", "a refused set stores nothing");
        assert_eq!(s.get("keep"), "me", "a refusal must not disturb what was there");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn key_count_cap_is_reported_and_stores_nothing() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        for i in 0..MAX_KEYS {
            s.set(&format!("k{i}"), "v").unwrap();
        }
        let err = s
            .set("one-too-many", "v")
            .expect_err("key number 1001 must be refused");
        assert!(
            err.contains("1000"),
            "the message must name the cap so an author can act: {err:?}"
        );
        assert_eq!(s.get("one-too-many"), "");
        // At the cap, REPLACING an existing key must still work.
        s.set("k0", "updated")
            .expect("a replacement is not a new key");
        assert_eq!(s.get("k0"), "updated");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn key_length_cap_is_inclusive() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        let ok = "k".repeat(MAX_KEY_CHARS);
        s.set(&ok, "v").expect("a 128-char key is accepted — the cap is `key ≤ 128`");
        assert_eq!(s.get(&ok), "v");
        let too_long = "k".repeat(MAX_KEY_CHARS + 1);
        let err = s.set(&too_long, "v").expect_err("129 chars must be refused");
        assert!(err.contains("128"), "message must name the cap: {err:?}");
        assert_eq!(s.get(&too_long), "", "a refused set stores nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn malformed_or_empty_store_reads_empty_and_a_following_set_succeeds() {
        for corpse in ["", "{not json", "[1,2,3]"] {
            let root = tmp_root();
            let s = PluginStore::new(root.clone(), "memory");
            std::fs::create_dir_all(root.join("memory")).unwrap();
            std::fs::write(root.join("memory").join("store.json"), corpse).unwrap();
            assert_eq!(
                s.get("anything"),
                "",
                "a torn or hand-edited store reads empty, not an error ({corpse:?})"
            );
            s.set("fresh", "v")
                .expect("a following set must succeed — a bad file must not brick the plugin");
            assert_eq!(s.get("fresh"), "v");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn commit_leaves_no_temp_file_behind() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        s.set("a", "1").unwrap();
        s.set("b", "2").unwrap();
        let names: Vec<String> = std::fs::read_dir(root.join("memory"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["store.json".to_string()],
            "temp+rename must leave exactly store.json, no litter"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T-d87-01: a key shaped like a path traversal is just a key. It
    /// must not create anything outside the store directory.
    #[test]
    fn traversal_shaped_key_is_an_ordinary_key() {
        let root = tmp_root();
        let s = PluginStore::new(root.clone(), "memory");
        s.set("../../etc/passwd", "nope").unwrap();
        assert_eq!(s.get("../../etc/passwd"), "nope");
        let names: Vec<String> = std::fs::read_dir(root.join("memory"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["store.json".to_string()]);
        // And nothing appeared beside the plugin's own directory.
        let siblings: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(siblings, vec!["memory".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }
}

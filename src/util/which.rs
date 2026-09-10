//! Minimal `which`. No new dependency: the `which` crate pulls in
//! `either` + `home` + `rustix` for what is a PATH split and a stat, and
//! this binary's whole pitch is not doing that.
//!
//! Shared by `util::shell` (bash vs sh) and the grep tool (ripgrep vs the
//! built-in walker). Both need the same question answered — "is this
//! executable on PATH, and where" — and both need it answered against an
//! injected PATH so the interesting case is testable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Look `name` up against the process PATH.
///
/// A name containing `/` is taken as a path and only checked for
/// executability — PATH is not consulted, matching execvp.
pub fn which(name: &str) -> Option<PathBuf> {
    find_in(name, &std::env::var_os("PATH"))
}

/// `which` with PATH injected. Separate so tests can build a fake PATH
/// containing exactly one tool; `which()` alone cannot express "a host
/// where ripgrep does not exist" without mutating global state.
pub fn find_in(name: &str, path: &Option<OsString>) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = path.as_ref()?;
    for dir in std::env::split_paths(path) {
        // An empty PATH entry means cwd by POSIX. Skipped deliberately:
        // honouring it is how a repo ships its own `./rg` and gets it run.
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            // On Windows the entry on PATH is `rg.exe` / `bash.exe`.
            for ext in ["exe", "cmd", "bat"] {
                let p = dir.join(format!("{name}.{ext}"));
                if is_executable(&p) {
                    return Some(p);
                }
            }
        }
    }
    None
}

#[cfg(unix)]
pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // The mode check is load-bearing, not decoration: `/system/bin` on
    // Android and `$PREFIX/bin` in Termux both contain non-executable
    // entries, and `is_file()` alone would happily return one of those.
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Build a directory containing the named files, mode 0755, for tests
/// that need a PATH with an exact tool inventory.
#[cfg(all(test, unix))]
pub fn fake_path_dir(names: &[&str]) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    for n in names {
        let p = dir.path().join(n);
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_names_skip_path_entirely() {
        // `None` PATH proves the `/`-containing branch never consults it.
        assert!(find_in("/definitely/not/here/rg", &None).is_none());
        #[cfg(unix)]
        assert!(find_in("/bin/sh", &None).is_some());
    }

    #[test]
    #[cfg(unix)]
    fn finds_only_what_is_on_the_given_path() {
        let dir = fake_path_dir(&["rg"]);
        let path = Some(dir.path().as_os_str().to_os_string());
        assert_eq!(find_in("rg", &path).unwrap(), dir.path().join("rg"));
        assert!(find_in("bash", &path).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn non_executable_files_are_not_tools() {
        let dir = fake_path_dir(&[]);
        let f = dir.path().join("rg");
        std::fs::write(&f, b"not runnable\n").unwrap();
        let path = Some(dir.path().as_os_str().to_os_string());
        assert!(!is_executable(&f));
        assert!(find_in("rg", &path).is_none());
    }

    #[test]
    fn missing_path_var_is_not_a_panic() {
        assert!(find_in("rg", &None).is_none());
    }
}

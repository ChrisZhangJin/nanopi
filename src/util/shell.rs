//! Which shell the `bash` tool and the hook runner spawn.
//!
//! Both used to hardcode `Command::new("bash")`. That is fine on a normal
//! Linux distro and on Termux, but there are two environments where it
//! leaves the agent with no working `bash` tool and no working hooks:
//!
//!   - a bare Android system shell (`adb shell`), which ships mksh as
//!     `/system/bin/sh` and no bash at all;
//!   - minimal containers (alpine, distroless, busybox-based images),
//!     which is squarely the "old and low-resource" target in the crate
//!     description.
//!
//! Spawn failed with ENOENT in both, and the model saw an opaque
//! "failed to spawn bash" on every single tool call.
//!
//! Resolution order: `$NANOPI_SHELL` if set, then `bash`, then `sh`. The
//! env var is the escape hatch for someone who pushed a static busybox
//! onto a device and wants it used explicitly.
//!
//! Resolved once per process. A PATH scan is cheap but this runs on every
//! bash tool call and every hook, and the answer cannot change under us
//! in any way we care about.

use std::sync::OnceLock;

use crate::util::which::find_in;

/// Tried in order when `$NANOPI_SHELL` is unset.
///
/// `sh` last, not first: it is the one guaranteed to exist, so probing it
/// first would mean never finding bash. Nothing beyond these two is worth
/// probing — `dash`/`mksh`/busybox all install themselves as `sh`.
const CANDIDATES: &[&str] = &["bash", "sh"];

/// The shell to pass `-c` to.
///
/// Never fails: falls back to `sh` when nothing is found on PATH, so the
/// spawn error the caller reports is about a missing `sh` rather than
/// about this function having no answer.
pub fn shell() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED
        .get_or_init(|| resolve(std::env::var_os("NANOPI_SHELL"), &std::env::var_os("PATH")))
        .as_str()
}

/// The resolution itself, with both inputs injected.
///
/// Split out from `shell()` because `shell()` memoizes in a process-wide
/// `OnceLock`: a test that mutated `$PATH` and called it would either see
/// a value cached by an earlier test or poison the cache for a later one.
/// The whole point of this module is the bashless case, and that case is
/// untestable through the memoized entry point.
fn resolve(env_shell: Option<std::ffi::OsString>, path: &Option<std::ffi::OsString>) -> String {
    if let Some(explicit) = env_shell {
        let explicit = explicit.to_string_lossy().to_string();
        if !explicit.trim().is_empty() {
            return explicit;
        }
    }
    for candidate in CANDIDATES {
        // The absolute path, not the bare name. Two reasons, both real:
        //
        //   - `run_hook` calls `env_clear()`, so a bare name would be
        //     resolved against the CHILD's PATH. It happens to work today
        //     because `extract_env` re-adds all of `std::env::vars()`, but
        //     that is a coincidence one edit away from breaking.
        //   - it makes "found on PATH" distinguishable from "fell through
        //     to the default below", which is what lets the bashless test
        //     actually fail when the candidate list is wrong.
        if let Some(found) = find_in(candidate, path) {
            return found.to_string_lossy().to_string();
        }
    }
    // Nothing found. Hand back a bare `sh` anyway so the caller's spawn
    // produces a real ENOENT naming `sh`, rather than this returning an
    // Option every call site has to unwrap into the same message.
    "sh".to_string()
}

/// Just the shell's file name, for text shown to the model. The full
/// path is right for spawning and noise in a tool description.
pub fn shell_display() -> &'static str {
    let s = shell();
    match s.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name,
        _ => s,
    }
}

/// True when the resolved shell is not bash, i.e. the caller is running
/// under a POSIX shell that will reject bashisms. Used to tell the model
/// so it stops emitting `[[ ]]` and arrays into a shell that cannot parse
/// them — a silent syntax error is worse than a missing feature.
pub fn is_posix_fallback() -> bool {
    !shell_display().starts_with("bash")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sh_on_any_unix() {
        // Not asserting *which* shell: CI has bash, a minimal container
        // may not. Asserting only that we never hand back nothing.
        assert!(!shell().is_empty());
    }

    #[test]
    fn absolute_path_is_probed_directly_not_via_path() {
        // `None` PATH proves the `/`-containing branch never consults it.
        assert!(find_in("/definitely/not/here/bash", &None).is_none());
        #[cfg(unix)]
        assert!(find_in("/bin/sh", &None).is_some());
    }

    /// The whole reason this module exists: a host with `sh` and no
    /// `bash` — a bare Android shell, alpine, distroless. Before the
    /// fallback this resolved to `bash` and every spawn hit ENOENT.
    #[test]
    #[cfg(unix)]
    fn bashless_host_falls_back_to_sh() {
        let dir = crate::util::which::fake_path_dir(&["sh"]);
        let path = Some(dir.path().as_os_str().to_os_string());
        // The DISCOVERED path, not the bare "sh" default. Asserting the
        // literal would also pass when the PATH scan found nothing, which
        // is how the first version of this test had no teeth.
        assert_eq!(
            resolve(None, &path),
            dir.path().join("sh").to_string_lossy()
        );
    }

    /// And the ordering actually prefers bash when both exist — a
    /// fallback that always picked `sh` would silently downgrade every
    /// normal Linux box, which is the failure this pins against.
    #[test]
    #[cfg(unix)]
    fn prefers_bash_when_both_exist() {
        let dir = crate::util::which::fake_path_dir(&["sh", "bash"]);
        let path = Some(dir.path().as_os_str().to_os_string());
        assert_eq!(
            resolve(None, &path),
            dir.path().join("bash").to_string_lossy()
        );
    }

    /// Nothing on PATH at all: still answers, so the caller's spawn
    /// error names a missing `sh` instead of this returning no shell.
    #[test]
    fn empty_path_still_answers_sh() {
        let dir = tempfile::tempdir().unwrap();
        let path = Some(dir.path().as_os_str().to_os_string());
        assert_eq!(resolve(None, &path), "sh");
        assert_eq!(resolve(None, &None), "sh");
    }

    /// `$NANOPI_SHELL` wins over PATH — the escape hatch for a static
    /// busybox pushed onto a device.
    #[test]
    #[cfg(unix)]
    fn env_override_beats_path() {
        let dir = crate::util::which::fake_path_dir(&["sh", "bash"]);
        let path = Some(dir.path().as_os_str().to_os_string());
        let busybox = dir.path().join("busybox").as_os_str().to_os_string();
        assert_eq!(
            resolve(Some(busybox.clone()), &path),
            busybox.to_string_lossy()
        );
        // Blank / whitespace-only is treated as unset, not as a shell
        // named "  " — an exported-but-empty NANOPI_SHELL is common.
        let bash = dir.path().join("bash").to_string_lossy().to_string();
        assert_eq!(resolve(Some("   ".into()), &path), bash);
        assert_eq!(resolve(Some("".into()), &path), bash);
    }

    /// A non-executable `bash` on PATH must not win. This is the Android
    /// `/system/bin` case the mode check in `is_executable` guards.
    #[test]
    #[cfg(unix)]
    fn non_executable_bash_is_skipped_for_sh() {
        let dir = crate::util::which::fake_path_dir(&["sh"]);
        std::fs::write(dir.path().join("bash"), b"not runnable\n").unwrap();
        let path = Some(dir.path().as_os_str().to_os_string());
        assert_eq!(
            resolve(None, &path),
            dir.path().join("sh").to_string_lossy()
        );
    }

    #[test]
    fn posix_fallback_tracks_the_resolved_shell() {
        // bash resolves to a name ending in "bash"; anything else (sh,
        // mksh via sh, an explicit busybox path) counts as the fallback.
        assert_eq!(is_posix_fallback(), !shell_display().starts_with("bash"));
    }
}

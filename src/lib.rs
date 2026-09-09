//! nanopi v0.5 — library crate.
//!
//! `main.rs` is a thin shim that dispatches to `mode::*`. All real logic lives here.

pub mod command;
pub mod config;
pub mod event;
pub mod keys;
pub mod paths;
pub mod models;
/// Unconditionally compiled, like `subscriber` below and for the same
/// reason: turn assembly in `agent::loop_` reads it, and that path must
/// stay free of `#[cfg(feature = "wasm")]`. Without the feature the
/// registry is simply always empty.
pub mod plugin_context;
/// Unconditionally compiled, for the same reason as `plugin_context`
/// above: `agent::loop_` is on the reading side of this seam, so the
/// tool-execution path stays free of `#[cfg(feature = "wasm")]`.
/// Without the feature nothing ever installs a dispatch and nothing
/// ever calls one.
pub mod plugin_tools;
/// Unconditionally compiled, same seam as `plugin_context` and
/// `subscriber`: `mode::tui` reads this to render `/tools`'s grant
/// rows, so the TUI needs no `#[cfg(feature = "wasm")]`. Without the
/// feature the vec is always empty and the section is omitted.
pub mod plugin_grants;
/// Unconditionally compiled, same seam again: `mode::tui` installs the
/// sink, drains the echoes and marks the turn origin, so the turn loop
/// needs no `#[cfg(feature = "wasm")]`. Without the feature nothing
/// ever calls `send`, both drains are always empty, and the loop is
/// byte-identical.
pub mod plugin_send;
pub mod resources;
pub mod session;
pub mod settings;
pub mod settings_toml;
pub mod subscriber;
pub mod trust;
pub mod wizard;

pub mod agent;
pub mod mode;
pub mod provider;
pub mod render;
pub mod tool;
pub mod util;
pub mod vendor;

#[cfg(feature = "wasm")]
pub mod wasm;

/// Process-wide test mutex. Tests that mutate `$NANOPI_HOME` (or any
/// other global env var) MUST acquire this lock before changing it,
/// so parallel test execution can't poison each other's environment.
///
/// Acquire it through [`test_lock`], never with `.lock().unwrap()` —
/// see that function for why.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire [`TEST_LOCK`], recovering from poisoning.
///
/// A test that panics **while holding the lock** poisons it, and every
/// later `.lock().unwrap()` then panics too — so one real failure was
/// reported as 13-14, and the true one was not the first in the list.
/// The env var the poisoning test was mutating is restored by its own
/// guard on unwind, so the data this mutex protects is not actually
/// corrupted; only the flag is. Recovering is therefore correct, not a
/// papered-over race.
///
/// Exists as one function rather than an idiom copied to every call
/// site because the idiom was already copied wrong 7 times.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A scratch `$NANOPI_HOME` that is put back when the test ends —
/// **including when the test ends by panicking**.
///
/// Replaces ~127 hand-rolled blocks of the shape
///
/// ```ignore
/// let _g = crate::test_lock();
/// let prev = std::env::var_os("NANOPI_HOME");
/// std::env::set_var("NANOPI_HOME", &home);
/// ...body...
/// if let Some(p) = prev { std::env::set_var("NANOPI_HOME", p) }
/// else { std::env::remove_var("NANOPI_HOME") }
/// ```
///
/// which restore correctly on the happy path and **not at all on
/// unwind** — the restore is the last statement of the body, so a
/// failing assertion skips it. That is the actual race behind the
/// flaky suite: the first failure leaves `$NANOPI_HOME` pointing at a
/// deleted temp dir, and every later test that reads it fails for a
/// reason that has nothing to do with what it tests.
///
/// [`test_lock`] fixed the *reporting* (one failure no longer cascades
/// into fourteen); this fixes the *leak*. Both are needed: recovering
/// from a poisoned mutex still leaves the env var wrong.
#[cfg(test)]
pub struct TempNanopiHome {
    // Field order is drop order: env restored and the directory removed
    // (both inside `inner`), THEN the lock released. Releasing the lock
    // first would let a waiting test observe the temp dir mid-teardown.
    inner: ScopedEnvHome,
    _guard: std::sync::MutexGuard<'static, ()>,
}

/// The env swap and its restore, WITHOUT the lock.
///
/// Split out for exactly one caller: the test that verifies the restore
/// happens on unwind. That test has to hold [`TEST_LOCK`] across its own
/// assertion — reading `$NANOPI_HOME` after releasing the lock is racy
/// by construction, and the first version of the test did precisely
/// that and failed under `cargo test` without `--test-threads=1`. It
/// captured a *concurrent* test's scratch path as its baseline. The
/// mutex is not reentrant, so that test cannot construct a
/// `TempNanopiHome`; it constructs this instead and supplies the lock
/// itself.
///
/// Production-shaped code should never reach for this — use
/// [`TempNanopiHome`], which cannot be used without the lock.
#[cfg(test)]
pub struct ScopedEnvHome {
    prev: Option<std::ffi::OsString>,
    dir: tempfile::TempDir,
}

#[cfg(test)]
impl ScopedEnvHome {
    /// Caller MUST already hold [`TEST_LOCK`].
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create scratch NANOPI_HOME");
        let prev = std::env::var_os("NANOPI_HOME");
        std::env::set_var("NANOPI_HOME", dir.path());
        Self { prev, dir }
    }

    pub fn path(&self) -> &std::path::Path {
        self.dir.path()
    }
}

#[cfg(test)]
impl Drop for ScopedEnvHome {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(p) => std::env::set_var("NANOPI_HOME", p),
            None => std::env::remove_var("NANOPI_HOME"),
        }
    }
}

#[cfg(test)]
impl TempNanopiHome {
    /// Acquire the test lock, point `$NANOPI_HOME` at a fresh temp dir,
    /// and hold both until the returned guard drops.
    pub fn new() -> Self {
        let guard = test_lock();
        Self {
            inner: ScopedEnvHome::new(),
            _guard: guard,
        }
    }

    /// The scratch home itself, for tests that need to plant files in
    /// it or assert on what was written.
    pub fn path(&self) -> &std::path::Path {
        self.inner.path()
    }
}

#[cfg(test)]
mod test_lock_tests {
    /// The cascade, reproduced and then shown not to happen.
    ///
    /// A dedicated mutex rather than `TEST_LOCK` itself: poisoning the
    /// real one would be a process-wide side effect on every other
    /// test in the binary, which is precisely the class of bug this
    /// module exists to remove.
    ///
    /// Teeth: swap the body of `recover` for `m.lock().unwrap()` and
    /// this test panics with `PoisonError`, which is the 13-14-failure
    /// cascade in miniature.
    #[test]
    fn a_panic_while_holding_the_lock_does_not_wedge_every_later_acquire() {
        static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
        fn recover() -> std::sync::MutexGuard<'static, ()> {
            M.lock().unwrap_or_else(|e| e.into_inner())
        }

        let poisoned = std::thread::spawn(|| {
            let _g = recover();
            panic!("a test failing while it holds the lock");
        })
        .join();
        assert!(poisoned.is_err(), "the helper thread must have panicked");
        assert!(M.is_poisoned(), "and that must have poisoned the mutex");

        // The real assertion: the NEXT acquire still works. Before this
        // change it panicked, and so did every one after it.
        let _g = recover();
    }

    /// The whole reason `TempNanopiHome` exists rather than another
    /// copy of the set/restore block: the hand-rolled form puts the
    /// restore at the END OF THE BODY, so a failing assertion jumps
    /// over it and leaves `$NANOPI_HOME` pointing at a temp dir that is
    /// about to be deleted.
    ///
    /// Teeth: delete the `Drop` impl and this test fails with the
    /// scratch path still in the environment.
    #[test]
    fn the_scratch_home_is_restored_even_when_the_test_panics() {
        // The lock is held by THIS test for the whole body, including
        // the assertion. Reading $NANOPI_HOME after releasing it is
        // racy by construction: the first version of this test did
        // that, and under parallel execution it captured a concurrent
        // test's scratch path as `before` and failed with
        // `left: None, right: Some("/tmp/.tmp8S6bHd")`. Hence
        // `ScopedEnvHome`, which does the swap without re-locking a
        // mutex we already hold.
        let _g = super::test_lock();
        let before = std::env::var_os("NANOPI_HOME");

        let unwound = std::panic::catch_unwind(|| {
            let home = super::ScopedEnvHome::new();
            let p = home.path().to_path_buf();
            // Prove the swap is actually in effect before we unwind,
            // so a no-op `new()` could not make this test vacuous.
            assert_eq!(
                std::env::var_os("NANOPI_HOME").map(std::path::PathBuf::from),
                Some(p.clone())
            );
            panic!("a test failing with the scratch home installed: {}", p.display());
        });
        assert!(unwound.is_err(), "the closure must have panicked");

        assert_eq!(
            std::env::var_os("NANOPI_HOME"),
            before,
            "NANOPI_HOME must be exactly what it was before the panicking test"
        );
    }

    /// `.lock().unwrap()` is the idiom that caused the cascade, and it
    /// was reintroduced by copy-paste 7 times. Nothing outside
    /// `test_lock` may acquire `TEST_LOCK` directly again.
    #[test]
    fn test_lock_is_the_only_way_to_acquire_the_test_mutex() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src/") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                for (i, line) in text.lines().enumerate() {
                    // The definition of `test_lock` itself is the one
                    // permitted acquisition.
                    if line.contains("TEST_LOCK.lock()")
                        && !path.ends_with("lib.rs")
                        && !line.trim_start().starts_with("///")
                    {
                        offenders.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "acquire TEST_LOCK through crate::test_lock(), not directly: {offenders:?}"
        );
    }
}

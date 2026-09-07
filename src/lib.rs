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

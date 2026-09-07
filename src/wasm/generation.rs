//! Which instance of a plugin is the LIVE one.
//!
//! v0.12 hot reload. `/reload` re-reads `[[extensions]]` and stands up a
//! fresh `ComponentBridge` per plugin, but the bridges it replaces are
//! not destroyed on the spot: `WasmTool`, `WasmCommandHandler` and
//! `WasmEventHandler` each hold an `Arc<dyn WasmExecuteBridge>`, and a
//! tool call already in flight when the reload lands still holds the OLD
//! one. That is memory-safe and WRONG in the way the user cannot see:
//! the call executes against code that has been replaced, and its result
//! is attributed to the plugin now loaded.
//!
//! So a bridge is not identified by the plugin it came from — several
//! bridges over a session share that — but by an INSTANCE ID handed out
//! here, once, at load. This table records which id is currently live
//! for each plugin name, and everything on the bridge's entry path
//! checks itself against it and refuses in-band when it loses
//! (`docs/plugin-capabilities.md` invariant 3).
//!
//! Keyed PER PLUGIN, not one process-wide counter, and that is
//! load-bearing rather than tidiness: it is what lets a plugin that
//! FAILS to reload keep its previously loaded instance alive and
//! callable while its neighbours are replaced. A single global
//! generation would invalidate every bridge the moment any plugin was
//! replaced, so the only available answer to a failed reload would be
//! "you now have neither".
//!
//! Compiled only with the `wasm` feature, like the rest of this module.
//! Nothing outside it needs the ids — the reload path in
//! `agent::build` speaks in plugin NAMES, and the bridge does the
//! comparing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// Instance ids, never reused for the life of the process.
///
/// Starts at 1 so 0 can never collide with a real id — a bridge built
/// by a future code path that forgot to ask for one would then be
/// permanently stale rather than accidentally live, which is the safe
/// direction.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// plugin name → the instance id currently allowed to run.
static ACTIVE: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, u64>> {
    // Recovered rather than propagated, for the reason `notify::lock`
    // gives: a poisoned table must not disable every plugin for the rest
    // of the session. The worst a panic mid-insert leaves is one
    // plugin's row missing, and a missing row reads as "stale", which
    // refuses in-band instead of running replaced code.
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Claim an id for a bridge about to be built.
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Declare `id` the live instance of `plugin`, retiring whatever held
/// the row before.
///
/// Called at the END of a successful load, never before it: a load that
/// fails must leave the previous instance live, which is the whole
/// rollback story for case 3 in the reload design.
pub fn activate(plugin: &str, id: u64) {
    lock().insert(plugin.to_string(), id);
}

/// Whether this bridge is still the one calls should reach.
///
/// A plugin with no row at all is NOT live. That case is only reachable
/// for a bridge that was never activated (a load that failed after
/// building the bridge, or a hand-built one in a test), and refusing is
/// the safe reading.
pub fn is_live(plugin: &str, id: u64) -> bool {
    lock().get(plugin).copied() == Some(id)
}

/// Retire `plugin` entirely: no bridge is live for it any more.
///
/// For a plugin that disappeared from `[[extensions]]` between reloads.
/// Implemented by parking a FRESH id in the row rather than removing the
/// row, so it does not matter whether "absent" is read as live by some
/// later caller — the row holds an id no bridge can ever hold.
pub fn retire(plugin: &str) {
    let unheld = next_id();
    lock().insert(plugin.to_string(), unheld);
}

/// Forget every row. Test-only: nothing in the running program
/// invalidates all plugins at once.
#[cfg(test)]
pub fn reset_all() {
    lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is process-wide, so these must not interleave with
    /// anything else that touches it.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::test_lock();
        reset_all();
        g
    }

    #[test]
    fn an_activated_id_is_live_and_its_predecessor_is_not() {
        let _g = guard();
        let first = next_id();
        let second = next_id();
        activate("p", first);
        assert!(is_live("p", first));
        activate("p", second);
        assert!(
            !is_live("p", first),
            "the replaced instance must lose the row"
        );
        assert!(is_live("p", second));
    }

    /// Ids are unique across plugins as well as across reloads: two
    /// plugins reloaded in the same pass must not be able to end up
    /// holding the same id, or activating one would silently validate
    /// the other's stale bridge.
    #[test]
    fn ids_are_never_handed_out_twice() {
        let _g = guard();
        let ids: Vec<u64> = (0..64).map(|_| next_id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate instance id handed out");
    }

    /// A bridge whose load never reached `activate` must be stale, not
    /// live. This is the failed-load path.
    #[test]
    fn a_bridge_that_was_never_activated_is_not_live() {
        let _g = guard();
        let id = next_id();
        assert!(!is_live("never-loaded", id));
    }

    #[test]
    fn retiring_a_plugin_leaves_nothing_live_for_it() {
        let _g = guard();
        let id = next_id();
        activate("gone", id);
        retire("gone");
        assert!(!is_live("gone", id));
        // And not by luck of an empty row: a fresh bridge that has not
        // been activated must not be live either.
        assert!(!is_live("gone", next_id()));
    }

    /// Per-plugin, not global — the rollback story depends on it.
    #[test]
    fn replacing_one_plugin_leaves_another_alone() {
        let _g = guard();
        let a = next_id();
        let b = next_id();
        activate("a", a);
        activate("b", b);
        activate("a", next_id());
        assert!(is_live("b", b), "b's bridge must survive a's replacement");
    }
}

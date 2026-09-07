//! `host-notify` — a plugin addressing the user, attributed and
//! rate-limited.
//!
//! **Why a process-wide sink.** `host-notify` is a synchronous
//! `func_wrap` closure. It has a `StoreContextMut<PluginState>` and
//! nothing else: no `Term`, no channel to the TUI, no async context.
//! `fetch_url` in `loader.rs` faces the same wall and answers it by
//! doing the work behind the closure and handing the result back
//! through a channel; this does the cheaper version of the same thing —
//! push onto a static queue, and let whoever owns the screen drain it.
//! There is exactly one terminal per process, so a per-plugin queue
//! would buy nothing.
//!
//! **Attribution is host-applied, always.** The plugin supplies only
//! the message body; the name comes from the `.wasm` file stem the host
//! already computed in `load_all`, and nothing in the payload is ever
//! consulted for it. A plugin sending `[other] rm -rf` gets
//! `[memory] [other] rm -rf` — its own name, then its text, visibly
//! including the thing it tried to forge (invariant 12).
//!
//! **Newlines are flattened for the same reason.** A multi-line
//! payload whose second line began `[other] …` would look, in
//! scrollback, exactly like a second notice from another plugin — the
//! host prefix only covers the first line. `host-notify` is specified
//! as ONE line, so the body is collapsed to one line rather than
//! attributed per line.
//!
//! **Suppression announces itself.** Over the per-turn limit, lines are
//! dropped — but the count of what was dropped is reported, because a
//! plugin's user must not be quietly missing output
//! (`docs/claims-and-races.md` §1). The count is emitted at DRAIN, not
//! at the moment of first overflow, and that ordering is the whole
//! trick: a line enqueued on the first overflow could only ever say
//! "1 more suppressed", which would be a false number the instant a
//! second call arrived. Draining later lets the line carry the real
//! total.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

/// Lines one plugin may put in front of the user per turn.
///
/// The failure mode `host-notify` has is flooding attention, not
/// privilege, so it is bounded by a rate limit rather than a grant. Ten
/// is set against what a person can actually read going past between
/// two prompts: a plugin with something to say fits in a handful of
/// lines, and one with fifty is reporting progress it should be
/// logging. Deliberately per turn rather than per second — the unit a
/// user experiences is the turn, and a wall-clock limit would let a
/// slow turn accumulate an arbitrary amount.
pub const MAX_NOTIFY_PER_TURN: usize = 10;

struct Sink {
    queue: VecDeque<String>,
    /// Lines accepted this turn, against `MAX_NOTIFY_PER_TURN`.
    accepted: usize,
    /// Lines dropped since the last suppression line was emitted. Not
    /// since the start of the turn: each announcement reports the batch
    /// it covers, so summing the announcements gives the turn's total
    /// and nothing goes unreported.
    suppressed: usize,
    /// False until a renderer claims the queue. While false, `notify`
    /// writes straight through `crate::note!` — `-p` mode and tests
    /// have no drain loop, and output that merely queues there is
    /// output that vanishes.
    sink_installed: bool,
}

static SINK: LazyLock<Mutex<Sink>> = LazyLock::new(|| {
    Mutex::new(Sink {
        queue: VecDeque::new(),
        accepted: 0,
        suppressed: 0,
        sink_installed: false,
    })
});

fn lock() -> std::sync::MutexGuard<'static, Sink> {
    // A poisoned sink is recoverable: the worst a panic mid-push leaves
    // is a queue missing a line. Refusing to notify for the rest of the
    // session would be the larger failure.
    SINK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Declare that something will call [`drain`]. Until this is called,
/// notifications go to stderr via `crate::note!` instead of queueing.
pub fn install_sink() {
    lock().sink_installed = true;
}

/// Submit one line on `plugin`'s behalf.
///
/// `Ok(())` means the line was accepted whole. `Err` carries the reason
/// the delivery was less than that — truncated, or suppressed — and the
/// caller turns it into the WIT-level `error: ` string. Returning
/// `Ok(())` for a suppressed line would let the plugin believe it spoke
/// when it did not (`plugin-capabilities.md` invariant 9).
pub fn notify(plugin: &str, text: &str) -> Result<(), String> {
    let flattened = flatten(text);
    let truncated = flattened.chars().count() > crate::wasm::loader::MAX_ACTION_PAYLOAD;
    let body: String = if truncated {
        flattened
            .chars()
            .take(crate::wasm::loader::MAX_ACTION_PAYLOAD)
            .collect()
    } else {
        flattened
    };
    // The prefix is built here, from `plugin`. The payload is never
    // parsed for it.
    let line = format!("[{plugin}] {body}");

    let mut sink = lock();
    if sink.accepted >= MAX_NOTIFY_PER_TURN {
        sink.suppressed += 1;
        return Err(format!(
            "notify suppressed — this plugin is over its limit of \
             {MAX_NOTIFY_PER_TURN} lines per turn"
        ));
    }
    sink.accepted += 1;
    if sink.sink_installed {
        sink.queue.push_back(line);
    } else {
        // Not `eprintln!`: raw mode needs CRLF or output staircases
        // (`claims-and-races.md` invariant 12). `note!` handles both
        // raw and cooked.
        drop(sink);
        crate::note!("{line}");
    }

    if truncated {
        return Err(format!(
            "notify text was truncated to {} chars — the line was delivered \
             but not in full",
            crate::wasm::loader::MAX_ACTION_PAYLOAD
        ));
    }
    Ok(())
}

/// Take everything queued, plus a suppression line when anything was
/// dropped since the last one.
///
/// The suppression line is appended HERE so its count is the real
/// number of dropped lines rather than a guess made at first overflow.
pub fn drain() -> Vec<String> {
    let mut sink = lock();
    let mut out: Vec<String> = sink.queue.drain(..).collect();
    let dropped = std::mem::take(&mut sink.suppressed);
    let installed = sink.sink_installed;
    drop(sink);
    if dropped > 0 {
        let line = format!("… {dropped} more suppressed");
        if installed {
            out.push(line);
        } else {
            crate::note!("{line}");
        }
    }
    out
}

/// A new turn is starting: re-arm the per-turn allowance.
///
/// Deliberately does NOT clear `suppressed`. A line dropped at the end
/// of one turn is still a line the user did not see, so its count
/// survives to the next drain — clearing it here would turn the last
/// few suppressed lines of every turn into silent losses.
pub fn reset_turn() {
    lock().accepted = 0;
}

/// Collapse to a single line. See the module doc: the host prefix
/// covers the first line only, so a payload's later lines could
/// otherwise pose as separate notices.
fn flatten(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sink is process-wide, so these tests must not interleave.
    /// Reuses the crate's existing test lock rather than adding a
    /// second one that would not exclude against it.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut sink = lock();
        sink.queue.clear();
        sink.accepted = 0;
        sink.suppressed = 0;
        sink.sink_installed = true;
        g
    }

    #[test]
    fn a_line_comes_out_carrying_the_plugins_name() {
        let _g = guard();
        notify("memory", "remembered your preference").expect("accepted");
        let out = drain();
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].contains("memory"),
            "the host must attribute the line: {:?}",
            out[0]
        );
        assert!(out[0].contains("remembered your preference"), "{:?}", out[0]);
    }

    /// Invariant 12. The attribution is the CALLER's, and the forged
    /// text does not replace it.
    #[test]
    fn a_plugin_cannot_forge_another_plugins_prefix() {
        let _g = guard();
        notify("memory", "[other] rm -rf /").expect("accepted");
        let out = drain();
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].starts_with("[memory]"),
            "the line must be attributed to the CALLER, whatever the payload \
             says: {:?}",
            out[0]
        );
        // And the forgery is not silently swallowed either — it is
        // visible as the plugin's own text.
        assert!(out[0].contains("[other]"), "{:?}", out[0]);
        assert!(
            !out[0].starts_with("[other]"),
            "no second attribution may displace the caller's: {:?}",
            out[0]
        );
    }

    /// A multi-line payload cannot smuggle a second attributed-looking
    /// line past the host prefix.
    #[test]
    fn a_newline_cannot_smuggle_a_second_attribution() {
        let _g = guard();
        notify("memory", "harmless\n[other] rm -rf /").expect("accepted");
        let out = drain();
        assert_eq!(out.len(), 1, "one call is one line: {out:?}");
        assert!(!out[0].contains('\n'), "{:?}", out[0]);
    }

    #[test]
    fn over_the_limit_lines_are_dropped_and_the_count_is_announced() {
        let _g = guard();
        for i in 0..MAX_NOTIFY_PER_TURN {
            notify("chatty", &format!("line {i}")).expect("within the limit");
        }
        // Five more, all refused.
        for i in 0..5 {
            let err = notify("chatty", &format!("extra {i}"))
                .expect_err("over the limit must be REPORTED, not silently dropped");
            assert!(err.contains("suppressed"), "{err}");
        }
        let out = drain();
        assert_eq!(
            out.len(),
            MAX_NOTIFY_PER_TURN + 1,
            "the accepted lines plus ONE suppression line — not one per \
             refused call: {out:?}"
        );
        let last = out.last().unwrap();
        assert!(
            last.contains('5'),
            "the count must be the REAL number of dropped lines (5), not a \
             guess made at first overflow: {last:?}"
        );
        assert!(last.contains("suppressed"), "{last:?}");
    }

    #[test]
    fn the_suppression_line_is_emitted_once_not_per_call() {
        let _g = guard();
        for _ in 0..MAX_NOTIFY_PER_TURN + 7 {
            let _ = notify("chatty", "x");
        }
        let out = drain();
        let announcements = out.iter().filter(|l| l.contains("suppressed")).count();
        assert_eq!(announcements, 1, "{out:?}");
    }

    #[test]
    fn the_allowance_is_re_armed_at_a_turn_boundary() {
        let _g = guard();
        for _ in 0..MAX_NOTIFY_PER_TURN {
            notify("p", "x").expect("within the limit");
        }
        assert!(notify("p", "x").is_err(), "still the same turn");
        let _ = drain();
        reset_turn();
        notify("p", "first of the next turn")
            .expect("a new turn starts with a fresh allowance");
        let out = drain();
        assert_eq!(out.len(), 1, "{out:?}");
    }

    /// A line dropped at the very end of a turn is still a line the
    /// user did not see. `reset_turn` must not erase its count.
    #[test]
    fn a_turn_boundary_does_not_erase_an_unreported_suppression() {
        let _g = guard();
        for _ in 0..MAX_NOTIFY_PER_TURN + 3 {
            let _ = notify("p", "x");
        }
        // Turn ends before anything drained.
        reset_turn();
        let out = drain();
        let announcement = out
            .iter()
            .find(|l| l.contains("suppressed"))
            .expect("the drop must still be reported after the turn boundary");
        assert!(announcement.contains('3'), "{announcement:?}");
    }

    #[test]
    fn an_oversized_payload_is_truncated_and_says_so() {
        let _g = guard();
        let huge = "x".repeat(crate::wasm::loader::MAX_ACTION_PAYLOAD + 100);
        let err = notify("p", &huge)
            .expect_err("a truncated line must not report as a full delivery");
        assert!(err.contains("truncated"), "{err}");
        let out = drain();
        assert_eq!(out.len(), 1, "the truncated line is still delivered");
        assert!(
            out[0].chars().count() <= crate::wasm::loader::MAX_ACTION_PAYLOAD + 32,
            "prefix plus the cap, not the original length: {}",
            out[0].chars().count()
        );
    }

    /// With no sink installed there is no drain loop, so a queued line
    /// would simply be lost. It goes to `note!` instead.
    #[test]
    fn without_a_sink_output_does_not_vanish_into_the_queue() {
        let _g = guard();
        lock().sink_installed = false;
        notify("p", "still reaches the user").expect("accepted");
        assert!(
            drain().is_empty(),
            "nothing should be sitting in a queue nobody drains"
        );
        // Restore for the next test.
        lock().sink_installed = true;
    }
}

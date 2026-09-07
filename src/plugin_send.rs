//! The host side of `host-send-user-message`
//! (`docs/plugin-capabilities.md` §2.4): the one import that makes a
//! plugin spend the user's money.
//!
//! **Why this lives at the crate root and not under `src/wasm/`.** The
//! same reason `src/subscriber.rs`, `src/plugin_context.rs` and
//! `src/plugin_tools.rs` give: `src/mode/tui.rs` is on the reading
//! side of this seam, so the TUI stays free of
//! `#[cfg(feature = "wasm")]` and the plugin layer reaches IN rather
//! than the loop reaching out. Without the feature nothing ever calls
//! [`send`], every drain returns nothing, and the non-wasm build is
//! byte-identical apart from this dead-but-compiled module.
//!
//! **Why an installed sink and not a channel in `PluginState`.**
//! `steer_tx_slot` and `follow_up_slot` are LOCALS of the TUI's turn
//! loop, passed by `&mut` through `handle_action`. A plugin calls this
//! import from a synchronous guest on an arbitrary thread — during an
//! event handler, for instance — so neither queue is reachable from a
//! `PluginState` built at load time. Stage 3 hit the same wall with
//! `run_one_tool`'s nine parameters and answered it the same way.
//!
//! **The two halves are not symmetric.** `steer_tx` is an
//! `mpsc::Sender`: cloneable, and it outlives the agent leaving the
//! slot because the channel is independent of the `Agent`. So the sink
//! carries a clone and uses it FIRST, because that reaches the
//! machinery `b90b27f` repaired rather than a parallel one. Pushing a
//! too-late message onto `Agent::pending_follow_ups` — the tidy-looking
//! option — cannot work: `tui.rs` says in its own words that a steer
//! which missed its turn is noticed "in a window where the agent has
//! been taken out of the slot and cannot be written to", and during a
//! turn is precisely when a `turn_start` subscriber calls this. That is
//! why the TUI already keeps `follow_up_slot` as a second source it
//! cannot merge into the first, and why this module keeps a third.
//!
//! **Echo and send are decided together, under one lock.** §Required
//! tests used to say the text is echoed BEFORE it is sent. The code
//! does the opposite on purpose and `b90b27f` is why: with the echo
//! first, the one case that can fail — the turn ending between the call
//! and the dispatch, dropping the receiver — printed the echo and then
//! discarded the text. The user saw their message land and it was gone.
//! What the user actually needs is a biconditional, not an ordering:
//!
//!   * no send without an echo, and
//!   * no echo without a send.
//!
//! Both are structural here. The echo is recorded only on the branch
//! where `try_send` returned `Ok`, and the overflow branch is echoed by
//! `KeyAction::StartTurn`'s own `render_user_echo` when the queued text
//! starts its turn. The spec was amended in this stage rather than
//! quietly diverged from.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

use crate::event::SteerMessage;
use tokio::sync::mpsc;

/// How many turns ONE plugin may start or steer in a single session.
///
/// **This is not one of §2.4's two rules. It is a third bound stage 4
/// found necessary, and the reason is worth keeping next to the
/// constant.** §2.4 mandates (1) at most one pending message per plugin
/// and (2) no call during a turn this plugin's own message started.
/// Neither bounds the obvious loop: A sends → a turn runs → A's
/// `turn_end` subscriber sends again → a turn runs → forever. Rule 2 is
/// satisfied every time, because each new turn's origin is a *new*
/// turn, and rule 1 is satisfied too, because each message is consumed
/// before the next one is submitted. The pair reads like a loop guard
/// and is not one.
///
/// Twenty is set against what the bound is protecting: the user's
/// money. A plugin with a legitimate reason to drive the agent does it
/// a handful of times per session; one on its fiftieth turn is looping.
/// Per session rather than per hour, because a wall-clock window lets a
/// patient loop run forever.
///
/// Per PLUGIN, like both real rules — a shared budget would let one
/// looping plugin spend another's.
pub const MAX_PLUGIN_TURNS_PER_SESSION: usize = 20;

/// The refusal when nothing is listening.
///
/// Reachable in ordinary operation, and headless (`nanopi -p`) is the
/// case that matters: `src/mode/print.rs` does not consume
/// `CommandAction` and has no steer channel, so the whole
/// plugin-message path is TUI-only and the sink is never installed
/// there. Invariant 9 is why this cannot be a silent no-op — a plugin
/// must never believe it sent something it did not.
const NOT_AVAILABLE: &str = "sending a message is not available right now";

/// Everything the TUI publishes so a guest thread can reach the turn
/// loop. One field today; a struct because [`install`] replaces
/// wholesale and a bare `Option<Sender>` argument would read as
/// "clear the sink" at the call site.
pub struct Sink {
    /// A clone of the CURRENT turn's steer sender, or `None` when no
    /// turn has ever run.
    pub steer_tx: Option<mpsc::Sender<SteerMessage>>,
}

/// A message that had nowhere live to go, waiting to start a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub plugin: String,
    pub text: String,
}

#[derive(Default)]
struct State {
    /// `None` until the TUI installs one. Distinct from
    /// `Some(Sink { steer_tx: None })`, which means "installed, but no
    /// turn has run yet" — the first refuses, the second queues.
    sink: Option<Sink>,
    /// Rule 1's state: plugins with a message that has been accepted
    /// and not yet handed to a turn.
    pending_plugins: HashSet<String>,
    /// Accepted messages with no live channel, in arrival order. Read
    /// LAST of the three follow-up sources: a human's queued line
    /// outranks a plugin's.
    overflow: VecDeque<Pending>,
    /// Verbatim echoes owed to the user for messages that DID reach a
    /// live turn. Recorded only on that branch; see the module doc.
    echoes: VecDeque<Pending>,
    /// Rule 2's state: whose message started the turn now running.
    /// `None` for a human turn.
    current_turn_origin: Option<String>,
    /// Staged origin for the turn about to start, set by
    /// [`take_pending`] and promoted by [`reset_turn`]. Two fields
    /// rather than one because the TUI's turn boundary runs after the
    /// drain and would otherwise clobber what the drain just learned.
    next_turn_origin: Option<String>,
    /// Turns each plugin has caused this session, against
    /// [`MAX_PLUGIN_TURNS_PER_SESSION`].
    turns_started: HashMap<String, usize>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn lock() -> std::sync::MutexGuard<'static, State> {
    // Recovered rather than propagated, for the reason `notify::lock`
    // gives: a poisoned cell should not disable the capability — or
    // worse, the guard — for the rest of the session.
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Publish (or replace) the sink. Idempotent, and REPLACING is the
/// point: the steer sender is created fresh for every turn, so a sink
/// installed once at startup would hold a sender whose receiver died
/// with the first turn and would push every later message into the
/// overflow queue instead of steering.
pub fn install(s: Sink) {
    lock().sink = Some(s);
}

/// Whether anything is listening at all.
pub fn is_installed() -> bool {
    lock().sink.is_some()
}

/// Forget everything. Test-only, and it clears the guard state too so
/// one test's session cap is not another's.
#[cfg(test)]
pub fn reset_all() {
    *lock() = State::default();
}

/// Submit one message on `plugin`'s behalf.
///
/// `Ok(())` means the text WILL reach a turn and WILL be echoed
/// verbatim. `Err` carries a bare reason body — the `error: ` prefix
/// belongs to `loader::send_gated`, exactly once, the same division of
/// labour `PluginStore::set` and `plugin_context::set` use.
///
/// Order inside: rule 2, then rule 1, then the session cap, then the
/// route. Rule 2 first because "you are inside your own turn" is the
/// more specific diagnosis; being told you already have a message
/// pending when the real problem is that you are looping would send an
/// author looking in the wrong place.
pub fn send(plugin: &str, text: &str) -> Result<(), String> {
    let mut st = lock();

    let Some(sink) = st.sink.as_ref() else {
        return Err(NOT_AVAILABLE.to_string());
    };
    let steer_tx = sink.steer_tx.clone();

    // §2.4 rule 2, PER PLUGIN and not globally. A global flag would be
    // easier and would break the audit-plus-rules pair of plugins the
    // spec's own motivation rests on: B must still be able to send
    // during a turn A started.
    if st.current_turn_origin.as_deref() == Some(plugin) {
        return Err(
            "this plugin cannot send during a turn its own message started".to_string(),
        );
    }

    // §2.4 rule 1, verbatim. Cleared at HAND-OFF, not here — see
    // `take_pending` / `take_echoes`. Clearing it when `send` returns
    // would make the rule decorative.
    if st.pending_plugins.contains(plugin) {
        return Err("a message from this plugin is already pending".to_string());
    }

    // The third bound. Announced, never silent (invariant 9).
    let used = st.turns_started.get(plugin).copied().unwrap_or(0);
    if used >= MAX_PLUGIN_TURNS_PER_SESSION {
        return Err(format!(
            "this plugin has started too many turns in this session \
             ({MAX_PLUGIN_TURNS_PER_SESSION})"
        ));
    }

    let entry = Pending {
        plugin: plugin.to_string(),
        text: text.to_string(),
    };

    // PREFERRED PATH FIRST. A live channel reaches the machinery
    // `b90b27f` repaired — `Steering`, the same variant a typed
    // mid-stream line takes, so a plugin's message steers the running
    // turn rather than waiting behind it. `try_send`, not `send`,
    // because this runs on a synchronous guest thread with no runtime.
    let routed = match &steer_tx {
        Some(tx) => tx
            .try_send(SteerMessage::Steering {
                text: text.to_string(),
            })
            .is_ok(),
        None => false,
    };

    if routed {
        // The echo is recorded HERE, on the success branch only. This
        // is the biconditional in one place: nothing is echoed that was
        // not sent, and nothing is sent that will not be echoed.
        st.echoes.push_back(entry);
    } else {
        // No channel, a dead receiver, or a full one. Queued rather
        // than dropped; `KeyAction::StartTurn` echoes it when it runs.
        st.overflow.push_back(entry);
    }

    st.pending_plugins.insert(plugin.to_string());
    *st.turns_started.entry(plugin.to_string()).or_insert(0) += 1;
    Ok(())
}

/// Take the next message that has nowhere live to go, if any.
///
/// Called by the TUI as the THIRD and last follow-up source. Also
/// stages the turn origin, because this is the only place that knows
/// which plugin the turn about to start belongs to.
pub fn take_pending() -> Option<Pending> {
    let mut st = lock();
    let p = st.overflow.pop_front()?;
    // Hand-off: rule 1 clears now, not when `send` returned.
    st.pending_plugins.remove(&p.plugin);
    st.next_turn_origin = Some(p.plugin.clone());
    Some(p)
}

/// Take the verbatim echoes owed for messages that reached a live turn.
///
/// Drained on the TUI's ticker, beside `notify::drain`, for the same
/// reason: the host function is a synchronous wasmtime closure with no
/// `Term`.
pub fn take_echoes() -> Vec<Pending> {
    let mut st = lock();
    let out: Vec<Pending> = st.echoes.drain(..).collect();
    for p in &out {
        // Hand-off for the steered half. The message is inside the
        // turn's channel by now; a `Steering` the turn did not reach in
        // time is DEMOTED to `pending_follow_ups` by
        // `drain_steer_to_follow_ups` rather than dropped, so it still
        // runs and the plugin is not wedged.
        st.pending_plugins.remove(&p.plugin);
    }
    out
}

/// Declare whose message the NEXT turn starts from. `None` for a human
/// turn.
///
/// Staged rather than applied, so the TUI's single turn boundary
/// ([`reset_turn`]) stays the only place the live origin changes.
pub fn mark_turn_origin(plugin: Option<&str>) {
    lock().next_turn_origin = plugin.map(str::to_string);
}

/// A new turn is starting: promote the staged origin.
///
/// Sits beside `notify::reset_turn()` at the TUI's existing turn
/// boundary rather than getting a second one of its own — two
/// boundaries drift, and the drift would be a rule-2 hole.
pub fn reset_turn() {
    let mut st = lock();
    st.current_turn_origin = st.next_turn_origin.take();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state is process-wide, so these must not interleave.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        let g = L.lock().unwrap_or_else(|e| e.into_inner());
        reset_all();
        g
    }

    /// Install a sink with a live channel and hand back the receiver,
    /// which the caller must HOLD — dropping it is what makes a send
    /// fall through to the overflow queue.
    fn live() -> mpsc::Receiver<SteerMessage> {
        let (tx, rx) = mpsc::channel(32);
        install(Sink { steer_tx: Some(tx) });
        rx
    }

    /// Invariant 9, and Q4's headless case: `-p` never installs a sink,
    /// so the call must be REFUSED rather than silently dropped.
    #[test]
    fn without_a_sink_the_call_is_refused_in_band() {
        let _g = guard();
        let err = send("p", "hi").expect_err(
            "a plugin must never believe it sent something it did not",
        );
        assert_eq!(err, NOT_AVAILABLE, "{err}");
        assert!(take_echoes().is_empty(), "and nothing was echoed either");
        assert!(take_pending().is_none());
    }

    #[test]
    fn a_message_with_a_live_turn_steers_it_and_is_echoed() {
        let _g = guard();
        let mut rx = live();
        send("p", "hello").expect("granted, unguarded, first message");
        match rx.try_recv().expect("it must reach the RUNNING turn") {
            SteerMessage::Steering { text } => assert_eq!(text, "hello"),
            other => panic!(
                "a mid-stream message steers rather than queueing behind \
                 the turn: {other:?}"
            ),
        }
        let echoes = take_echoes();
        assert_eq!(echoes.len(), 1, "{echoes:?}");
        assert_eq!(echoes[0].text, "hello", "verbatim");
        assert_eq!(echoes[0].plugin, "p", "attributed to the caller");
        assert!(
            take_pending().is_none(),
            "a steered message must not ALSO queue as a follow-up"
        );
    }

    /// The biconditional's second half. With the receiver gone the send
    /// did not happen, so no echo may be owed — the text goes to the
    /// overflow queue, where `StartTurn` echoes it exactly once.
    #[test]
    fn a_dead_receiver_queues_the_text_and_owes_no_echo() {
        let _g = guard();
        let rx = live();
        drop(rx);
        send("p", "hello").expect("queued, not refused");
        assert!(
            take_echoes().is_empty(),
            "NO ECHO WITHOUT A SEND — echoing here and then queueing is \
             the double echo, and echoing here without queueing is \
             `b90b27f`'s phantom exactly"
        );
        let p = take_pending().expect("and the text is not dropped");
        assert_eq!(p.text, "hello");
    }

    /// §2.4 rule 1, verbatim, and it must not clear when `send`
    /// returns.
    #[test]
    fn a_second_message_while_one_is_pending_is_refused() {
        let _g = guard();
        let _rx = live();
        send("p", "first").expect("first is fine");
        let err = send("p", "second").expect_err("rule 1");
        assert_eq!(err, "a message from this plugin is already pending", "{err}");
    }

    /// …and it clears at HAND-OFF, so the plugin is not wedged.
    #[test]
    fn the_pending_flag_clears_once_the_text_is_handed_to_a_turn() {
        let _g = guard();
        let _rx = live();
        send("p", "first").expect("first");
        assert!(send("p", "second").is_err(), "still pending");
        let _ = take_echoes();
        send("p", "second").expect("hand-off released rule 1");
    }

    /// The overflow half of the same rule.
    #[test]
    fn the_pending_flag_clears_when_an_overflow_message_starts_its_turn() {
        let _g = guard();
        install(Sink { steer_tx: None });
        send("p", "first").expect("queued");
        assert!(send("p", "second").is_err(), "still pending");
        take_pending().expect("drained");
        send("p", "second").expect("hand-off released rule 1");
    }

    /// §2.4 rule 2.
    #[test]
    fn a_plugin_cannot_send_during_a_turn_its_own_message_started() {
        let _g = guard();
        let _rx = live();
        mark_turn_origin(Some("p"));
        reset_turn();
        let err = send("p", "again").expect_err("rule 2");
        assert_eq!(
            err, "this plugin cannot send during a turn its own message started",
            "{err}"
        );
    }

    /// THE THING A CARELESS IMPLEMENTATION GETS WRONG. The guard is per
    /// plugin, not global: an auditing plugin must still be able to
    /// speak during a turn the rules plugin started.
    #[test]
    fn another_plugin_may_still_send_during_that_turn() {
        let _g = guard();
        let _rx = live();
        mark_turn_origin(Some("a"));
        reset_turn();
        assert!(send("a", "x").is_err(), "A is inside its own turn");
        send("b", "y").expect(
            "the guard is PER PLUGIN — a global flag would silence every \
             other plugin for the whole turn",
        );
    }

    /// A human turn clears the origin, so the plugin may speak again.
    #[test]
    fn a_human_turn_releases_rule_2() {
        let _g = guard();
        let _rx = live();
        mark_turn_origin(Some("p"));
        reset_turn();
        assert!(send("p", "x").is_err());
        mark_turn_origin(None);
        reset_turn();
        send("p", "x").expect("a human turn is not this plugin's turn");
    }

    /// The bound §2.4's two rules do not provide: a `turn_end`
    /// subscriber that sends once per turn satisfies both of them
    /// forever.
    #[test]
    fn a_turn_end_subscriber_that_sends_every_turn_is_stopped_at_the_cap() {
        let _g = guard();
        let _rx = live();
        for i in 0..MAX_PLUGIN_TURNS_PER_SESSION {
            // Each iteration is a fresh turn started by someone else,
            // and each message is consumed before the next — so rule 1
            // and rule 2 are both satisfied every single time.
            mark_turn_origin(None);
            reset_turn();
            send("looper", &format!("turn {i}")).unwrap_or_else(|e| {
                panic!("iteration {i} is within the cap: {e}")
            });
            let _ = take_echoes();
        }
        mark_turn_origin(None);
        reset_turn();
        let err = send("looper", "one more").expect_err(
            "§2.4's two rules do not bound this loop; the session cap does",
        );
        assert_eq!(
            err,
            format!(
                "this plugin has started too many turns in this session \
                 ({MAX_PLUGIN_TURNS_PER_SESSION})"
            ),
            "{err}"
        );
        // And it is the LOOPER that is stopped, not the capability.
        send("innocent", "hello").expect("the cap is per plugin");
    }

    /// A refused message costs nothing: it must not consume a turn from
    /// the cap, or a plugin could be locked out by its own refusals.
    #[test]
    fn a_refused_message_does_not_spend_the_session_budget() {
        let _g = guard();
        let _rx = live();
        for _ in 0..MAX_PLUGIN_TURNS_PER_SESSION + 5 {
            // Every one of these is refused by rule 1 after the first.
            let _ = send("p", "x");
        }
        let _ = take_echoes();
        send("p", "still allowed").expect("only accepted messages count");
    }

    /// **The biconditional itself, pinned as one property rather than
    /// as two anecdotes.**
    ///
    /// Q3 replaced §Required tests' wall-clock ordering ("echoed BEFORE
    /// it is sent") with a pair of implications, because ordering is
    /// unobservable to the user and `b90b27f` proved the ordering the
    /// spec asked for is the one that loses text. The pair is:
    ///
    ///   * no send without an echo — every accepted message is owed
    ///     exactly one rendering, either `take_echoes` (steered) or
    ///     `take_pending` → `StartTurn`'s `render_user_echo` (queued);
    ///   * no echo without a send — nothing is owed a rendering that is
    ///     not also going to reach a turn.
    ///
    /// Both directions collapse to one countable invariant: across any
    /// mixture of live and dead channels, `echoes + overflow` equals the
    /// number of `Ok` sends, EXACTLY — never fewer (a lost message) and
    /// never more (a double echo). Asserting it over an interleaving is
    /// what makes this a pin and not a restatement of the two
    /// single-case tests above.
    #[test]
    fn every_accepted_message_is_owed_exactly_one_rendering_and_no_other_is() {
        let _g = guard();
        let mut accepted = 0usize;
        let mut alive: Vec<mpsc::Receiver<SteerMessage>> = Vec::new();

        for i in 0..6 {
            // Alternate the two branches without draining in between,
            // so echoes and overflow accumulate side by side and a
            // mix-up between the queues cannot cancel itself out.
            if i % 2 == 0 {
                alive.push(live());
            } else {
                drop(live()); // installed, receiver already gone
            }
            if send(&format!("p{i}"), &format!("m{i}")).is_ok() {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 6, "each plugin sends once, so none is refused");

        let echoes = take_echoes();
        let mut queued = Vec::new();
        while let Some(p) = take_pending() {
            queued.push(p);
        }

        assert_eq!(
            echoes.len() + queued.len(),
            accepted,
            "NO SEND WITHOUT AN ECHO and NO ECHO WITHOUT A SEND. Fewer \
             renderings than accepted sends is `b90b27f` inverted — the \
             text runs and the user never sees it. More is a double \
             echo, which means one message would be rendered twice and \
             possibly run twice. echoes={echoes:?} queued={queued:?}"
        );
        // …and it is a partition, not merely the right total: no text
        // may appear on both sides.
        for e in &echoes {
            assert!(
                !queued.iter().any(|q| q.text == e.text),
                "{} is owed an echo AND queued to be echoed again by \
                 StartTurn — the same message rendered twice",
                e.text
            );
        }
    }

    /// The stale-sender bug the per-turn refresh exists to prevent.
    #[test]
    fn installing_a_fresh_sink_replaces_the_previous_turns_sender() {
        let _g = guard();
        let rx_a = live();
        drop(rx_a);
        let mut rx_b = live();
        send("p", "second turn").expect("accepted");
        assert!(
            rx_b.try_recv().is_ok(),
            "install must REPLACE — a sink kept from the first turn holds a \
             sender whose receiver is already gone, and every later message \
             would queue instead of steering"
        );
    }
}

//! Lifecycle-event subscribers supplied by something other than a shell
//! hook.
//!
//! Today that means WASM plugins, but nothing here knows about WASM —
//! and that is the point, exactly as `src/command.rs` does for plugin
//! slash commands: `src/agent/loop_.rs` and `src/mode/tui.rs` stay free
//! of `#[cfg(feature = "wasm")]`, because the vocabulary and the
//! precomputed dispatch table live here, unconditionally compiled, and
//! the plugin layer reaches them as `Arc<dyn EventHandler>`.
//!
//! The `event → subscribers` index is PRECOMPUTED at
//! [`EventSubscribers::from_subscribers`], not built per-emit, because
//! `docs/v0.12-events.md` §4.3 makes it load-bearing: an event with zero
//! subscribers must cost a slice/hashmap lookup, not a lock and not a
//! JSON serialization. [`EventSubscribers::deliver_with`] enforces that
//! by taking the payload as a closure and never calling it when nobody
//! is subscribed.

use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::hook::HookEvent;

/// Delivers one event to one subscriber. Returns nothing: observe-only
/// (`docs/v0.12-events.md` §3) is expressed in the type — there is no
/// way for an implementor to hand back a veto or a transform.
pub trait EventHandler: Send + Sync {
    fn handle_event(&self, event: &str, payload_json: &str);
}

/// One plugin's subscription: which events it was granted AND asked
/// for, and where to deliver them.
pub struct Subscriber {
    pub plugin_name: Arc<str>,
    /// PI event names, drawn from `agent::hook::EVENT_NAMES`.
    pub events: Vec<&'static str>,
    pub handler: Arc<dyn EventHandler>,
}

/// The precomputed `event → subscribers` table. `Clone` is cheap (an
/// `Arc` inside) so it can live on `Agent` and be handed around freely.
#[derive(Clone)]
pub struct EventSubscribers(Arc<HashMap<&'static str, Vec<Arc<Subscriber>>>>);

impl Default for EventSubscribers {
    fn default() -> Self {
        Self(Arc::new(HashMap::new()))
    }
}

impl EventSubscribers {
    /// Build the table from a flat list of subscriptions, indexing by
    /// event name so [`deliver_with`](Self::deliver_with) never has to
    /// scan every subscriber for every event.
    pub fn from_subscribers(subscribers: Vec<Subscriber>) -> Self {
        let mut index: HashMap<&'static str, Vec<Arc<Subscriber>>> = HashMap::new();
        for s in subscribers {
            let s = Arc::new(s);
            for &event in &s.events {
                index.entry(event).or_default().push(s.clone());
            }
        }
        Self(Arc::new(index))
    }

    /// Deliver `event` to every subscriber, building the payload at most
    /// once (§4.3: one payload, shared by every subscriber). Returns
    /// immediately, WITHOUT calling `build_payload`, when nobody is
    /// subscribed to `event` — the whole reason the index is
    /// precomputed rather than derived per-call.
    ///
    /// A subscriber's handler runs on `spawn_blocking` and is awaited
    /// (design decision D1: wasmtime's guest call is synchronous and
    /// runs to completion on the calling thread, and nanopi targets
    /// machines whose tokio runtime has one or two workers, so an inline
    /// call would stall the TUI ticker, the SSE stream, and key
    /// handling). A panicking handler (`JoinError`) is logged and
    /// delivery continues to the next subscriber — one bad plugin must
    /// not stop delivery to the rest.
    pub async fn deliver_with<F>(&self, event: HookEvent, build_payload: F)
    where
        F: FnOnce() -> String,
    {
        let name = event.pi_name();
        let Some(subs) = self.0.get(name) else {
            return;
        };
        if subs.is_empty() {
            return;
        }
        let payload = Arc::new(build_payload());
        for sub in subs {
            let sub = sub.clone();
            let payload = payload.clone();
            let event_name = name.to_string();
            let plugin_name = sub.plugin_name.clone();
            let result = tokio::task::spawn_blocking(move || {
                sub.handler.handle_event(&event_name, &payload);
            })
            .await;
            if let Err(join_err) = result {
                eprintln!(
                    "nanopi: event handler panicked [plugin={plugin_name} event={name} error={join_err}]"
                );
            }
        }
    }

    /// `(plugin_name, sorted event names)` for every subscribed plugin,
    /// sorted by plugin name for stable `/tools` output.
    pub fn subscriptions(&self) -> Vec<(String, Vec<String>)> {
        let mut by_plugin: HashMap<String, Vec<String>> = HashMap::new();
        for subs in self.0.values() {
            for s in subs {
                let entry = by_plugin.entry(s.plugin_name.to_string()).or_default();
                for &e in &s.events {
                    if !entry.iter().any(|x| x == e) {
                        entry.push(e.to_string());
                    }
                }
            }
        }
        let mut out: Vec<(String, Vec<String>)> = by_plugin.into_iter().collect();
        for (_, events) in out.iter_mut() {
            events.sort();
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// `/tools`'s "nothing is watching" case.
    pub fn is_empty(&self) -> bool {
        self.0.values().all(|v| v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountingHandler {
        calls: Arc<AtomicUsize>,
        last_payload: std::sync::Mutex<Option<String>>,
    }

    impl EventHandler for CountingHandler {
        fn handle_event(&self, _event: &str, payload_json: &str) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_payload.lock().unwrap() = Some(payload_json.to_string());
        }
    }

    fn counting() -> (Arc<CountingHandler>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(CountingHandler {
                calls: calls.clone(),
                last_payload: std::sync::Mutex::new(None),
            }),
            calls,
        )
    }

    #[tokio::test]
    async fn empty_table_does_not_call_the_builder() {
        let table = EventSubscribers::default();
        let called = Arc::new(AtomicBool::new(false));
        let flag = called.clone();
        table
            .deliver_with(HookEvent::TurnStart, move || {
                flag.store(true, Ordering::SeqCst);
                "{}".to_string()
            })
            .await;
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_subscriber_receives_only_its_event() {
        let (handler, calls) = counting();
        let table = EventSubscribers::from_subscribers(vec![Subscriber {
            plugin_name: Arc::from("watcher"),
            events: vec!["turn_start"],
            handler: handler.clone(),
        }]);

        let build_calls = Arc::new(AtomicUsize::new(0));
        let bc = build_calls.clone();
        table
            .deliver_with(HookEvent::TurnStart, move || {
                bc.fetch_add(1, Ordering::SeqCst);
                "payload".to_string()
            })
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(build_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            handler.last_payload.lock().unwrap().as_deref(),
            Some("payload")
        );

        let build_calls2 = Arc::new(AtomicUsize::new(0));
        let bc2 = build_calls2.clone();
        table
            .deliver_with(HookEvent::TurnEnd, move || {
                bc2.fetch_add(1, Ordering::SeqCst);
                "nope".to_string()
            })
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "TurnEnd must not deliver");
        assert_eq!(build_calls2.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn two_subscribers_to_the_same_event_share_one_payload_build() {
        let (h1, c1) = counting();
        let (h2, c2) = counting();
        let table = EventSubscribers::from_subscribers(vec![
            Subscriber {
                plugin_name: Arc::from("a"),
                events: vec!["turn_start"],
                handler: h1.clone(),
            },
            Subscriber {
                plugin_name: Arc::from("b"),
                events: vec!["turn_start"],
                handler: h2.clone(),
            },
        ]);
        let build_calls = Arc::new(AtomicUsize::new(0));
        let bc = build_calls.clone();
        table
            .deliver_with(HookEvent::TurnStart, move || {
                bc.fetch_add(1, Ordering::SeqCst);
                "shared".to_string()
            })
            .await;
        assert_eq!(build_calls.load(Ordering::SeqCst), 1);
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
        assert_eq!(h1.last_payload.lock().unwrap().as_deref(), Some("shared"));
        assert_eq!(h2.last_payload.lock().unwrap().as_deref(), Some("shared"));
    }

    #[test]
    fn subscriptions_are_sorted_by_plugin_and_event() {
        let (h, _) = counting();
        let table = EventSubscribers::from_subscribers(vec![Subscriber {
            plugin_name: Arc::from("zeta"),
            events: vec!["turn_end", "turn_start"],
            handler: h.clone(),
        }]);
        let subs = table.subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].0, "zeta");
        assert_eq!(subs[0].1, vec!["turn_end", "turn_start"]);
    }

    #[test]
    fn is_empty_reflects_the_table() {
        assert!(EventSubscribers::default().is_empty());
        let (h, _) = counting();
        let table = EventSubscribers::from_subscribers(vec![Subscriber {
            plugin_name: Arc::from("a"),
            events: vec!["turn_start"],
            handler: h,
        }]);
        assert!(!table.is_empty());
    }

    struct PanickingHandler;
    impl EventHandler for PanickingHandler {
        fn handle_event(&self, _event: &str, _payload_json: &str) {
            panic!("boom");
        }
    }

    #[tokio::test]
    async fn a_panicking_handler_does_not_stop_delivery_to_the_next_subscriber() {
        let (h2, c2) = counting();
        let table = EventSubscribers::from_subscribers(vec![
            Subscriber {
                plugin_name: Arc::from("bad"),
                events: vec!["turn_start"],
                handler: Arc::new(PanickingHandler),
            },
            Subscriber {
                plugin_name: Arc::from("good"),
                events: vec!["turn_start"],
                handler: h2,
            },
        ]);
        table
            .deliver_with(HookEvent::TurnStart, || "{}".to_string())
            .await;
        assert_eq!(c2.load(Ordering::SeqCst), 1, "the healthy subscriber must still run");
    }
}

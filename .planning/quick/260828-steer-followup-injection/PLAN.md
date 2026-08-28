---
gsd_quick: true
task: "Steer / follow-up message injection into a running agent turn"
branch: v0.11.0
created: 2026-08-28
status: in-progress
---

## Goal

Let the user inject a new message while the agent is mid-turn (steer)
or queue a message for after the current turn completes (follow-up).
Matches Pi's `getSteeringMessages()` / `getFollowUpMessages()`.

## Design

nanopi's agent loop is async. The natural mechanism is a second
`mpsc::Receiver<SteerMessage>` channel alongside the existing
`AgentEvent` channel that feeds the TUI/renderer.

### Data flow

```
TUI / -p mode
  │
  ├─ AgentEvent stream (existing) ──→ renderer
  │
  └─ SteerChannel (new)            ──→ run_turn
       │
       ├─ SteerMessage::Inject { text }
       │    pushed into context.messages as User message
       │    at the next iteration boundary
       │
       └─ SteerMessage::FollowUp { text }
            queued; run_turn sees it after the for-loop
            and re-enters the loop with the queued text
```

### Types

```rust
pub enum SteerMessage {
    /// Inject a user message at the next iteration boundary.
    /// Equivalent to Pi's `getSteeringMessages()`.
    Inject { text: String },
    /// Queue a message for after the current turn ends.
    /// Equivalent to Pi's `getFollowUpMessages()`.
    FollowUp { text: String },
}
```

### Changes

| File | Change |
|------|--------|
| `src/event.rs` | Add `SteerMessage` enum |
| `src/agent/loop_.rs` | `run_turn` takes `Option<mpsc::Receiver<SteerMessage>>`; checks at each iteration boundary + after the for-loop |
| `src/mode/tui.rs` | Wire a `mpsc::channel` for steer; TUI keyboard handler sends `SteerMessage::Inject` on some key (e.g. Ctrl+S or a /steer command) |
| `src/mode/print.rs` | Ignore steer channel (non-interactive) |

### run_turn changes (conceptual)

```rust
// In the for loop, after finish_reason match + TurnEnd hook:
if let Some(steer_rx) = steer_rx.as_mut() {
    while let Ok(msg) = steer_rx.try_recv() {
        match msg {
            SteerMessage::Inject { text } => {
                // Push as user message into context
                self.context.push_user_text(&text);
                session::append_entry(...)?;
                // Don't break the loop — the next iteration's
                // stream_turn will include this in the LLM call.
            }
            SteerMessage::FollowUp { text } => {
                follow_up_queue.push(text);
            }
        }
    }
}

// After the for-loop, before MessageEnd hook:
if !follow_up_queue.is_empty() {
    let next_msg = follow_up_queue.remove(0);
    // Re-enter the loop? Or just return and let the caller
    // re-invoke run_turn? The latter is simpler and more composable.
    // Store the follow-up text on self and let the TUI's next
    // prompt pre-fill.
}
```

## Acceptance

- `cargo test --all-targets -- --test-threads=1` passes
- TUI can send a steer message mid-turn (verified by a unit test with a
  SteppedProvider that emits 2 iterations)
- Follow-up text is returned to the caller for re-invocation
- No new dependencies

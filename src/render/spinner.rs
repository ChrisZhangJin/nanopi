//! Braille spinner for "still working" feedback during long LLM turns.
//!
//! Design (borrowing from PI's `packages/tui/src/components/loader.ts`
//! and Claude Code's `Spinner.tsx`):
//!   - 10-frame braille rotation, ~120ms per frame
//!   - Renders to stderr with `\r\x1b[K` (carriage return + clear-to-EOL)
//!     so it stays on one line and doesn't pollute stdout
//!   - Auto-hides on first sign of real output (TextDelta / ToolCall /
//!     Error) — the presence of activity IS the reassurance
//!   - Cheap: a single tokio::spawn task, cancelled via CancellationToken
//!
//! Terminals that don't understand `\r\x1b[K` (rare; every modern one
//! does) just see extra whitespace, no crash.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FRAME_MS: u64 = 120;

/// Handle to a running spinner task. Drop or `.stop()` to erase the
/// line and let the task exit.
pub struct Spinner {
    handle: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl Spinner {
    /// Start a spinner in the background. `label` shows next to the
    /// spinning glyph — e.g. "thinking", "running bash". Spinner
    /// begins immediately; a first update lands within `FRAME_MS`.
    pub fn start(label: impl Into<String>) -> Self {
        let label = label.into();
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            let mut frame_idx: usize = 0;
            loop {
                if cancel_task.is_cancelled() {
                    break;
                }
                let elapsed = started.elapsed();
                let frame = FRAMES[frame_idx % FRAMES.len()];
                let secs = elapsed.as_secs_f64();
                // \r → column 0, \x1b[K → clear from cursor to EOL.
                let _ = write!(io::stderr(), "\r\x1b[K{} {} ({:.1}s)", frame, label, secs);
                let _ = io::stderr().flush();
                frame_idx = frame_idx.wrapping_add(1);
                // Cancellable sleep so `.stop()` responds within ~10ms.
                let sleep = tokio::time::sleep(Duration::from_millis(FRAME_MS));
                tokio::select! {
                    _ = cancel_task.cancelled() => break,
                    _ = sleep => {}
                }
            }
            // Erase the spinner line on exit.
            let _ = write!(io::stderr(), "\r\x1b[K");
            let _ = io::stderr().flush();
        });
        Self {
            handle: Some(handle),
            cancel,
        }
    }

    /// Cancel the spinner and wait briefly for it to erase the line.
    /// Idempotent.
    pub async fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for Spinner {
    /// Best-effort cancel on drop. The task's own final `\r\x1b[K` may
    /// not have flushed yet if we're dropped without awaiting — call
    /// `.stop().await` for a clean line clear.
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        let _ = write!(io::stderr(), "\r\x1b[K");
        let _ = io::stderr().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spinner_starts_and_stops_cleanly() {
        let mut s = Spinner::start("test");
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.stop().await;
        // Nothing to assert — we're proving it doesn't panic and
        // completes within a bounded time.
    }

    #[tokio::test]
    async fn dropped_spinner_doesnt_leak() {
        {
            let _s = Spinner::start("drop-test");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Give the aborted task a moment to unwind.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

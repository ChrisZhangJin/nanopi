//! Live progress of the Dream run in this process, for the TUI's panel.
//!
//! Dream runs on a background task while the conversation goes on; the
//! TUI reads this on its tick to show what it is doing. Process-global
//! because only one Dream runs at a time (the store's `.dream.lock`).

use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Local maintenance and reading the notes.
    Preparing,
    /// Waiting on the model (call `calls` of at most `max_calls`).
    Asking,
    /// Writing the model's changes into the library.
    Applying,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub model: String,
    pub range: String,
    pub started: Instant,
    pub phase: Phase,
    /// Notes this run will send to the model.
    pub notes_total: usize,
    pub notes_done: usize,
    pub calls: usize,
    pub max_calls: usize,
    pub changes: usize,
}

impl Progress {
    /// 0.0 ..= 1.0 by notes processed; `None` before there is a basis.
    pub fn fraction(&self) -> Option<f64> {
        (self.notes_total > 0).then(|| (self.notes_done as f64 / self.notes_total as f64).min(1.0))
    }
}

static STATE: Mutex<Option<Progress>> = Mutex::new(None);

fn state() -> std::sync::MutexGuard<'static, Option<Progress>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clears the progress when the run ends, however it ends.
#[must_use = "dropping the guard ends the progress"]
pub struct RunGuard(());

impl Drop for RunGuard {
    fn drop(&mut self) {
        *state() = None;
    }
}

pub fn begin(model: &str, range: &str, max_calls: usize) -> RunGuard {
    *state() = Some(Progress {
        model: model.to_string(),
        range: range.to_string(),
        started: Instant::now(),
        phase: Phase::Preparing,
        notes_total: 0,
        notes_done: 0,
        calls: 0,
        max_calls,
        changes: 0,
    });
    RunGuard(())
}

fn update(f: impl FnOnce(&mut Progress)) {
    if let Some(p) = state().as_mut() {
        f(p);
    }
}

pub fn notes(total: usize) {
    update(|p| p.notes_total = total);
}

pub fn asking(call: usize) {
    update(|p| {
        p.phase = Phase::Asking;
        p.calls = call;
    });
}

pub fn applying() {
    update(|p| p.phase = Phase::Applying);
}

pub fn applied(notes_done: usize, changes: usize) {
    update(|p| {
        p.notes_done = notes_done;
        p.changes = changes;
    });
}

pub fn snapshot() -> Option<Progress> {
    state().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_reports_progress_and_clears_when_it_ends() {
        let _g = crate::test_lock();
        assert!(snapshot().is_none());
        {
            let _run = begin("m", "unprocessed notes", 3);
            notes(4);
            asking(1);
            let p = snapshot().unwrap();
            assert_eq!((p.phase, p.calls, p.notes_total), (Phase::Asking, 1, 4));
            assert_eq!(p.fraction(), Some(0.0));
            applying();
            applied(2, 3);
            let p = snapshot().unwrap();
            assert_eq!((p.phase, p.changes, p.fraction()), (Phase::Applying, 3, Some(0.5)));
        }
        assert!(snapshot().is_none(), "cleared however the run ends");
    }
}

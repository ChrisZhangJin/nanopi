//! Live progress of the one compaction pass that can run in this process.
//!
//! Compaction asks the model for a summary in a fixed format of nine
//! headed sections (see `SUMMARIZER_SYSTEM`), so progress is real, not a
//! guessed clock: the bar advances by the section the model is writing,
//! and smoothly within a section by the characters streamed since its
//! heading. Before the first token arrives the pass is `Waiting`, and the
//! UI shows an indeterminate sweep instead of a number.
//!
//! The state is process-global because the summary streams deep inside
//! `compact()`, while the TUI reads it on its own tick. Only the main
//! agent compacts inside a TUI process; agents compact in their own.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Sections in the summary format, in order.
pub const SECTIONS: [&str; 9] = [
    "Goal",
    "Constraints & Preferences",
    "Progress",
    "Done",
    "In Progress",
    "Blocked",
    "Key Decisions",
    "Next Steps",
    "Critical Context",
];

/// Characters of a section counted as "most of it written" for the
/// within-section creep.
const SECTION_CHARS: f64 = 700.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Sent to the model; nothing streamed back yet.
    Waiting,
    /// The summary is streaming.
    Writing,
    /// Summary done; the context is being rebuilt and saved.
    Rebuilding,
    /// Finished. `chars_after` / `used_llm` are set.
    Done,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub reason: String,
    pub phase: Phase,
    pub started: Instant,
    pub messages: usize,
    pub chars_before: usize,
    /// Characters of summary streamed so far.
    pub streamed: usize,
    /// Index into [`SECTIONS`] of the section being written, if any yet.
    pub section: Option<usize>,
    /// Characters streamed since the current section's heading.
    pub in_section: usize,
    pub chars_after: Option<usize>,
    pub used_llm: bool,
    pub finished: Option<Instant>,
    /// The streamed text not yet ended by a newline (heading detection).
    partial: String,
}

impl Progress {
    pub fn new(reason: &str, messages: usize, chars_before: usize, now: Instant) -> Self {
        Self {
            reason: reason.to_string(),
            phase: Phase::Waiting,
            started: now,
            messages,
            chars_before,
            streamed: 0,
            section: None,
            in_section: 0,
            chars_after: None,
            used_llm: false,
            finished: None,
            partial: String::new(),
        }
    }

    /// Feed streamed summary text.
    pub fn push(&mut self, text: &str) {
        if matches!(self.phase, Phase::Waiting) {
            self.phase = Phase::Writing;
        }
        self.streamed += text.chars().count();
        self.partial.push_str(text);
        while let Some(i) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=i).collect();
            self.line(line.trim());
        }
        // A heading is complete once its line is; meanwhile the partial
        // line counts toward the current section.
        self.in_section += text.chars().count();
    }

    fn line(&mut self, line: &str) {
        let Some(title) = line.strip_prefix("### ").or_else(|| line.strip_prefix("## ")) else {
            return;
        };
        let title = title.trim();
        // Only move forward: a later section never moves the bar back.
        let from = self.section.map_or(0, |s| s + 1);
        if let Some(k) = SECTIONS[from.min(SECTIONS.len())..].iter().position(|s| s.eq_ignore_ascii_case(title)) {
            self.section = Some(from + k);
            self.in_section = 0;
        }
    }

    /// 0.0 ..= 1.0, or `None` while there is no basis for a number yet.
    pub fn fraction(&self) -> Option<f64> {
        match self.phase {
            Phase::Waiting => None,
            Phase::Writing => {
                let n = SECTIONS.len() as f64;
                let done = self.section.map_or(0.0, |s| s as f64);
                let creep = (self.in_section as f64 / SECTION_CHARS).min(0.9);
                // Writing spans 5% .. 95%; rebuilding takes the rest.
                Some(0.05 + 0.90 * ((done + creep) / n).min(0.99))
            }
            Phase::Rebuilding => Some(0.96),
            Phase::Done => Some(1.0),
        }
    }

    pub fn section_name(&self) -> Option<&'static str> {
        self.section.map(|s| SECTIONS[s])
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.finished.unwrap_or(now).saturating_duration_since(self.started)
    }
}

/// How long a finished pass stays visible (the completion flash).
pub const LINGER: Duration = Duration::from_millis(1500);

static STATE: Mutex<Option<Progress>> = Mutex::new(None);

fn state() -> std::sync::MutexGuard<'static, Option<Progress>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends a pass that never reached [`finish`]: the compaction future was
/// dropped (the turn was cancelled) or returned early. Without it the
/// UI would hold new input forever.
#[must_use = "dropping the guard abandons the pass"]
pub struct PassGuard(());

impl Drop for PassGuard {
    fn drop(&mut self) {
        if running() {
            abandon();
        }
    }
}

/// A pass starts (after the boundary check found something to do).
/// Keep the guard until the pass is over.
pub fn begin(reason: &str, messages: usize, chars_before: usize) -> PassGuard {
    *state() = Some(Progress::new(reason, messages, chars_before, Instant::now()));
    PassGuard(())
}

/// Summary text streamed back. Ignored when no pass is running.
pub fn push(text: &str) {
    if let Some(p) = state().as_mut().filter(|p| p.finished.is_none()) {
        p.push(text);
    }
}

pub fn rebuilding() {
    if let Some(p) = state().as_mut().filter(|p| p.finished.is_none()) {
        p.phase = Phase::Rebuilding;
    }
}

pub fn finish(chars_after: usize, used_llm: bool) {
    if let Some(p) = state().as_mut() {
        p.phase = Phase::Done;
        p.chars_after = Some(chars_after);
        p.used_llm = used_llm;
        p.finished = Some(Instant::now());
    }
}

/// The pass did not run after all (nothing to compact, or it failed).
pub fn abandon() {
    *state() = None;
}

/// What to show now: the running pass, or a finished one for [`LINGER`].
pub fn snapshot(now: Instant) -> Option<Progress> {
    let mut s = state();
    if s.as_ref().and_then(|p| p.finished).is_some_and(|t| now.saturating_duration_since(t) > LINGER) {
        *s = None;
    }
    s.clone()
}

/// A pass is running (not merely lingering after it finished).
pub fn running() -> bool {
    state().as_ref().is_some_and(|p| p.finished.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Progress {
        Progress::new("manual", 186, 412_000, Instant::now())
    }

    #[test]
    fn waiting_has_no_number_until_the_first_token() {
        let mut p = p();
        assert_eq!(p.fraction(), None);
        p.push("## Go");
        assert_eq!(p.phase, Phase::Writing);
        assert!(p.fraction().unwrap() >= 0.05);
    }

    #[test]
    fn progress_follows_the_sections_and_only_moves_forward() {
        let mut p = p();
        let mut last = 0.0;
        for (i, name) in SECTIONS.iter().enumerate() {
            let hashes = if matches!(*name, "Done" | "In Progress" | "Blocked") { "###" } else { "##" };
            // Headings can arrive split across stream chunks.
            p.push(&format!("{hashes} {}", &name[..1]));
            p.push(&format!("{}\n- a point about it\n", &name[1..]));
            assert_eq!(p.section_name(), Some(*name), "after heading {i}");
            let f = p.fraction().unwrap();
            assert!(f > last, "{name}: {f} > {last}");
            last = f;
        }
        assert!(last < 0.96, "writing never claims the rebuild's share: {last}");
        // A repeated or earlier heading never moves the bar back.
        p.push("## Goal\n");
        assert_eq!(p.section_name(), Some("Critical Context"));
        // Unknown headings and body text are not sections.
        let mut q = self::p();
        q.push("## Something Else\nsome text\n");
        assert_eq!(q.section, None);
    }

    #[test]
    fn creep_within_a_section_is_bounded_by_the_next() {
        let mut p = p();
        p.push("## Goal\n");
        let start = p.fraction().unwrap();
        p.push(&"x".repeat(100_000));
        let crept = p.fraction().unwrap();
        p.push("\n## Constraints & Preferences\n");
        assert!(start < crept && crept < p.fraction().unwrap());
    }

    #[test]
    fn global_pass_lifecycle_and_linger() {
        let _g = crate::test_lock();
        abandon();
        push("ignored: no pass");
        assert!(snapshot(Instant::now()).is_none());
        let pass = begin("threshold", 10, 1000);
        assert!(running());
        push("## Goal\nx");
        assert_eq!(snapshot(Instant::now()).unwrap().section_name(), Some("Goal"));
        rebuilding();
        assert_eq!(snapshot(Instant::now()).unwrap().fraction(), Some(0.96));
        finish(300, true);
        assert!(!running(), "finished: input is released at once");
        let s = snapshot(Instant::now()).unwrap();
        assert_eq!((s.phase, s.chars_after), (Phase::Done, Some(300)));
        drop(pass);
        assert!(snapshot(Instant::now()).is_some(), "a finished pass still flashes");
        assert!(snapshot(Instant::now() + LINGER + Duration::from_millis(10)).is_none(), "gone after the flash");
        // A pass whose future is dropped (turn cancelled) releases input.
        let pass = begin("manual", 1, 1);
        push("## Goal\n");
        drop(pass);
        assert!(!running());
        assert!(snapshot(Instant::now()).is_none());
    }
}

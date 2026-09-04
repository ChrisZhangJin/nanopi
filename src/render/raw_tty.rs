//! Stderr notices that survive raw mode.
//!
//! While the TUI is up the terminal is in raw mode, where `\n` is a
//! LINE FEED and nothing else: the cursor drops one row and keeps its
//! column. A plain `eprintln!` from anywhere in the program therefore
//! staircases, each line starting where the previous one ended:
//!
//! ```text
//! [wasm:trace] events-plugin: observed input
//!                                           [wasm:trace] events-plugin: observed turn_start
//! ```
//!
//! Writing `\r\n` instead puts every line back at column 0. That is all
//! this module does, and it does it in one place because the writers
//! are scattered — plugin `host-log`, provider retry notices, hook
//! diagnostics, extension load warnings on `/new` — and each of them is
//! reachable both before the TUI starts (where `\n` is correct and a
//! stray `\r` is harmless) and while it is running.
//!
//! What this does NOT fix: these notices bypass ratatui's
//! `insert_before`, so they land in the region it manages and are wiped
//! on the next redraw. Routing them into scrollback needs a channel
//! from the writer to the TUI loop, which `host-log` — a `func_wrap`
//! closure holding only `PluginState` — has no way to reach today. The
//! line being legible while it is on screen is the part worth having
//! now; see `docs/v0.12-manual-test-plan.md` T4.7.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// True while the terminal is in raw mode (the TUI owns the screen).
static RAW: AtomicBool = AtomicBool::new(false);

/// Called by the TUI on setup and teardown. Anything else reading this
/// is asking "will a bare \n staircase right now?".
pub fn set_raw_mode(on: bool) {
    RAW.store(on, Ordering::Relaxed);
}

pub fn is_raw_mode() -> bool {
    RAW.load(Ordering::Relaxed)
}

/// Write one notice to stderr, line-terminated correctly for whichever
/// mode the terminal is in. Embedded newlines are translated too — a
/// multi-line hook warning staircases just as readily as two separate
/// ones.
///
/// Failures are ignored, deliberately: this is a diagnostic path, and a
/// closed stderr (`nanopi … 2>&-`, or a pipe whose reader exited) must
/// not take down the run that was working. That is the same reason the
/// hook layer stopped propagating EPIPE.
pub fn note(msg: &str) {
    let mut err = std::io::stderr().lock();
    let _ = if is_raw_mode() {
        write!(err, "{}\r\n", crlf(msg))
    } else {
        writeln!(err, "{msg}")
    };
    let _ = err.flush();
}

/// Every line break in `msg` as CRLF, whatever it started as. Normalize
/// existing CRLFs down to LF first — otherwise a caller that already
/// hand-wrote `\r\n` gets `\r\r\n`, which some terminals render as a
/// blank row.
fn crlf(msg: &str) -> String {
    msg.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// `eprintln!`, but raw-mode aware. Same formatting arguments.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {
        $crate::render::raw_tty::note(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state is global, so these run as one test rather than
    /// racing each other through `cargo test`'s thread pool.
    #[test]
    fn raw_mode_flag_round_trips() {
        let before = is_raw_mode();
        set_raw_mode(true);
        assert!(is_raw_mode());
        set_raw_mode(false);
        assert!(!is_raw_mode());
        set_raw_mode(before);
    }

    /// The translation itself, tested on the string rather than on
    /// stderr — the point is that every line break carries a `\r`,
    /// including ones inside the message.
    #[test]
    fn every_newline_gains_a_carriage_return() {
        assert_eq!(crlf("first\nsecond\nthird"), "first\r\nsecond\r\nthird");
    }

    /// A caller that already hand-wrote CRLF must not end up with
    /// `\r\r\n`, which some terminals render as an extra blank row.
    #[test]
    fn an_existing_crlf_is_not_doubled() {
        assert_eq!(crlf("a\r\nb"), "a\r\nb");
        assert_eq!(crlf("a\r\nb\nc"), "a\r\nb\r\nc");
    }

    /// Nothing to translate is the common case — a single-line notice.
    #[test]
    fn a_single_line_message_is_untouched() {
        assert_eq!(crlf("plain"), "plain");
        assert_eq!(crlf(""), "");
    }
}

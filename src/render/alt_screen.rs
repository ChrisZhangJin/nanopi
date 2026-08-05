//! Alt-screen lifecycle: EnterAlternateScreen on init, LeaveAlternateScreen
//! on drop. RAII via Drop so we always restore the user's terminal.

use std::io::{self, Write};

use crossterm::{ExecutableCommand, QueueableCommand, cursor, queue, terminal};

/// RAII guard that puts the terminal into alt-screen on construction
/// and restores it on drop (or explicit `leave()`).
pub struct AltScreen {
    /// stdout we're writing to
    stdout: io::Stdout,
    /// true if we successfully entered the alt-screen and haven't left yet
    entered: bool,
}

impl AltScreen {
    /// Enter alt-screen. Also hides the cursor (common companion).
    /// Returns the guard; on Drop the screen is restored.
    pub fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        stdout
            .execute(terminal::EnterAlternateScreen)?
            .execute(terminal::Clear(terminal::ClearType::All))?
            .execute(cursor::Hide)?;
        Ok(Self { stdout, entered: true })
    }

    /// Restore the main screen early. Idempotent. After this, Drop is a no-op.
    pub fn leave(&mut self) -> io::Result<()> {
        if self.entered {
            self.stdout
                .execute(cursor::Show)?
                .execute(terminal::LeaveAlternateScreen)?;
            self.entered = false;
        }
        Ok(())
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        // Best-effort restore. We can't return an error from Drop.
        if self.entered {
            let _ = self.stdout
                .queue(cursor::Show)
                .and_then(|q| q.queue(terminal::LeaveAlternateScreen))
                .and_then(|q| q.flush());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AltScreen::enter()` must not panic and must succeed even in
    /// environments where the terminal supports the alt-screen sequence.
    /// In a non-TTY test runner (e.g. cargo test piped), `execute()`
    /// returns Ok(()) without actually emitting bytes.
    #[test]
    fn enter_returns_ok_and_leave_is_idempotent() {
        let mut screen = AltScreen::enter().expect("enter");
        // Second leave is a no-op (entered == false).
        screen.leave().expect("first leave");
        screen.leave().expect("second leave is no-op");
        // Drop is also a no-op after explicit leave.
    }
}
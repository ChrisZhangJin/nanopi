//! Process signals that end the TUI: SIGTERM (`kill`, `docker stop`,
//! `systemctl stop`) and SIGHUP (the terminal went away: window closed,
//! ssh dropped).
//!
//! These are not handled here; they are turned into an exit *code* that
//! `run_app` hands back exactly as it does for `/quit`, so a signalled
//! exit walks the one shutdown path in `run_tui_mode` (restore the
//! terminal, stop agents, end MCP sessions) rather than a copy of it.
//!
//! SIGINT is deliberately absent: in raw mode Ctrl-C is a key, not a
//! signal, and a `kill -INT` keeps its default disposition.
//!
//! Off unix there is nothing to listen for and every method is inert.

/// Shell convention: 128 + signal number.
#[cfg(unix)]
pub const EXIT_SIGTERM: i32 = 128 + 15;
#[cfg(unix)]
pub const EXIT_SIGHUP: i32 = 128 + 1;

/// Listeners for the signals above. Installing them replaces the default
/// disposition (terminate), so install them only once something is there
/// to act on them, and keep one value for the whole session: a signal that
/// arrives while it is being acted on stays queued for the next `recv`,
/// which is how a *second* signal during shutdown is noticed.
pub struct ExitSignals {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hup: Option<tokio::signal::unix::Signal>,
}

impl ExitSignals {
    /// Best effort: a signal that cannot be listened for keeps its
    /// default disposition, and `recv` never resolves for it.
    pub fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Self {
                term: signal(SignalKind::terminate()).ok(),
                hup: signal(SignalKind::hangup()).ok(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// Resolve with the exit code for the next signal. Cancel-safe.
    pub async fn recv(&mut self) -> i32 {
        #[cfg(unix)]
        {
            async fn one(s: &mut Option<tokio::signal::unix::Signal>, code: i32) -> i32 {
                if let Some(s) = s {
                    if s.recv().await.is_some() {
                        return code;
                    }
                }
                std::future::pending().await
            }
            tokio::select! {
                code = one(&mut self.term, EXIT_SIGTERM) => code,
                code = one(&mut self.hup, EXIT_SIGHUP) => code,
            }
        }
        #[cfg(not(unix))]
        {
            std::future::pending().await
        }
    }

    /// The exit code for a signal that has already arrived, without
    /// waiting for one.
    pub fn try_recv(&mut self) -> Option<i32> {
        use futures_util::FutureExt;
        self.recv().now_or_never()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn raise(sig: libc::c_int) {
        // SAFETY: a plain syscall on our own pid; a handler is installed.
        assert_eq!(unsafe { libc::kill(libc::getpid(), sig) }, 0);
    }

    /// One test, because the handlers are process-wide: a second test
    /// raising a signal would wake this one's listeners too.
    #[tokio::test]
    async fn sigterm_and_sighup_map_to_shell_exit_codes() {
        let mut sigs = ExitSignals::install();
        assert_eq!(sigs.try_recv(), None, "nothing pending yet");

        raise(libc::SIGTERM);
        assert_eq!(sigs.recv().await, 143);
        assert_eq!(sigs.try_recv(), None, "one signal is delivered once");

        raise(libc::SIGHUP);
        assert_eq!(sigs.recv().await, 129);

        // Signals that arrive while the first is being acted on queue up:
        // this is what a second signal during shutdown looks like.
        raise(libc::SIGTERM);
        assert_eq!(sigs.recv().await, 143);
        raise(libc::SIGTERM);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), sigs.recv())
                .await
                .ok(),
            Some(143)
        );
    }
}

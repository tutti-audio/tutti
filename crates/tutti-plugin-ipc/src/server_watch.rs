//! [`ServerWatch`]: whether a plugin server is alive, and why not.
//!
//! One latch answers for the whole host side of a served plugin. Every way of
//! learning that the server is gone feeds it, and every reader reads it:
//!
//! - **the control stream** ended or failed (the bridge thread reads an EOF, an
//!   I/O error, a timeout or a protocol violation, or never connected);
//! - **the process** ended (`ProcessGuard`'s exit thread, woken by the OS, so a
//!   server killed while another process still holds its socket open is seen
//!   at once and not when a timeout expires);
//! - **the host** ended it (`Health::terminate`, the session's verdict from its
//!   `Liveness` debounce).
//!
//! The first signal wins and keeps its cause: a stream's death cascades into
//! follow-on errors, and the first is the diagnosis.

use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_plugin_types::PluginFailure;

/// The latch for "this server is gone": set once, with the cause that set it.
///
/// Reads are wait-free ([`is_dead`](Self::is_dead)), so the audio thread asks
/// it every block; the cause behind it is read under a lock off the audio
/// thread.
#[derive(Debug, Default)]
pub struct ServerWatch {
    dead: AtomicBool,
    cause: Mutex<Option<PluginFailure>>,
}

impl ServerWatch {
    /// A watch on a server believed alive.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that the server is gone, for `cause`. `true` when this call set
    /// the latch; `false` when it was set already, in which case the first
    /// cause stands.
    pub fn declare(&self, cause: PluginFailure) -> bool {
        let mut slot = self.cause.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return false;
        }
        *slot = Some(cause);
        // After the cause: a reader that sees the flag finds the reason.
        self.dead.store(true, Ordering::Release);
        true
    }

    /// Whether the server is gone. Wait-free: no lock, no allocation.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Why the server is gone, once it is. A dead server always has a cause:
    /// every [`declare`](Self::declare) passes one.
    #[must_use]
    pub fn cause(&self) -> Option<PluginFailure> {
        self.cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::TerminationReason;

    /// A server believed alive has no cause, and the first signal sets the
    /// latch with its cause; the second is refused and changes nothing.
    #[test]
    fn the_first_signal_sets_the_latch_once_with_its_cause() {
        let watch = ServerWatch::new();
        assert!(!watch.is_dead());
        assert!(watch.cause().is_none());

        assert!(watch.declare(PluginFailure::StreamClosed), "the first sets");
        assert!(watch.is_dead());
        assert!(matches!(watch.cause(), Some(PluginFailure::StreamClosed)));

        assert!(
            !watch.declare(PluginFailure::Unreported),
            "a later signal does not set it again"
        );
        assert!(
            matches!(watch.cause(), Some(PluginFailure::StreamClosed)),
            "the follow-on does not replace the fault"
        );
    }

    /// Of signals racing from several threads exactly one sets the latch, and
    /// the cause read back is that one's.
    #[test]
    fn racing_signals_set_it_exactly_once() {
        for _ in 0..50 {
            let watch = std::sync::Arc::new(ServerWatch::new());
            let winners: usize = (0..4)
                .map(|n| {
                    let watch = std::sync::Arc::clone(&watch);
                    std::thread::spawn(move || {
                        let cause = if n % 2 == 0 {
                            PluginFailure::StreamClosed
                        } else {
                            PluginFailure::Terminated {
                                reason: TerminationReason::Hung {
                                    blocks: 1,
                                    held: std::time::Duration::ZERO,
                                },
                            }
                        };
                        usize::from(watch.declare(cause))
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|t| t.join().expect("no panic"))
                .sum();
            assert_eq!(winners, 1);
            assert!(watch.is_dead() && watch.cause().is_some());
        }
    }
}

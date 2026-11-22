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

use crate::Wake;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tutti_plugin_types::PluginFailure;
use tutti_rt::{Mutex, Tally, TallyWriter, tally};

/// The latch for "this server is gone": set once, with the cause that set it.
///
/// Reads are wait-free ([`is_dead`](Self::is_dead)), so the audio thread asks
/// it every block; the cause behind it is read under a lock off the audio
/// thread.
#[derive(Debug)]
pub struct ServerWatch {
    dead: AtomicBool,
    cause: Mutex<Option<PluginFailure>>,
    /// Notified when [`declare`](Self::declare) sets the latch, and shared with
    /// whoever else waits on the server ([`wake`](Self::wake)).
    wake: Arc<Wake>,
    /// What [`declare`](Self::declare) calls once it has set the latch: the
    /// rings of the threads that wait on the server.
    wakers: Mutex<Wakers>,
    /// Tasks of this server's host side that did not stop within their
    /// patience and were left running ([`note_detached`](Self::note_detached)).
    detached: TallyWriter<u64>,
}

impl Default for ServerWatch {
    fn default() -> Self {
        Self {
            dead: AtomicBool::default(),
            cause: Mutex::default(),
            wake: Arc::default(),
            wakers: Mutex::default(),
            detached: tally::<u64>().writer(),
        }
    }
}

/// The calls a declaration makes ([`ServerWatch::on_declared`]).
#[derive(Default)]
struct Wakers(Vec<Box<dyn Fn() + Send + Sync>>);

impl std::fmt::Debug for Wakers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Wakers").field(&self.0.len()).finish()
    }
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
        let mut slot = self.cause.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(cause);
        // After the cause: a reader that sees the flag finds the reason.
        self.dead.store(true, Ordering::Release);
        drop(slot);
        self.wake.notify();
        for wake in &self.wakers.lock().0 {
            wake();
        }
        true
    }

    /// Has `wake` called when the latch is set, or at once when it is set
    /// already: how a thread that waits on a descriptor or a bell, which
    /// [`wake`](Self::wake)'s gate does not reach, hears the server's end.
    /// Called on whichever thread declares, so it takes no lock and does not wait.
    pub(crate) fn on_declared(&self, wake: impl Fn() + Send + Sync + 'static) {
        let mut wakers = self.wakers.lock();
        wakers.0.push(Box::new(wake));
        // A declaration that landed before the registration called nothing.
        if self.is_dead()
            && let Some(wake) = wakers.0.last()
        {
            wake();
        }
    }

    /// The gate that waits on this server notify: [`declare`](Self::declare)
    /// notifies it, and a thread waiting on the server's end or on its replies
    /// shares it.
    #[must_use]
    pub(crate) fn wake(&self) -> &Arc<Wake> {
        &self.wake
    }

    /// Waits up to `timeout` for the server to be gone, and wakes at the
    /// declaration. `true` when it is gone.
    pub fn wait_dead(&self, timeout: Duration) -> bool {
        let deadline = Wake::deadline_in(timeout);
        self.wake.wait_until(deadline, || self.is_dead())
    }

    /// Counts a host-side task of this server (a bridge lane, the exit task)
    /// that did not stop within its patience and was left to end on its own.
    pub(crate) fn note_detached(&self) {
        self.detached.add(1);
    }

    /// The count of [`note_detached`](Self::note_detached), for the pipeline's
    /// diagnostics.
    #[must_use]
    pub(crate) fn detached_cell(&self) -> Tally<u64> {
        self.detached.tally()
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
        self.cause.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::TerminationReason;

    /// A wait on a live server ends when the server is declared gone, long
    /// before its timeout, and a wait on a gone one returns at once.
    #[test]
    fn a_wait_ends_at_the_declaration() {
        let watch = Arc::new(ServerWatch::new());
        let waiter = {
            let watch = Arc::clone(&watch);
            std::thread::spawn(move || watch.wait_dead(Duration::from_secs(60)))
        };
        assert!(watch.declare(PluginFailure::StreamClosed));
        assert!(waiter.join().expect("no panic"), "woken by the declaration");
        assert!(watch.wait_dead(Duration::ZERO), "a gone server is gone");
    }

    /// A wait that no one ends reports a live server at its timeout.
    #[test]
    fn a_wait_on_a_live_server_times_out_alive() {
        assert!(!ServerWatch::new().wait_dead(Duration::ZERO));
    }

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

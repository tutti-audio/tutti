//! Progress accounting for a chunked state transfer.
//!
//! # Why a progress deadline and not a total
//!
//! Plugin state travels as a `StateChunk` sequence, and its size is chosen by
//! the plugin — up to [`MAX_STATE_BYTES`], a gigabyte. A single fixed budget for
//! the whole transfer makes that limit unreachable: streaming a gigabyte through
//! the control socket takes longer than any total short enough to be a useful
//! liveness check, so the cap and the timeout contradict each other and the
//! timeout always wins. The failure then reads as a hung plugin, because a
//! timeout is what a hung plugin produces — a large-but-legal preset is lost
//! under an explanation that blames the wrong component.
//!
//! Scaling a total by the declared size at some floor bandwidth was the other
//! candidate and is worse here for two reasons. The receive direction has no
//! declared size to scale by — `SaveState` learns the total only when the `last`
//! chunk lands, so the budget would have to be picked before the quantity it
//! depends on is known. And a floor bandwidth is a second tunable that is wrong
//! in both directions: too low and a genuinely wedged plugin is waited on for
//! minutes, too high and a slow disk-backed plugin is failed for being slow.
//!
//! A progress deadline needs neither. "No chunk in [`Timeouts::state_progress`]" is the
//! question worth asking — *is this transfer still moving?* — and it is the same
//! question in both directions, at any size, with no bandwidth assumption. A
//! transfer that advances is allowed to take as long as it takes; one that stops
//! fails one deadline after it stops, not one deadline after it started.
//!
//! # Shape
//!
//! The bridge thread owns the socket and the caller owns the wait, so progress
//! has to cross threads. [`StateProgress`] is that seam: the bridge thread calls
//! [`StateProgress::advance`] as each chunk lands, and the caller polls
//! [`StateProgress::wait`], which returns only when the reply arrives or when
//! the counter has stood still for the deadline.
//!
//! It carries the bridge thread's per-chunk **deadline** and the caller's
//! **patience** (the deadline and a margin) as well as the counter, so both
//! halves of a transfer run on one configuration, and the caller always outlasts
//! the bridge thread: the stall report that says how far the transfer got is the
//! one that arrives.
//!
//! # A stall is not a disconnection
//!
//! `pump` treats every error out of `dispatch::handle` as connection-level and
//! calls `crash()`. A state deadline expiring is not that: the socket is
//! synchronised, nothing is malformed, and the peer has merely not spoken yet.
//! So the bridge thread reports a stall *to the caller* and stays alive, the way
//! the over-limit `LoadState` branch already does. Getting this wrong produced a
//! self-contradiction a caller could not defend against — [`StateError::Stalled`]
//! promises a healthy session and a reasonable retry, while the crash it rode in
//! on had already destroyed the session it would retry against.
//!
//! [`MAX_STATE_BYTES`]: tutti_plugin_types::MAX_STATE_BYTES

use super::ask::Ask;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tutti_plugin_types::{StateError, Timeouts};

/// How often [`StateProgress::wait`] rechecks the counter.
///
/// The wait is a poll rather than a condvar because the reply and the progress
/// signal arrive through two different primitives — a one-shot channel and an
/// atomic — and a caller must wake for either. Short enough that a stalled
/// transfer is not detected appreciably late, long enough that the poll costs
/// nothing next to a transfer measured in seconds.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What a [`StallWatch`] makes of the transfer at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Liveness {
    /// The transfer moved within its window.
    Live,
    /// No byte moved for the whole window.
    Stalled,
}

/// The progress deadline as a pure rule: a transfer is alive while its byte
/// count advances and stalled once it stands still for `window`.
///
/// It reads no clock. The waiting caller passes the instant it sampled, so the
/// rule is a function of the instants it is given and a test drives it with
/// constructed ones.
#[derive(Clone, Copy, Debug)]
pub(super) struct StallWatch {
    window: Duration,
    seen: usize,
    since: Instant,
}

impl StallWatch {
    /// A watch that starts at `now` with `bytes` already moved.
    pub(super) fn new(window: Duration, now: Instant, bytes: usize) -> Self {
        Self {
            window,
            seen: bytes,
            since: now,
        }
    }

    /// The verdict at `now` given the transfer's byte count.
    ///
    /// A count other than the last one seen is progress and restarts the
    /// window at `now`; the same count is stalled once `now` is `window` past
    /// the last progress, and live until then.
    pub(super) fn observe(&mut self, now: Instant, bytes: usize) -> Liveness {
        if bytes != self.seen {
            self.seen = bytes;
            self.since = now;
        }
        if now >= self.since + self.window {
            Liveness::Stalled
        } else {
            Liveness::Live
        }
    }
}

/// Shared byte counter for one in-flight state transfer.
///
/// Cloned into the [`Command`](super::messages::Command) so the bridge thread
/// can advance it while the caller watches. One counter per transfer, created at
/// the call and dropped with it, so a previous transfer's progress can never be
/// mistaken for this one's.
#[derive(Clone)]
pub(super) struct StateProgress {
    bytes: Arc<AtomicUsize>,
    /// The bridge thread's per-chunk deadline for *this* transfer
    /// ([`Timeouts::state_progress`]).
    ///
    /// Carried on the transfer rather than read at each wait site, so the
    /// caller's wait and the bridge thread's per-chunk reads are one
    /// configuration.
    deadline: Duration,
    /// How long the caller lets the transfer stand still before it gives up
    /// ([`Timeouts::state_wait`]): the deadline and a margin, so the bridge
    /// thread's own stall report (which says how far the transfer got) is
    /// the one that arrives first.
    patience: Duration,
}

impl StateProgress {
    pub(super) fn new(timeouts: &Timeouts) -> Self {
        Self {
            bytes: Arc::new(AtomicUsize::new(0)),
            deadline: timeouts.state_progress,
            patience: timeouts.state_wait(),
        }
    }

    /// The deadline this transfer runs under; see the field.
    pub(super) fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Record that `bytes` more have moved.
    ///
    /// `Relaxed` is sufficient: the counter guards nothing and publishes no
    /// other memory. The caller reads it only to decide whether to keep waiting,
    /// and the value it eventually acts on is delivered through the reply
    /// channel, which carries its own ordering.
    pub(super) fn advance(&self, bytes: usize) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The stall error for this transfer, as far as it got.
    ///
    /// One constructor rather than three literals, so the bridge thread's two
    /// stall sites and the caller's cannot describe the same event differently.
    pub(super) fn stalled(&self) -> StateError {
        StateError::Stalled {
            bytes: self.bytes(),
            after: self.deadline,
        }
    }

    /// Wait for `ask`'s reply, giving up only once progress has stalled.
    ///
    /// Returns the transfer's own answer when one arrives. Otherwise the counter
    /// stood still for the caller's patience and the result is
    /// [`StateError::Stalled`], carrying how far the transfer got.
    ///
    /// A disconnected channel is the bridge thread having dropped the `Reply`
    /// without sending, which happens when it exits — the plugin is gone, so
    /// that answers [`StateError::PluginCrashed`] rather than waiting out a
    /// deadline for a reply nobody will send.
    pub(super) fn wait<T>(
        &self,
        ask: Ask<std::result::Result<T, StateError>>,
    ) -> std::result::Result<T, StateError> {
        let mut watch = StallWatch::new(self.patience, Instant::now(), self.bytes());
        loop {
            match ask.recv_timeout_borrowed(POLL_INTERVAL) {
                Ok(value) => return value,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err(StateError::PluginCrashed);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
            if watch.observe(Instant::now(), self.bytes()) == Liveness::Stalled {
                return Err(self.stalled());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(2);

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    /// A transfer that has not moved is live until the window has passed and
    /// stalled from the window's last instant on.
    #[test]
    fn a_transfer_standing_still_stalls_exactly_at_the_window() {
        let start = Instant::now();
        let mut watch = StallWatch::new(WINDOW, start, 0);
        assert_eq!(watch.observe(at(start, 1_900), 0), Liveness::Live);
        assert_eq!(watch.observe(at(start, 1_999), 0), Liveness::Live);
        assert_eq!(watch.observe(at(start, 2_000), 0), Liveness::Stalled);
        assert_eq!(watch.observe(at(start, 2_100), 0), Liveness::Stalled);
    }

    /// Progress restarts the window at the instant it is seen, so a transfer
    /// that advances every 1.9 s lives however long it runs, and one that stops
    /// is stalled one window after its last byte and not one after the start.
    #[test]
    fn progress_restarts_the_window_so_a_steady_transfer_outlives_it() {
        let start = Instant::now();
        let mut watch = StallWatch::new(WINDOW, start, 0);
        for step in 1..=60u64 {
            let now = at(start, step * 1_900);
            assert_eq!(
                watch.observe(now, step as usize),
                Liveness::Live,
                "step {step}"
            );
        }
        let last = at(start, 60 * 1_900);
        assert_eq!(
            watch.observe(last + Duration::from_millis(1_999), 60),
            Liveness::Live
        );
        assert_eq!(watch.observe(last + WINDOW, 60), Liveness::Stalled);
    }

    /// A watch started with bytes already moved counts them as the baseline:
    /// they are not progress.
    #[test]
    fn bytes_moved_before_the_watch_started_are_not_progress() {
        let start = Instant::now();
        let mut watch = StallWatch::new(WINDOW, start, 64);
        assert_eq!(watch.observe(at(start, 2_000), 64), Liveness::Stalled);
    }
}

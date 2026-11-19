//! How the streamer is woken: one [`Bell`] the thread sleeps on,
//! and the [`StreamerSender`] that rings it after every command.
//!
//! The streamer has nothing to do until one of these happens, and each one
//! rings the bell:
//!
//! | Source | Rung by |
//! |---|---|
//! | a command (open, close, session rate) | [`StreamerSender::send`], after the push |
//! | a loop or direction a stream's control side publishes | [`ControlCell::publish`](super::control_cell::ControlCell::publish), through [`Bell::ring_control`] |
//! | the decode lane's answer to an open | the same send (the lane holds a `StreamerSender`) |
//! | a voice's ring below its refill threshold, or a jump outside the window | [`Bell::ring_refill`], from the audio thread (`RingReader::block`, through [`needs_refill`](super::io::refill::needs_refill)) |
//! | shutdown | [`Bell::shut`] |
//! | the reclaim backstop | the wait's timeout, which stays |
//!
//! The wait is [`Parker::park_timeout`] over what each source publishes: a
//! command is in the queue, the refill or steer flag is set, or the shutdown
//! flag is.
//! The flag a voice sets and the unpark are in the order the parker's
//! protocol asks (publish, then unpark), so a ring that lands between the
//! streamer's last look and its park is not lost. The backstop bounds how
//! long a *missed* ring (a bug, not a design) can delay a refill, and is the
//! control-side tick a cell that goes quiet still gets its `reclaim()` on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{SendError, Sender, TrySendError};
use tutti_rt::{Parker, Unparker, parker};

use super::command::StreamerCommand;
use tutti_rt::RingFloat;

/// The streamer thread's wake-up, shared by every sender, every voice's
/// reader and the thread itself.
pub(crate) struct Bell {
    unparker: Unparker,
    /// A voice found its ring wanting a refill since the streamer's last
    /// cycle began ([`begin_cycle`](Self::begin_cycle) clears it).
    refill: AtomicBool,
    /// A loop or direction was published for the streamer to take since its
    /// last cycle began.
    steer: AtomicBool,
    /// The streamer is to stop: the one shutdown mechanism.
    shut: AtomicBool,
    /// Cycles the thread has begun (tests: how often it woke).
    #[cfg(test)]
    cycles: std::sync::atomic::AtomicU64,
}

impl Bell {
    /// The bell and the [`Parker`] of its unparker, which the streamer
    /// thread alone waits on.
    pub(crate) fn new() -> (Arc<Self>, Parker) {
        let (unparker, parker) = parker();
        let bell = Arc::new(Self {
            unparker,
            refill: AtomicBool::new(false),
            steer: AtomicBool::new(false),
            shut: AtomicBool::new(false),
            #[cfg(test)]
            cycles: std::sync::atomic::AtomicU64::new(0),
        });
        (bell, parker)
    }

    /// Wake the streamer after something was published for it to find (a
    /// command pushed to its queue). Wait-free: no lock, no allocation.
    pub(crate) fn ring(&self) {
        self.unparker.unpark();
    }

    /// A voice's ring wants a refill: set the flag the streamer reads, then
    /// ring. Audio thread: a store, a fence and a load, plus a futex wake when
    /// the streamer is parked (the wake-up, never a wait on it).
    #[inline]
    pub(crate) fn ring_refill(&self) {
        self.refill.store(true, Ordering::Release);
        self.unparker.unpark();
    }

    /// A request was published for the streamer (a loop or direction): set
    /// the flag it reads, then ring. Control thread.
    pub(crate) fn ring_control(&self) {
        self.steer.store(true, Ordering::Release);
        self.unparker.unpark();
    }

    /// Ask the streamer to stop, and wake it to see it.
    pub(crate) fn shut(&self) {
        self.shut.store(true, Ordering::Release);
        self.unparker.unpark();
    }

    /// Whether [`shut`](Self::shut) was called.
    pub(crate) fn is_shut(&self) -> bool {
        self.shut.load(Ordering::Acquire)
    }

    /// The streamer begins a cycle: the refill flag is cleared before the
    /// cycle looks at the rings, so a voice that asks after this is seen by
    /// the next wait.
    pub(crate) fn begin_cycle(&self) {
        self.refill.store(false, Ordering::Relaxed);
        self.steer.store(false, Ordering::Relaxed);
        #[cfg(test)]
        self.cycles.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether a flag the streamer sleeps on is set: a voice's refill, a
    /// published request, or shutdown.
    pub(crate) fn has_work(&self) -> bool {
        self.is_shut() || self.refill.load(Ordering::Acquire) || self.steer.load(Ordering::Acquire)
    }

    /// Whether the streamer is parked, or about to be (tests).
    #[cfg(test)]
    pub(crate) fn is_parked(&self) -> bool {
        self.unparker.is_parked()
    }

    /// Cycles begun so far (tests).
    #[cfg(test)]
    pub(crate) fn cycles(&self) -> u64 {
        self.cycles.load(Ordering::Relaxed)
    }

    /// Sleep until a source rings, or `backstop` passes. `queued` says whether
    /// a command is waiting; it is read after the parker is armed, so a push
    /// that lands before the park is seen. May return with nothing to do (a
    /// spurious park return, a ring whose work the last cycle already did).
    pub(crate) fn sleep(
        &self,
        parker: &mut Parker,
        queued: impl FnOnce() -> bool,
        backstop: Duration,
    ) {
        parker.park_timeout(|| queued() || self.has_work(), backstop);
    }
}

/// The sending end of the streamer's command queue: a bounded
/// `crossbeam_channel` and the [`Bell`] each send rings. Sent on by control
/// threads, and by the decode lane. A send waits while the queue is full and
/// fails once the streamer has gone (every receiver dropped: the thread's on
/// its exit, a built streamer's on its drop).
#[derive(Clone)]
pub(crate) struct StreamerSender<F: RingFloat> {
    tx: Sender<StreamerCommand<F>>,
    bell: Arc<Bell>,
}

impl<F: RingFloat> StreamerSender<F> {
    pub(crate) fn new(tx: Sender<StreamerCommand<F>>, bell: Arc<Bell>) -> Self {
        Self { tx, bell }
    }

    /// Queues `cmd` and rings the bell, waiting while the queue is full.
    pub(crate) fn send(
        &self,
        cmd: StreamerCommand<F>,
    ) -> Result<(), SendError<StreamerCommand<F>>> {
        self.tx.send(cmd)?;
        self.bell.ring();
        Ok(())
    }

    /// [`send`](Self::send) that refuses instead of waiting.
    pub(crate) fn try_send(
        &self,
        cmd: StreamerCommand<F>,
    ) -> Result<(), TrySendError<StreamerCommand<F>>> {
        self.tx.try_send(cmd)?;
        self.bell.ring();
        Ok(())
    }

    /// Whether the queue holds no command (tests).
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.tx.is_empty()
    }
}

/// A sender whose bell nobody sleeps on: for a test that holds the receiving
/// end itself.
#[cfg(test)]
impl<F: RingFloat> From<Sender<StreamerCommand<F>>> for StreamerSender<F> {
    fn from(tx: Sender<StreamerCommand<F>>) -> Self {
        Self::new(tx, Bell::new().0)
    }
}

impl<F: RingFloat> std::fmt::Debug for StreamerSender<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamerSender").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Bell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bell")
            .field("shut", &self.is_shut())
            .finish_non_exhaustive()
    }
}

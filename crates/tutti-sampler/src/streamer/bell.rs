//! How the streamer is woken, and the [`StreamerSender`] that rings it after
//! every command.
//!
//! The streamer future awaits one [`tutti_rt::Bell`]'s listener. Each source of
//! work rings the same bell, and one cycle serves them all, so a source needs no
//! flag of its own:
//!
//! | Source | Rung by |
//! |---|---|
//! | a command (open, close, session rate) | [`StreamerSender::send`], after the push |
//! | a loop or direction a stream's control side publishes | [`ControlCell::publish`](super::control_cell::ControlCell::publish) |
//! | the decode lane's answer to an open | the same send (the lane holds a `StreamerSender`) |
//! | a voice's ring below its refill threshold, or a jump outside the window | [`RingReader::block`](super::block::RingReader::block), from the audio thread |
//! | the retired controls of a quiet cell | `Session::update`, through [`DiskStreamer::wake`](crate::DiskStreamer::wake) |
//! | shutdown | [`Running`](super::thread::Running)'s drop |
//!
//! A ring that lands while a cycle runs is kept by the bell, so the listener
//! resolves again at once. Nothing times out: a cell that goes quiet gets its
//! `reclaim()` on the control side's own tick.

use std::sync::Arc;

use crossbeam_channel::{SendError, Sender};
use tutti_rt::{Bell, RingFloat};

use super::command::StreamerCommand;

/// The sending end of the streamer's command queue: a bounded
/// `crossbeam_channel` and the [`Bell`] each send rings. Sent on by control
/// threads, and by the decode lane. A send waits while the queue is full and
/// fails once the streamer has gone (every receiver dropped: its future ended, a
/// built streamer's on its drop).
#[derive(Clone)]
pub(crate) struct StreamerSender<F: RingFloat> {
    tx: Sender<StreamerCommand<F>>,
    bell: Arc<Bell>,
}

impl<F: RingFloat> StreamerSender<F> {
    pub(crate) fn new(tx: Sender<StreamerCommand<F>>, bell: Bell) -> Self {
        Self {
            tx,
            bell: Arc::new(bell),
        }
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

    /// [`send`](Self::send) that refuses instead of waiting: what a future uses,
    /// which must not block.
    pub(crate) fn try_send(
        &self,
        cmd: StreamerCommand<F>,
    ) -> Result<(), crossbeam_channel::TrySendError<StreamerCommand<F>>> {
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

/// A sender whose bell nobody awaits: for a test that holds the receiving end
/// itself.
#[cfg(test)]
impl<F: RingFloat> From<Sender<StreamerCommand<F>>> for StreamerSender<F> {
    fn from(tx: Sender<StreamerCommand<F>>) -> Self {
        let bells = tutti_rt::Bells::new().expect("a tick thread");
        Self::new(tx, bells.bell().0)
    }
}

impl<F: RingFloat> std::fmt::Debug for StreamerSender<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamerSender").finish_non_exhaustive()
    }
}

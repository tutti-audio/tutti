//! The streamer's future: the main loop.
//!
//! The *work* of a cycle is
//! [`StreamerCycle::step`](super::step::StreamerCycle::step), synchronous and
//! shared with the hand-stepped driver. What lives here is the waiting: when a cycle
//! finds nothing urgent the future awaits its bell until a source rings it (a
//! command, a voice's ring wanting a refill, shutdown, a loop or direction
//! published for it, the control side's tick). Nothing polls and nothing times out.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::Receiver;
use futures_lite::future::yield_now;
use tutti_rt::{Listener, RingFloat};

use super::command::StreamerCommand;
use super::handlers::Handles;
use super::step::{StepOutcome, StreamerCycle};

/// The queue's receiving end, which frees what is still queued when the future
/// ends. A queued command can hold a stream's record, and the queue outlives the
/// future while any stream's handle holds a sender: the record is dropped here, which
/// tells its stream the streamer is gone.
struct Queue<F: RingFloat>(Receiver<StreamerCommand<F>>);

impl<F: RingFloat> Drop for Queue<F> {
    fn drop(&mut self) {
        while self.0.try_recv().is_ok() {}
    }
}

/// The streamer's body: wait for a ring, step until the rings are filled, and end
/// when `stop` is raised.
///
/// Each ring is followed by `step_until_idle`: the cycles that fill
/// every open ring once from empty. A cycle still [`Busy`](StepOutcome::Busy) after
/// them yields and goes on. No wake is lost: a ring that lands during a cycle is
/// kept by the bell, so the next wait resolves at once.
pub(super) async fn streamer_future<F: RingFloat>(
    queue: Receiver<StreamerCommand<F>>,
    shared: Handles<F>,
    mut cycle: StreamerCycle<F>,
    mut listener: Listener,
    stop: Arc<AtomicBool>,
) {
    let queue = Queue(queue);
    loop {
        listener.rung().await;
        while !stop.load(Ordering::Acquire) {
            let outcome = cycle.step_until_idle(&shared, || queue.0.try_recv().ok());
            if outcome != StepOutcome::Busy {
                break;
            }
            yield_now().await;
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
    }
}

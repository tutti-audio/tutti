//! [`TimerDriver`]: a [`StreamDriver`] whose clock is a dedicated task of the
//! session's executor that sleeps to absolute deadlines, for an app with MIDI and
//! no sound card.
//!
//! `Pacer` is the schedule as a pure function of the time it is told; the task's
//! loop reads the OS clock, polls it, and sleeps to the deadline it answers.

mod pacer;

use std::sync::Arc;
use std::time::Duration;

use tutti_buffers::Float;
use tutti_platform::{
    Executor, PriorityRequest, StopToken, Task, Unjoined, join_within, monotonic_ns, sleep_until_ns,
};
use tutti_units::Samples;

use crate::callback::OutputBlock;
use crate::faults::StreamFaults;
use crate::stream::{OutputQuantum, OutputSpec, RunningStream, StreamDriver};

use pacer::{Due, Pacer};

/// The block a [`TimerDriver`] renders when the spec fixes no
/// [`quantum`](OutputSpec::quantum): 256 frames, 5.3 ms at 48 kHz.
pub const DEFAULT_BLOCK: OutputQuantum = OutputQuantum::of(256);

/// Runs an output stream on a dedicated thread of an [`Executor`] that sleeps to
/// each block's deadline,
/// with no device: the graph renders through the same [`OutputBlock`] a sound
/// card's callback does, into a scratch buffer nobody plays, and
/// [`StreamLatency`](crate::StreamLatency) reads `Some(0)`.
///
/// # The schedule
///
/// Block `n` is rendered at `start + n * block / rate - block`: one block
/// before it would play, so the MIDI it makes reaches its out with a block's
/// lead.
///
/// - Every block renders. A thread that wakes late renders the blocks it owes
///   back to back, each dated by its own deadline, so frames stay continuous.
/// - A thread that wakes more than four blocks late (a machine
///   that was suspended, a thread starved) gives the owed blocks up: it
///   re-anchors the schedule at the present, and records one
///   [`StreamFaultKind::Overrun`](crate::StreamFaultKind::Overrun).
/// - The block clock reads each block's *deadline* as the block's OS time,
///   never the wake-up that ran it (`OutputBlock::render_at`), so a reader
///   that maps frames to the OS clock sees no one-sided jitter.
///
/// The block is the stream's [`quantum`](OutputSpec::quantum), or
/// [`DEFAULT_BLOCK`] when the spec has none. The thread asks the OS for its
/// highest ordinary priority ([`Executor::spawn_dedicated_local`]); a refusal leaves
/// it at the default.
///
/// Pass it to [`OutputStream::start_with`](crate::OutputStream::start_with) or
/// [`StreamHost::start_with`](crate::StreamHost::start_with); dropping the
/// stream (or [`stop`](crate::OutputStream::stop)) waits for the loop to end, so
/// no block renders after the stop returns. Drop the stream before the executor:
/// the loop ends only on the stream's stop.
#[derive(Debug, Clone)]
pub struct TimerDriver {
    executor: Executor,
}

impl TimerDriver {
    /// A driver whose clock runs on a dedicated thread of `executor`, rendering
    /// [`DEFAULT_BLOCK`] frames per block when the spec fixes none.
    #[must_use]
    pub fn new(executor: &Executor) -> Self {
        Self {
            executor: executor.clone(),
        }
    }
}

/// The longest a stop waits for the loop to end. The loop reads its stop once per
/// block, so it ends within one block's sleep and render; this bound guards only a
/// render that hangs.
const STOP_PATIENCE: Duration = Duration::from_secs(10);

/// Why a [`TimerDriver`] did not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TimerError {
    /// The OS refused the timer thread, or the target has no threads (the web).
    #[error("the timer thread could not start: {0}")]
    Thread(#[from] std::io::Error),
}

/// The handle of a started [`TimerDriver`] stream. Dropping it stops the loop
/// and waits for it to end.
///
/// A panic of the loop itself (the render's are caught in the block) ends the
/// stream silently: the drop's join discards it. A drop on a thread that drives
/// an executor does not wait: it stops the loop and detaches, with a warning.
#[derive(Debug)]
pub struct TimerRunning {
    stop: StopToken,
    task: Option<Task<()>>,
}

impl Drop for TimerRunning {
    fn drop(&mut self) {
        self.stop.stop();
        let Some(task) = self.task.take() else {
            return;
        };
        match join_within(task, STOP_PATIENCE) {
            Ok(_) => {}
            Err(Unjoined::TimedOut(task)) => {
                tracing::warn!("the timer loop did not stop in time; leaving it");
                task.detach();
            }
            Err(Unjoined::Driving(task)) => task.detach(),
        }
    }
}

impl RunningStream for TimerRunning {
    fn stop(self: Box<Self>) {
        // Drop does it.
    }
}

impl<F: Float> StreamDriver<F> for TimerDriver {
    type Running = TimerRunning;
    type Error = TimerError;

    fn open(
        self,
        spec: &OutputSpec,
        mut block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<TimerRunning, TimerError> {
        let frames = spec.quantum.unwrap_or(DEFAULT_BLOCK).get();
        let rate = spec.sample_rate;
        // Sized here, on the control thread; the loop only fills it.
        let mut scratch = vec![F::ZERO; block.layout().interleaved_len(Samples::new(frames))];
        let stop = StopToken::new();
        let token = stop.clone();
        // The loop owns its dedicated thread and sleeps on the OS clock's absolute
        // deadlines, never on the executor's timers, so it needs a thread of its own:
        // the web, which has none, refuses it here. A refusal of the priority leaves
        // the thread at the default.
        let task = self.executor.spawn_dedicated_local(
            "tutti-timer",
            Some(PriorityRequest::Highest),
            move || async move {
                // The first block is due now and plays one block from now.
                let mut pacer = Pacer::new(monotonic_ns(), frames, rate);
                while !token.is_stopped() {
                    match pacer.poll(monotonic_ns()) {
                        Due::Wait(until_ns) => sleep_until_ns(until_ns),
                        Due::Render(os_ns) => {
                            block.render_at(&mut scratch, os_ns, Some(Duration::ZERO));
                            faults.delivered();
                        }
                        Due::Reanchored => overran(&faults),
                    }
                }
            },
        )?;
        Ok(TimerRunning {
            stop,
            task: Some(task),
        })
    }
}

/// What the loop records when the pacer re-anchors: one overrun, which does not
/// end the stream.
fn overran(faults: &StreamFaults) {
    faults.record_overrun("the timer fell behind and began its schedule again");
}

#[cfg(test)]
mod tests;

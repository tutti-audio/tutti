//! [`TimerDriver`]: a [`StreamDriver`] whose clock is a thread of its own that
//! sleeps to absolute deadlines, for an app with MIDI and no sound card.
//!
//! `Pacer` is the schedule as a pure function of the time it is told, and
//! `run` is the thread's loop: the pacer over a `Timebase`, the OS clock in the
//! driver and a scripted one under test.

mod pacer;

use std::sync::Arc;
use std::time::Duration;

use tutti_buffers::Float;
use tutti_buffers::MaxBlock;
use tutti_platform::{
    PriorityRequest, StopToken, ThreadHandle, monotonic_ns, sleep_until_ns, spawn_named,
};
use tutti_units::Samples;

use crate::callback::{MAX_FRAMES, OutputBlock};
use crate::faults::StreamFaults;
use crate::stream::{OutputSpec, RunningStream, StreamDriver};

pub(crate) use pacer::Timebase;
use pacer::{Due, Pacer};

/// The block a [`TimerDriver`] renders when the spec fixes no
/// [`quantum`](OutputSpec::quantum): 256 frames, 5.3 ms at 48 kHz.
pub const DEFAULT_BLOCK: MaxBlock = MaxBlock::of(Samples::new(256));

/// Runs an output stream on a thread that sleeps to each block's deadline,
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
/// highest ordinary priority ([`tutti_platform::spawn_named`]); a refusal leaves
/// it at the default.
///
/// Pass it to [`OutputStream::start_with`](crate::OutputStream::start_with) or
/// [`StreamHost::start_with`](crate::StreamHost::start_with); dropping the
/// stream (or [`stop`](crate::OutputStream::stop)) joins the thread, so no block
/// renders after the stop returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimerDriver;

impl TimerDriver {
    /// A driver that renders [`DEFAULT_BLOCK`] frames per block when the spec
    /// fixes none.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Why a [`TimerDriver`] did not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TimerError {
    /// The OS refused the timer thread.
    #[error("the timer thread could not start: {0}")]
    Thread(#[from] std::io::Error),
}

/// The handle of a started [`TimerDriver`] stream. Dropping it stops the
/// thread and waits for it.
///
/// A panic of the loop itself (the render's are caught in the block) is caught
/// on the thread and ends the stream silently: the drop's join discards it.
#[derive(Debug)]
pub struct TimerRunning {
    _thread: ThreadHandle<()>,
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
        let frames = spec.quantum.unwrap_or(DEFAULT_BLOCK).get().min(MAX_FRAMES);
        let rate = spec.sample_rate;
        // Sized here, on the control thread; the loop only fills it.
        let mut scratch = vec![F::ZERO; block.layout().interleaved_len(Samples::new(frames))];
        // A refusal of the priority leaves the thread at the default.
        #[expect(
            clippy::disallowed_methods,
            reason = "the deviceless audio clock: it paces and renders blocks against OS \
                      deadlines, so it is no executor's task"
        )]
        let thread = spawn_named("tutti-timer", Some(PriorityRequest::Highest), move |stop| {
            let time = OsTime;
            // The first block is due now and plays one block from now.
            let mut pacer = Pacer::new(time.now_ns(), frames, rate);
            run(
                &time,
                &mut pacer,
                stop,
                |os_ns| {
                    block.render_at(&mut scratch, os_ns, Some(Duration::ZERO));
                    faults.delivered();
                },
                || faults.record_overrun("the timer fell behind and began its schedule again"),
            );
        })?;
        Ok(TimerRunning { _thread: thread })
    }
}

/// The OS monotonic clock and its absolute sleep.
struct OsTime;

impl Timebase for OsTime {
    fn now_ns(&self) -> i64 {
        monotonic_ns()
    }

    fn sleep_until_ns(&self, deadline_ns: i64) {
        sleep_until_ns(deadline_ns);
    }
}

/// The timer thread's loop: `pacer` over `time`, until `stop`.
///
/// `render` is called once per block with the block's deadline, in order and
/// with none skipped between two calls of `overran`; `overran` is called once
/// each time the pacer re-anchors.
fn run(
    time: &impl Timebase,
    pacer: &mut Pacer,
    stop: &StopToken,
    mut render: impl FnMut(i64),
    mut overran: impl FnMut(),
) {
    while !stop.is_stopped() {
        match pacer.poll(time.now_ns()) {
            Due::Wait(until_ns) => time.sleep_until_ns(until_ns),
            Due::Render(os_ns) => render(os_ns),
            Due::Reanchored => overran(),
        }
    }
}

#[cfg(test)]
mod tests;

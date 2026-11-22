//! The Active Sensing heartbeat of one output: a dedicated thread of the executor
//! that sends `0xFE` whenever the output has been silent for
//! [`ACTIVE_SENSING_INTERVAL`](tutti_midi_runtime::ACTIVE_SENSING_INTERVAL).
//!
//! The schedule is [`ActiveSensingSender`]; this module owns the clock and the
//! task it is polled in. The task has a thread of its own, at raised priority,
//! which polls only it and fires its sleeps, so it runs from the heartbeat's
//! start to its drop whether or not the app drives the executor (MIDI 1.0
//! Detailed Specification p. 32: a transmitter that starts must go on). It sends only
//! while the output's Protocol is MIDI 1.0: a MIDI 2.0 Protocol output has no
//! Active Sensing.

use futures_channel::oneshot;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard};
use std::task::Poll;
use std::time::Duration;
use tutti_platform::{Executor, Instant, PriorityRequest, Task, Timer, Unjoined, join_within};
use tutti_rt::Mutex;

use tutti_graph::Offset;
use tutti_midi_runtime::ActiveSensingSender;
use tutti_ump::{MidiGroup, Protocol, Ump};

use crate::MidiHardwareError;
use crate::MidiThread;
use crate::endpoints::DeviceOut;

/// The schedule of one output's Active Sensing and the Protocol gate on it:
/// every method that depends on the time takes it as `now`.
pub(crate) struct Pulse {
    schedule: Mutex<ActiveSensingSender>,
    midi1: AtomicBool,
}

impl Pulse {
    /// A pulse for an output in `protocol` that has been silent since `now`.
    pub(crate) fn new(protocol: Protocol, now: Instant) -> Self {
        Self {
            schedule: Mutex::new(ActiveSensingSender::new(MidiGroup::FIRST, now)),
            midi1: AtomicBool::new(protocol == Protocol::Midi1),
        }
    }

    /// The output sent something at `now`: it is not silent.
    pub(crate) fn sent(&self, now: Instant) {
        lock(&self.schedule).sent(now);
    }

    /// The output's Protocol is `protocol` from now on.
    pub(crate) fn follow(&self, protocol: Protocol) {
        self.midi1
            .store(protocol == Protocol::Midi1, Ordering::Relaxed);
    }

    /// When [`due`](Self::due) next has a message.
    pub(crate) fn deadline(&self) -> Instant {
        lock(&self.schedule).deadline()
    }

    /// The Active Sensing to send at `now`: due after an interval of silence,
    /// and only in the MIDI 1.0 Protocol. A due message in the MIDI 2.0
    /// Protocol is consumed, so the schedule moves on.
    pub(crate) fn due(&self, now: Instant) -> Option<Ump> {
        let due = lock(&self.schedule).poll(now);
        due.filter(|_| self.midi1.load(Ordering::Relaxed))
    }
}

/// The longest a drop waits for the heartbeat's thread to end. The thread wakes on
/// the stop and ends at once; this bound guards only a send that hangs.
const STOP_PATIENCE: Duration = Duration::from_secs(2);

/// The running heartbeat: a dedicated task of the executor. Dropping it stops
/// the task and waits for it to end; a drop on a thread that drives an executor
/// does not wait, and the task ends on its own once it sees the stop.
pub(crate) struct Heartbeat {
    pulse: Arc<Pulse>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<Task<()>>,
}

impl Heartbeat {
    /// Starts the heartbeat over `sink` on its own thread of `executor`, for an
    /// output in `protocol`.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Spawn`] when the OS refuses the thread.
    pub(crate) fn start(
        executor: &Executor,
        sink: Arc<dyn DeviceOut>,
        protocol: Protocol,
    ) -> Result<Self, MidiHardwareError> {
        let pulse = Arc::new(Pulse::new(protocol, Instant::now()));
        let (stop, stopped) = oneshot::channel::<()>();
        let task = executor
            .spawn_dedicated(
                MidiThread::ActiveSensing.name(),
                Some(PriorityRequest::Raised),
                {
                    let pulse = Arc::clone(&pulse);
                    let timer = executor.timer();
                    async move { beat(&timer, &sink, &pulse, stopped).await }
                },
            )
            .map_err(|source| MidiHardwareError::Spawn {
                thread: MidiThread::ActiveSensing,
                source,
            })?;
        Ok(Self {
            pulse,
            stop: Some(stop),
            task: Some(task),
        })
    }

    /// The output sent something now: it is not silent.
    pub(crate) fn sent(&self) {
        self.pulse.sent(Instant::now());
    }

    /// The output's Protocol is `protocol` from now on.
    pub(crate) fn follow(&self, protocol: Protocol) {
        self.pulse.follow(protocol);
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        drop(self.stop.take());
        let Some(task) = self.task.take() else {
            return;
        };
        match join_within(task, STOP_PATIENCE) {
            Ok(_) => {}
            Err(Unjoined::TimedOut(task)) => {
                tracing::warn!("the Active Sensing thread did not stop in time; leaving it");
                task.detach();
            }
            Err(Unjoined::Driving(task)) => task.detach(),
        }
    }
}

fn lock(schedule: &Mutex<ActiveSensingSender>) -> MutexGuard<'_, ActiveSensingSender> {
    schedule.lock()
}

/// Sleeps to the schedule's deadline, sends what is due, and repeats until
/// `stopped` resolves (the heartbeat's drop) or the executor shuts down.
///
/// A [`Timer`]'s sleep fails once the executor has shut down: that ends the
/// heartbeat, so it never spins.
async fn beat(
    timer: &Timer,
    sink: &Arc<dyn DeviceOut>,
    pulse: &Pulse,
    mut stopped: oneshot::Receiver<()>,
) {
    loop {
        let mut sleep = std::pin::pin!(timer.sleep_until(pulse.deadline()));
        let woke = std::future::poll_fn(|cx| {
            if Pin::new(&mut stopped).poll(cx).is_ready() {
                return Poll::Ready(false);
            }
            sleep.as_mut().poll(cx).map(|slept| slept.is_ok())
        })
        .await;
        if !woke {
            return;
        }
        if let Some(ump) = pulse.due(Instant::now()) {
            let _ = sink.queue(&[(Offset::ZERO, ump)]);
        }
    }
}

#[cfg(test)]
mod tests;

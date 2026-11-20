//! The Active Sensing heartbeat of one output: a task on the executor that, while
//! the executor is driven, sends
//! `0xFE` whenever the output has been silent for
//! [`ACTIVE_SENSING_INTERVAL`](tutti_midi_runtime::ACTIVE_SENSING_INTERVAL).
//!
//! The schedule is [`ActiveSensingSender`]; this module owns the clock and the
//! task it is polled in. The task runs from the heartbeat's start to its
//! drop, whatever the caller does or stops doing (MIDI 1.0 Detailed
//! Specification p. 32: a transmitter that starts must go on). It sends only
//! while the output's Protocol is MIDI 1.0: a MIDI 2.0 Protocol output has no
//! Active Sensing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tutti_platform::Instant;
use tutti_rt::LockExt;

use tutti_graph::Offset;
use tutti_midi_runtime::ActiveSensingSender;
use tutti_platform::{Executor, Task, Timer};
use tutti_ump::{MidiGroup, Protocol, Ump};

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

/// The running heartbeat: a task on the executor. Dropping it cancels the task.
pub(crate) struct Heartbeat {
    pulse: Arc<Pulse>,
    _task: Task<()>,
}

impl Heartbeat {
    /// Starts the heartbeat over `sink` on `executor`, for an output in `protocol`.
    pub(crate) fn start(executor: &Executor, sink: Arc<dyn DeviceOut>, protocol: Protocol) -> Self {
        let pulse = Arc::new(Pulse::new(protocol, Instant::now()));
        let task = executor.spawn({
            let pulse = Arc::clone(&pulse);
            let timer = executor.timer();
            async move { beat(&timer, &sink, &pulse).await }
        });
        Self { pulse, _task: task }
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

fn lock(schedule: &Mutex<ActiveSensingSender>) -> MutexGuard<'_, ActiveSensingSender> {
    schedule.lock_recover()
}

/// Sleeps to the schedule's deadline, sends what is due, and repeats until the
/// task is cancelled or the executor shuts down.
///
/// A [`Timer`]'s sleep fails once the executor has shut down: that ends the
/// heartbeat, so it never spins.
async fn beat(timer: &Timer, sink: &Arc<dyn DeviceOut>, pulse: &Pulse) {
    loop {
        let deadline = pulse.deadline();
        if timer.sleep_until(deadline).await.is_err() {
            return;
        }
        if let Some(ump) = pulse.due(Instant::now()) {
            let _ = sink.queue(&[(Offset::ZERO, ump)]);
        }
    }
}

#[cfg(test)]
mod tests;

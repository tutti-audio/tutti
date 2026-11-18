//! The Active Sensing heartbeat of one output: a thread of its own that sends
//! `0xFE` whenever the output has been silent for
//! [`ACTIVE_SENSING_INTERVAL`](tutti_midi_runtime::ACTIVE_SENSING_INTERVAL).
//!
//! The schedule is [`ActiveSensingSender`]; this module owns the clock and the
//! thread it is polled on. The thread runs from the output's opening to its
//! drop, whatever the caller does or stops doing (MIDI 1.0 Detailed
//! Specification p. 32: a transmitter that starts must go on). It sends only
//! while the output's Protocol is MIDI 1.0: a MIDI 2.0 Protocol output has no
//! Active Sensing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tutti_rt::LockExt;

use tutti_graph::Offset;
use tutti_midi_runtime::ActiveSensingSender;
use tutti_rt::{StopToken, ThreadHandle, spawn_named};
use tutti_ump::{MidiGroup, Protocol, Ump};

use crate::endpoints::DeviceOut;
use crate::error::MidiHardwareError;

/// How long dropping a heartbeat waits for its thread: the thread sleeps on
/// its stop token, so it ends at once unless the sink is stuck in a send.
const STOP_BOUND: Duration = Duration::from_secs(1);

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

/// The running heartbeat. Dropping it stops the thread and waits for it.
pub(crate) struct Heartbeat {
    pulse: Arc<Pulse>,
    thread: Option<ThreadHandle<()>>,
}

impl Heartbeat {
    /// Starts the heartbeat over `sink`, for an output in `protocol`.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Spawn`] when the thread cannot start.
    pub(crate) fn start(sink: Arc<dyn DeviceOut>, protocol: Protocol) -> crate::Result<Self> {
        let pulse = Arc::new(Pulse::new(protocol, Instant::now()));
        let thread = spawn_named("tutti-midi-sense", None, {
            let pulse = Arc::clone(&pulse);
            move |stop| beat(&sink, &pulse, stop)
        })
        .map_err(|source| MidiHardwareError::Spawn {
            thread: "tutti-midi-sense",
            source,
        })?;
        Ok(Self {
            pulse,
            thread: Some(thread),
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
        if let Some(thread) = self.thread.take() {
            match thread.stop_within(STOP_BOUND) {
                Ok(Ok(())) => {}
                Ok(Err(panicked)) => tracing::error!("Active Sensing thread: {panicked}"),
                Err(stuck) => {
                    tracing::warn!(
                        "Active Sensing thread did not stop within {STOP_BOUND:?}; left running"
                    );
                    stuck.detach();
                }
            }
        }
    }
}

fn lock(schedule: &Mutex<ActiveSensingSender>) -> MutexGuard<'_, ActiveSensingSender> {
    schedule.lock_recover()
}

/// Sleeps to the schedule's deadline, sends what is due, and repeats until
/// `stop` is requested.
fn beat(sink: &Arc<dyn DeviceOut>, pulse: &Pulse, stop: &StopToken) {
    loop {
        let wait = pulse.deadline().saturating_duration_since(Instant::now());
        if stop.wait(wait) {
            return;
        }
        if let Some(ump) = pulse.due(Instant::now()) {
            let _ = sink.queue(&[(Offset::ZERO, ump)]);
        }
    }
}

#[cfg(test)]
mod tests;

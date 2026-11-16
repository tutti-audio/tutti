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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tutti_graph::Offset;
use tutti_midi_runtime::ActiveSensingSender;
use tutti_rt::{StopToken, ThreadHandle, spawn_named};
use tutti_ump::{MidiGroup, Protocol};

use crate::endpoints::DeviceOut;

/// How long dropping a heartbeat waits for its thread: the thread sleeps on
/// its stop token, so it ends at once unless the sink is stuck in a send.
const STOP_BOUND: Duration = Duration::from_secs(1);

/// The running heartbeat. Dropping it stops the thread and waits for it.
pub(crate) struct Heartbeat {
    schedule: Arc<Mutex<ActiveSensingSender>>,
    midi1: Arc<AtomicBool>,
    thread: Option<ThreadHandle<()>>,
}

impl Heartbeat {
    /// Starts the heartbeat over `sink`, for an output in `protocol`.
    ///
    /// # Errors
    ///
    /// The OS's error when the thread cannot start.
    pub(crate) fn start(sink: Arc<dyn DeviceOut>, protocol: Protocol) -> std::io::Result<Self> {
        let schedule = Arc::new(Mutex::new(ActiveSensingSender::new(
            MidiGroup::FIRST,
            Instant::now(),
        )));
        let midi1 = Arc::new(AtomicBool::new(protocol == Protocol::Midi1));
        let thread = spawn_named("tutti-midi-sense", None, {
            let schedule = Arc::clone(&schedule);
            let midi1 = Arc::clone(&midi1);
            move |stop| beat(&sink, &schedule, &midi1, stop)
        })?;
        Ok(Self {
            schedule,
            midi1,
            thread: Some(thread),
        })
    }

    /// The output sent something now: it is not silent.
    pub(crate) fn sent(&self) {
        lock(&self.schedule).sent(Instant::now());
    }

    /// The output's Protocol is `protocol` from now on.
    pub(crate) fn follow(&self, protocol: Protocol) {
        self.midi1
            .store(protocol == Protocol::Midi1, Ordering::Relaxed);
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
    schedule.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sleeps to the schedule's deadline, sends what is due, and repeats until
/// `stop` is requested.
fn beat(
    sink: &Arc<dyn DeviceOut>,
    schedule: &Mutex<ActiveSensingSender>,
    midi1: &AtomicBool,
    stop: &StopToken,
) {
    loop {
        let wait = lock(schedule)
            .deadline()
            .saturating_duration_since(Instant::now());
        if stop.wait(wait) {
            return;
        }
        let due = lock(schedule).poll(Instant::now());
        if let Some(ump) = due
            && midi1.load(Ordering::Relaxed)
        {
            let _ = sink.queue(&[(Offset::ZERO, ump)]);
        }
    }
}

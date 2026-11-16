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
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use tutti_graph::Offset;
use tutti_midi_runtime::ActiveSensingSender;
use tutti_ump::{MidiGroup, Protocol};

use crate::endpoints::DeviceOut;

/// The running heartbeat. Dropping it stops the thread and waits for it.
pub(crate) struct Heartbeat {
    schedule: Arc<Mutex<ActiveSensingSender>>,
    midi1: Arc<AtomicBool>,
    /// Dropped to end the thread: its `recv_timeout` then disconnects.
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
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
        let (stop, ended) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("tutti-active-sensing".into())
            .spawn({
                let schedule = Arc::clone(&schedule);
                let midi1 = Arc::clone(&midi1);
                move || beat(&sink, &schedule, &midi1, &ended)
            })?;
        Ok(Self {
            schedule,
            midi1,
            stop: Some(stop),
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
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock(schedule: &Mutex<ActiveSensingSender>) -> std::sync::MutexGuard<'_, ActiveSensingSender> {
    schedule.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sleeps to the schedule's deadline, sends what is due, and repeats until
/// `ended` disconnects.
fn beat(
    sink: &Arc<dyn DeviceOut>,
    schedule: &Mutex<ActiveSensingSender>,
    midi1: &AtomicBool,
    ended: &mpsc::Receiver<()>,
) {
    loop {
        let wait = lock(schedule)
            .deadline()
            .saturating_duration_since(Instant::now());
        match ended.recv_timeout(wait) {
            Err(RecvTimeoutError::Disconnected) => return,
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                let due = lock(schedule).poll(Instant::now());
                if let Some(ump) = due
                    && midi1.load(Ordering::Relaxed)
                {
                    let _ = sink.queue(&[(Offset::ZERO, ump)]);
                }
            }
        }
    }
}

//! Sending MIDI 1.0 to a winmm output, each event at its time.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::super::legacy_port::bytes::{ByteOut, Wire};
use super::super::legacy_port::schedule::Schedule;
use crate::endpoints::{DeviceOut, Tally};
use crate::error::{Error, Result};
use tutti_graph::UmpAt;
use tutti_midi_runtime::{MidiOut, Outgoing, SendAt};
use tutti_ump::Ump;
use tutti_winmm_ffi::{MidiOut as Port, Sleeper};

/// How many events wait at most; a queue past it takes a prefix and says so.
const MAX_PENDING: usize = 8192;

/// How long dropping an output waits for its thread. The thread looks at the
/// stop before each event, so it ends within the send it is in; a SysEx long
/// enough to take longer is left to finish on its own, and the thread is
/// detached with a warning.
const JOIN_BOUND: Duration = Duration::from_secs(2);

struct State {
    queue: Schedule<Ump>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Ends the thread's wait when an earlier event arrives or the output stops.
    sleeper: Sleeper,
    /// Cleared when the thread ends other than by a stop.
    alive: AtomicBool,
    sent: AtomicU64,
    lost: AtomicU64,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An open winmm output, as a [`MidiOut`]: a thread holds each event until its
/// time and sends in order. Dropping it stops the thread and closes the port;
/// events not yet due are dropped.
pub struct WinMmOutput {
    shared: Arc<Shared>,
    /// Disconnects when the thread ends, however it ends.
    ended: Mutex<mpsc::Receiver<()>>,
    thread: Option<JoinHandle<()>>,
}

impl MidiOut for WinMmOutput {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.queue_timed(&Outgoing::now_batch(events))
    }

    /// Returns how many events were taken: an unbroken prefix, short only when
    /// [`MAX_PENDING`] events already wait, and none once the output is dead.
    /// A send that then fails is counted in [`tally`](DeviceOut::tally).
    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        if !self.shared.alive.load(Ordering::Acquire) {
            return 0;
        }
        let now = Instant::now();
        let mut state = self.shared.state();
        let room = MAX_PENDING.saturating_sub(state.queue.len());
        let taken = state.queue.push_batch(
            room,
            events.iter().map(|event| {
                let due = match event.send {
                    SendAt::Now => now,
                    SendAt::At(at) => at,
                };
                (due, event.ump)
            }),
        );
        drop(state);
        if taken > 0
            && let Err(e) = self.shared.sleeper.wake()
        {
            tracing::warn!("winmm MIDI output wake: {e}");
        }
        taken
    }
}

impl DeviceOut for WinMmOutput {
    fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    fn tally(&self) -> Tally {
        Tally {
            sent: self.shared.sent.load(Ordering::Acquire),
            lost: self.shared.lost.load(Ordering::Acquire),
        }
    }
}

impl Drop for WinMmOutput {
    fn drop(&mut self) {
        self.shared.state().stop = true;
        if let Err(e) = self.shared.sleeper.wake() {
            tracing::warn!("winmm MIDI output wake: {e}");
        }
        if let Some(thread) = self.thread.take() {
            match self
                .ended
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(JOIN_BOUND)
            {
                Err(mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                    "winmm MIDI output did not stop within {JOIN_BOUND:?}; left running"
                ),
                // Disconnected: the thread has ended (or is ending).
                _ => {
                    let _ = thread.join();
                }
            }
        }
    }
}

/// Sends what is due, in order, then waits for the next time or a wake. Ends
/// on a stop, or marks the output dead when the wait fails.
fn run(mut port: Port, shared: &Shared) {
    let mut bytes = ByteOut::new();
    loop {
        let (due, wait) = {
            let mut state = shared.state();
            if state.stop {
                return;
            }
            let now = Instant::now();
            let due: Vec<Ump> = std::iter::from_fn(|| state.queue.pop_due(now)).collect();
            (due, state.queue.wait(now))
        };
        if !due.is_empty() {
            for ump in due {
                if shared.state().stop {
                    return;
                }
                let sent = bytes.encode(ump, |wire| match wire {
                    Wire::Short(message) => port.short(message),
                    Wire::Long(message) => port.long(message),
                });
                match sent {
                    Ok(()) => shared.sent.fetch_add(1, Ordering::AcqRel),
                    Err(e) => {
                        tracing::debug!("winmm MIDI send: {e}");
                        shared.lost.fetch_add(1, Ordering::AcqRel)
                    }
                };
            }
            // Look again: more may have come due while sending.
            continue;
        }
        if let Err(e) = shared.sleeper.wait(wait) {
            tracing::warn!("winmm MIDI output stopped: {e}");
            // What is held can no longer be sent: it is lost.
            let mut state = shared.state();
            let held = state.queue.clear() as u64;
            shared.lost.fetch_add(held, Ordering::AcqRel);
            shared.alive.store(false, Ordering::Release);
            return;
        }
    }
}

/// Opens the output at `index`.
///
/// # Errors
///
/// [`Error::WinMm`] from the port's or the timer's creation, and
/// [`Error::MidiDevice`] if the thread cannot be spawned.
pub(super) fn open(index: u32) -> Result<WinMmOutput> {
    let port = Port::open(index)?;
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: Schedule::new(),
            stop: false,
        }),
        sleeper: Sleeper::new()?,
        alive: AtomicBool::new(true),
        sent: AtomicU64::new(0),
        lost: AtomicU64::new(0),
    });
    let (ending, ended) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("tutti-midi-out".to_string())
        .spawn({
            let shared = Arc::clone(&shared);
            move || {
                // Dropped when the thread returns or unwinds.
                let _ending: mpsc::Sender<()> = ending;
                run(port, &shared);
            }
        })
        .map_err(|e| Error::device_from("spawn MIDI output thread", e))?;
    Ok(WinMmOutput {
        shared,
        ended: Mutex::new(ended),
        thread: Some(thread),
    })
}

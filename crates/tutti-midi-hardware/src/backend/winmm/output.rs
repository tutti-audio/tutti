//! Sending MIDI 1.0 to a winmm output, each event at its time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::super::legacy_port::bytes::{ByteOut, Wire};
use super::super::legacy_port::schedule::{Next, Outbox};
use crate::endpoints::DeviceOut;
use crate::error::{Error, Result};
use tutti_graph::{Losses, UmpAt};
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

struct Shared {
    /// The held events, whether the sender lives, and how many are pending:
    /// all decided under this one lock, which the sender also holds when it
    /// ends.
    state: Mutex<Outbox<Ump>>,
    /// Ends the thread's wait when an earlier event arrives or the output stops.
    sleeper: Sleeper,
    /// Sends the device refused.
    lost: AtomicU64,
    /// Events dropped when the sender ended.
    refused: AtomicU64,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, Outbox<Ump>> {
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
    /// A send that then fails is counted in [`losses`](DeviceOut::losses).
    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        let now = Instant::now();
        let taken = self.shared.state().take(
            MAX_PENDING,
            events.iter().map(|event| {
                let due = match event.send {
                    SendAt::Now => now,
                    SendAt::At(at) => at,
                };
                (due, event.ump)
            }),
        );
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
        self.shared.state().alive()
    }

    fn losses(&self) -> Losses {
        Losses::lost(self.shared.lost.load(Ordering::Acquire))
            + Losses::refused(self.shared.refused.load(Ordering::Acquire))
    }

    fn pending(&self) -> usize {
        self.shared.state().pending()
    }
}

impl Drop for WinMmOutput {
    fn drop(&mut self) {
        self.shared.state().stop();
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

/// Marks the output dead when the sender thread ends, however it ends, a panic
/// included: what it held had nowhere to go.
struct Ends<'a>(&'a Shared);

impl Drop for Ends<'_> {
    fn drop(&mut self) {
        let held = self.0.state().end();
        self.0.refused.fetch_add(held as u64, Ordering::AcqRel);
    }
}

/// Sends what is due, in order, then waits for the next time or a wake. Ends on
/// a stop, or when the wait fails.
fn run(mut port: Port, shared: &Shared) {
    let _ends = Ends(shared);
    let mut bytes = ByteOut::new();
    loop {
        // The stop is looked at with each event, not once per batch: a long run
        // of due events must not keep a dropped output sending.
        let step = shared.state().next(Instant::now());
        match step {
            Next::Stop => return,
            Next::Send(ump) => {
                let sent = bytes.encode(ump, |wire| match wire {
                    Wire::Short(message) => port.short(message),
                    Wire::Long(message) => port.long(message),
                });
                if let Err(e) = sent {
                    tracing::debug!("winmm MIDI send: {e}");
                    shared.lost.fetch_add(1, Ordering::AcqRel);
                }
                shared.state().done();
            }
            Next::Wait(wait) => {
                if let Err(e) = shared.sleeper.wait(wait) {
                    tracing::warn!("winmm MIDI output stopped: {e}");
                    return;
                }
            }
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
        state: Mutex::new(Outbox::new()),
        sleeper: Sleeper::new()?,
        lost: AtomicU64::new(0),
        refused: AtomicU64::new(0),
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

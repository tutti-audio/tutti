//! A device watch for an OS that has no notification: list again on a timer.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::Watch;
use crate::error::{Error, Result};
use tutti_midi_device::OsDevice;
use tutti_rt::{ThreadHandle, spawn_named};

/// How often the list is read again: a device plugged in shows within this.
pub(in crate::backend) const INTERVAL: Duration = Duration::from_secs(1);

/// A running poll. Dropping it ends the thread and returns once it has ended
/// (a thread that does not within [`JOIN_BOUND`] is left running and logged),
/// so no report reaches the sink afterwards.
pub(in crate::backend) struct PollWatch {
    /// Disconnects when the thread ends, however it ends.
    ended: Mutex<mpsc::Receiver<()>>,
    thread: Option<ThreadHandle<()>>,
}

/// How long dropping a watch waits for its thread before it lets the thread
/// go: a thread ends within a look of the stop, and a drop that waited for ever
/// would hang the caller with it.
const JOIN_BOUND: Duration = Duration::from_secs(2);

impl Watch for PollWatch {}

impl Drop for PollWatch {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // Wakes the thread out of its wait.
            thread.token().stop();
            match self
                .ended
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(JOIN_BOUND)
            {
                Err(RecvTimeoutError::Timeout) => {
                    tracing::warn!("MIDI device watch did not stop within {JOIN_BOUND:?}");
                    thread.detach();
                }
                // Disconnected: the thread has ended (or is ending), so the
                // drop's join is brief.
                _ => drop(thread),
            }
        }
    }
}

/// Seeds `sink` with `snapshot()` before it returns, then reports each
/// difference between looks every `interval` until the watch drops. A panic in
/// `snapshot` ends the watch and says so through [`ChangeSink::fail`].
///
/// # Errors
///
/// [`Error::MidiDevice`] if the thread cannot be spawned, or the sink was
/// already seeded.
pub(in crate::backend) fn start(
    sink: ChangeSink,
    snapshot: fn() -> Vec<OsDevice>,
    interval: Duration,
) -> Result<PollWatch> {
    let failures = sink.clone();
    let mut reporter = Reporter::new(sink);
    reporter
        .seed(snapshot())
        .map_err(|e| Error::device_from("seed the MIDI device list", e))?;
    let (ending, ended) = mpsc::channel::<()>();
    let thread = spawn_named("tutti-devwatch", None, move |stop| {
        // Dropped when the thread returns or unwinds.
        let _ending = ending;
        while !stop.wait(interval) {
            let look = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reporter.report(snapshot(), |_| false);
            }));
            if look.is_err() {
                failures.fail(crate::WatchFailure::Panicked);
                return;
            }
        }
    })
    .map_err(|e| Error::device_from("spawn the MIDI device watch", e))?;
    Ok(PollWatch {
        ended: Mutex::new(ended),
        thread: Some(thread),
    })
}

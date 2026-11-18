//! A device watch for an OS that has no notification: list again on a timer.

use std::time::Duration;

use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::Watch;
use crate::error::{MidiHardwareError, Result};
use tutti_midi_device::OsDevice;
use tutti_rt::{ThreadHandle, spawn_named};

/// How often the list is read again: a device plugged in shows within this.
pub(in crate::backend) const INTERVAL: Duration = Duration::from_secs(1);

/// A running poll. Dropping it ends the thread and returns once it has ended
/// (a thread that does not within [`JOIN_BOUND`] is left running and logged),
/// so no report reaches the sink afterwards.
pub(in crate::backend) struct PollWatch {
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
            match thread.stop_within(JOIN_BOUND) {
                Ok(Ok(())) => {}
                // A panic in a look is caught and reported to the sink; one
                // that escapes the catch is reported here.
                Ok(Err(panicked)) => tracing::error!("MIDI device watch: {panicked}"),
                Err(stuck) => {
                    tracing::warn!("MIDI device watch did not stop within {JOIN_BOUND:?}");
                    stuck.detach();
                }
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
/// [`MidiHardwareError::Spawn`] if the thread cannot be spawned, and
/// [`MidiHardwareError::Seed`] if the sink was already seeded.
pub(in crate::backend) fn start(
    sink: ChangeSink,
    snapshot: fn() -> Vec<OsDevice>,
    interval: Duration,
) -> Result<PollWatch> {
    let failures = sink.clone();
    let mut reporter = Reporter::new(sink);
    reporter.seed(snapshot())?;
    let thread = spawn_named("tutti-devwatch", None, move |stop| {
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
    .map_err(|source| MidiHardwareError::Spawn {
        thread: "tutti-devwatch",
        source,
    })?;
    Ok(PollWatch {
        thread: Some(thread),
    })
}

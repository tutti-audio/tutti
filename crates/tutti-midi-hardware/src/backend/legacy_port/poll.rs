//! A device watch for an OS that has no notification: list again on a timer.

use std::time::Duration;

use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::Watch;
use crate::error::Result;
use tutti_midi_device::OsDevice;
use tutti_platform::{Executor, Instant, Task, Timer};

/// How often the list is read again: a device plugged in shows within this.
pub(in crate::backend) const INTERVAL: Duration = Duration::from_secs(1);

/// A running poll: a task on the executor. Dropping it cancels the task, so no
/// report reaches the sink after a look under way ends.
pub(in crate::backend) struct PollWatch {
    _task: Task<()>,
}

impl Watch for PollWatch {}

/// Seeds `sink` with `snapshot()` before it returns, then reports each
/// difference between looks every `interval`, from a task on `executor`, until
/// the watch drops or the executor shuts down. A panic in `snapshot` ends the
/// watch and says so through [`ChangeSink::fail`].
///
/// # Errors
///
/// [`MidiHardwareError::Seed`](crate::MidiHardwareError::Seed) if the sink was already seeded.
pub(in crate::backend) fn start(
    executor: &Executor,
    sink: ChangeSink,
    snapshot: fn() -> Vec<OsDevice>,
    interval: Duration,
) -> Result<PollWatch> {
    let failures = sink.clone();
    let mut reporter = Reporter::new(sink);
    reporter.seed(snapshot())?;
    let task = executor.spawn(run(
        executor.timer(),
        reporter,
        failures,
        snapshot,
        interval,
    ));
    Ok(PollWatch { _task: task })
}

/// Looks every `interval` and reports the difference, until a look panics or the
/// executor behind `timer` shuts down. A `Timer`'s sleep resolves at once after
/// the shutdown, before its deadline: that ends the poll, so it never spins.
pub(in crate::backend) async fn run(
    timer: Timer,
    mut reporter: Reporter,
    failures: ChangeSink,
    snapshot: fn() -> Vec<OsDevice>,
    interval: Duration,
) {
    loop {
        let deadline = Instant::now() + interval;
        timer.sleep_until(deadline).await;
        if Instant::now() < deadline {
            return;
        }
        let look = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reporter.report(snapshot(), |_| false);
        }));
        if look.is_err() {
            failures.fail(crate::WatchFailure::Panicked);
            return;
        }
    }
}

//! The probe on a platform that starts processes: `plugin-probe` is spawned, and its
//! exit is awaited as an event, not polled.
//!
//! Each look at the child races two wakes. The exit is a `oneshot` a dedicated thread
//! fills when the OS says the child may have ended ([`ExitWait`]); the deadline is a
//! [`Sleep`](tutti_platform::Sleep) the executor fires. The scan reads the [`Deadline`]
//! against [`Instant::now`] to decide a timeout, so the sleep only wakes the race.

use crate::scanner::ProbeListing;
use futures_channel::oneshot;
use futures_lite::future::{or, pending, yield_now};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::{Instant, WeakExecutor, monotonic_ns, sleep_until_ns};
use tutti_plugin_ipc_ffi::{ExitWait, HOST_PID_ENV, Reaped, Spawn, SpawnError};
use tutti_plugin_types::{
    Deadline, Helper, Locator, PluginFormat, ProbeDeath, ProbeError, ProbeOutcome, ProbeRefusal,
    ResultError, read_result,
};

/// How long the exit thread waits on the OS between looks at its receiver: a
/// dropped wait ends within one slice, and the end of the child wakes it at once.
const EXIT_SLICE: Duration = Duration::from_millis(100);

/// The shortest slice: a deadline that is up leaves the thread a millisecond to hand
/// the executor its due sleeps, not a spin.
const MIN_SLICE: Duration = Duration::from_millis(1);

/// Spawns `plugin-probe` on `plugin_path` and reads what it wrote. The probe ends
/// with the future: dropped, it kills and reaps the child ([`Reaped`]).
pub(super) async fn run(
    executor: &WeakExecutor,
    locator: &Locator,
    plugin_path: &Path,
    format: PluginFormat,
    timeout: Duration,
    flags: &[&OsStr],
) -> Result<ProbeListing, ProbeError> {
    let probe = locator
        .resolve(Helper::Probe)
        .ok_or(ProbeError::ProbeNotFound)?;
    // A directory of the probe's own (mode 0700), removed when it drops, on a
    // panic as well: the result path is not a name in a shared directory.
    let dir = tempfile::Builder::new()
        .prefix("tutti-probe-")
        .tempdir()
        .map_err(|e| ProbeError::Io(e.kind()))?;
    let result = dir.path().join("result");

    let mut command = Command::new(&probe);
    command
        .arg(plugin_path)
        .arg(&result)
        // The format the caller knows the file by, which a link's target need
        // not name in its own extension.
        .args(["--as", format.extension_id()])
        .args(flags)
        // The probe ends with this process: a plugin hung in its init does not
        // outlive its host.
        .env(HOST_PID_ENV, std::process::id().to_string());
    // The probe loads plugin code: it gets the standard descriptors and nothing else of the host's.
    let mut child = tutti_plugin_ipc_ffi::spawn(command, Spawn::Helper).map_err(|e| match e {
        SpawnError::Start(e) | SpawnError::NotBound(e) => ProbeError::Io(e.kind()),
    })?;
    let deadline = Deadline::after(Instant::now(), timeout);
    let status = wait(&mut child, executor, deadline).await;
    settle(read_result(&result), status, timeout)
}

/// What a probe's result file and the way its wait ended make of the probe. The file
/// wins whatever the end: it is the only proof of finishing, so a listing written
/// before a crash or a hang at the deadline is the result; without one, the end is
/// the failure.
fn settle(
    file: Result<Option<ProbeOutcome>, ResultError>,
    status: Waited,
    timeout: Duration,
) -> Result<ProbeListing, ProbeError> {
    let outcome = match (file, status) {
        (Ok(Some(outcome)), _) => outcome,
        (Err(e), _) => return Err(ProbeError::Result(e)),
        (Ok(None), Waited::Exited(status)) => return Err(ProbeError::Died(ProbeDeath::of(status))),
        (Ok(None), Waited::TimedOut) => return Err(ProbeError::TimedOut { after: timeout }),
        (Ok(None), Waited::Lost(e)) => return Err(ProbeError::Io(e.kind())),
        (Ok(None), Waited::ShutDown) => return Err(ProbeError::ShutDown),
    };
    match outcome {
        ProbeOutcome::Described { plugins, .. } if plugins.is_empty() => {
            Err(ProbeRefusal::Empty.into())
        }
        ProbeOutcome::Described {
            plugins,
            unmeasured,
        } => Ok(ProbeListing {
            plugins,
            unmeasured,
        }),
        ProbeOutcome::Failed(refusal) => Err(refusal.into()),
        // `ProbeOutcome` is `#[non_exhaustive]`: an outcome from a newer probe.
        _ => Err(ResultError::UnknownOutcome.into()),
    }
}

enum Waited {
    Exited(ExitStatus),
    TimedOut,
    ShutDown,
    Lost(std::io::Error),
}

/// What woke one look at the child.
enum Wake {
    Exit,
    Deadline,
    ShutDown,
}

/// Waits for `child` to exit, killing and reaping it at `deadline` or when the
/// executor shuts down.
///
/// Each look at the child spawns one `plugin-scan-exit` thread task that fills a
/// `oneshot` when the OS wakes it, and races it against a sleep to the deadline.
/// The executor is upgraded only to spawn and to take its timer, so a scan waiting
/// on a probe does not keep the executor alive.
async fn wait(child: &mut Reaped, executor: &WeakExecutor, deadline: Deadline) -> Waited {
    let exit = Arc::new(child.exit_wait());
    // The first wait only says "may have ended"; the look below reads the status.
    let _ = exit.wait(Duration::ZERO, pause);
    loop {
        let now = Instant::now();
        if let Some(waited) = verdict(child.try_reap(), deadline, now) {
            return finish(child, waited);
        }
        let Some(live) = executor.upgrade() else {
            return finish(child, Waited::ShutDown);
        };
        let (ended, exited) = oneshot::channel();
        let watch = live.spawn_dedicated(
            "plugin-scan-exit",
            None,
            watch_exit(Arc::clone(&exit), ended, deadline),
        );
        let timer = live.timer();
        drop(live);
        // Dropped with the look: it cancels the thread's future.
        let _watch = match watch {
            Ok(watch) => watch,
            Err(e) => return finish(child, Waited::Lost(e)),
        };
        let sleep = deadline.left(now).map(|left| timer.sleep(left));
        let woke = or(
            async {
                match exited.await {
                    Ok(()) => Wake::Exit,
                    // The thread's future ended unsent: the executor shut down.
                    Err(oneshot::Canceled) => Wake::ShutDown,
                }
            },
            async {
                match sleep {
                    Some(sleep) => match sleep.await {
                        Ok(()) => Wake::Deadline,
                        Err(_) => Wake::ShutDown,
                    },
                    None => pending().await,
                }
            },
        )
        .await;
        match woke {
            // Looked at again from the top: the status, then the deadline.
            Wake::Exit | Wake::Deadline => {}
            Wake::ShutDown => return finish(child, Waited::ShutDown),
        }
    }
}

/// What one look at the child, at `now`, settles: its exit (even at the deadline: a
/// probe that finished at the deadline did), a failure to look, or the deadline;
/// `None` while the child runs within it.
fn verdict(
    looked: std::io::Result<Option<ExitStatus>>,
    deadline: Deadline,
    now: Instant,
) -> Option<Waited> {
    match looked {
        Ok(Some(status)) => Some(Waited::Exited(status)),
        Err(e) => Some(Waited::Lost(e)),
        Ok(None) if deadline.passed(now) => Some(Waited::TimedOut),
        Ok(None) => None,
    }
}

/// `waited`, after the child is killed and reaped unless it ended by itself.
fn finish(child: &mut Reaped, waited: Waited) -> Waited {
    if !matches!(waited, Waited::Exited(_)) {
        let _ = child.kill_all();
        let _ = child.wait();
    }
    waited
}

/// The exit thread's task: fills `ended` once the OS says the child may have ended.
/// It waits in slices, so a dropped receiver (the scan moved on) or the deadline
/// returns it to the executor, which fires the due sleeps.
async fn watch_exit(exit: Arc<ExitWait>, ended: oneshot::Sender<()>, deadline: Deadline) {
    while !ended.is_canceled() {
        let slice = deadline
            .left(Instant::now())
            .map_or(EXIT_SLICE, |left| left.clamp(MIN_SLICE, EXIT_SLICE));
        if exit.wait(slice, pause) {
            let _ = ended.send(());
            return;
        }
        yield_now().await;
    }
}

/// Pauses the exit thread for `slice`: what an OS with no wake for a process's end
/// waits between looks.
fn pause(slice: Duration) {
    let nanos = i64::try_from(slice.as_nanos()).unwrap_or(i64::MAX);
    sleep_until_ns(monotonic_ns().saturating_add(nanos));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// **The deadline rule over constructed instants**: a running child is timed out
    /// at its deadline and after, never before; an exit is told whatever the clock
    /// says, and a failure to look ends the wait.
    #[test]
    fn a_running_child_times_out_at_the_deadline_and_after_not_before() {
        let start = Instant::now();
        let timeout = Duration::from_secs(15);
        let deadline = Deadline::after(start, timeout);
        let running = || Ok(None);
        assert!(verdict(running(), deadline, start).is_none());
        assert!(
            verdict(
                running(),
                deadline,
                start + timeout - Duration::from_nanos(1)
            )
            .is_none()
        );
        for at in [timeout, timeout + Duration::from_secs(1)] {
            assert!(matches!(
                verdict(running(), deadline, start + at),
                Some(Waited::TimedOut)
            ));
        }
        let exited = ExitStatus::from_raw(0);
        assert!(matches!(
            verdict(Ok(Some(exited)), deadline, start + timeout),
            Some(Waited::Exited(_))
        ));
        let lost = std::io::Error::other("no status");
        assert!(matches!(
            verdict(Err(lost), deadline, start),
            Some(Waited::Lost(_))
        ));
        assert!(verdict(running(), Deadline::never(), start + timeout).is_none());
    }

    /// **A listing written before a hang or a crash is the result**: the file wins
    /// over a timeout, a signal and a shut-down alike, and without one a timeout is
    /// `TimedOut` after the caller's deadline.
    #[test]
    fn a_listing_wins_over_how_the_wait_ended() {
        use tutti_plugin_types::{Described, PluginClass, PluginDescriptor, PluginId};
        let timeout = Duration::from_secs(15);
        let plugins = vec![Described::new(
            PluginId::Clap("a.plugin".into()),
            PluginDescriptor::new("A", PluginClass::Unknown),
        )];
        let written = || {
            Ok(Some(ProbeOutcome::Described {
                plugins: plugins.clone(),
                unmeasured: Vec::new(),
            }))
        };
        let ends = || {
            [
                Waited::TimedOut,
                Waited::Exited(ExitStatus::from_raw(11)),
                Waited::ShutDown,
            ]
        };
        for end in ends() {
            assert_eq!(settle(written(), end, timeout).unwrap().plugins, plugins);
        }
        assert_eq!(
            settle(Ok(None), Waited::TimedOut, timeout).unwrap_err(),
            ProbeError::TimedOut { after: timeout }
        );
    }
}

//! Watching CoreMIDI for devices that come and go.
//!
//! CoreMIDI delivers a client's notifications on the run loop of the thread
//! that created it. One `SharedClient` on one thread, started on first use and
//! never ended, serves every watch: a client per `Devices` would accumulate,
//! since a CoreMIDI client is never disposed. A watch is a registration with
//! that hub, and dropping it removes the registration.
//!
//! On a notification that the device setup changed, each registered watch
//! enumerates again and reports the difference, as the ALSA watch does.
//!
//! CoreMIDI sends a client's notifications to the run loop that was current when
//! the process created its *first* client, whichever thread creates later ones.
//! Measured on GitHub's macOS runner: with the hub's client first, its callback
//! fired on the hub thread for every endpoint added or removed in the process
//! (13 fires in a few seconds); with another client created first, on the
//! calling thread, it never fired in four seconds, and fired when that thread
//! ran its own run loop. So the hub starts before any other client the backend
//! creates (`hub_first`, called by both creation sites, starts it), and a host
//! that creates a client of its own earlier still takes the notifications. The hub thread therefore also looks
//! again once a second whenever its loop has nothing to wait for (a loop with no
//! source returns at once), which finds a hot-plug in about a second in that
//! case. Nothing here has been run on a Mac with a main run loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_rt::LockExt;

use core_foundation::runloop::CFRunLoop;

use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::{NoWatch, Watch};
use crate::error::{MidiHardwareError, MidiThread, Result};
use tutti_coremidi_ffi::SharedClient;
use tutti_midi_device::OsDevice;
use tutti_platform::spawn_named;

/// A run loop that returns sooner than this had nothing to wait for. A loop that
/// has a source but was stopped (`CFRunLoopStop`, which nothing here sends)
/// would look the same; the cost is a rest and one extra look, which is
/// harmless because a look reports only a difference.
const IDLE_RETURN: Duration = Duration::from_millis(50);
/// How long the hub thread rests before it runs the loop again after such a
/// return, so a loop with no source is four wakes a second and not a spin.
const IDLE_REST: Duration = Duration::from_millis(250);
/// How often a hub whose run loop has no source looks again by itself: the
/// fallback for a process whose first CoreMIDI client was not the hub's.
const IDLE_LOOK: Duration = Duration::from_secs(1);

/// When a hub whose run loop has no source looks by itself: every method takes
/// the time it is given.
struct IdleLooks {
    looked: Instant,
}

impl IdleLooks {
    fn new(now: Instant) -> Self {
        Self { looked: now }
    }

    /// Whether a run of the loop that lasted `ran` had nothing to wait for.
    fn is_idle(ran: Duration) -> bool {
        ran < IDLE_RETURN
    }

    /// Whether a look is due at `now`, after a rest; when it is, it is taken.
    fn look_due(&mut self, now: Instant) -> bool {
        let due = now.saturating_duration_since(self.looked) >= IDLE_LOOK;
        if due {
            self.looked = now;
        }
        due
    }

    /// The loop waited for a source and returned at `now`: notifications reach
    /// it, so the timer starts over.
    fn waited(&mut self, now: Instant) {
        self.looked = now;
    }
}

/// One registered watch: the reporting state of a `Devices`, and the sink to
/// tell if it fails.
struct Watcher {
    reporter: Reporter,
    snapshot: fn() -> Vec<OsDevice>,
    sink: ChangeSink,
    /// False once its `Devices` has dropped.
    live: bool,
}

type Shared = Arc<Mutex<Watcher>>;

/// Looks again at CoreMIDI and reports what changed. A panic does not unwind
/// into CoreMIDI's block: it fails the watch.
///
/// The hub's own lock is not held here. The watcher's enumeration calls into
/// CoreMIDI, and a thread holding the hub lock across such a call would block
/// the notification callback that CoreMIDI may be waiting on.
fn look(watcher: &Shared) {
    let mut watcher = watcher.lock_recover();
    if !watcher.live {
        return;
    }
    let Watcher {
        reporter, snapshot, ..
    } = &mut *watcher;
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reporter.report(snapshot(), |_| false);
    }));
    if run.is_err() {
        watcher.live = false;
        watcher.sink.fail(crate::WatchFailure::Panicked);
    }
}

/// The watches registered with the process-wide client.
pub(super) struct Hub {
    watches: Mutex<Vec<(u64, Shared)>>,
    next: AtomicU64,
}

impl Hub {
    fn watches(&self) -> MutexGuard<'_, Vec<(u64, Shared)>> {
        self.watches.lock_recover()
    }

    /// Has every registered watch look again.
    fn notify(&self) {
        let all: Vec<Shared> = self.watches().iter().map(|(_, w)| Arc::clone(w)).collect();
        for watcher in &all {
            look(watcher);
        }
    }
}

/// The hub and its client, started on first use, or why there is none (CoreMIDI
/// refused the client). A refusal is not kept: the next watch tries again.
fn started() -> Result<(&'static Hub, SharedClient)> {
    static HUB: Mutex<Option<(&'static Hub, SharedClient)>> = Mutex::new(None);
    let mut slot = HUB.lock_recover();
    if let Some(started) = *slot {
        return Ok(started);
    }
    let started = start_hub()?;
    *slot = Some(started);
    Ok(started)
}

/// The hub, started on first use.
pub(super) fn hub() -> Result<&'static Hub> {
    started().map(|(hub, _)| hub)
}

/// The process's CoreMIDI client, made by the hub thread on first use.
pub(super) fn client() -> Result<SharedClient> {
    started().map(|(_, client)| client)
}

fn start_hub() -> Result<(&'static Hub, SharedClient)> {
    {
        let hub: &'static Hub = Box::leak(Box::new(Hub {
            watches: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }));
        let (ready, started) = mpsc::channel();
        // The hub thread lives as long as the process: no owner stops it.
        spawn_named(MidiThread::DeviceWatch.name(), None, move |_| {
            let client = SharedClient::start("tutti-midi-watch", move |notification| {
                if notification.changes_devices() {
                    hub.notify();
                }
            });
            let _ = ready.send(client);
            // The client lives as long as the process.
            if client.is_err() {
                return;
            }
            let mut looks = IdleLooks::new(Instant::now());
            loop {
                let started = Instant::now();
                CFRunLoop::run_current();
                if IdleLooks::is_idle(started.elapsed()) {
                    // The loop had nothing to wait for: no notification
                    // can reach it, so look on a timer instead.
                    std::thread::sleep(IDLE_REST);
                    if looks.look_due(Instant::now()) {
                        hub.notify();
                    }
                } else {
                    looks.waited(Instant::now());
                }
            }
        })
        .map_err(|source| MidiHardwareError::Spawn {
            thread: MidiThread::DeviceWatch,
            source,
        })?
        .detach();
        match started.recv() {
            Ok(Ok(client)) => Ok((hub, client)),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(MidiHardwareError::ThreadEnded {
                thread: MidiThread::DeviceWatch,
            }),
        }
    }
}

/// A registration with the hub. Dropping it removes the registration, and
/// returns once a look in progress has finished (it takes the watch's own lock,
/// which an enumeration holds): nothing is reported after, and a drop can wait
/// for one enumeration.
struct CoreMidiWatch {
    hub: &'static Hub,
    key: u64,
    watcher: Shared,
}

impl Watch for CoreMidiWatch {}

impl Drop for CoreMidiWatch {
    fn drop(&mut self) {
        self.hub.watches().retain(|(key, _)| *key != self.key);
        self.watcher.lock_recover().live = false;
    }
}

/// Seeds `sink` with `snapshot()`, then reports it again on each notification
/// that the device setup changed, until the returned watch drops. Where CoreMIDI
/// refuses a client the watch reports nothing, as enumeration lists nothing, and
/// says so through [`ChangeSink::fail`].
pub(super) fn start(sink: ChangeSink, snapshot: fn() -> Vec<OsDevice>) -> Result<Box<dyn Watch>> {
    let hub = match hub() {
        Ok(hub) => hub,
        Err(why) => {
            sink.fail(why);
            return Ok(Box::new(NoWatch));
        }
    };
    let failures = sink.clone();
    let mut reporter = Reporter::new(sink);
    reporter.seed(snapshot())?;
    let watcher: Shared = Arc::new(Mutex::new(Watcher {
        reporter,
        snapshot,
        sink: failures,
        live: true,
    }));
    let key = hub.next.fetch_add(1, Ordering::Relaxed);
    hub.watches().push((key, Arc::clone(&watcher)));
    // A change between the seed and the registration sent no notification this
    // watch could act on; look once more.
    look(&watcher);
    Ok(Box::new(CoreMidiWatch { hub, key, watcher }))
}

/// How many watches are registered, for the test that one drops.
#[cfg(test)]
pub(super) fn registered() -> usize {
    hub().map_or(0, |hub| hub.watches().len())
}

#[cfg(test)]
mod tests;

//! Watching CoreMIDI for devices that come and go, and the process's one client.
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
//! creates (the hub makes it on a thread of its own, and every endpoint and port is
//! created on it once it is made),
//! and a host that creates a client of its own earlier still takes the
//! notifications. The hub thread therefore runs its loop with
//! `tutti_coremidi_ffi::run_notifications`, which, only where the loop has no
//! notification source, looks again once a second on a timer; that finds a hot-plug
//! in about a second in that case. Nothing here has been run on a Mac with a main
//! run loop.
//!
//! # Starting
//!
//! The hub's thread makes the client with a polled `StartClient`: it polls, and parks
//! until the start's deadline when CoreMIDI's server is not answering (it may take
//! about five seconds to come back). The wait of everyone else is a [`Start`]: a
//! future the thread wakes when the client is made or the start failed. Nothing waits
//! on the hub while holding a lock, and the client is made on the hub thread only,
//! never on a caller's thread or inside a poll. A watch registered before the client
//! is made is seeded when it is; a failed start fails every watch registered, and the
//! next [`Start`] asks the thread for a new one.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::Poll;
use std::time::Duration;
use tutti_rt::{LockExt, Unparker, parker};

use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::{NoWatch, Watch};
use crate::error::{MidiHardwareError, MidiThread, Result};
use crate::start::{Begun, Start, StartGate};
use tutti_coremidi_ffi::{SharedClient, StartClient, run_notifications};
use tutti_midi_device::OsDevice;
use tutti_platform::{Instant, spawn_named};

/// How often a hub whose run loop has no notification source looks again by
/// itself: the fallback for a process whose first CoreMIDI client was not the hub's.
/// A hub whose loop has the source (the usual case) never looks on a timer.
const IDLE_LOOK: Duration = Duration::from_secs(1);

/// One registered watch: the reporting state of a `Devices`, and the sink to
/// tell if it fails.
struct Watcher {
    reporter: Reporter,
    snapshot: fn() -> Vec<OsDevice>,
    sink: ChangeSink,
    /// Whether the device list has been seeded: only once the client exists.
    seeded: bool,
    /// False once its `Devices` has dropped, or the hub failed.
    live: bool,
}

type Shared = Arc<Mutex<Watcher>>;

/// Looks again at CoreMIDI and reports what changed, seeding the watch first if it
/// has not been. A panic does not unwind into CoreMIDI's block: it fails the watch.
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
        reporter,
        snapshot,
        seeded,
        ..
    } = &mut *watcher;
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if *seeded {
            reporter.report(snapshot(), |_| false);
            Ok(())
        } else {
            let seeded_now = reporter.seed(snapshot());
            *seeded = seeded_now.is_ok();
            seeded_now
        }
    }));
    match run {
        Ok(Ok(())) => {}
        Ok(Err(seed)) => {
            watcher.live = false;
            watcher.sink.fail(MidiHardwareError::from(seed));
        }
        Err(_) => {
            watcher.live = false;
            watcher.sink.fail(crate::WatchFailure::Panicked);
        }
    }
}

/// Fails `watcher` with the error the hub's start ended in.
fn fail(watcher: &Shared, error: &Arc<MidiHardwareError>) {
    let mut watcher = watcher.lock_recover();
    if watcher.live {
        watcher.live = false;
        watcher
            .sink
            .fail(crate::WatchFailure::Failed(Arc::clone(error)));
    }
}

/// The watches registered with the process-wide client, and the state of its start.
pub(super) struct Hub {
    watches: Mutex<Vec<(u64, Shared)>>,
    next: AtomicU64,
    /// The state of the start and the futures waiting on it.
    gate: Arc<StartGate>,
    /// The client, set once, before the gate says ready.
    client: OnceLock<SharedClient>,
    /// Wakes the hub thread; `None` while there is no thread.
    unparker: Mutex<Option<Unparker>>,
}

impl Hub {
    fn new() -> Self {
        Self {
            watches: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
            gate: Arc::new(StartGate::new()),
            client: OnceLock::new(),
            unparker: Mutex::new(None),
        }
    }

    fn watches(&self) -> MutexGuard<'_, Vec<(u64, Shared)>> {
        self.watches.lock_recover()
    }

    fn all(&self) -> Vec<Shared> {
        self.watches().iter().map(|(_, w)| Arc::clone(w)).collect()
    }

    /// Has every registered watch look again.
    fn notify(&self) {
        for watcher in &self.all() {
            look(watcher);
        }
    }

    /// The client, once made; why there is none yet otherwise.
    pub(super) fn client(&self) -> Result<SharedClient> {
        self.client.get().copied().ok_or_else(|| {
            self.gate
                .failure()
                .map_or(MidiHardwareError::BackendStarting, |error| {
                    MidiHardwareError::BackendFailed(error)
                })
        })
    }

    /// Starts the hub thread if there is none (and no client to keep).
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Spawn`] when the OS refuses the thread; nothing is kept,
    /// so the next call tries again.
    fn ensure_thread(&'static self) -> Result<()> {
        if self.client.get().is_some() {
            return Ok(());
        }
        let mut slot = self.unparker.lock_recover();
        if slot.is_some() {
            return Ok(());
        }
        let (unparker, parker) = parker();
        // The hub is a process-static CoreMIDI client whose notification loop never
        // returns, so its thread outlives every `Executor`, and no owner stops it
        // (`check-threads` exempts this file).
        spawn_named(MidiThread::DeviceWatch.name(), None, move |_| {
            let _ended = EndGuard(self);
            self.run(parker);
        })
        .map_err(|source| MidiHardwareError::Spawn {
            thread: MidiThread::DeviceWatch,
            source,
        })?
        .detach();
        *slot = Some(unparker);
        Ok(())
    }

    /// The hub thread ended, however it did: a start still under way failed, and
    /// the next start makes a thread again.
    fn thread_ended(&self) {
        self.gate.ended();
        *self.unparker.lock_recover() = None;
    }

    /// What the hub thread does: makes the client for each start asked of it.
    fn run(&'static self, mut parker: tutti_rt::Parker) {
        let mut done = None;
        loop {
            let (generation, carried) = loop {
                if let Some(wanted) = self.gate.wanted(done) {
                    break wanted;
                }
                parker.park(|| self.gate.wanted(done).is_some());
            };
            done = Some(generation);
            let hub = self;
            let started = StartClient::new(
                "tutti-midi-watch",
                move |notification| {
                    if notification.changes_devices() {
                        hub.notify();
                    }
                },
                Instant::now(),
                carried,
            );
            let mut start = match started {
                Ok(start) => start,
                Err(error) => {
                    self.fail_start(error, carried);
                    continue;
                }
            };
            let made = loop {
                match start.poll(Instant::now()) {
                    Poll::Ready(made) => break made,
                    Poll::Pending => parker.park_timeout(
                        || false,
                        start.deadline().saturating_duration_since(Instant::now()),
                    ),
                }
            };
            match made {
                Ok(client) => {
                    self.publish_client(client);
                    // The client lives as long as the process. The loop runs for the
                    // thread's life; where it has no notification source, its timer
                    // has the watches look.
                    run_notifications(client, IDLE_LOOK, move || hub.notify());
                    return;
                }
                Err(error) => self.fail_start(error, start.gave_up_at().or(carried)),
            }
        }
    }

    /// Records the client, seeds every watch registered, and only then says ready
    /// and wakes the futures: a caller that awaited the start finds the list seeded.
    fn publish_client(&self, client: SharedClient) {
        let _ = self.client.set(client);
        self.notify();
        self.gate.publish_ready();
    }

    /// The start failed: say so and wake the futures, then fail the watches.
    fn fail_start(&self, error: tutti_coremidi_ffi::Error, gave_up: Option<Instant>) {
        let error = Arc::new(MidiHardwareError::Os(error));
        self.gate.publish_failed(Arc::clone(&error), gave_up);
        for watcher in &self.all() {
            fail(watcher, &error);
        }
    }
}

/// Reports the hub thread's end, on its return or its unwind.
struct EndGuard(&'static Hub);

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.0.thread_ended();
    }
}

fn cell() -> &'static Hub {
    static HUB: OnceLock<&'static Hub> = OnceLock::new();
    HUB.get_or_init(|| Box::leak(Box::new(Hub::new())))
}

/// The hub, with its thread running (made on first use). Returns at once.
///
/// # Errors
///
/// [`MidiHardwareError::Spawn`] when the OS refuses the thread.
pub(super) fn hub() -> Result<&'static Hub> {
    let hub = cell();
    hub.ensure_thread()?;
    Ok(hub)
}

/// The wait for the hub's client: a future the hub thread wakes. A start that
/// failed is asked of the thread again.
pub(super) fn begin_start() -> Start {
    let hub = match hub() {
        Ok(hub) => hub,
        Err(error) => return Start::failed(error),
    };
    let begun: Begun = hub.gate.begin();
    if begun.restarted
        && let Some(unparker) = hub.unparker.lock_recover().as_ref()
    {
        unparker.unpark();
    }
    Start::over(Arc::clone(&hub.gate), begun)
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

/// Registers `sink` with the hub: it is seeded with `snapshot()` once the hub's
/// client exists (at once, if it does), then reports it again on each notification
/// that the device setup changed, until the returned watch drops. Returns at once.
/// Where CoreMIDI refuses a client the watch reports nothing, as enumeration lists
/// nothing, and says so through [`ChangeSink::fail`].
pub(super) fn start(sink: ChangeSink, snapshot: fn() -> Vec<OsDevice>) -> Result<Box<dyn Watch>> {
    let hub = match hub() {
        Ok(hub) => hub,
        Err(why) => {
            sink.fail(why);
            return Ok(Box::new(NoWatch));
        }
    };
    let watcher: Shared = Arc::new(Mutex::new(Watcher {
        reporter: Reporter::new(sink.clone()),
        snapshot,
        sink,
        seeded: false,
        live: true,
    }));
    let key = hub.next.fetch_add(1, Ordering::Relaxed);
    hub.watches().push((key, Arc::clone(&watcher)));
    // The hub seeds the watches registered when it makes the client, and fails them
    // when it gives up; one that registers after either looks at what it missed.
    if hub.client.get().is_some() {
        look(&watcher);
    }
    if let Some(error) = hub.gate.failure() {
        fail(&watcher, &error);
    }
    Ok(Box::new(CoreMidiWatch { hub, key, watcher }))
}

/// How many watches are registered, for the test that one drops.
#[cfg(test)]
pub(super) fn registered() -> usize {
    hub().map_or(0, |hub| hub.watches().len())
}

#[cfg(test)]
mod tests;

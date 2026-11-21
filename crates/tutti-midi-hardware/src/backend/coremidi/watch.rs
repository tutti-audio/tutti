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
//! is made is seeded when it is. A failed start ends no watch: the watches stay
//! registered and unseeded, the failure reaches the callers through their [`Start`],
//! and the next [`Start`] asks the thread for a new client, which seeds them.
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::Duration;
use tutti_rt::{LockExt, Parker, Unparker, parker};

use crate::backend::hub::{Registry, StartGate, failed_start};
use crate::devices::ChangeSink;
use crate::endpoints::{NoWatch, Watch};
use crate::error::{MidiHardwareError, MidiThread, Result};
use crate::start::Start;
use tutti_coremidi_ffi::{SharedClient, StartClient, run_notifications};
use tutti_midi_device::OsDevice;
use tutti_platform::{Instant, spawn_named};

/// How often a hub whose run loop has no notification source looks again by
/// itself: the fallback for a process whose first CoreMIDI client was not the hub's.
/// A hub whose loop has the source (the usual case) never looks on a timer.
const IDLE_LOOK: Duration = Duration::from_secs(1);

/// The watches registered with the process-wide client, and the state of its start.
pub(super) struct Hub {
    registry: Arc<Registry>,
    /// The state of the start and the futures waiting on it.
    gate: Arc<StartGate>,
    /// The client, set once, before the gate says ready.
    client: OnceLock<SharedClient>,
    /// Wakes the hub thread; `None` while there is no thread. Its lock orders a
    /// thread's end against a future asking for a new round: both run under it.
    unparker: Mutex<Option<Unparker>>,
}

impl Hub {
    fn new() -> Self {
        Self {
            registry: Arc::new(Registry::new()),
            gate: Arc::new(StartGate::new()),
            client: OnceLock::new(),
            unparker: Mutex::new(None),
        }
    }

    /// Starts the hub thread if there is none (and no client to keep), under the
    /// lock `slot` holds.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Spawn`] when the OS refuses the thread; nothing is kept,
    /// so the next call tries again.
    #[expect(
        clippy::disallowed_methods,
        reason = "the CoreMIDI hub: it makes the process-static client on a thread of its own that \
                  no Executor owns, and the client's notification loop outlives every Executor"
    )]
    fn ensure_thread(&'static self, slot: &mut Option<Unparker>) -> Result<()> {
        if self.client.get().is_some() || slot.is_some() {
            return Ok(());
        }
        let (unparker, parker) = parker();
        // The thread makes the process-static CoreMIDI client and then runs its
        // notification loop, which outlives every `Executor`; no owner stops it.
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

    /// The hub thread ended, however it did: a round still under way is over (the
    /// client exists, or the round failed), and the next start makes a thread again.
    fn thread_ended(&self) {
        let wakers = {
            let mut slot = self.unparker.lock_recover();
            let wakers = self.gate.ended(self.client.get().is_some());
            *slot = None;
            wakers
        };
        // Woken with no lock held: a waker may call back into the hub.
        wakers.wake();
    }

    /// What the hub thread does: makes the client for each round asked of it.
    fn run(&'static self, mut parker: Parker) {
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
                        hub.registry.look_all();
                    }
                },
                Instant::now(),
                carried,
            );
            let mut start = match started {
                Ok(start) => start,
                Err(error) => {
                    self.fail_round(error, carried);
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
                    run_notifications(client, IDLE_LOOK, move || hub.registry.look_all());
                    return;
                }
                Err(error) => self.fail_round(error, start.gave_up_at().or(carried)),
            }
        }
    }

    /// Records the client, seeds every watch registered, and only then says ready
    /// and wakes the futures: a caller that awaited the start finds the list seeded.
    fn publish_client(&self, client: SharedClient) {
        let _ = self.client.set(client);
        self.registry.look_all();
        self.gate.publish_ready().wake();
    }

    /// The round failed: the futures waiting on it are woken with the error. The
    /// watches stay registered and are seeded by a later round that makes the client.
    fn fail_round(&self, error: tutti_coremidi_ffi::Error, gave_up: Option<Instant>) {
        self.gate
            .publish_failed(Arc::new(MidiHardwareError::Os(error)), gave_up)
            .wake();
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
    hub.ensure_thread(&mut hub.unparker.lock_recover())?;
    Ok(hub)
}

/// The process's client, once the hub made it.
pub(super) fn client() -> Option<SharedClient> {
    cell().client.get().copied()
}

/// The wait for the hub's client: a future the hub thread wakes. After a failed
/// round it asks the thread for a new one. The thread's end and this run under one
/// lock, so a round is never handed to a thread that is ending.
pub(super) fn begin_start() -> Start {
    let hub = cell();
    let begun = {
        let mut slot = hub.unparker.lock_recover();
        hub.ensure_thread(&mut slot).map(|()| {
            let (round, wakers) = hub.gate.begin(hub.client.get().is_some());
            if round.restarted
                && let Some(unparker) = slot.as_ref()
            {
                unparker.unpark();
            }
            (round, wakers)
        })
    };
    // Woken with no lock held: a waker may call back into the hub.
    match begun {
        Err(error) => failed_start(error),
        Ok((round, wakers)) => {
            wakers.wake();
            hub.gate.start(round)
        }
    }
}

/// Registers `sink` with the hub: it is seeded with `snapshot()` once the hub's
/// client exists (at once, if it does), then reports it again on each notification
/// that the device setup changed, until the returned watch drops. Returns at once.
/// The watch stays registered if a start fails, and is seeded by the start that
/// makes the client.
pub(super) fn start(sink: ChangeSink, snapshot: fn() -> Vec<OsDevice>) -> Result<Box<dyn Watch>> {
    let hub = match hub() {
        Ok(hub) => hub,
        Err(why) => {
            sink.fail(why);
            return Ok(Box::new(NoWatch));
        }
    };
    Ok(hub
        .registry
        .register(sink, snapshot, || hub.client.get().is_some()))
}

/// How many watches are registered, for the test that one drops.
#[cfg(test)]
pub(super) fn registered() -> usize {
    hub().map_or(0, |hub| hub.registry.len())
}

#[cfg(test)]
mod tests;

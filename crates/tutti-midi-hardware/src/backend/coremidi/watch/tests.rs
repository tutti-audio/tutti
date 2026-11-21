//! `Devices` against CoreMIDI: a virtual endpoint is a device to every
//! client, so creating and dropping one is a hot-plug.

use std::time::Duration;

use tutti_platform::Instant;

use super::*;
use crate::Devices;
use crate::backend::coremidi::tests::plain;
use crate::endpoints::MidiEndpoints;
use crate::virtual_endpoint::VirtualProtocol;
use tutti_midi_device::DeviceEvent;

/// `Devices` with one subscription, and the blocking batch `wait` these tests
/// are written around.
struct Watched {
    devices: Devices,
    events: crate::DeviceEvents,
}

impl Watched {
    /// `Devices` whose backend has started: the device list is seeded once the
    /// hub's client is made.
    fn open() -> Self {
        let devices = Devices::open(&tutti_platform::Executor::new()).unwrap();
        tutti_fixtures::block_on(devices.start()).expect("the hub makes its client");
        Self::of(devices)
    }

    fn of(devices: Devices) -> Self {
        let events = devices.subscribe();
        Self { devices, events }
    }

    /// The change that comes before the hang guard `within` and any queued
    /// behind it; none if the guard passes first.
    fn wait(&self, within: Duration) -> Vec<DeviceEvent> {
        tutti_fixtures::block_on_within(self.events.recv(), within)
            .and_then(|received| received.ok())
            .into_iter()
            .chain(self.events.try_iter())
            .collect()
    }
}

impl std::ops::Deref for Watched {
    type Target = Devices;
    fn deref(&self) -> &Devices {
        &self.devices
    }
}

const PATIENCE: Duration = Duration::from_secs(20);

fn until(devices: &Watched, want: impl Fn(&DeviceEvent) -> bool) -> DeviceEvent {
    let deadline = Instant::now() + PATIENCE;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Some(event) = devices.wait(left).into_iter().find(&want) {
            return event;
        }
    }
    panic!(
        "no matching event within {PATIENCE:?}; list: {:?}",
        devices.list()
    );
}

/// A virtual endpoint that appears is `Added` and, dropped, `Removed`; the
/// watch's registration ends with `Devices`. On GitHub's macOS runner CoreMIDI
/// sent the background client no notification, so this passes on the hub's own
/// look (see `watch.rs`); without `hub.notify` in the timer the first wait times out
/// there.
#[test]
fn a_virtual_endpoint_comes_and_goes_and_the_watch_unregisters() {
    let name = format!("tutti-test-{}", std::process::id());
    let devices = Watched::open();
    assert_eq!(registered(), 1);

    let destination = plain(&name, VirtualProtocol::Midi2, |_| {});
    let added = until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.name.contains(&name)),
    );
    drop(destination);
    let id = added.info().id.clone();
    until(
        &devices,
        |e| matches!(e, DeviceEvent::Removed(i) if i.id == id),
    );

    drop(devices);
    assert_eq!(registered(), 0);
}

/// With an output port opened before the watch (a port is made on the process's
/// client like the watch's own registration), a virtual endpoint that appears
/// is still reported `Added`: the change reaches the watch's receiver, which is
/// waited on (`PATIENCE` only guards a hang).
#[test]
fn a_change_is_reported_when_an_output_port_comes_first() {
    let pid = std::process::id();
    let first_name = format!("tutti-port-first-{pid}");
    let first = plain(&first_name, VirtualProtocol::Midi2, |_| {});
    let backend = crate::backend::coremidi::CoreMidiEndpoints::new();
    let started = tutti_fixtures::block_on(backend.start()).expect("the hub makes its client");
    let id = backend
        .outputs()
        .into_iter()
        .find(|e| e.name.contains(&first_name))
        .expect("the virtual endpoint is listed")
        .id;
    let out = backend.open_output(started, id).expect("opens");
    let devices = Watched::open();
    let name = format!("tutti-after-port-{pid}");
    let destination = plain(&name, VirtualProtocol::Midi2, |_| {});
    until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.name.contains(&name)),
    );
    drop((destination, out, first));
}

/// As above, with a virtual endpoint created before the watch: another that
/// appears after the watch opened is still reported.
#[test]
fn a_change_is_reported_when_a_virtual_endpoint_comes_first() {
    let pid = std::process::id();
    let source = plain(
        &format!("tutti-source-{pid}"),
        VirtualProtocol::Midi2,
        |_| {},
    );
    let devices = Watched::open();
    let name = format!("tutti-after-source-{pid}");
    let destination = plain(&name, VirtualProtocol::Midi2, |_| {});
    until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.name.contains(&name)),
    );
    drop((destination, source));
}

/// A future's round, waking whatever the call took.
fn begin(gate: &StartGate) -> crate::backend::hub::Round {
    let (round, wakers) = gate.begin(false);
    wakers.wake();
    round
}

/// **A hub thread that ends before it answers fails the start by name and is
/// replaced**: the start waiting on it resolves to `ThreadEnded` and the next start
/// makes a thread. No thread runs here: the guard's action is `thread_ended`.
#[test]
fn a_hub_thread_that_ends_before_answering_fails_the_start() {
    let hub = Hub::new();
    let start = hub.gate.start(begin(&hub.gate));
    *hub.unparker.lock_recover() = Some(parker().0);
    hub.thread_ended();
    let Err(why) = tutti_fixtures::block_on(start) else {
        panic!("the start fails");
    };
    assert!(matches!(
        why.cause(),
        MidiHardwareError::ThreadEnded {
            thread: MidiThread::DeviceWatch
        }
    ));
    assert!(
        hub.unparker.lock_recover().is_none(),
        "the next start makes a thread"
    );
}

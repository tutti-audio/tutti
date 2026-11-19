//! `Devices` against CoreMIDI: a virtual destination is a device to every
//! client, so creating and dropping one is a hot-plug.

use std::time::Duration;

use tutti_platform::Instant;

use super::*;
use crate::Devices;
use crate::backend::coremidi::UmpVirtualDestination;
use crate::virtual_endpoint::VirtualProtocol;
use tutti_midi_device::DeviceEvent;

/// `Devices` with one subscription, and the blocking batch `wait` these tests
/// are written around.
struct Watched {
    devices: Devices,
    events: crate::DeviceEvents,
}

impl Watched {
    fn open() -> Self {
        Self::of(Devices::open().unwrap())
    }

    fn of(devices: Devices) -> Self {
        let events = devices.subscribe();
        Self { devices, events }
    }

    /// The change that comes within `timeout` and any queued behind it.
    fn wait(&self, timeout: Duration) -> Vec<DeviceEvent> {
        self.events
            .recv_timeout(timeout)
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

/// A virtual destination that appears is `Added` and, dropped, `Removed`; the
/// watch's registration ends with `Devices`. On GitHub's macOS runner CoreMIDI
/// sent the background client no notification, so this passes on the hub's own
/// look (see `watch.rs`); without `hub.notify` in the idle branch (`IDLE_LOOK`
/// never reached) the first wait times out there.
#[test]
fn a_virtual_destination_comes_and_goes_and_the_watch_unregisters() {
    let name = format!("tutti-test-{}", std::process::id());
    let devices = Watched::open();
    assert_eq!(registered(), 1);

    let destination =
        UmpVirtualDestination::new(&name, VirtualProtocol::Midi2, |_| {}).expect("creates");
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

/// With the engine's shared client created first, as `Devices::open` does, a
/// virtual destination that appears is still reported `Added`: the change
/// reaches the watch's receiver, which is waited on (`PATIENCE` only guards a
/// hang).
#[test]
fn a_change_is_reported_when_the_shared_client_is_created_first() {
    let _ = crate::backend::coremidi::shared_client().expect("shared client");
    let devices = Watched::open();
    let name = format!("tutti-callbacks-{}", std::process::id());
    let destination =
        UmpVirtualDestination::new(&name, VirtualProtocol::Midi2, |_| {}).expect("creates");
    until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.name.contains(&name)),
    );
    drop(destination);
}

/// As above, with a virtual source created before the watch: its raw client is
/// the other creation site, and a destination that appears after the watch
/// opened is still reported.
#[test]
fn a_change_is_reported_when_a_virtual_source_comes_first() {
    use crate::backend::coremidi::UmpVirtualSource;
    let pid = std::process::id();
    let source = UmpVirtualSource::new(&format!("tutti-source-{pid}"), VirtualProtocol::Midi2)
        .expect("creates");
    let devices = Watched::open();
    let name = format!("tutti-after-source-{pid}");
    let destination =
        UmpVirtualDestination::new(&name, VirtualProtocol::Midi2, |_| {}).expect("creates");
    until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.name.contains(&name)),
    );
    drop((destination, source));
}

/// **A hub whose loop has no source looks once per `IDLE_LOOK`**: a run shorter
/// than `IDLE_RETURN` is idle, a look is due an interval after the last, and a
/// run that waited starts the interval over.
#[test]
fn an_idle_hub_looks_once_per_interval_and_a_waiting_loop_restarts_it() {
    let t0 = Instant::now();
    assert!(IdleLooks::is_idle(IDLE_RETURN - Duration::from_millis(1)));
    assert!(!IdleLooks::is_idle(IDLE_RETURN));
    let mut looks = IdleLooks::new(t0);
    assert!(!looks.look_due(t0 + IDLE_LOOK - Duration::from_millis(1)));
    assert!(looks.look_due(t0 + IDLE_LOOK));
    assert!(
        !looks.look_due(t0 + IDLE_LOOK + IDLE_REST),
        "taken at the last look"
    );
    assert!(looks.look_due(t0 + IDLE_LOOK * 2));
    looks.waited(t0 + IDLE_LOOK * 2 + Duration::from_millis(10));
    assert!(!looks.look_due(t0 + IDLE_LOOK * 3));
    assert!(looks.look_due(t0 + IDLE_LOOK * 3 + Duration::from_millis(10)));
}

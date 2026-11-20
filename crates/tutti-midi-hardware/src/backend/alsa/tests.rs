//! `Devices` against the real ALSA sequencer. A user-space client with a
//! readable and writable port is a device to every other client, so creating
//! and dropping one is a hot-plug the kernel announces: nothing here is faked.

use std::time::Duration;

use tutti_platform::Instant;

use crate::DeviceOut;
use crate::{Devices, HardwareMidiInputs};
use tutti_alsa_ffi::Client;
use tutti_alsa_ffi::{Address, Event, PortCaps, PortId, PortKind};
use tutti_graph::Losses;
use tutti_midi_device::{DeviceEvent, DeviceId, DeviceInfo, Endpoint, Identity, UMP_VERSION};
use tutti_midi_runtime::DeviceWriters;
use tutti_ump::{
    BlockDirection, EndpointInfoNotification, Field, FirstGroup, FunctionBlockCount,
    FunctionBlockId, FunctionBlockInfo, FunctionBlocks, GroupSpan, Midi1Port, Protocol, UiHint,
};

/// Long enough to be a failure, not a race: the announce arrives at once.
const PATIENCE: Duration = Duration::from_secs(20);

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

    fn with_backend(backend: Box<dyn crate::MidiEndpoints>) -> Self {
        Self::of(Devices::open_over(backend).unwrap())
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

    fn watch_failure(&self) -> Option<crate::WatchFailure> {
        self.events.failure()
    }
}

impl std::ops::Deref for Watched {
    type Target = Devices;
    fn deref(&self) -> &Devices {
        &self.devices
    }
}

/// A device named `name` until dropped.
struct Virtual(Client);

impl Virtual {
    fn rename(&self, name: &str) {
        self.0.rename(name).expect("a name without a NUL");
    }
}

fn plug(name: &str) -> Virtual {
    let seq = super::client::open(
        name,
        tutti_alsa_ffi::Blocking::NonBlock,
        tutti_alsa_ffi::UmpVersion::Midi2,
    )
    .expect("a sequencer to open (modprobe snd-seq)");
    let caps = PortCaps::READ | PortCaps::SUBS_READ | PortCaps::WRITE | PortCaps::SUBS_WRITE;
    seq.open_port("port", caps, PortKind::MIDI_GENERIC | PortKind::APPLICATION)
        .expect("a port");
    Virtual(seq)
}

/// A name no other test process or machine client shares.
fn unique(tag: &str) -> String {
    format!("tutti-test-{}-{tag}", std::process::id())
}

/// Waits until an event satisfies `want`, and returns it. Events about other
/// clients (the machine has its own) pass by.
fn until(devices: &Watched, want: impl Fn(&DeviceEvent) -> bool) -> DeviceEvent {
    until_in(devices, &mut Vec::new(), want)
}

/// As [`until`], keeping in `backlog` the events that arrived with the wanted
/// one, and taking from it first: the next wait finds them.
fn until_in(
    devices: &Watched,
    backlog: &mut Vec<DeviceEvent>,
    want: impl Fn(&DeviceEvent) -> bool,
) -> DeviceEvent {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(i) = backlog.iter().position(&want) {
            return backlog.remove(i);
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        backlog.extend(devices.wait(left));
    }
    panic!(
        "no matching event within {PATIENCE:?}; list: {:?}",
        devices.list()
    );
}

fn added(name: &str) -> impl Fn(&DeviceEvent) -> bool {
    let want = format!("{name}: port");
    move |e| matches!(e, DeviceEvent::Added(i) if i.name == want)
}

fn removed(id: DeviceId) -> impl Fn(&DeviceEvent) -> bool {
    move |e| matches!(e, DeviceEvent::Removed(i) if i.id == id)
}

/// Runs `open` until it succeeds, for an open that lands in the moment a
/// device is being replaced (an exit announcement the sequencer's queue
/// overran is the one thing the watch cannot pair with its look).
fn when_listed<T>(devices: &Watched, open: impl Fn() -> crate::Result<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match open() {
            Ok(opened) => return opened,
            Err(e) => {
                assert!(Instant::now() < deadline, "{e}");
                devices.wait(Duration::from_millis(20));
            }
        }
    }
}

fn open_input_when_listed(
    devices: &Watched,
    id: &DeviceId,
    ports: &std::sync::Arc<HardwareMidiInputs>,
) -> crate::OpenInput {
    when_listed(devices, || {
        devices
            .open_input(id, ports, DeviceWriters::unread())
            .map(|(input, _inlet)| input)
    })
}

fn info(event: DeviceEvent) -> DeviceInfo {
    event.info().clone()
}

/// Waits for the number of watch threads to be `want`. The kernel lists a task
/// from `spawn` but names it only once it runs, and keeps it listed for a
/// moment after `join`, so a count read at once can be one off either way; the
/// wait is bounded, and the thing under test (the thread ends) is `join`'s.
fn settle_watch_threads(want: usize) {
    let deadline = Instant::now() + PATIENCE;
    while watch_threads() != want {
        assert!(
            Instant::now() < deadline,
            "{want} watch threads expected, {} running",
            watch_threads()
        );
        std::thread::yield_now();
    }
}

fn watch_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|t| {
            std::fs::read_to_string(t.path().join("comm"))
                .is_ok_and(|name| name.trim() == crate::MidiThread::DeviceWatch.name())
        })
        .count()
}

// - `watch::settle`'s loop bound `0..0` (the order the watch had before: a drain,
//   a look):
//   `a_device_that_takes_a_departed_clients_number_is_added_once` fails
//   when four copies of it run at once (3 of 4 failed, 12 of 12 pass with the
//   fix); alone and idle the race is not hit and it passes. The review that
//   found the bug saw the extra `Removed` in 139 of 300 cycles.
//
// The watch pairs each look with the announcements read before it (`settle`), and
// a client's start is what says a number has a new occupant, so a reused number
// is one removal and one addition. These tests still read ids from the list or
// retry an open (`when_listed`) where a dropped announcement could matter.

/// A client that appears is listed with its identity.
#[test]
fn a_client_that_appears_is_added_and_listed() {
    let devices = Watched::open();
    let name = unique("appear");
    let _device = plug(&name);
    let device = info(until(&devices, added(&name)));
    assert!(devices.list().contains(&device));
    assert_eq!(device.id.identity(), &Identity::alsa(name.as_str(), "port"));
    assert_eq!(device.id.ordinal(), 0);
}

#[test]
fn a_client_that_exits_is_removed_and_unlisted() {
    let devices = Watched::open();
    let name = unique("vanish");
    let device = plug(&name);
    let id = info(until(&devices, added(&name))).id;

    drop(device);
    until(&devices, removed(id.clone()));
    assert!(devices.list().iter().all(|d| d.id != id));
}

/// On ALSA the identity is the name, so a saved device comes back as a
/// same-name match and never as exact, whether it is the same client or
/// another of that name.
#[test]
fn a_saved_alsa_device_resolves_by_name_only() {
    let devices = Watched::open();
    let name = unique("saved");
    let first = plug(&name);
    let saved = info(until(&devices, added(&name)));
    assert_eq!(
        devices.resolve(&saved),
        tutti_midi_device::Resolved::SameName(saved.clone())
    );

    drop(first);
    until(&devices, removed(saved.id.clone()));
    assert_eq!(
        devices.resolve(&saved),
        tutti_midi_device::Resolved::Missing
    );

    let _second = plug(&name);
    let again = info(until(&devices, added(&name)));
    assert_eq!(
        devices.resolve(&saved),
        tutti_midi_device::Resolved::SameName(again)
    );
}

/// A port is listed once it has capabilities, in both directions it has: a
/// duplex port with none yet is not listed, and when it gains them it is added
/// once, as a duplex (one walk reads its capabilities once).
#[test]
fn a_port_gaining_its_capabilities_is_added_as_a_duplex() {
    let devices = Watched::open();
    let name = unique("caps");
    let seq = super::client::open(
        &name,
        tutti_alsa_ffi::Blocking::NonBlock,
        tutti_alsa_ffi::UmpVersion::Midi2,
    )
    .expect("a sequencer to open (modprobe snd-seq)");
    let kind = PortKind::MIDI_GENERIC | PortKind::APPLICATION;
    let port = seq
        .open_port("port", PortCaps::SUBS_READ, kind)
        .expect("a port");
    let want = format!("{name}: port");
    assert!(devices.list().iter().all(|d| d.name != want));

    let caps = PortCaps::READ | PortCaps::SUBS_READ | PortCaps::WRITE | PortCaps::SUBS_WRITE;
    seq.set_port_caps(port, caps).expect("capabilities");
    let added = info(until(&devices, added(&name)));
    assert_eq!(added.direction, tutti_midi_device::PortDirection::Duplex);
}

#[test]
fn an_input_held_when_its_device_vanishes_is_reported_removed() {
    let devices = Watched::open();
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(
        tutti_midi_runtime::InputInlets::new(tutti_graph::EventCapacity::of(16), Duration::ZERO),
    ));
    let name = unique("held");
    let device = plug(&name);
    let id = info(until(&devices, added(&name))).id;

    let connection = open_input_when_listed(&devices, &id, &ports);
    drop(device);
    until(&devices, removed(id.clone()));
    assert!(
        devices
            .open_input(&id, &ports, DeviceWriters::unread())
            .is_err()
    );
    drop(connection);
}

#[test]
fn two_identical_clients_are_two_devices() {
    let devices = Watched::open();
    let name = unique("twin");
    let first = plug(&name);
    until(&devices, added(&name));
    let second = plug(&name);
    // Read the ids from the list once both are there: the kernel can announce
    // a lagging exit for a number a client has just taken, which re-reports it
    // (see the doc on `Reporter`), so which of the two is `#0` is not fixed.
    let deadline = Instant::now() + PATIENCE;
    let twins = || {
        let want = format!("{name}: port");
        let mut twins: Vec<_> = devices
            .list()
            .into_iter()
            .filter(|d| d.name == want)
            .collect();
        twins.sort_by_key(|d| d.id.ordinal());
        twins
    };
    while twins().len() < 2 {
        assert!(Instant::now() < deadline, "{:?}", devices.list());
        devices.wait(Duration::from_millis(50));
    }
    let twins = twins();
    assert_eq!(twins[0].id.identity(), twins[1].id.identity());
    assert_eq!((twins[0].id.ordinal(), twins[1].id.ordinal()), (0, 1));
    drop((first, second));
}

#[test]
fn dropping_devices_ends_the_watch_thread() {
    assert_eq!(watch_threads(), 0);
    let devices = Watched::open();
    settle_watch_threads(1);
    drop(devices);
    settle_watch_threads(0);
}

#[test]
fn opening_twice_gives_each_its_own_watch() {
    let a = Watched::open();
    let b = Watched::open();
    settle_watch_threads(2);
    let name = unique("both");
    let _device = plug(&name);
    until(&a, added(&name));
    until(&b, added(&name));
}

/// Opening an input and an output on a device makes the engine's own clients
/// (`tutti-in`, `tutti-out`); they are not devices, in this process or another.
#[test]
fn the_engines_own_clients_are_not_listed() {
    let devices = Watched::open();
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(
        tutti_midi_runtime::InputInlets::new(tutti_graph::EventCapacity::of(16), Duration::ZERO),
    ));
    let name = unique("own");
    let device = plug(&name);
    let mut backlog = Vec::new();
    let id = info(until_in(&devices, &mut backlog, added(&name))).id;

    let input = open_input_when_listed(&devices, &id, &ports);
    let output = when_listed(&devices, || devices.open_output(&id));
    drop(device);
    // Announcements are in order: whatever the opens announced is before this.
    until_in(&devices, &mut backlog, removed(id));
    // The engine's clients carry exactly these names, in this process and in
    // any other copy of the suite running beside it; another test's device
    // (`tutti-loop-<pid>-source`, `tutti-in-<pid>`) is a device and is listed.
    let engine = |i: &DeviceInfo| match i.id.identity() {
        Identity::Alsa { client, .. } => {
            client == super::input::CLIENT || client == super::output::CLIENT
        }
        _ => false,
    };
    assert!(!backlog.iter().any(|e| engine(e.info())), "{backlog:?}");
    assert!(!devices.list().iter().any(engine), "{:?}", devices.list());
    drop((input, output));
}

/// The engine's clients are told by their capabilities (the pump's and the
/// output's ports are not subscribable), not their names: a device may be
/// called like one. The engine opens exactly `tutti-in` and `tutti-out`
/// (`input.rs`, `output.rs`); this device is `tutti-in-<pid>`, because client
/// names are global to the machine and a fixed `tutti-in` is shared by parallel
/// copies of the suite. So the exact-name case is given up: a skip of the
/// single name `tutti-in` would not be caught here, though a skip by the
/// `tutti-` prefix is. No function filters by name (there is nothing to unit
/// test); the listing is by capability.
#[test]
fn a_device_called_like_an_engine_client_is_listed() {
    let devices = Watched::open();
    let name = format!("tutti-in-{}", std::process::id());
    let _device = plug(&name);
    let device = info(until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.id.identity() == &Identity::alsa(&name, "port")),
    ));
    assert!(devices.list().contains(&device));
}

/// A device replaced at once by an identical one is two devices: the client
/// exited and the next took its number before the watch looked. The
/// announcement of the exit is what tells.
#[test]
fn an_identical_device_that_takes_a_number_at_once_is_a_new_device() {
    let devices = Watched::open();
    let name = unique("swap");
    let mut backlog = Vec::new();
    let mut device = plug(&name);
    let mut id = info(until_in(&devices, &mut backlog, added(&name))).id;
    for _ in 0..30 {
        drop(device);
        device = plug(&name);
        until_in(&devices, &mut backlog, removed(id.clone()));
        id = info(until_in(&devices, &mut backlog, added(&name))).id;
    }
    drop(device);
}

/// On ALSA a device is its name, so a renamed client is a different device:
/// removed, then added under the new name, which closes a port held on the old.
#[test]
fn a_renamed_client_is_removed_and_added_under_its_new_name() {
    let devices = Watched::open();
    let name = unique("rename");
    let device = plug(&name);
    let mut backlog = Vec::new();
    let before = info(until_in(&devices, &mut backlog, added(&name))).id;
    let renamed = format!("{name}-two");
    device.rename(&renamed);
    until_in(&devices, &mut backlog, removed(before.clone()));
    let after = info(until_in(&devices, &mut backlog, added(&renamed)));
    assert_ne!(after.id, before);
}

/// A client renamed in place is reported removed and added (the identity is the
/// name), and no exit follows it: an identical device that later takes the
/// number must still be reported removed and added. The kernel posts the new
/// client's start for it, whatever came before.
#[test]
fn a_renamed_client_replaced_by_an_identical_one_is_removed_then_added() {
    let devices = Watched::open();
    let name = unique("rename-replug");
    let device = plug(&name);
    let mut backlog = Vec::new();
    until_in(&devices, &mut backlog, added(&name));
    let renamed = format!("{name}-two");
    device.rename(&renamed);
    let id = info(until_in(&devices, &mut backlog, added(&renamed))).id;
    drop(device);
    let _next = plug(&renamed);
    until_in(&devices, &mut backlog, removed(id.clone()));
    assert_eq!(
        info(until_in(&devices, &mut backlog, added(&renamed))).id,
        id
    );
}

/// The kernel's own client is not a device.
#[test]
fn the_kernels_system_client_is_not_listed() {
    let devices = Watched::open();
    let system = |d: &DeviceInfo| matches!(d.id.identity(), Identity::Alsa { client, .. } if client == "System");
    assert!(!devices.list().iter().any(system), "{:?}", devices.list());
}

/// A backend whose enumeration panics once `armed` is set (not before: other
/// processes' clients wake the watch at any time).
struct Panicky(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl crate::MidiEndpoints for Panicky {
    fn watch(&self, sink: crate::ChangeSink) -> crate::Result<Box<dyn crate::Watch>> {
        let armed = std::sync::Arc::clone(&self.0);
        super::watch::start(sink, move |_| {
            assert!(
                !armed.load(std::sync::atomic::Ordering::SeqCst),
                "enumeration panics"
            );
            Some(Vec::new())
        })
    }
    fn inputs(&self) -> Vec<crate::OsEndpoint> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<crate::OsEndpoint> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: tutti_midi_device::EndpointId,
        _: crate::InputProducerHandle,
    ) -> crate::Result<Box<dyn crate::InputConnection>> {
        Err(crate::MidiHardwareError::Unsupported("none"))
    }
    fn open_output(
        &self,
        _: tutti_midi_device::EndpointId,
    ) -> crate::Result<Box<dyn crate::DeviceOut>> {
        Err(crate::MidiHardwareError::Unsupported("none"))
    }
}

/// A watch that dies says so: `watch_failure` is set, not a frozen table with
/// no word.
#[test]
fn a_watch_that_panics_reports_its_failure() {
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let devices = Watched::with_backend(Box::new(Panicky(std::sync::Arc::clone(&armed))));
    assert!(devices.watch_failure().is_none());
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let _device = plug(&unique("panic"));
    let deadline = Instant::now() + PATIENCE;
    while devices.watch_failure().is_none() {
        assert!(Instant::now() < deadline, "no failure reported");
        devices.wait(Duration::from_millis(50));
    }
    assert!(matches!(
        devices.watch_failure(),
        Some(crate::WatchFailure::Panicked)
    ));
}

/// Asking the backend what a device can carry enumerates, and must not open a
/// client (each would be announced to every watch).
#[test]
fn asking_for_capabilities_does_not_wake_the_watch() {
    let devices = Watched::open();
    let name = unique("caps");
    let _device = plug(&name);
    let id = info(until(&devices, added(&name))).id;
    // The first call opens the enumerating client, which announces itself once.
    assert!(devices.capability(&id).is_some());
    let opens = tutti_alsa_ffi::clients_opened();
    for _ in 0..1000 {
        let _ = devices.capability(&id);
    }
    let opened = tutti_alsa_ffi::clients_opened() - opens;
    assert_eq!(opened, 0, "{opened} sequencer clients opened for 1000 asks");
}

/// A client that takes the number of one that just exited is a new device, not
/// a device that comes and goes: the exit the kernel announces for the old
/// owner arrives after the look that already shows the new one, and the new
/// device must be added once and never removed while it lives. One `Devices`,
/// `A` dropped and `B` plugged at once, for 40 cycles.
///
/// The old owner's `CLIENT_EXIT` is posted after the number is free, so the
/// watch can read it after a look that already lists `B`; only a start says a
/// number has a new occupant, so that exit is nothing. It is posted while `A`
/// closes, before `B` opens, so once `B` is added a sentinel plugged after it
/// is the barrier: the watch reads announcements in the order they were posted,
/// and the sentinel's add comes after that exit has been read. Every event for
/// `B` up to it is judged.
///
/// On a real kernel the exit is most often read before `B` is listed, so this
/// test seldom meets the late order; `watch::tests::late_exits_and_changes_of_an_old_owner_are_nothing`
/// holds the rule that a late exit is nothing. This test holds the whole watch
/// to it on the sequencer: no removal and no second add of a live device.
///
/// The sequencer has 64 dynamic client slots, so `.config/nextest.toml` runs the
/// tests of this file at most four at a time (test group `sequencer`): parallel
/// copies of the suite ran out of slots (`snd_seq_open` ENOMEM) with this test
/// beside them.
#[test]
fn a_device_that_takes_a_departed_clients_number_is_added_once() {
    let devices = Watched::open();
    let mut a = plug(&unique("flap-a"));
    for cycle in 0..40 {
        let name = unique(&format!("flap-b-{cycle}"));
        drop(a);
        let b = plug(&name);
        let mut seen = Vec::new();
        until_in(&devices, &mut seen, added(&name));
        let sentinel_name = unique(&format!("flap-s-{cycle}"));
        let sentinel = plug(&sentinel_name);
        until_in(&devices, &mut seen, added(&sentinel_name));
        drop(sentinel);
        let more_of_b: Vec<_> = seen
            .iter()
            .filter(|e| e.info().name.starts_with(&format!("{name}:")))
            .collect();
        assert!(
            more_of_b.is_empty(),
            "cycle {cycle}: the live device, once added, saw {more_of_b:?}"
        );
        assert!(
            devices
                .list()
                .iter()
                .any(|d| d.name.starts_with(&format!("{name}:"))),
            "cycle {cycle}"
        );
        a = b;
    }
}

/// Renders blocks until the rig's clock reaches OS time `now`: the audio
/// thread of a test that runs on the wall clock.
fn run_to(rig: &mut crate::testing::Rig, now: Instant) {
    let due =
        (now.saturating_duration_since(rig.origin).as_secs_f64() * crate::testing::RATE) as u64;
    while rig.frames + crate::testing::Rig::BLOCK as u64 <= due {
        rig.run(1);
    }
}

/// A device and an input pump connected to its port, with the inlet the pump
/// fills and the graph (on the wall clock) that plays it.
struct Pumped {
    /// Keeps the device (and so the port the pump reads) alive.
    device: Virtual,
    rig: crate::testing::Rig,
    connection: Option<Box<dyn crate::endpoints::InputConnection>>,
    _ports: std::sync::Arc<HardwareMidiInputs>,
}

impl Pumped {
    /// A pump whose events are placed `lead` after they arrived.
    fn open(lead: Duration) -> Self {
        Self::over(plug(&unique("pump")), lead)
    }

    /// A pump on `device`'s port 0.
    fn over(device: Virtual, lead: Duration) -> Self {
        let id = super::handle_of(Address::new(device.0.id(), PortId::of(0)));
        Self::start(device, id, lead, |_| {})
    }

    /// A pump on `device`'s port 0 whose client runs `hook` before each read.
    fn hooked(
        device: Virtual,
        id: tutti_midi_device::EndpointId,
        hook: impl FnMut() -> std::io::Result<()> + Send + 'static,
    ) -> Self {
        Self::start(device, id, Duration::ZERO, |seq| seq.set_read_hook(hook))
    }

    fn start(
        device: Virtual,
        id: tutti_midi_device::EndpointId,
        lead: Duration,
        prime: impl FnOnce(&Client),
    ) -> Self {
        let inlets = tutti_midi_runtime::InputInlets::new(tutti_graph::EventCapacity::of(64), lead);
        let ports = std::sync::Arc::new(HardwareMidiInputs::new(std::sync::Arc::clone(&inlets)));
        let (producer, inlet) = ports.open_input_port("pump");
        let mut rig = crate::testing::Rig::over(&inlets, Instant::now());
        rig.attach(inlet);
        rig.run(1);
        let (seq, clock) = super::input::prepare(id).expect("the pump's client");
        prime(&seq);
        let connection = super::input::start(seq, clock, producer).expect("the pump to open");
        Self {
            device,
            rig,
            connection: Some(Box::new(connection)),
            _ports: ports,
        }
    }
}

/// A legacy (non-UMP) sequencer client with a subscribable port, the kind of
/// source a MIDI 1.0 device or an old application is.
fn plug_legacy(name: &str) -> Virtual {
    let seq = tutti_alsa_ffi::Client::open_legacy(name, tutti_alsa_ffi::Blocking::NonBlock)
        .expect("a sequencer to open (modprobe snd-seq)");
    let caps = PortCaps::READ | PortCaps::SUBS_READ;
    seq.open_port("port", caps, PortKind::MIDI_GENERIC | PortKind::APPLICATION)
        .expect("a port");
    Virtual(seq)
}

/// Plays the graph until `count` events have reached it, and returns them.
fn arrive(pumped: &mut Pumped, count: usize) -> Vec<tutti_ump::Ump> {
    let deadline = Instant::now() + PATIENCE;
    let mut got = Vec::new();
    while got.len() < count {
        assert!(Instant::now() < deadline, "only {got:?} arrived");
        run_to(&mut pumped.rig, Instant::now());
        got.extend(pumped.rig.take().into_iter().map(|(_, _, ump)| ump));
        std::thread::yield_now();
    }
    got
}

/// A MIDI 1.0 source reaches the graph as MIDI 1.0 UMP (type 2), not converted
/// to MIDI 2.0 by the kernel: tutti's own `Midi2Translator` is the one conversion.
/// A legacy client's Note On and control change, both.
#[test]
fn a_legacy_source_arrives_as_midi1_ump_not_converted_by_the_kernel() {
    use tutti_alsa_ffi::LegacyEvent;
    let mut pumped = Pumped::over(plug_legacy(&unique("legacy")), Duration::ZERO);
    let send = |event| {
        pumped
            .device
            .0
            .send_legacy_to_subscribers(PortId::of(0), event)
            .unwrap()
    };
    send(LegacyEvent::NoteOn {
        channel: 2,
        note: 60,
        velocity: 100,
    });
    send(LegacyEvent::Controller {
        channel: 2,
        param: 7,
        value: 90,
    });
    let got = arrive(&mut pumped, 2);
    // The message type is the top nibble of the first word.
    let words: Vec<u32> = got.iter().map(|u| u.words()[0]).collect();
    assert_eq!(words, [0x2092_3C64, 0x20B2_075A], "{got:?}");
}

/// An Endpoint that offers the MIDI 2.0 Protocol is a MIDI 2.0 client: its own
/// type 4 packet arrives as sent, not translated.
#[test]
fn a_midi2_endpoint_arrives_as_midi2_ump() {
    let mut pumped = Pumped::over(plug_ump(&unique("two-in"), Protocol::Midi2), Duration::ZERO);
    // Note On, group 0, channel 0, note 60, velocity 0xC000, no attribute.
    let note_on = [0x4090_3C00, 0xC000_0000];
    pumped
        .device
        .0
        .send_to_subscribers(
            PortId::of(0),
            &tutti_ump::Ump([note_on[0], note_on[1], 0, 0]),
        )
        .unwrap();
    let got = arrive(&mut pumped, 1);
    assert_eq!(got[0].words(), note_on, "{got:?}");
}

/// Which sources are opened as MIDI 1.0: a legacy client (no record) and an
/// Endpoint whose record offers no MIDI 2.0 Protocol. An Endpoint that offers
/// it is opened as MIDI 2.0 even while it is in MIDI 1.0, since the session
/// negotiates up and the kernel would narrow what it then sends natively.
#[test]
fn only_a_source_that_cannot_speak_midi2_is_opened_as_midi1() {
    use tutti_alsa_ffi::{UmpEndpoint, UmpVersion};
    let record = |protocol, midi1, midi2| UmpEndpoint {
        protocol,
        offered: offered(midi1, midi2),
        name: None,
        blocks: Vec::new(),
    };
    let version = super::input::version_for;
    assert_eq!(version(None), UmpVersion::Midi1);
    assert_eq!(
        version(Some(&record(Protocol::Midi1, true, false))),
        UmpVersion::Midi1
    );
    assert_eq!(
        version(Some(&record(Protocol::Midi1, true, true))),
        UmpVersion::Midi2
    );
    assert_eq!(
        version(Some(&record(Protocol::Midi2, true, true))),
        UmpVersion::Midi2
    );
}

/// Dropping an input returns while its pump is blocked in `poll`: the drop
/// completes, where a pump that waited for its next wake would never end.
#[test]
fn dropping_an_input_stops_a_blocked_pump() {
    let mut pumped = Pumped::open(Duration::ZERO);
    let connection = pumped.connection.take().unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(connection);
        let _ = done.send(());
    });
    finished.recv_timeout(PATIENCE).expect("the pump to stop");
}

/// A pump whose reads keep failing ends and raises its latch: the holder can
/// see that the device went silent, and `connect_input` replaces it. The
/// failure is the client's read hook (`set_read_hook`): a healthy sequencer has
/// no other way to a failing read. The pump runs on this thread, so its return
/// is the event.
#[test]
fn a_pump_whose_reads_fail_ends_and_raises_its_latch() {
    let device = plug(&unique("dies"));
    let id = super::handle_of(Address::new(device.0.id(), PortId::of(0)));
    let (seq, clock) = super::input::prepare(id).expect("the pump's client");
    seq.set_read_hook(|| Err(std::io::Error::from_raw_os_error(EIO)));
    let (_stop, stop_rx) = std::os::unix::net::UnixStream::pair().unwrap();
    let poller = seq.into_poller(stop_rx.into()).expect("descriptors");
    let ports = HardwareMidiInputs::new(tutti_midi_runtime::InputInlets::new(
        tutti_graph::EventCapacity::of(16),
        Duration::ZERO,
    ));
    let (producer, _inlet) = ports.open_input_port("dies");
    let died = tutti_rt::FaultLatch::<crate::InputFault>::new();
    super::input::pump(poller, clock, &died, producer);
    assert_eq!(
        died.cause(),
        Some(crate::InputFault::ReadFailed(crate::fault::Errno::new(EIO))),
        "a pump that gave up raises the latch, for the read's errno"
    );
}

/// `EIO`, what the injected failure reads as.
const EIO: i32 = 5;

/// Dropping an input does not wait for ever on a pump that will not stop: here
/// one held in its first read, which `JOIN_BOUND` cuts short, so the drop
/// returns and detaches it. The test waits for the pump to be held (the hook
/// says so), drops on another thread, and requires that drop to complete while
/// the pump is still held; `PATIENCE` only guards the hang a waiting drop is.
#[test]
fn dropping_an_input_does_not_wait_on_a_stuck_pump() {
    let device = plug(&unique("stuck"));
    let id = super::handle_of(Address::new(device.0.id(), PortId::of(0)));
    let (held, held_rx) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let mut pumped = Pumped::hooked(device, id, move || {
        let _ = held.send(());
        // Held until the sender drops.
        let _ = released.recv();
        Ok(())
    });
    held_rx.recv_timeout(PATIENCE).expect("the pump to be held");
    let connection = pumped.connection.take();
    let (dropped, dropped_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(connection);
        let _ = dropped.send(());
    });
    dropped_rx
        .recv_timeout(PATIENCE)
        .expect("the drop to return while the pump is still held");
    drop(release);
}

/// The next note message (type `0x2`) that reaches `device`'s port, polling
/// for up to `patience`; the sequencer's own notices about the subscription
/// (Stream messages) pass by.
fn next_note(device: &Virtual, patience: Duration) -> Option<(u32, Instant)> {
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        match device.0.read_event() {
            Ok(tutti_alsa_ffi::Read::Event(tutti_alsa_ffi::Event::Ump { ump, .. }))
                if ump.message_type() == 2 =>
            {
                return Some((ump.0[0], Instant::now()));
            }
            Ok(tutti_alsa_ffi::Read::Event(_)) => {}
            _ => std::thread::sleep(Duration::from_micros(500)),
        }
    }
    None
}

/// **The sequencer holds a scheduled event until its time.** Against the real
/// kernel: an event sent `At` 2 s ahead, then one sent `Now`, reach the
/// subscriber in the other order, and the scheduled one is not there before the
/// `Instant` the send asked for (less the pairing's few ms of slack). Only
/// lateness depends on load; an early delivery or a held event that is not
/// held cannot pass.
#[test]
fn a_scheduled_event_arrives_at_its_time_and_now_at_once() {
    let target = plug(&unique("scheduled"));
    let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
    let output = super::output::connect(id).expect("the output to open");
    let slack = Duration::from_millis(10);

    let asked = Instant::now() + Duration::from_secs(2);
    send(&output, 0x2090_3C64, tutti_midi_runtime::SendAt::At(asked));
    send(&output, 0x2090_3D64, tutti_midi_runtime::SendAt::Now);
    let (first, _) = next_note(&target, PATIENCE).expect("the event sent now");
    let (second, arrived) = next_note(&target, PATIENCE).expect("the scheduled event");
    assert_eq!(
        (first, second),
        (0x2090_3D64, 0x2090_3C64),
        "now first, then the scheduled one"
    );
    assert!(
        arrived + slack >= asked,
        "arrived {:?} before the instant it asked for",
        asked - arrived
    );
}

/// **A time already past is delivered at once**, and a run of events keeps its
/// order: scheduled out of order, they arrive by time.
#[test]
fn events_are_delivered_by_their_times_and_a_past_time_at_once() {
    let target = plug(&unique("ordered"));
    let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
    let output = super::output::connect(id).expect("the output to open");
    let now = Instant::now();
    let late = now + Duration::from_secs(2);
    let early = now + Duration::from_secs(1);
    let first = [0x2090_3C64];
    let second = [0x2090_3D64];
    let third = [0x2090_3E64];
    use tutti_midi_runtime::SendAt;
    send(&output, first[0], SendAt::At(late));
    send(&output, second[0], SendAt::At(early));
    send(
        &output,
        third[0],
        SendAt::At(now - Duration::from_millis(500)),
    );
    let got: Vec<u32> = (0..3)
        .map_while(|_| next_note(&target, PATIENCE).map(|(w, _)| w))
        .collect();
    assert_eq!(got, [third[0], second[0], first[0]]);
}

/// Sends the note-on word `word` through `output`, at `at`, and expects it
/// taken.
fn send(output: &super::output::AlsaOutput, word: u32, at: tutti_midi_runtime::SendAt) {
    use tutti_midi_runtime::MidiOut;
    let packet = [tutti_midi_runtime::Outgoing::new(
        tutti_graph::Offset::ZERO,
        at,
        tutti_ump::Ump([word, 0, 0, 0]),
    )];
    assert!(
        output.queue_timed(&packet).is_all(&packet),
        "the send to go"
    );
}

/// What an output sends: `ump`, at once.
fn now(ump: tutti_ump::Ump) -> tutti_midi_runtime::Outgoing {
    tutti_midi_runtime::Outgoing::new(
        tutti_graph::Offset::ZERO,
        tutti_midi_runtime::SendAt::Now,
        ump,
    )
}

/// A System Exclusive message of three packets, start, continue and end.
fn sysex() -> [tutti_midi_runtime::Outgoing; 3] {
    use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus};
    [SysexStatus::Start, SysexStatus::Continue, SysexStatus::End].map(|status| {
        now(tutti_ump::Ump::from(Message::Sysex7(
            MidiGroup::FIRST,
            Sysex7Packet::new(status, &[1, 2]).unwrap(),
        )))
    })
}

/// Sends notes to `output` until it refuses one: the destination is full.
fn fill(output: &super::output::AlsaOutput) -> usize {
    use tutti_midi_runtime::MidiOut;
    let note = now(tutti_ump::Ump([0x2090_3C64, 0, 0, 0]));
    (0..10_000)
        .take_while(|_| !output.queue_timed(&[note]).is_none())
        .count()
}

/// **The output never waits for room.** Events held for a minute fill the
/// client's own output pool; a batch longer than the pool holds comes back at
/// once with what it took, short of the batch, where a blocking client would
/// wait for the pool to drain, which takes the minute.
#[test]
fn an_output_with_a_full_pool_takes_what_fits_and_does_not_wait() {
    use tutti_midi_runtime::MidiOut;
    let target = plug(&unique("pool"));
    let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
    let output = super::output::connect(id).expect("the output to open");
    let held = held(tutti_ump::Ump([0x2090_3C64, 0, 0, 0]));
    let batch = vec![held; 10_000];
    let started = Instant::now();
    let taken = output.queue_timed(&batch);
    // A blocking client waits for the pool to drain, which takes the minute the
    // events are held; returning before half of it is returning without waiting.
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "waited {:?} for room in the pool",
        started.elapsed()
    );
    assert!(!taken.is_none(), "an empty pool takes some");
    assert!(!taken.is_all(&batch), "the pool holds fewer than 10 000");
}

/// A packet held by the queue for a minute.
fn held(ump: tutti_ump::Ump) -> tutti_midi_runtime::Outgoing {
    tutti_midi_runtime::Outgoing::new(
        tutti_graph::Offset::ZERO,
        tutti_midi_runtime::SendAt::At(Instant::now() + Duration::from_secs(60)),
        ump,
    )
}

/// **A scheduled message the output pool has no room for whole is refused
/// whole.** With room for two of a message's three packets, none of it is
/// queued, and the output has lost nothing.
#[test]
fn a_scheduled_message_the_pool_cannot_hold_whole_is_refused_whole() {
    use tutti_midi_runtime::MidiOut;
    let note = held(tutti_ump::Ump([0x2090_3C64, 0, 0, 0]));
    let pool = {
        let target = plug(&unique("pool-size"));
        let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
        let output = super::output::connect(id).expect("the output to open");
        (0..10_000)
            .take_while(|_| !output.queue_timed(&[note]).is_none())
            .count()
    };
    let target = plug(&unique("pool-room"));
    let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
    let output = super::output::connect(id).expect("the output to open");
    let notes = vec![note; pool - 2];
    assert!(output.queue_timed(&notes).is_all(&notes));
    let message = sysex().map(|packet| held(packet.ump));
    assert!(output.queue_timed(&message).is_none());
    assert_eq!(output.losses(), Losses::NONE);
    assert_eq!(output.output_free(), 2, "none of it was queued");
}

/// **A full destination refuses a note at once**, and the destination is not
/// waited on either.
#[test]
fn a_full_destination_refuses_a_send_at_once() {
    use tutti_midi_runtime::MidiOut;
    let target = plug(&unique("full"));
    let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
    let output = super::output::connect(id).expect("the output to open");
    assert!(fill(&output) > 0, "an empty destination takes some");
    assert!(
        output
            .queue_timed(&[now(tutti_ump::Ump([0x2090_3C64, 0, 0, 0]))])
            .is_none()
    );
}

/// **A message the destination has no room to start is not taken, and nothing
/// of it goes.** With the destination full, a message of three packets is
/// refused whole, and the output has lost nothing: the refusal is the caller's
/// to count.
#[test]
fn a_message_a_full_destination_cannot_start_is_refused_whole() {
    use tutti_midi_runtime::MidiOut;
    let target = plug(&unique("refused"));
    let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
    let output = super::output::connect(id).expect("the output to open");
    fill(&output);
    assert!(output.queue_timed(&sysex()).is_none());
    assert_eq!(output.losses(), Losses::NONE);
}

/// **A destination that fills inside a message cuts it, and the tail is
/// counted.** With room for two of a message's three packets (a destination of
/// the same size as one `fill` found full, less two notes), its head goes and
/// its end is lost; the message is taken, so that the caller does not send its
/// head again, and the lost packet is in `losses`.
#[test]
fn a_destination_that_fills_inside_a_message_has_the_lost_tail_counted() {
    use tutti_midi_runtime::MidiOut;
    let room = {
        let target = plug(&unique("measured"));
        let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
        fill(&super::output::connect(id).expect("the output to open"))
    };
    assert!(room > 2, "a destination holds more than two");
    let target = plug(&unique("cut"));
    let id = super::handle_of(Address::new(target.0.id(), PortId::of(0)));
    let output = super::output::connect(id).expect("the output to open");
    let note = now(tutti_ump::Ump([0x2090_3C64, 0, 0, 0]));
    let notes = vec![note; room - 2];
    assert!(output.queue_timed(&notes).is_all(&notes));
    let batch = sysex();
    assert!(output.queue_timed(&batch).is_all(&batch));
    assert_eq!(output.losses(), Losses::lost(1));
}

/// An Endpoint Info Notification offering the protocols given, and no JR
/// Timestamps.
fn offered(midi1: bool, midi2: bool) -> EndpointInfoNotification {
    EndpointInfoNotification {
        version: UMP_VERSION,
        static_function_blocks: false,
        function_blocks: FunctionBlocks::Count(FunctionBlockCount::of(1)),
        midi2,
        midi1,
        rx_jr: false,
        tx_jr: false,
    }
}

/// A device named `name` that publishes a UMP Endpoint in `protocol` with a
/// block of two Groups, as a kernel UMP client does for a USB device.
fn plug_ump(name: &str, protocol: Protocol) -> Virtual {
    let device = plug(name);
    device
        .0
        .publish_ump_endpoint(&tutti_alsa_ffi::UmpEndpoint {
            protocol,
            offered: offered(true, true),
            name: Some("Endpoint".into()),
            blocks: vec![tutti_alsa_ffi::UmpBlock {
                info: FunctionBlockInfo {
                    active: true,
                    id: Field::Value(FunctionBlockId::of(0)),
                    direction: BlockDirection::Bidirectional,
                    ui_hint: UiHint::Both,
                    midi1: Midi1Port::No,
                    first_group: Field::Value(FirstGroup::of(2)),
                    groups_spanned: Field::Value(GroupSpan::of(2)),
                    midi_ci_version: 0,
                    max_sysex8_streams: 0,
                },
                name: Some("Keys".into()),
            }],
        })
        .expect("the sequencer to take a UMP endpoint (kernel 6.5+)");
    device
}

/// The capability of the device named `name`, once it is listed.
fn capability_of(devices: &Watched, name: &str) -> Endpoint {
    let want = format!("{name}: port");
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(d) = devices.list().into_iter().find(|d| d.name == want) {
            return devices.capability(&d.id).expect("a capability");
        }
        assert!(Instant::now() < deadline, "{name} never listed");
        devices.wait(Duration::from_millis(20));
    }
}

/// A client's protocol is the one its UMP Endpoint record has in use, not
/// "has a block, so MIDI 2.0": the same Endpoint, with the same block, is
/// MIDI 2.0 in one record and MIDI 1.0 in another. Its blocks are the ones
/// the record lists, with their Groups, direction and name; a client with no
/// record is MIDI 1.0 with none.
#[test]
fn a_clients_protocol_and_blocks_are_read_from_its_ump_endpoint() {
    let devices = Watched::open();
    let (two, one, legacy) = (unique("two"), unique("one"), unique("legacy"));
    let _two = plug_ump(&two, Protocol::Midi2);
    let _one = plug_ump(&one, Protocol::Midi1);
    let _legacy = plug(&legacy);

    let cap = capability_of(&devices, &two);
    assert_eq!(cap.active().protocol, Protocol::Midi2);
    let blocks: Vec<_> = cap.function_blocks().collect();
    let [block] = blocks[..] else {
        panic!("blocks: {blocks:?}");
    };
    assert_eq!(block.first_group().get(), 2);
    assert_eq!(block.groups().get(), 2);
    assert_eq!(block.direction(), tutti_ump::BlockDirection::Bidirectional);
    assert_eq!(block.name().map(|n| n.as_str()), Some("Keys"));

    let cap = capability_of(&devices, &one);
    assert_eq!(cap.active().protocol, Protocol::Midi1);
    assert_eq!(cap.function_blocks().count(), 1);

    let cap = capability_of(&devices, &legacy);
    assert_eq!(cap.active().protocol, Protocol::Midi1);
    assert_eq!(cap.function_blocks().count(), 0);
}

/// A UMP Stream conversation over the real sequencer, both ways: a device
/// that answers as `Endpoint::reply` does, on its own thread, is asked by the
/// session's `poll` through the ALSA input pump and output, and the stream
/// settles on what it answered. Proves the Stream messages cross the
/// sequencer in both directions and that the pump routes them to the stream
/// ring, not the graph.
#[test]
fn a_device_on_the_sequencer_is_negotiated_through_the_pump_and_the_output() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tutti_alsa_ffi::Read;
    use tutti_midi_device::{Capabilities, Endpoint, Supports};
    use tutti_ump::{Message, Protocol};

    let name = unique("negotiated");
    let device = plug_ump(&name, Protocol::Midi1);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let inlets =
        tutti_midi_runtime::InputInlets::new(tutti_graph::EventCapacity::of(64), Duration::ZERO);
    let mut session = crate::MidiSession::open(inlets, crate::MidiHardwareConfig::new()).unwrap();
    let want = format!("{name}: port");
    let deadline = Instant::now() + PATIENCE;
    let id = loop {
        if let Some(d) = session.inputs().into_iter().find(|d| d.name == want) {
            break d.id;
        }
        assert!(Instant::now() < deadline, "device never listed");
        std::thread::sleep(Duration::from_millis(10));
    };
    let thread = {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            // The device: in MIDI 1.0, offering MIDI 2.0.
            let mut endpoint = Endpoint::new(Supports::of(Capabilities {
                midi1: true,
                midi2: true,
                ..Capabilities::default()
            }));
            while !stop.load(Ordering::Acquire) {
                match device.0.read_event() {
                    Ok(Read::Event(Event::Ump { ump, .. })) => {
                        let Ok(message @ Message::Stream(_)) = ump.decode() else {
                            continue;
                        };
                        let mut answers: Vec<tutti_ump::Ump> = Vec::new();
                        let _ = endpoint.reply(message, &mut answers);
                        for answer in answers {
                            device
                                .0
                                .send_to_subscribers(PortId::of(0), &answer)
                                .unwrap();
                        }
                    }
                    _ => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        })
    };
    let _ = session.connect_input(&id).unwrap();
    let deadline = Instant::now() + PATIENCE;
    let stream = loop {
        session.poll(Instant::now());
        if let Some(found) = session.stream(&id) {
            break found;
        }
        assert!(Instant::now() < deadline, "the negotiation never settled");
        std::thread::sleep(Duration::from_millis(2));
    };
    stop.store(true, Ordering::Release);
    thread.join().unwrap();
    assert_eq!(stream.basis(), crate::Basis::Negotiated);
    assert_eq!(stream.config().protocol, Protocol::Midi2);
    assert!(!stream.config().rx_jr && !stream.config().tx_jr);
}

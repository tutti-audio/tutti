use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Waker;
use std::time::Duration;

use super::*;
use crate::HardwareMidiInputs;
use crate::capability::OsEndpoint;
use crate::devices::reporter::{Reporter, os_devices};
use crate::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_midi_device::Identity;
use tutti_midi_runtime::DeviceWriters;
use tutti_ump::ProductInstanceId;

/// Long enough to be a failure, not a race: a wake returns at once.
const PATIENCE: Duration = Duration::from_secs(30);

/// Waits for the next change, failing at the hang guard when the change did
/// not wake the waiter.
fn next(events: &DeviceEvents) -> std::result::Result<DeviceEvent, WatchFailure> {
    tutti_fixtures::block_on_within(events.recv(), PATIENCE).expect("an event before the guard")
}

/// The event that woke the waiter and any queued behind it.
fn wake(events: &DeviceEvents) -> Vec<DeviceEvent> {
    let first = next(events).expect("an event");
    std::iter::once(first).chain(events.try_iter()).collect()
}

fn fake() -> (Devices, DeviceEvents, FakeCounters) {
    fake_on(&tutti_platform::Executor::new())
}

fn fake_on(executor: &tutti_platform::Executor) -> (Devices, DeviceEvents, FakeCounters) {
    let (backend, counters) = FakeBackend::build_with(Vec::new(), Vec::new(), usize::MAX);
    let devices = Devices::open_over(executor, backend).unwrap();
    let events = devices.subscribe();
    (devices, events, counters)
}

fn id_of(devices: &Devices, name: &str) -> DeviceId {
    devices
        .list()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no device {name}"))
        .id
}

fn ports() -> Arc<HardwareMidiInputs> {
    Arc::new(HardwareMidiInputs::new(
        tutti_midi_runtime::InputInlets::new(
            tutti_graph::EventCapacity::of(16),
            std::time::Duration::ZERO,
        ),
    ))
}

#[test]
fn open_lists_what_is_plugged_in_and_reports_no_event_for_it() {
    let (backend, _) = FakeBackend::build();
    let devices = Devices::open_over(&tutti_platform::Executor::new(), backend).unwrap();
    let events = devices.subscribe();
    // Keystep Pro (in), IAC Bus 1 (in), Synth A (out), IAC Bus 1 (out): the
    // two IAC endpoints share an identity and get two ordinals.
    assert_eq!(devices.list().len(), 4);
    assert_eq!(events.initial().len(), 4);
    assert!(events.try_iter().next().is_none());
}

#[test]
fn plug_is_added_and_unplug_is_removed() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let added = wake(&events);
    let [DeviceEvent::Added(info)] = &added[..] else {
        panic!("one Added, got {added:?}");
    };
    assert_eq!(info.name, "Keys");
    assert_eq!(devices.list(), std::slice::from_ref(info));

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(wake(&events), [DeviceEvent::Removed(info.clone())]);
    assert!(devices.list().is_empty());
}

#[test]
fn a_replug_of_a_known_state_and_an_unknown_unplug_are_silent() {
    let (_devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.unplug(EndpointId::from_raw(9));
    // The channel is in order, so the sentinel arrives after the silent ones.
    fake.plug(endpoint(2, "Pads"), PortDirection::Input);
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.extend(wake(&events));
    }
    let names: Vec<_> = seen.iter().map(|e| e.info().name.as_str()).collect();
    assert_eq!(names, ["Keys", "Pads"]);
    assert!(seen.iter().all(|e| matches!(e, DeviceEvent::Added(_))));
}

#[test]
fn a_rename_is_changed_and_keeps_the_id() {
    let (_devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let [DeviceEvent::Added(before)] = &wake(&events)[..] else {
        panic!("Added");
    };
    let mut renamed = endpoint(1, "Keys");
    renamed.name = "Keys II".into();
    fake.plug(renamed, PortDirection::Input);
    let events = wake(&events);
    let [DeviceEvent::Changed(after)] = &events[..] else {
        panic!("Changed, got {events:?}");
    };
    assert_eq!((&after.id, after.name.as_str()), (&before.id, "Keys II"));
}

#[test]
fn a_wake_takes_the_change_off_the_queue() {
    let (_devices, events, fake) = fake();
    assert!(events.try_iter().next().is_none());
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let got = wake(&events);
    assert_eq!(got.len(), 1);
    assert!(events.try_iter().next().is_none(), "wake consumed it");
}

#[test]
fn a_watch_that_fails_says_so_and_the_list_freezes() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    assert!(events.failure().is_none());
    fake.fail_watch();
    assert!(matches!(
        events.failure(),
        Some(WatchFailure::Failed(e)) if matches!(*e, MidiHardwareError::DescriptorBroken)
    ));
    // The thread has ended: a later plug is not reported.
    fake.plug(endpoint(2, "Pads"), PortDirection::Input);
    assert_eq!(devices.list().len(), 1);
}

/// With no change reported, a `recv` is pending and takes nothing.
#[test]
fn recv_is_pending_while_nothing_changed() {
    let (_devices, events, _fake) = fake();
    let mut recv = std::pin::pin!(events.recv());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(recv.as_mut().poll(&mut cx).is_pending());
    assert!(events.try_iter().next().is_none());
}

/// A pending `recv` is woken by the change that resolves it, once.
#[test]
fn a_change_wakes_the_pending_recv() {
    let (_devices, events, fake) = fake();
    let count = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&count));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut recv = std::pin::pin!(events.recv());
    assert!(recv.as_mut().poll(&mut cx).is_pending());
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        recv.as_mut().poll(&mut cx),
        std::task::Poll::Ready(Ok(DeviceEvent::Added(_)))
    ));
}

/// A pending `recv` is woken when the watch ends, and resolves to why.
#[test]
fn the_end_of_the_watch_wakes_the_pending_recv() {
    let (_devices, events, fake) = fake();
    let count = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&count));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut recv = std::pin::pin!(events.recv());
    assert!(recv.as_mut().poll(&mut cx).is_pending());
    fake.fail_watch();
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        recv.as_mut().poll(&mut cx),
        std::task::Poll::Ready(Err(WatchFailure::Failed(_)))
    ));
}

/// A `recv` polled with one waker and then another, then dropped, leaves no
/// waker in the inbox: a change wakes neither.
#[test]
fn a_dropped_recv_and_a_replaced_waker_leave_nothing_to_wake() {
    let (_devices, events, fake) = fake();
    let (a, b) = (
        Arc::new(WakeCount::default()),
        Arc::new(WakeCount::default()),
    );
    let (wa, wb) = (Waker::from(Arc::clone(&a)), Waker::from(Arc::clone(&b)));
    let mut recv = Box::pin(events.recv());
    assert!(
        recv.as_mut()
            .poll(&mut std::task::Context::from_waker(&wa))
            .is_pending()
    );
    assert!(
        recv.as_mut()
            .poll(&mut std::task::Context::from_waker(&wb))
            .is_pending()
    );
    assert_eq!(
        events.queue.inbox().wakers.len(),
        1,
        "the re-poll replaced it"
    );
    drop(recv);
    assert!(events.queue.inbox().wakers.is_empty());
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    assert_eq!(
        (a.0.load(Ordering::SeqCst), b.0.load(Ordering::SeqCst)),
        (0, 0)
    );
}

/// A waker that counts its wakes.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl std::task::Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn recv_wakes_for_a_change_made_on_another_thread() {
    let (_devices, events, fake) = fake();
    let plugger = std::thread::spawn(move || fake.plug(endpoint(1, "Keys"), PortDirection::Input));
    let events = wake(&events);
    plugger.join().unwrap();
    assert!(matches!(&events[..], [DeviceEvent::Added(_)]));
}

/// Two subscribers each see every change; one reading takes nothing from the
/// other.
#[test]
fn each_subscriber_sees_every_change() {
    let (devices, first, fake) = fake();
    let second = devices.subscribe();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    assert_eq!(wake(&first).len(), 1);
    assert_eq!(wake(&second).len(), 1);
}

/// The list at subscription and the stream after it neither overlap nor leave
/// a gap.
#[test]
fn initial_and_stream_split_at_the_subscription() {
    let (devices, _first, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let late = devices.subscribe();
    fake.plug(endpoint(2, "Pads"), PortDirection::Input);
    let names: Vec<_> = late.initial().iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["Keys"]);
    let streamed = wake(&late);
    assert!(matches!(&streamed[..], [DeviceEvent::Added(i)] if i.name == "Pads"));
}

/// A dropped subscriber leaves the list at once, with no later change to prune
/// it, so an app that wants no events keeps none.
#[test]
fn a_dropped_subscriber_is_forgotten() {
    let (devices, events, _fake) = fake();
    let other = devices.subscribe();
    assert_eq!(devices.state().subscribers.len(), 2);
    drop(events);
    assert_eq!(devices.state().subscribers.len(), 1);
    drop(other);
    assert!(devices.state().subscribers.is_empty());
}

/// The events a failed watch reported are delivered before `Ended`, and a
/// subscriber that arrives later is told at once.
#[test]
fn a_failed_watch_ends_the_stream_after_what_it_reported() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.fail_watch();
    assert!(matches!(next(&events), Ok(DeviceEvent::Added(_))));
    assert!(matches!(next(&events), Err(WatchFailure::Failed(_))));
    assert!(next(&devices.subscribe()).is_err());
}

/// A dropped `OpenInput` retires its port.
#[test]
fn a_dropped_input_retires_its_port() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    wake(&events);
    let ports = ports();
    let (input, _inlet) = devices
        .open_input(
            crate::testing::started(),
            &id_of(&devices, "Keys"),
            &ports,
            DeviceWriters::unread(),
        )
        .unwrap();
    assert_eq!(ports.list_input_ports().len(), 1);
    assert_eq!(input.name(), "Keys");
    drop(input);
    assert!(ports.list_input_ports().is_empty());
}

/// A backend that lists one input and refuses to open it.
struct Refuses;

impl MidiEndpoints for Refuses {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        sink.seed(os_devices(&self.inputs(), &[]))?;
        Ok(Box::new(crate::endpoints::NoWatch))
    }
    fn inputs(&self) -> Vec<OsEndpoint> {
        vec![endpoint(1, "Keys")]
    }
    fn outputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn open_input(
        &self,
        _started: crate::Started,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported("refused"))
    }
    fn open_output(
        &self,
        _started: crate::Started,
        _: EndpointId,
    ) -> Result<Box<dyn crate::DeviceOut>> {
        Err(MidiHardwareError::Unsupported("refused"))
    }
}

/// An open the backend refuses leaves no port behind, however many times it
/// is tried.
#[test]
fn a_backend_failure_leaves_no_port() {
    let devices = Devices::open_over(&tutti_platform::Executor::new(), Box::new(Refuses)).unwrap();
    let ports = ports();
    let keys = id_of(&devices, "Keys");
    for _ in 0..3 {
        assert!(matches!(
            devices.open_input(
                crate::testing::started(),
                &keys,
                &ports,
                DeviceWriters::unread()
            ),
            Err(MidiHardwareError::Unsupported(_))
        ));
    }
    assert!(ports.list_input_ports().is_empty());
}

/// A change that lands while `subscribe` runs is in `initial` or in the
/// stream, never both and never neither. A sender reports 2000 distinct
/// devices on one thread while another subscribes; every device must be
/// accounted for once.
#[test]
fn a_change_during_subscribe_is_in_initial_or_in_the_stream_once() {
    const DEVICES: i32 = 2000;
    for _ in 0..10 {
        let devices =
            Devices::open_over(&tutti_platform::Executor::new(), Box::new(Refuses)).unwrap();
        let sink = ChangeSink {
            shared: Arc::clone(&devices.inner.shared),
        };
        let sender = std::thread::spawn(move || {
            for n in 0..DEVICES {
                sink.send(OsChange::Present(OsDevice {
                    handle: EndpointId::from_raw(1_000 + u64::try_from(n).unwrap()),
                    identity: Identity::CoreMidi(n),
                    name: format!("d{n}"),
                    direction: PortDirection::Input,
                }));
            }
        });
        // `Refuses` seeds one device; wait for the sender to be under way.
        while devices.list().len() < 20 {
            std::thread::yield_now();
        }
        let events = devices.subscribe();
        sender.join().unwrap();
        let mut seen: Vec<String> = events.initial().iter().map(|d| d.name.clone()).collect();
        seen.extend(events.try_iter().map(|e| e.info().name.clone()));
        seen.retain(|name| name != "Keys");
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            total,
            "a device is in both initial and the stream"
        );
        assert_eq!(
            total,
            usize::try_from(DEVICES).unwrap(),
            "a device is in neither"
        );
    }
}

/// A saved device resolves through the real `Devices`: a unit's ports by name
/// and direction, and a lone same-named device only by name.
#[test]
fn a_saved_device_resolves_through_devices() {
    let (devices, events, fake) = fake();
    let port = |raw, name: &str, direction| {
        let mut e = endpoint(raw, name);
        e.identity = Identity::Product(ProductInstanceId::new("unit-1").unwrap());
        fake.plug(e, direction);
    };
    port(1, "Pad In", PortDirection::Input);
    port(2, "Pad Out", PortDirection::Output);
    fake.plug(endpoint(3, "Keys"), PortDirection::Input);
    let mut seen = 0;
    while seen < 3 {
        seen += wake(&events).len();
    }
    let by_name = |name: &str| devices.list().into_iter().find(|d| d.name == name).unwrap();
    let (pad_out, keys) = (by_name("Pad Out"), by_name("Keys"));
    assert_eq!(devices.resolve(&pad_out), Resolved::Exact(pad_out.clone()));
    assert_eq!(devices.resolve(&keys), Resolved::SameName(keys.clone()));

    // Unplugged and plugged back: the same answers.
    fake.unplug(EndpointId::from_raw(2));
    assert_eq!(
        devices.resolve(&pad_out),
        Resolved::Ambiguous(vec![by_name("Pad In")])
    );
    let mut again = endpoint(7, "Pad Out");
    again.identity = Identity::Product(ProductInstanceId::new("unit-1").unwrap());
    fake.plug(again, PortDirection::Output);
    assert_eq!(
        devices.resolve(&pad_out),
        Resolved::Exact(by_name("Pad Out"))
    );
}

#[test]
fn dropping_devices_stops_the_watch_thread() {
    let (backend, fake) = FakeBackend::build();
    let devices = Devices::open_over(&tutti_platform::Executor::new(), backend).unwrap();
    assert_eq!(
        fake.watchers(),
        1,
        "the watch thread runs while Devices lives"
    );
    drop(devices);
    assert_eq!(fake.watchers(), 0, "and has ended, joined, when it drops");
}

#[test]
fn open_input_takes_a_present_input_only() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.plug(endpoint(10, "Synth"), PortDirection::Output);
    let mut seen = 0;
    while seen < 2 {
        seen += wake(&events).len();
    }
    assert!(
        devices
            .open_input(
                crate::testing::started(),
                &id_of(&devices, "Keys"),
                &ports(),
                DeviceWriters::unread()
            )
            .is_ok()
    );
    let synth = id_of(&devices, "Synth");
    assert!(matches!(
        devices.open_input(
            crate::testing::started(),
            &synth,
            &ports(),
            DeviceWriters::unread()
        ),
        Err(MidiHardwareError::WrongDirection { .. })
    ));
    assert!(
        devices
            .open_output(crate::testing::started(), &synth)
            .is_ok()
    );
    assert!(
        devices
            .open_output(crate::testing::started(), &id_of(&devices, "Keys"))
            .is_err()
    );
}

#[test]
fn open_input_refuses_a_device_that_was_unplugged() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    wake(&events);
    let keys = id_of(&devices, "Keys");
    fake.unplug(EndpointId::from_raw(1));
    wake(&events);
    assert!(matches!(
        devices.open_input(
            crate::testing::started(),
            &keys,
            &ports(),
            DeviceWriters::unread()
        ),
        Err(MidiHardwareError::NotFound(NotFound::Device(_)))
    ));
}

#[test]
fn a_duplex_device_opens_both_ways() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Box"), PortDirection::Duplex);
    wake(&events);
    let id = id_of(&devices, "Box");
    assert_eq!(devices.list()[0].direction, PortDirection::Duplex);
    assert!(
        devices
            .open_input(
                crate::testing::started(),
                &id,
                &ports(),
                DeviceWriters::unread()
            )
            .is_ok()
    );
    assert!(devices.open_output(crate::testing::started(), &id).is_ok());
}

#[test]
fn capability_is_the_backends_for_a_present_device() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    wake(&events);
    let caps = devices.capability(&id_of(&devices, "Keys")).unwrap();
    assert_eq!(caps.active().protocol, tutti_ump::Protocol::Midi2);
}

#[test]
fn os_devices_merges_an_endpoint_listed_both_ways_into_a_duplex() {
    let both = endpoint(1, "Box");
    let only_out = endpoint(2, "Synth");
    let devices = os_devices(std::slice::from_ref(&both), &[both.clone(), only_out]);
    let directions: Vec<_> = devices.iter().map(|d| d.direction).collect();
    assert_eq!(directions, [PortDirection::Duplex, PortDirection::Output]);
}

/// A subscriber's queue on `shared`, as `Devices::subscribe` makes one.
fn queue_on(shared: &Shared) -> Arc<Queue> {
    let queue = Arc::new(Queue::default());
    shared.state().subscribers.push(Arc::downgrade(&queue));
    queue
}

fn shape(queue: &Queue) -> Vec<(&'static str, String)> {
    queue
        .inbox()
        .events
        .iter()
        .map(|e| match e {
            DeviceEvent::Added(i) => ("added", i.name.clone()),
            DeviceEvent::Removed(i) => ("removed", i.name.clone()),
            DeviceEvent::Changed(i) => ("changed", i.name.clone()),
        })
        .collect()
}

fn reporter() -> (Arc<Queue>, Reporter) {
    let shared = Arc::new(Shared::default());
    let queue = queue_on(&shared);
    let reporter = Reporter::new(ChangeSink { shared });
    (queue, reporter)
}

/// A handle the OS says departed is a new device when it is present again, even
/// under the same name.
#[test]
fn a_handle_the_os_says_departed_is_a_new_device() {
    let (queue, mut reporter) = reporter();
    let same = os_devices(&[endpoint(7, "Twin")], &[]);
    reporter.report(same.clone(), |_| false);
    reporter.report(same, |h| h == EndpointId::from_raw(7));
    assert_eq!(
        shape(&queue),
        [
            ("added", "Twin".into()),
            ("removed", "Twin".into()),
            ("added", "Twin".into())
        ]
    );
}

/// Where the identity is the name, a handle that stays under a new name is a
/// different device: removed, then added.
#[test]
fn a_handle_under_another_identity_is_a_different_device() {
    let (queue, mut reporter) = reporter();
    reporter.report(os_devices(&[endpoint(7, "Old")], &[]), |_| false);
    reporter.report(os_devices(&[endpoint(7, "New")], &[]), |_| false);
    assert_eq!(
        shape(&queue),
        [
            ("added", "Old".into()),
            ("removed", "Old".into()),
            ("added", "New".into())
        ]
    );
}

/// Where the identity survives a rename, the device is `Changed`.
#[test]
fn a_handle_with_the_same_identity_and_a_new_name_is_changed() {
    let (queue, mut reporter) = reporter();
    reporter.report(os_devices(&[endpoint(7, "Old")], &[]), |_| false);
    let mut renamed = endpoint(7, "Old");
    renamed.name = "New".into();
    reporter.report(os_devices(&[renamed], &[]), |_| false);
    assert_eq!(
        shape(&queue),
        [("added", "Old".into()), ("changed", "New".into())]
    );
}

#[test]
fn the_reporter_reports_a_departure_a_re_enumeration_omits() {
    let (queue, mut reporter) = reporter();
    let both = os_devices(&[endpoint(1, "A"), endpoint(2, "B")], &[]);
    reporter.report(both.clone(), |_| false);
    reporter.report(both.clone(), |_| false);
    assert_eq!(queue.inbox().events.len(), 2, "a repeat adds nothing");
    reporter.report(both[..1].to_vec(), |_| false);
    let pending: Vec<_> = queue.inbox().events.iter().cloned().collect();
    assert!(matches!(
        pending.last(),
        Some(DeviceEvent::Removed(info)) if info.name == "B"
    ));
}

/// A device reported gone and present again is never missing from the list
/// between the two: they are applied under one hold of the state. A reader
/// polls `list` while the reporter replaces the device; a miss only passes, so
/// the loop is long.
#[test]
fn a_replaced_device_is_never_missing_from_the_list() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let shared = Arc::new(Shared::default());
    let devices = Devices {
        inner: Arc::new(Inner {
            _watch: Box::new(crate::endpoints::NoWatch),
            backend: FakeBackend::build().0,
            shared: Arc::clone(&shared),
            executor: tutti_platform::Executor::new(),
        }),
    };
    let mut reporter = Reporter::new(ChangeSink { shared });
    let twin = os_devices(&[endpoint(7, "Twin")], &[]);
    reporter.seed(twin.clone()).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (devices, done) = (devices.clone(), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut misses = 0_usize;
            while !done.load(Ordering::Relaxed) {
                misses += usize::from(devices.list().is_empty());
            }
            misses
        })
    };
    for _ in 0..20_000 {
        reporter.report(twin.clone(), |h| h == EndpointId::from_raw(7));
    }
    done.store(true, Ordering::Relaxed);
    assert_eq!(reader.join().unwrap(), 0, "the list was seen without Twin");
}

/// A sink over a fresh table, and the `Devices` that reads it.
fn sink_over_empty() -> (ChangeSink, Devices) {
    let shared = Arc::new(Shared::default());
    let devices = Devices {
        inner: Arc::new(Inner {
            _watch: Box::new(crate::endpoints::NoWatch),
            backend: FakeBackend::build().0,
            shared: Arc::clone(&shared),
            executor: tutti_platform::Executor::new(),
        }),
    };
    (ChangeSink { shared }, devices)
}

// The list is seeded once: a second seed is refused and changes nothing.
#[test]
fn a_second_seed_is_refused_and_leaves_the_list() {
    let (sink, devices) = sink_over_empty();
    assert_eq!(sink.seed(os_devices(&[endpoint(1, "First")], &[])), Ok(()));
    assert_eq!(
        sink.seed(os_devices(&[endpoint(2, "Second")], &[])),
        Err(SeedError)
    );
    let names: Vec<_> = devices.list().into_iter().map(|d| d.name).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].contains("First"), "{names:?}");
}

// A seed after a change was reported is late: it would rewrite what a
// subscriber may already have read.
#[test]
fn a_seed_after_a_change_is_refused() {
    let (sink, devices) = sink_over_empty();
    let events = devices.subscribe();
    let present = os_devices(&[endpoint(1, "Keys")], &[]);
    sink.send(OsChange::Present(present[0].clone()));
    assert_eq!(sink.seed(present), Err(SeedError));
    assert_eq!(events.try_iter().count(), 1, "only the reported change");
}

/// A backend whose one output records the Protocols it is told to follow.
struct Tagging(Arc<std::sync::Mutex<Vec<tutti_ump::Protocol>>>);

struct TaggedSink(Arc<std::sync::Mutex<Vec<tutti_ump::Protocol>>>);

impl tutti_midi_runtime::MidiOut for TaggedSink {
    fn queue_timed(&self, events: &[tutti_midi_runtime::Outgoing]) -> tutti_midi_runtime::Taken {
        tutti_midi_runtime::Taken::within(events, events.len())
    }
}

impl crate::DeviceOut for TaggedSink {
    fn follow(&self, protocol: tutti_ump::Protocol) {
        self.0.lock().unwrap().push(protocol);
    }
}

impl MidiEndpoints for Tagging {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        sink.seed(os_devices(&[], &self.outputs()))?;
        Ok(Box::new(crate::endpoints::NoWatch))
    }
    fn inputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<OsEndpoint> {
        vec![endpoint(1, "Synth")]
    }
    fn open_input(
        &self,
        _started: crate::Started,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported("no inputs"))
    }
    fn open_output(
        &self,
        _started: crate::Started,
        _: EndpointId,
    ) -> Result<Box<dyn crate::DeviceOut>> {
        Ok(Box::new(TaggedSink(Arc::clone(&self.0))))
    }
}

/// The sink is told the Protocol the batches are in, as the output opens and
/// again at each change of configuration: a transport that tags its packets
/// (CoreMIDI's event list) tags them with the negotiated Protocol, not the one
/// the OS reported at open (M2-104 §7.1.6.2: the Protocol is what the Stream
/// Configuration settled).
#[test]
fn the_sink_follows_the_protocol_the_output_is_in() {
    use tutti_ump::{Protocol, StreamConfig};
    let told = Arc::default();
    let devices = Devices::open_over(
        &tutti_platform::Executor::new(),
        Box::new(Tagging(Arc::clone(&told))),
    )
    .unwrap();
    let mut out = devices
        .open_output(crate::testing::started(), &id_of(&devices, "Synth"))
        .unwrap();
    assert_eq!(*told.lock().unwrap(), [Protocol::Midi2], "at open");
    out.set_config(StreamConfig {
        protocol: Protocol::Midi1,
        rx_jr: false,
        tx_jr: false,
    });
    assert_eq!(*told.lock().unwrap(), [Protocol::Midi2, Protocol::Midi1]);
}

/// **`for event in &events` takes what `try_iter` would**, oldest first, and
/// leaves nothing queued: the same queue is read, not a copy.
#[test]
fn a_reference_to_the_events_iterates_the_queued_changes() {
    let (_devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.plug(endpoint(2, "Pads"), PortDirection::Input);
    // Both reached the queue (the sentinel orders the watch thread).
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let first = wake(&events);
        seen.extend(first.iter().map(|e| e.info().name.clone()));
    }
    fake.plug(endpoint(3, "Drums"), PortDirection::Input);
    fake.plug(endpoint(4, "Bass"), PortDirection::Input);
    let mut queued = Vec::new();
    while queued.len() < 2 {
        let mut batch = Vec::new();
        for event in &events {
            batch.push(event.info().name.clone());
        }
        queued.extend(batch);
        std::thread::yield_now();
    }
    assert_eq!(queued, ["Drums", "Bass"], "oldest first");
    assert!(events.try_iter().next().is_none(), "the loop took them");
}

/// What the Active Sensing thread tests share: a fake output open over
/// `Synth`, in `protocol`, and the fake that records what it was sent. The
/// schedule's timing is `Pulse`'s tests; these cover the thread that runs it.
mod sensing {
    use std::num::NonZeroUsize;
    use tutti_platform::{Executor, Instant, Threads};

    use tutti_midi_device::{EndpointId, PortDirection};
    use tutti_ump::{Message, MidiGroup, Protocol, StreamConfig, System, Ump};

    use super::{Devices, FakeCounters, PATIENCE, endpoint, fake_on, id_of, wake};
    use crate::OpenOutput;

    const SYNTH: u64 = 1;

    fn active_sensing() -> Ump {
        Ump::from(Message::System(MidiGroup::FIRST, System::ActiveSensing))
    }

    /// The output, the fake behind it, its `Devices`, and the threads that drive
    /// the heartbeat's executor: they stop when the last of these drops.
    fn output_in(
        protocol: Protocol,
        sensing: bool,
    ) -> (OpenOutput, FakeCounters, Devices, Threads) {
        let executor = Executor::new();
        let threads = executor.spawn_threads(NonZeroUsize::MIN).unwrap();
        let (devices, events, fake) = fake_on(&executor);
        fake.plug(endpoint(SYNTH, "Synth"), PortDirection::Output);
        wake(&events);
        let mut out = devices
            .open_output(crate::testing::started(), &id_of(&devices, "Synth"))
            .unwrap();
        out.set_config(StreamConfig {
            protocol,
            rx_jr: false,
            tx_jr: false,
        });
        if sensing {
            out.active_sensing();
        }
        (out, fake, devices, threads)
    }

    fn sent(fake: &FakeCounters) -> Vec<Ump> {
        fake.take_sent(EndpointId::from_raw(SYNTH))
    }

    /// What the output is sent next, polled under a hang guard.
    fn next_sent(fake: &FakeCounters) -> Vec<Ump> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let got = sent(fake);
            if !got.is_empty() {
                return got;
            }
            assert!(Instant::now() < deadline, "nothing was sent");
            std::thread::yield_now();
        }
    }

    /// **An idle output with the option sends Active Sensing, and goes on.**
    #[test]
    fn an_idle_output_with_the_option_sends_active_sensing_and_keeps_sending() {
        let (_out, fake, _devices, _threads) = output_in(Protocol::Midi1, true);
        let first = next_sent(&fake);
        assert!(first.iter().all(|&u| u == active_sensing()), "{first:?}");
        let later = next_sent(&fake);
        assert!(
            later.iter().all(|&u| u == active_sensing()),
            "and more: {later:?}"
        );
    }

    /// **An output opened without the option never sends it**: it has no
    /// heartbeat task.
    #[test]
    fn an_output_without_the_option_never_sends_active_sensing() {
        let (out, fake, _devices, _threads) = output_in(Protocol::Midi1, false);
        assert!(out.heartbeat.is_none());
        assert_eq!(sent(&fake), []);
    }

    /// **A MIDI 2.0 Protocol output sends once the Protocol is MIDI 1.0.**
    #[test]
    fn a_midi2_protocol_output_sends_active_sensing_once_it_is_in_midi1() {
        let (mut out, fake, _devices, _threads) = output_in(Protocol::Midi2, true);
        out.set_config(StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: false,
        });
        let got = next_sent(&fake);
        assert!(got.iter().all(|&u| u == active_sensing()), "{got:?}");
    }
}

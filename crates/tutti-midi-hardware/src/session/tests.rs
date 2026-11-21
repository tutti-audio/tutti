use super::*;
use crate::InputProducerHandle;
use crate::capability::OsEndpoint;
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, NoWatch, Watch};
use crate::test_support::FakeBackend;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_fixtures::midi;
use tutti_graph::{Offset, UmpAt};
use tutti_midi_device::EndpointId;

/// A session over the shared fake backend, plus its two counters.
///
/// Returned as a tuple rather than the `FakeCounters` struct so each test
/// names only the counter it uses — `(s, _, opens)` says at a glance that
/// this one is about opens.
/// Inlets of 64 events, with no lead.
fn inlets() -> Arc<InputInlets> {
    InputInlets::new(
        tutti_graph::EventCapacity::of(64),
        std::time::Duration::ZERO,
    )
}

fn session() -> (MidiSession, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (backend, counters) = FakeBackend::build();
    let inlets = inlets();
    (
        MidiSession::open_over(
            &tutti_platform::Executor::new(),
            backend,
            inlets,
            crate::MidiHardwareConfig::new(),
        )
        .unwrap(),
        counters.sent,
        counters.opens,
    )
}

/// The id of the input listed under `name`.
fn input(s: &MidiSession, name: &str) -> DeviceId {
    s.inputs()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no input {name}"))
        .id
}

fn note() -> UmpAt {
    use tutti_ump::MidiChannel;
    (Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 0x8000))
}

#[test]
fn enumeration_passes_the_backend_through() {
    let (s, _, _) = session();
    assert_eq!(s.inputs().len(), 2);
    assert_eq!(s.outputs().len(), 2);
    let names: Vec<_> = s.inputs().into_iter().map(|d| d.name).collect();
    assert_eq!(names, ["IAC Bus 1", "Keystep Pro"], "ordered by id");
}

#[test]
fn connecting_an_input_makes_it_connected() {
    let (mut s, _, _) = session();
    assert!(!s.is_any_input_connected());
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "Keystep Pro"))
        .unwrap();
    assert!(s.is_any_input_connected());
    assert_eq!(s.connected_input_names(), vec!["Keystep Pro".to_string()]);
}

/// Reconnecting must not reopen the port — a second driver connection to one
/// device duplicates every inbound event.
#[test]
fn connecting_an_open_input_is_idempotent() {
    let (mut s, _, opens) = session();
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "Keystep Pro"))
        .unwrap();
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "Keystep Pro"))
        .unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 1, "the port opens once");
    assert_eq!(s.connected_input_names().len(), 1);
}

#[test]
fn by_name_matching_is_case_insensitive_substring() {
    let (mut s, _, _) = session();
    let _ = s
        .connect_input(crate::testing::started(), "keystep")
        .unwrap();
    assert_eq!(s.connected_input_names(), vec!["Keystep Pro".to_string()]);
}

#[test]
fn connecting_an_absent_device_is_an_error_not_a_silent_noop() {
    let (backend, counters) = FakeBackend::build();
    let mut s = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets(),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    // A name that matches nothing says what it searched, so a typo shows what
    // was there.
    let Err(MidiHardwareError::NoDeviceNamed { name, searched }) =
        s.connect_input(crate::testing::started(), "nonexistent")
    else {
        panic!("a name that matches no input is NoDeviceNamed");
    };
    assert_eq!(name, "nonexistent");
    assert_eq!(searched, ["IAC Bus 1", "Keystep Pro"], "the inputs, by id");
    // The output side searches the outputs.
    let Err(MidiHardwareError::NoDeviceNamed { searched, .. }) =
        s.connect_output(crate::testing::started(), "nonexistent")
    else {
        panic!("a name that matches no output is NoDeviceNamed");
    };
    assert_eq!(searched, ["IAC Bus 1", "Synth A"], "the outputs, by id");
    // An id that was listed and is gone: the shape of a stale id.
    let gone = input(&s, "IAC Bus 1");
    let events = s.devices().subscribe();
    counters.unplug(EndpointId::from_raw(2));
    assert!(
        tutti_fixtures::block_on_within(events.recv(), std::time::Duration::from_secs(10))
            .expect("an event before the guard")
            .is_ok()
    );
    assert!(s.connect_input(crate::testing::started(), &gone).is_err());
    assert!(!s.is_any_input_connected());
}

#[test]
fn disconnecting_closes_only_the_named_input() {
    let (mut s, _, _) = session();
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "Keystep Pro"))
        .unwrap();
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "IAC Bus 1"))
        .unwrap();
    s.disconnect_input("iac");
    assert_eq!(s.connected_input_names(), vec!["Keystep Pro".to_string()]);
    s.disconnect_all_inputs();
    assert!(!s.is_any_input_connected());
}

/// **A name and an id pick the same device**: one `connect` and one
/// `disconnect` take either, and a name that matches several devices
/// disconnects the lowest id among the open ones (the choice is not the map's
/// order).
#[test]
fn a_query_by_id_and_by_name_reach_the_same_device() {
    let (mut s, _, _) = session();
    let keystep = input(&s, "Keystep Pro");
    let _ = s.connect_input(crate::testing::started(), "pro").unwrap();
    assert!(s.is_input_connected(&keystep), "by name");
    assert!(s.disconnect_input(&keystep), "by id");
    assert!(!s.disconnect_input("pro"), "nothing is open to close");

    // Both open inputs' names contain an "s"; the disconnect closes the
    // lowest id.
    let iac = input(&s, "IAC Bus 1");
    let _ = s
        .connect_input(crate::testing::started(), &keystep)
        .unwrap();
    let _ = s.connect_input(crate::testing::started(), &iac).unwrap();
    assert!(
        iac < keystep,
        "the fixture's ids: {iac:?} below {keystep:?}"
    );
    assert!(s.disconnect_input("s"));
    assert!(
        s.is_input_connected(&keystep) && !s.is_input_connected(&iac),
        "the lowest id went"
    );
}

/// **A dead input's fault reaches the control side**: the session reads the
/// cause the backend's reader died of, for that device only, and none while it
/// delivers.
#[test]
fn a_dead_inputs_fault_is_read_through_the_session() {
    let (backend, fake) = FakeBackend::build();
    let mut s = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets(),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    let keystep = input(&s, "Keystep Pro");
    let iac = input(&s, "IAC Bus 1");
    let _ = s
        .connect_input(crate::testing::started(), &keystep)
        .unwrap();
    let _ = s.connect_input(crate::testing::started(), &iac).unwrap();
    assert_eq!(s.input_fault(&keystep), None);

    let fault = InputFault::WaitFailed(crate::fault::Errno::new(19));
    fake.kill_input_reader(EndpointId::from_raw(1), fault);
    assert_eq!(s.input_fault(&keystep), Some(fault));
    assert!(!s.is_input_connected(&keystep));
    assert_eq!(s.input_fault(&iac), None, "the other input delivers");
    assert!(s.is_input_connected(&iac));
}

/// **An input that is not held reads no fault**, before it is opened and after
/// it is closed; it is not connected either, so the two differ only in the
/// cause.
#[test]
fn an_input_that_is_not_held_reads_no_fault() {
    let (mut s, _, _) = session();
    let keystep = input(&s, "Keystep Pro");
    assert_eq!(s.input_fault(&keystep), None);
    assert!(!s.is_input_connected(&keystep));
    let _ = s
        .connect_input(crate::testing::started(), &keystep)
        .unwrap();
    assert!(s.disconnect_input(&keystep));
    assert_eq!(s.input_fault(&keystep), None);
}

#[test]
fn an_output_reaches_its_device() {
    let (s, sent, _) = session();
    let mut output = s
        .connect_output(crate::testing::started(), "synth a")
        .unwrap();
    assert_eq!(output.name(), "Synth A");
    let synth_a = s
        .outputs()
        .into_iter()
        .find(|d| d.name == "Synth A")
        .unwrap()
        .id;
    assert_eq!(output.id(), &synth_a);
    assert_eq!(output.queue(&[note(), note()]).packets(), 2);
    assert_eq!(sent.load(Ordering::SeqCst), 2);
}

/// **A send time reaches the device's sink with its packet.** Two events, one
/// `Now` and one `At` an instant, through the output's `queue_timed` and its
/// wire conversion: the sink is told each one's time, and a plain `queue` is
/// `Now` throughout.
#[test]
fn a_send_time_reaches_the_sink_with_its_packet() {
    use tutti_midi_runtime::{Outgoing, SendAt};
    let (backend, counters) = FakeBackend::build_accepting(usize::MAX);
    let s = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets(),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    let mut output = s
        .connect_output(crate::testing::started(), "synth a")
        .unwrap();
    let raw = EndpointId::from_raw(10);
    let t = tutti_platform::Instant::now() + std::time::Duration::from_secs(1);
    let (at, ump) = note();
    let events = [
        Outgoing::new(at, SendAt::Now, ump),
        Outgoing::new(at, SendAt::At(t), ump),
    ];
    assert_eq!(output.queue_timed(&events).packets(), 2);
    assert_eq!(
        counters.take_sent_timed(raw),
        [(ump, SendAt::Now), (ump, SendAt::At(t))]
    );
    assert_eq!(output.queue(&[note()]).packets(), 1);
    assert_eq!(counters.take_sent_timed(raw), [(ump, SendAt::Now)]);
}

/// A connected device that refuses everything must report `0`, not
/// `events.len()`.
///
/// The failure this pins: returning the *attempted* count whenever an output
/// is open makes a device rejecting every event indistinguishable from one
/// accepting them all. The sink counts what it was offered, so the `0` is the
/// device's refusal and not an output that never reached it.
#[test]
fn a_refusing_output_reports_zero_accepted() {
    let (backend, counters) = FakeBackend::build_accepting(0);
    let sent = counters.sent;
    let s = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets(),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    let mut output = s
        .connect_output(crate::testing::started(), "synth a")
        .unwrap();

    assert_eq!(
        output.queue(&[note(), note()]).packets(),
        0,
        "the device accepted none"
    );
    assert_eq!(
        sent.load(Ordering::SeqCst),
        2,
        "both were offered to the sink"
    );
}

/// A partial accept reports the prefix that landed, not the batch size.
#[test]
fn a_partial_accept_reports_what_landed() {
    let (backend, _counters) = FakeBackend::build_accepting(1);
    let s = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets(),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    let mut output = s
        .connect_output(crate::testing::started(), "synth a")
        .unwrap();

    assert_eq!(output.queue(&[note(), note(), note()]).packets(), 1);
}

/// A System Exclusive message of three packets, start, continue and end.
fn sysex() -> [UmpAt; 3] {
    use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus};
    [SysexStatus::Start, SysexStatus::Continue, SysexStatus::End].map(|status| {
        (
            Offset::ZERO,
            tutti_ump::Ump::from(Message::Sysex7(
                MidiGroup::FIRST,
                Sysex7Packet::new(status, &[1, 2]).unwrap(),
            )),
        )
    })
}

/// **A device that has room for two of a message's three packets takes none of
/// it**, nor anything after it, and the output counts what was left. A count
/// cut inside a System Exclusive message would leave the device waiting for an
/// end that never comes.
#[test]
fn a_message_the_device_cannot_take_whole_is_refused_whole_and_counted() {
    let (backend, counters) = FakeBackend::build_accepting(2);
    let s = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets(),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    let mut output = s
        .connect_output(crate::testing::started(), "synth a")
        .unwrap();
    let raw = EndpointId::from_raw(10);
    let [start, middle, end] = sysex();
    let batch = [note(), start, middle, end, note()];

    let taken = output.queue(&batch);
    assert_eq!(
        taken.packets(),
        1,
        "the note before the message, and no more"
    );
    assert_eq!(counters.take_sent(raw), [note().1]);
    assert_eq!(output.losses(), tutti_graph::Losses::refused(4));
}

/// An output is the caller's value, not the session's: each connect returns
/// its own, on its own device, and the first keeps delivering after a second
/// opens. A destination is replaced by the caller dropping one value for
/// another.
#[test]
fn each_connected_output_is_its_own_value_on_its_own_device() {
    let (backend, counters) = FakeBackend::build_accepting(usize::MAX);
    let s = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets(),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    let id = |name: &str| s.outputs().into_iter().find(|d| d.name == name).unwrap().id;
    let mut synth = s
        .connect_output(crate::testing::started(), &id("Synth A"))
        .unwrap();
    let mut iac = s
        .connect_output(crate::testing::started(), &id("IAC Bus 1"))
        .unwrap();
    assert_eq!(synth.id(), &id("Synth A"));
    assert_eq!(iac.name(), "IAC Bus 1");

    assert_eq!(synth.queue(&[note()]).packets(), 1);
    assert_eq!(iac.queue(&[note(), note()]).packets(), 2);
    assert_eq!(counters.take_sent(EndpointId::from_raw(10)).len(), 1);
    assert_eq!(counters.take_sent(EndpointId::from_raw(11)).len(), 2);
}

/// Repeated connect/disconnect cycles keep the port set bounded: a
/// disconnect retires its port, and the reconnect opens a new one.
///
/// Device 2 stays connected throughout.
#[test]
fn hot_plug_cycles_keep_the_port_set_bounded() {
    let (mut s, _, opens) = session();
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "Keystep Pro"))
        .unwrap();
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "IAC Bus 1"))
        .unwrap();
    for cycle in 0..100 {
        s.disconnect_input(&input(&s, "Keystep Pro"));
        assert_eq!(s.ports().list_input_ports().len(), 1, "cycle {cycle}");
        let _ = s
            .connect_input(crate::testing::started(), &input(&s, "Keystep Pro"))
            .unwrap();
        assert_eq!(s.ports().list_input_ports().len(), 2, "cycle {cycle}");
        assert_eq!(s.ports().port_count(), 2, "cycle {cycle}");
    }
    assert_eq!(
        opens.load(Ordering::SeqCst),
        102,
        "each cycle really reopened"
    );

    s.disconnect_all_inputs();
    assert!(s.ports().list_input_ports().is_empty());
    assert_eq!(s.ports().port_count(), 0, "the set shrinks back to nothing");
}

/// A backend whose connections, and itself, log their drop, and which can
/// be told to refuse every open.
struct LoggingBackend {
    log: Arc<Mutex<Vec<&'static str>>>,
    refuse: bool,
}

struct LoggingConnection {
    log: Arc<Mutex<Vec<&'static str>>>,
    _producer: InputProducerHandle,
}

impl InputConnection for LoggingConnection {}

impl Drop for LoggingConnection {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("connection");
    }
}

impl Drop for LoggingBackend {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("backend");
    }
}

impl MidiEndpoints for LoggingBackend {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        sink.seed(os_devices(&self.inputs(), &[]))?;
        Ok(Box::new(NoWatch))
    }
    fn inputs(&self) -> Vec<OsEndpoint> {
        vec![crate::test_support::endpoint(1, "Logged")]
    }
    fn outputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn open_input(
        &self,
        _started: crate::Started,
        _id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        if self.refuse {
            return Err(MidiHardwareError::Unsupported("refused"));
        }
        Ok(Box::new(LoggingConnection {
            log: Arc::clone(&self.log),
            _producer: producer,
        }))
    }
    fn open_output(&self, _started: crate::Started, _id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        Err(MidiHardwareError::Unsupported("no outputs"))
    }
}

fn logging_session(refuse: bool) -> (MidiSession, Arc<Mutex<Vec<&'static str>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let backend = LoggingBackend {
        log: Arc::clone(&log),
        refuse,
    };
    let inlets = inlets();
    (
        MidiSession::open_over(
            &tutti_platform::Executor::new(),
            Box::new(backend),
            inlets,
            crate::MidiHardwareConfig::new(),
        )
        .unwrap(),
        log,
    )
}

/// Dropping the session closes its inputs before the backend they were
/// opened on, and retires their ports from a manager that outlives it.
#[test]
fn dropping_the_session_closes_inputs_then_the_backend_and_retires_ports() {
    let (mut s, log) = logging_session(false);
    let _ = s
        .connect_input(crate::testing::started(), &input(&s, "Logged"))
        .unwrap();
    let ports = Arc::clone(s.ports());
    assert_eq!(ports.list_input_ports().len(), 1);

    drop(s);
    assert_eq!(*log.lock().unwrap(), vec!["connection", "backend"]);
    assert!(
        ports.list_input_ports().is_empty(),
        "the session's port does not outlive it in a shared manager"
    );
}

/// An open the backend refuses leaves no port behind.
#[test]
fn a_refused_open_leaves_no_port() {
    let (mut s, _log) = logging_session(true);
    for _ in 0..3 {
        assert!(
            s.connect_input(crate::testing::started(), &input(&s, "Logged"))
                .is_err()
        );
    }
    assert!(!s.is_any_input_connected());
    assert!(s.ports().list_input_ports().is_empty());
    assert_eq!(s.ports().port_count(), 0);
}

/// A session moves between threads with the Property Exchange Responders its
/// devices hold, so the Session that owns it does too.
#[test]
fn a_session_is_send() {
    // The bound is the test: this stops compiling when a session is not `Send`.
    fn is_send<T: Send>(_: &T) -> bool {
        true
    }
    let (backend, _counters) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let inlets = tutti_midi_runtime::InputInlets::new(
        tutti_graph::EventCapacity::of(8),
        std::time::Duration::ZERO,
    );
    let session = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        inlets,
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    assert!(is_send(&session));
}

/// The options are stored as written: a timing survives a round trip, and a
/// default value reads back as the default.
#[cfg(feature = "serde")]
#[test]
fn the_options_round_trip_through_serde() {
    let options = MidiHardwareConfig::new().input_timing(InputTiming::JitterReduction);
    let text = serde_json::to_string(&options).unwrap();
    assert_eq!(
        serde_json::from_str::<MidiHardwareConfig>(&text).unwrap(),
        options
    );
    let none = serde_json::to_string(&MidiHardwareConfig::default()).unwrap();
    assert_eq!(
        serde_json::from_str::<MidiHardwareConfig>(&none).unwrap(),
        MidiHardwareConfig::new()
    );
}

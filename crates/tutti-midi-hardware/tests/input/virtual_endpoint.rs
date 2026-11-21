//! Virtual endpoints against the kernel's sequencer: another client subscribes
//! to what this process publishes, as another application does.
//!
//! Each test names its endpoints with its own process id, so tests (each its
//! own process under nextest) and other clients on the machine do not meet.

use std::time::Duration;

use tutti_platform::Instant;

use super::listener::Listener;

use tutti_alsa_ffi::{Client, Event, PortCaps, PortEntry, PortId, PortKind, Read, UmpVersion};
use tutti_fixtures::main_thread;
use tutti_fixtures::midi::{
    device_identity, m1_cc, m1_note_on, note_on, per_note_pitch_bend, program_change, registered,
};
use tutti_graph::EventCapacity;
use tutti_midi_device::FunctionBlock;
use tutti_midi_hardware::{HardwareMidiInputs, VirtualEndpoint, VirtualProtocol, VirtualSpec};
use tutti_midi_runtime::InputInlets;
use tutti_platform::MainThreadMarker;
use tutti_ump::{
    Bank, BlockDirection, CCNumber, FirstGroup, FunctionBlockId, GroupSpan, Message, MidiChannel,
    ProductInstanceId, Protocol, Rpn, U7, UiHint, Ump, Utility, Voice1,
};
use tutti_ump::{BlockName, EndpointName};

const PATIENCE: Duration = Duration::from_secs(30);

/// The main-thread proof for an endpoint that declares no Function Blocks: such an
/// endpoint is no main-thread object on any backend, so the fixture's stand-in is
/// enough. Never use it for one with blocks (`tests/virtual_blocks`).
fn plain_endpoint_main() -> MainThreadMarker {
    main_thread()
}

fn spec(tag: &str) -> VirtualSpec {
    VirtualSpec::new(
        EndpointName::new(&format!("tutti-virt-{}-{tag}", std::process::id())).unwrap(),
        device_identity(),
        None,
    )
}

/// The sequencer's port of the endpoint `spec` published.
fn find(peer: &Client, spec: &VirtualSpec) -> Option<PortEntry> {
    peer.list_ports()
        .ports
        .into_iter()
        .find(|p| p.client_name == spec.name().as_str())
}

/// The endpoint `spec` describes, for a test that only sends: what peers write to
/// it goes to an inlet nothing reads.
fn open_sending(executor: &tutti_platform::Executor, spec: &VirtualSpec) -> VirtualEndpoint {
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let ports = HardwareMidiInputs::new(inlets);
    let producer = ports.open_input_port("virtual").0;
    super::opening::open(executor, spec, producer, plain_endpoint_main())
        .expect("a virtual endpoint")
}

fn peer(caps: PortCaps) -> (Client, PortId) {
    peer_in(caps, UmpVersion::Midi2)
}

/// A peer client delivered `version`.
fn peer_in(caps: PortCaps, version: UmpVersion) -> (Client, PortId) {
    let client = Client::open("tutti-virt-peer", version).expect("a sequencer");
    let port = client
        .open_port("peer", caps, PortKind::MIDI_GENERIC | PortKind::APPLICATION)
        .expect("a port");
    (client, port)
}

fn notes(count: u8) -> Vec<Ump> {
    (0..count)
        .map(|n| note_on(MidiChannel::of(2), 60 + n, 0x8000 + u16::from(n)))
        .collect()
}

/// Reads `count` UMP messages the peer receives, in arrival order.
fn read(peer: &Client, count: usize) -> Vec<Ump> {
    let deadline = Instant::now() + PATIENCE;
    let mut got = Vec::new();
    while got.len() < count {
        assert!(Instant::now() < deadline, "only {got:?} arrived");
        match peer.read_event().expect("a read") {
            Read::Event(Event::Ump { ump, .. }) => got.push(ump),
            Read::Event(_) => {}
            Read::Empty => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    got
}

/// What a subscriber of a virtual endpoint receives is what was sent into it, each
/// message whole and in the order sent.
#[test]
fn a_subscriber_receives_what_a_virtual_endpoint_sends_in_order() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("send");
    let endpoint = open_sending(&executor, &spec);
    let (peer, port) = peer(PortCaps::WRITE);
    let found = find(&peer, &spec).expect("the endpoint is listed");
    peer.connect_from(port, found.address).expect("subscribes");

    let sent = notes(5);
    for ump in &sent {
        endpoint.send(*ump).expect("sends");
    }
    let got = read(&peer, sent.len());
    for (ump, got) in sent.iter().zip(&got) {
        assert_eq!(*got, *ump);
    }
}

/// What a client writes to a virtual endpoint reaches the graph, each message
/// whole and in the order sent.
#[test]
fn a_virtual_endpoint_delivers_what_a_client_sends_to_the_graph_in_order() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("receive");
    let inlets = InputInlets::new(EventCapacity::of(64), Duration::ZERO);
    let mut listener = Listener::over(&inlets);
    let ports = HardwareMidiInputs::new(std::sync::Arc::clone(&inlets));
    let (producer, inlet) = ports.open_input_port("virtual");
    listener.attach(inlet);
    let endpoint = super::opening::open(&executor, &spec, producer, plain_endpoint_main())
        .expect("a virtual endpoint");
    let (peer, port) = peer(PortCaps::READ);
    let found = find(&peer, &spec).expect("the endpoint is listed");
    peer.connect_to(port, found.address).expect("subscribes");

    let sent = notes(5);
    for ump in &sent {
        peer.send_to_subscribers(port, ump).expect("sends");
    }
    let heard = listener.hear(sent.len(), PATIENCE);
    assert_eq!(heard, sent);
    assert!(endpoint.is_alive());
}

/// JR Clock and JR Timestamp messages (M2-104 §7.2.2) a client sends through the
/// kernel's UMP sequencer are not heard by a port timed by arrival, which hears
/// the note beside them.
#[test]
fn jr_messages_a_client_sends_are_not_heard_by_an_arrival_port() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("jr");
    let inlets = InputInlets::new(EventCapacity::of(64), Duration::ZERO);
    let mut listener = Listener::over(&inlets);
    let ports = HardwareMidiInputs::new(std::sync::Arc::clone(&inlets));
    let (producer, inlet) = ports.open_input_port("virtual");
    listener.attach(inlet);
    let _endpoint = super::opening::open(&executor, &spec, producer, plain_endpoint_main())
        .expect("a virtual endpoint");
    let (peer, port) = peer(PortCaps::READ);
    let found = find(&peer, &spec).expect("the endpoint is listed");
    peer.connect_to(port, found.address).expect("subscribes");

    let clock = Ump::from(Message::Utility(Utility::JrClock(0x1234)));
    let stamp = Ump::from(Message::Utility(Utility::JrTimestamp(0x1300)));
    let note = note_on(MidiChannel::of(2), 60, 0x8000);
    for ump in [clock, stamp, note] {
        peer.send_to_subscribers(port, &ump).expect("sends");
    }
    assert_eq!(listener.hear(1, PATIENCE), [note]);
}

/// A block of Groups 4 to 5, named, bidirectional, a receiver's hint.
fn block() -> FunctionBlock {
    FunctionBlock::builder(
        FunctionBlockId::new(1).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(4).unwrap(),
        GroupSpan::new(2).unwrap(),
    )
    .ui_hint(UiHint::Receiver)
    .midi_ci(2, 3)
    .name(BlockName::new("Pads").unwrap())
    .build()
    .unwrap()
}

/// The endpoint record another client reads for the endpoint carries the
/// protocol it was created with and the Function Blocks of its spec (M2-104
/// §7.1.2 and §7.1.8).
#[test]
fn the_published_endpoint_record_carries_the_protocol_and_the_blocks() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("record").block(block());
    let endpoint = open_sending(&executor, &spec);
    assert_eq!(endpoint.protocol(), VirtualProtocol::Midi2);
    // The sequencer publishes the spec's own numbers, which `blocks` reports.
    assert_eq!(endpoint.blocks(), [block().id()]);

    let (peer, _) = peer(PortCaps::READ);
    let record = find(&peer, &spec)
        .expect("listed")
        .ump
        .expect("a UMP Endpoint record");
    assert_eq!(record.protocol, Protocol::Midi2);
    assert!(record.offered.midi2 && !record.offered.midi1);
    assert_eq!(record.offered.version, tutti_midi_device::UMP_VERSION);
    assert!(record.offered.static_function_blocks);
    let [got] = record.blocks.as_slice() else {
        panic!("one block, got {:?}", record.blocks);
    };
    assert_eq!(got.info, block().info());
    assert_eq!(got.name.as_deref(), Some("Pads"));
}

/// The endpoint record another client reads carries the Device Identity
/// (M2-104 §7.1.3) and the Product Instance Id (§7.1.5) of the spec, field by
/// field; an Id of the 42-byte maximum arrives whole, and an endpoint with no Id
/// has none.
#[test]
fn the_published_endpoint_record_carries_the_device_identity_and_instance_id() {
    let executor = tutti_platform::Executor::new();
    let id = ProductInstanceId::new(&"0123456789".repeat(5)[..42]).unwrap();
    let identified = |tag, instance| {
        VirtualSpec::new(
            EndpointName::new(&format!("tutti-virt-{}-{tag}", std::process::id())).unwrap(),
            device_identity(),
            instance,
        )
    };
    let named_spec = identified("ident-named", Some(id));
    let bare_spec = identified("ident-bare", None);
    let _named = open_sending(&executor, &named_spec);
    let _bare = open_sending(&executor, &bare_spec);

    let (peer, _) = peer(PortCaps::READ);
    let record = |spec| {
        find(&peer, spec)
            .expect("listed")
            .ump
            .expect("a UMP Endpoint record")
    };
    let named = record(&named_spec);
    assert_eq!(named.device, Some(device_identity()));
    assert_eq!(named.instance, Some(id));
    assert_eq!(named.instance.map(|id| id.as_str().len()), Some(42));
    let bare = record(&bare_spec);
    assert_eq!(bare.device, Some(device_identity()));
    assert_eq!(bare.instance, None);
}

/// An endpoint created as MIDI 1.0 reports MIDI 1.0 and publishes it, and the
/// MIDI 1.0 channel voice message (type 2) a client sends it reaches the graph's
/// input node as it does from a MIDI 1.0 device, which translates it: velocity
/// `0x40` is `0x8000` (M2-115).
/// Spec: m2-115/2.3#1
#[test]
fn a_midi1_endpoint_reports_midi1_and_its_messages_arrive_widened() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("midi1").protocol(VirtualProtocol::Midi1);
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let mut listener = Listener::over_input(&inlets);
    let ports = HardwareMidiInputs::new(std::sync::Arc::clone(&inlets));
    let (producer, inlet) = ports.open_input_port("virtual");
    listener.attach(inlet);
    let endpoint = super::opening::open(&executor, &spec, producer, plain_endpoint_main())
        .expect("an endpoint");
    assert_eq!(endpoint.protocol(), VirtualProtocol::Midi1);

    let (peer, port) = peer(PortCaps::READ);
    let found = find(&peer, &spec).expect("listed");
    assert_eq!(
        found.ump.as_ref().map(|e| e.protocol),
        Some(Protocol::Midi1)
    );
    peer.connect_to(port, found.address).expect("subscribes");
    let note = m1_note_on(MidiChannel::of(2), 60, U7::of(0x40));
    peer.send_to_subscribers(port, &note).expect("sends");
    let heard = listener.hear(1, PATIENCE);
    assert_eq!(heard, [note_on(MidiChannel::of(2), 60, 0x8000)]);
}

/// A MIDI 2.0 note sent into a MIDI 1.0 endpoint reaches a subscriber as the
/// MIDI 1.0 note (type 2): the 16-bit velocity `0x8100` is the 7-bit `0x40`
/// (M2-115).
/// Spec: m2-115/2.3#1
#[test]
fn a_midi2_note_sent_into_a_midi1_endpoint_arrives_as_a_midi1_note() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("translate").protocol(VirtualProtocol::Midi1);
    let endpoint = open_sending(&executor, &spec);
    assert_eq!(endpoint.protocol(), VirtualProtocol::Midi1);
    let (peer, port) = peer_in(PortCaps::WRITE, UmpVersion::Midi1);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.address).expect("subscribes");

    endpoint
        .send(note_on(MidiChannel::of(2), 60, 0x8100))
        .expect("sends");
    let got = read(&peer, 1);
    let want = m1_note_on(MidiChannel::of(2), 60, U7::of(0x40));
    assert_eq!(got[0], want);
}

/// A MIDI 2.0 message with no MIDI 1.0 form (M2-104 Appendix D.2: a per-note
/// pitch bend) is refused by name by a MIDI 1.0 endpoint, and nothing reaches the
/// subscriber.
#[test]
fn a_message_with_no_midi1_form_is_refused_by_a_midi1_endpoint() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("noform").protocol(VirtualProtocol::Midi1);
    let endpoint = open_sending(&executor, &spec);
    let (peer, port) = peer_in(PortCaps::WRITE, UmpVersion::Midi1);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.address).expect("subscribes");

    let bend = per_note_pitch_bend(MidiChannel::of(2), 60, 0x8000_0000);
    assert!(matches!(
        endpoint.send(bend),
        Err(tutti_midi_hardware::MidiHardwareError::NoMidi1Form(
            tutti_ump::NoMidi1Form::PerNote
        ))
    ));
    // The refusal sent nothing: the next message through is the one that follows
    // it, with nothing before it.
    let after = note_on(MidiChannel::of(2), 60, 0x8000);
    endpoint.send(after).expect("sends");
    let got = read(&peer, 1);
    // The note reaches a MIDI 1.0 subscriber as a type 2 Note On of key 60.
    let word = got[0].0[0];
    assert_eq!(
        (word >> 28, (word >> 20) & 0xF, (word >> 8) & 0x7F),
        (2, 9, 60)
    );
}

/// A MIDI 2.0 Program Change with a bank sent into a MIDI 1.0 endpoint reaches a
/// subscriber as Bank Select MSB and LSB (CC 0, CC 32) and then the Program
/// Change (M2-104 Appendix D.2.4): the bank is not lost.
#[test]
fn a_program_change_with_a_bank_reaches_a_midi1_subscriber_as_bank_select_and_program() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("bank").protocol(VirtualProtocol::Midi1);
    let endpoint = open_sending(&executor, &spec);
    let (peer, port) = peer_in(PortCaps::WRITE, UmpVersion::Midi1);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.address).expect("subscribes");

    let channel = MidiChannel::of(2);
    let bank = Bank {
        msb: U7::of(3),
        lsb: U7::of(9),
    };
    endpoint
        .send(program_change(channel, 17, Some(bank)))
        .expect("sends");
    let got = read(&peer, 3);
    let want = [
        m1_cc(channel, CCNumber::BANK_SELECT, bank.msb),
        m1_cc(channel, CCNumber::BANK_SELECT_LSB, bank.lsb),
        Message::midi1(channel, Voice1::ProgramChange(U7::of(17))).into(),
    ];
    for (got, want) in got.iter().zip(&want) {
        assert_eq!(got, want);
    }
}

/// A Registered Controller sent into a MIDI 1.0 endpoint reaches a subscriber as
/// the four Control Changes of M2-104 Appendix D.2.3 (CC 101, 100, 6, 38), and
/// a second value for the same parameter is the four again.
#[test]
fn a_registered_controller_reaches_a_midi1_subscriber_as_the_rpn_sequence() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("rpn").protocol(VirtualProtocol::Midi1);
    let endpoint = open_sending(&executor, &spec);
    let (peer, port) = peer_in(PortCaps::WRITE, UmpVersion::Midi1);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.address).expect("subscribes");

    let channel = MidiChannel::of(2);
    let cc = |n: u8, value: u8| m1_cc(channel, CCNumber::of(n), U7::of(value));
    endpoint
        .send(registered(channel, Rpn::PITCH_BEND_SENSITIVITY, 12 << 25))
        .expect("sends");
    endpoint
        .send(registered(channel, Rpn::PITCH_BEND_SENSITIVITY, 7 << 25))
        .expect("sends");
    let got = read(&peer, 8);
    let want = [
        cc(101, 0),
        cc(100, 0),
        cc(6, 12),
        cc(38, 0),
        cc(101, 0),
        cc(100, 0),
        cc(6, 7),
        cc(38, 0),
    ];
    for (got, want) in got.iter().zip(&want) {
        assert_eq!(got, want);
    }
}

/// A MIDI 1.0 note sent into a MIDI 2.0 endpoint reaches a subscriber translated:
/// velocity `0x40` is `0x8000` (M2-115).
/// Spec: m2-115/2.3#1
#[test]
fn a_midi1_note_sent_into_a_midi2_endpoint_arrives_widened() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("widen");
    let endpoint = open_sending(&executor, &spec);
    let (peer, port) = peer(PortCaps::WRITE);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.address).expect("subscribes");

    endpoint
        .send(m1_note_on(MidiChannel::of(2), 60, U7::of(0x40)))
        .expect("sends");
    let got = read(&peer, 1);
    let want = note_on(MidiChannel::of(2), 60, 0x8000);
    assert_eq!(got[0], want);
}

/// A sender outlives its endpoint and refuses by name once the endpoint is
/// dropped, and one sent while it lived was accepted.
#[test]
fn a_sender_outlives_its_endpoint_and_refuses_by_name() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("sender-gone");
    let endpoint = open_sending(&executor, &spec);
    let sender = endpoint.sender();
    assert!(sender.send(note_on(MidiChannel::of(1), 60, 0x8000)).is_ok());
    drop(endpoint);
    assert!(matches!(
        sender.send(note_on(MidiChannel::of(1), 60, 0x8000)),
        Err(tutti_midi_hardware::MidiHardwareError::EndpointGone)
    ));
}

/// **A virtual endpoint is one sequencer client with one port**: another client
/// lists exactly one port under the spec's name, from one client, and that port
/// can be both subscribed to for reading and written to.
#[test]
fn a_virtual_endpoint_is_listed_as_one_client_with_one_port() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("one");
    let _endpoint = open_sending(&executor, &spec);
    let (peer, _) = peer(PortCaps::READ);
    let listed: Vec<PortEntry> = peer
        .list_ports()
        .ports
        .into_iter()
        .filter(|p| p.client_name.contains(spec.name().as_str()))
        .collect();
    let [one] = listed.as_slice() else {
        panic!("one port under the endpoint's name, got {listed:?}");
    };
    assert!(one.caps.can_read_from() && one.caps.can_write_to());
}

/// **Both directions run on the one endpoint**: a peer subscribed to it (reading)
/// hears what the app sends, and the same peer writing to it is heard through the
/// producer, each message whole and in order.
#[test]
fn a_virtual_endpoint_sends_to_a_subscriber_and_delivers_what_the_same_peer_writes() {
    let executor = tutti_platform::Executor::new();
    let spec = spec("both");
    let inlets = InputInlets::new(EventCapacity::of(64), Duration::ZERO);
    let mut listener = Listener::over(&inlets);
    let ports = HardwareMidiInputs::new(std::sync::Arc::clone(&inlets));
    let (producer, inlet) = ports.open_input_port("virtual");
    listener.attach(inlet);
    let endpoint = super::opening::open(&executor, &spec, producer, plain_endpoint_main())
        .expect("a virtual endpoint");
    let (peer, port) = peer(PortCaps::READ | PortCaps::WRITE);
    let found = find(&peer, &spec).expect("the endpoint is listed");
    peer.connect_to(port, found.address).expect("writes to it");
    peer.connect_from(port, found.address)
        .expect("reads from it");

    let received = notes(3);
    for ump in &received {
        peer.send_to_subscribers(port, ump).expect("sends");
    }
    assert_eq!(listener.hear(received.len(), PATIENCE), received);

    let sent: Vec<Ump> = (0..3)
        .map(|n| note_on(MidiChannel::of(5), 70 + n, 0x9000 + u16::from(n)))
        .collect();
    for ump in &sent {
        endpoint.send(*ump).expect("sends");
    }
    assert_eq!(read(&peer, sent.len()), sent);
}

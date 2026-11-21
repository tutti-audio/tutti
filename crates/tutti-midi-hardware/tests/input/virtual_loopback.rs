//! A virtual endpoint seen from a session of this process, on every platform
//! with a backend: the session lists the endpoint as it lists a device, and what
//! crosses it arrives whole and in order.

use std::sync::Arc;
use std::time::Duration;

use super::listener::Listener;

use tutti_fixtures::main_thread;
use tutti_fixtures::midi::{device_identity, note_on};
use tutti_graph::{EventCapacity, Offset};
use tutti_midi_device::DeviceInfo;
use tutti_midi_hardware::{MidiHardwareConfig, MidiSession, VirtualProtocol, VirtualSpec};
use tutti_midi_runtime::InputInlets;
use tutti_platform::{Executor, MainThreadMarker};
use tutti_ump::EndpointName;
use tutti_ump::{MidiChannel, Ump};

const PATIENCE: Duration = Duration::from_secs(30);

/// The main-thread proof for an endpoint that declares no Function Blocks: such an
/// endpoint is no main-thread object on any backend, so the fixture's stand-in is
/// enough. Never use it for one with blocks (`tests/virtual_blocks`).
fn plain_endpoint_main() -> MainThreadMarker {
    main_thread()
}

fn spec(tag: &str) -> VirtualSpec {
    let name = format!("tutti-loop-{}-{tag}", std::process::id());
    VirtualSpec::new(EndpointName::new(&name).unwrap(), device_identity(), None)
}

/// A session over four inlet slots, and the graph that plays them.
fn session(executor: &Executor) -> (MidiSession, Listener) {
    let inlets = InputInlets::new(EventCapacity::of(64), Duration::ZERO);
    let listener = Listener::over(&inlets);
    let session =
        MidiSession::open(executor, inlets, MidiHardwareConfig::new()).expect("a session");
    (session, listener)
}

/// The device `spec` published, as the session's input (`input`) or output,
/// once the session lists it.
fn listed(io: &MidiSession, spec: &VirtualSpec, input: bool) -> DeviceInfo {
    super::opening::listed(io, spec.name().as_str(), |d| {
        let way = if input {
            d.direction.is_input()
        } else {
            d.direction.is_output()
        };
        way && d.name.contains(spec.name().as_str())
    })
}

fn notes() -> Vec<Ump> {
    (0..5)
        .map(|n| note_on(MidiChannel::of(1), 60 + n, 0x8000 + u16::from(n)))
        .collect()
}

/// What a virtual endpoint is sent arrives at a session input connected to it, in
/// order.
#[test]
fn what_a_virtual_endpoint_sends_reaches_a_session_input_in_order() {
    let executor = Executor::new();
    let spec = spec("send");
    let (mut io, mut graph) = session(&executor);
    let (producer, _inlet) = Arc::clone(io.ports()).open_input_port("virtual");
    let endpoint = super::opening::open(&executor, &spec, producer, plain_endpoint_main())
        .expect("a virtual endpoint");
    let device = listed(&io, &spec, true);
    let tutti_midi_hardware::Connected::Opened(inlet) = io
        .connect_input(super::opening::started(&io), &device.id)
        .expect("connects")
    else {
        panic!("a new input opens");
    };
    graph.attach(inlet);
    // A probe on a key no test asserts on, so a late one is never a test note.
    let probe = note_on(MidiChannel::of(1), 59, 0x8000);
    graph.ready(|| endpoint.send(probe).expect("sends"), PATIENCE);

    let sent = notes();
    for ump in &sent {
        endpoint.send(*ump).expect("sends");
    }
    assert_eq!(graph.hear(sent.len(), PATIENCE), sent);
}

/// What a session output sends to a virtual endpoint arrives at the
/// endpoint's producer, in order, as the events of a device input.
#[test]
fn what_a_session_output_sends_reaches_a_virtual_endpoint_in_order() {
    let executor = Executor::new();
    let spec = spec("receive");
    let (io, mut graph) = session(&executor);
    let (producer, inlet) = Arc::clone(io.ports()).open_input_port("virtual");
    graph.attach(inlet);
    let endpoint = super::opening::open(&executor, &spec, producer, plain_endpoint_main())
        .expect("a virtual endpoint");
    let device = listed(&io, &spec, false);
    let mut out = io
        .connect_output(super::opening::started(&io), &device.id)
        .expect("connects");
    graph.ready(
        || {
            let _ = out.queue(&[(Offset::ZERO, note_on(MidiChannel::of(1), 59, 0x8000))]);
        },
        PATIENCE,
    );

    let sent = notes();
    for ump in &sent {
        assert_eq!(out.queue(&[(Offset::ZERO, *ump)]).packets(), 1);
    }
    assert_eq!(graph.hear(sent.len(), PATIENCE), sent);
    assert!(endpoint.is_alive());
}

/// A MIDI 1.0 endpoint receives MIDI 1.0 (the session's output translates for
/// it) and the graph's input node translates it as it does a MIDI 1.0 device's:
/// the 16-bit velocity `0x8100` is the 7-bit `0x40` on the wire and `0x8000`
/// after translation (M2-115).
/// Spec: m2-115/2.3#1
#[test]
fn a_midi1_endpoint_delivers_what_the_input_node_widens() {
    let spec = spec("midi1").protocol(VirtualProtocol::Midi1);
    let inlets = InputInlets::new(EventCapacity::of(64), Duration::ZERO);
    let mut graph = Listener::over_input(&inlets);
    let executor = Executor::new();
    let io = MidiSession::open(&executor, inlets, MidiHardwareConfig::new()).expect("a session");
    let (producer, inlet) = Arc::clone(io.ports()).open_input_port("virtual");
    graph.attach(inlet);
    let _endpoint = super::opening::open(&executor, &spec, producer, plain_endpoint_main())
        .expect("a virtual endpoint");
    let device = listed(&io, &spec, false);
    let mut out = io
        .connect_output(super::opening::started(&io), &device.id)
        .expect("connects");
    graph.ready(
        || {
            let _ = out.queue(&[(Offset::ZERO, note_on(MidiChannel::of(1), 59, 0x8100))]);
        },
        PATIENCE,
    );

    assert_eq!(
        out.queue(&[(Offset::ZERO, note_on(MidiChannel::of(1), 60, 0x8100))])
            .packets(),
        1
    );
    assert_eq!(
        graph.hear(1, PATIENCE),
        [note_on(MidiChannel::of(1), 60, 0x8000)]
    );
}

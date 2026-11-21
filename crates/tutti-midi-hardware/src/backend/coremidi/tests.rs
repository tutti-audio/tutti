use super::*;
use crate::virtual_endpoint::VirtualProtocol;
use tutti_fixtures::midi;
use tutti_ump::EndpointName;

/// Enumeration must not panic on a machine with no MIDI devices, and must
/// return well-formed records where there are some. CI boxes have none; a
/// dev machine has IAC.
///
/// This deliberately does *not* assert a device count — that would pass
/// vacuously on CI and fail spuriously on a laptop. It asserts the shape of
/// whatever comes back.
#[test]
fn enumeration_yields_well_formed_records() {
    let backend = CoreMidiEndpoints::new();
    for info in backend.inputs().into_iter().chain(backend.outputs()) {
        assert!(
            !info.name.is_empty(),
            "every endpoint must carry a name, even a synthesised one"
        );
    }
}

/// An id that no endpoint owns must fail to open rather than resolve to
/// whatever sits at that index — the reason `EndpointId` is opaque and not a
/// positional index.
#[test]
fn an_unknown_id_does_not_open() {
    let backend = CoreMidiEndpoints::new();
    let unknown = EndpointId::from_raw(u64::from(u32::MAX));
    assert!(
        !backend.outputs().iter().any(|e| e.id == unknown),
        "precondition: the sentinel id must not belong to a real endpoint"
    );
    assert!(backend.open_output(unknown).is_err());
}

/// Capability must be *read from the endpoint*, not assumed.
///
/// The trap this guards: inputs open with `Protocol::Midi2` so CoreMIDI
/// up-converts MIDI-1.0 traffic. If `endpoint_capability` returned that
/// choice rather than reading `kMIDIPropertyProtocolID`, every endpoint
/// would claim MIDI 2.0 and the capability would be vestigial.
///
/// Enumerating real devices **cannot** catch that here: every endpoint on a
/// modern Mac (the IAC buses included) genuinely reports MIDI 2.0, so
/// `assert!(matches!(.., Midi1 | Midi2))` passes under a hardcoded `midi2()`
/// too. The fixture has to *supply* the distinction,
/// so this creates a MIDI-1.0 virtual endpoint and requires the reader to say
/// so.
#[test]
fn capability_is_read_from_the_endpoint_not_assumed() {
    // A MIDI-1.0-protocol virtual endpoint, found as the OS lists it.
    let name = format!("tutti-cap-midi1-{}", std::process::id());
    let _endpoint = plain(&name, VirtualProtocol::Midi1, |_| {});
    let endpoint = tutti_coremidi_ffi::sources()
        .into_iter()
        .find(|source| source.name().as_deref() == Some(name.as_str()))
        .expect("the source is listed");
    let cap = endpoint_capability(&endpoint, None);
    assert_eq!(
        cap.active().protocol,
        Protocol::Midi1,
        "a MIDI-1.0 endpoint must report Midi1 — returning a constant Midi2 \
         (or echoing the protocol we open ports with) fails here"
    );
    assert!(!carries_midi2_only(&cap));
}

/// The `Midi2` direction is covered by the real-device enumeration above
/// (every endpoint on this machine reports MIDI 2.0), so a hardcoded
/// `midi1()` would fail *there*. Pinning it here too would need a
/// MIDI-2.0-protocol endpoint to hand `endpoint_capability` as a
/// `Endpoint` — `UmpVirtualEndpoint` keeps its `MIDIEndpointRef`
/// private, and widening that API purely for a test is the wrong trade.
/// Between the two, neither constant survives.
#[test]
fn a_midi2_endpoint_is_covered_by_enumeration() {
    let backend = CoreMidiEndpoints::new();
    let any_midi2 = backend
        .inputs()
        .into_iter()
        .chain(backend.outputs())
        .any(|e| e.capability.active().protocol == Protocol::Midi2);

    // Not an assertion: a machine with no MIDI-2.0 endpoint is legitimate,
    // and asserting here would fail spuriously on CI. Reported so a reader
    // knows whether this ran.
    if !any_midi2 {
        eprintln!(
            "note: no MIDI-2.0 endpoint on this machine; the Midi2 direction \
             of capability reading is uncovered here"
        );
    }
}

/// **A packet sent `At` a future instant is held by CoreMIDI until then, and
/// one sent `Now` goes at once**: the stamp each gets is pinned in `host_time`'s
/// tests; here CoreMIDI schedules them. A virtual UMP endpoint receives the
/// note sent 2 s ahead after the one sent now, though it was queued first. No
/// time is asserted: the order is what a hold changes, and the waits only guard
/// a hang.
#[test]
fn a_scheduled_packet_arrives_after_the_one_sent_now() {
    use std::sync::mpsc;
    use std::time::Duration;
    use tutti_midi_runtime::{Outgoing, SendAt};
    use tutti_platform::Instant;
    use tutti_ump::MidiChannel;

    let (tx, rx) = mpsc::channel();
    let _destination = plain("tutti-sched-out", VirtualProtocol::Midi2, move |ump| {
        let _ = tx.send(ump);
    });
    let backend = CoreMidiEndpoints::new();
    let id = backend
        .outputs()
        .into_iter()
        .find(|e| e.name.contains("tutti-sched-out"))
        .expect("the virtual endpoint is listed")
        .id;
    let out = backend.open_output(id).expect("opens");
    out.follow(Protocol::Midi2);

    let note = |n| midi::note_on(MidiChannel::FIRST, n, 0xFFFF);
    let due = Instant::now() + Duration::from_secs(2);
    let sent = out.queue_timed(&[
        Outgoing::new(tutti_graph::Offset::ZERO, SendAt::At(due), note(60)),
        Outgoing::new(tutti_graph::Offset::ZERO, SendAt::Now, note(61)),
    ]);
    assert_eq!(sent.packets(), 2);

    let wait = Duration::from_secs(30);
    let first = rx.recv_timeout(wait).expect("the note sent now");
    let second = rx.recv_timeout(wait).expect("the scheduled note");
    assert_eq!(
        (first, second),
        (note(61), note(60)),
        "now first, then the scheduled one"
    );
}

/// A virtual endpoint of `name` with no Function Blocks, carrying `protocol`,
/// passing each received message to `on_event`. An endpoint with no blocks is no
/// main-thread object, so the fixture's stand-in marker is enough; never use it
/// for blocks.
pub(in crate::backend::coremidi) fn plain(
    name: &str,
    protocol: VirtualProtocol,
    mut on_event: impl FnMut(tutti_ump::Ump) + Send + 'static,
) -> UmpVirtualEndpoint {
    let spec = VirtualSpec::new(
        EndpointName::new(name).unwrap(),
        midi::device_identity(),
        None,
    )
    .protocol(protocol);
    let main = tutti_fixtures::main_thread();
    // The client is the hub's: it exists once the start resolved.
    let devices = crate::Devices::open_over(
        &tutti_platform::Executor::new(),
        Box::new(CoreMidiEndpoints::new()),
    )
    .expect("devices");
    tutti_fixtures::block_on(devices.start()).expect("the hub makes its client");
    let mut opening = UmpVirtualEndpoint::create(&spec, main, move |ump, _| {
        on_event(ump);
    })
    .expect("an opening");
    // No Function Blocks: nothing is retried, so the first poll is the endpoint.
    match opening.poll_endpoint(main, tutti_platform::Instant::now()) {
        std::task::Poll::Ready(made) => made.expect("a virtual UMP endpoint"),
        std::task::Poll::Pending => panic!("an endpoint with no blocks is ready on the first poll"),
    }
}

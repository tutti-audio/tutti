use super::super::CoreMidiEndpoints;
use super::super::tests::plain;
use super::*;
use crate::MidiHardwareError;
use crate::endpoints::MidiEndpoints;
use std::sync::mpsc;
use std::time::Duration;
use tutti_fixtures::{midi, unique_name};
use tutti_graph::Offset;
use tutti_ump::{DiscoveryFilter, Message, MidiChannel, Stream, Ump, UmpVersion};

/// **A packet sent to the endpoint reaches its callback, every message of it, in
/// order** (M2-104 §7.1: a UMP Stream message has no MIDI 1.0 form, so this
/// endpoint is the only way one arrives). An output port of this process sends an
/// Endpoint Discovery and a MIDI 2.0 note to the endpoint, as another app would;
/// CoreMIDI delivers them on its own thread with a null connection reference, which
/// the receive block must not read.
#[test]
fn a_packet_sent_to_the_endpoint_reaches_its_callback() {
    let (tx, rx) = mpsc::channel();
    let name = unique_name("tutti-vend-delivery");
    let _endpoint = plain(&name, VirtualProtocol::Midi2, move |ump| {
        let _ = tx.send(ump);
    });
    let backend = CoreMidiEndpoints::new();
    let started = tutti_fixtures::block_on(backend.start()).expect("the hub makes its client");
    let id = backend
        .outputs()
        .into_iter()
        .find(|e| e.name.contains(&name))
        .expect("the virtual endpoint's destination is listed")
        .id;
    let out = backend.open_output(started, id).expect("opens");
    out.follow(Protocol::Midi2);

    let discovery = Ump::from(Message::Stream(Stream::EndpointDiscovery {
        version: UmpVersion { major: 1, minor: 1 },
        filter: DiscoveryFilter {
            endpoint_info: true,
            device_identity: true,
            endpoint_name: true,
            product_instance_id: true,
            stream_configuration: true,
        },
    }));
    let note = midi::note_on(MidiChannel::of(3), 60, 0x8000);
    assert_eq!(
        out.queue(&[(Offset::ZERO, discovery), (Offset::ZERO, note)])
            .packets(),
        2
    );

    let wait = Duration::from_secs(2);
    let got = [
        rx.recv_timeout(wait).expect("the discovery arrives"),
        rx.recv_timeout(wait).expect("the note arrives"),
    ];
    assert_eq!(got, [discovery, note]);
}

/// **A sender of an endpoint with no Function Blocks (a source and a destination
/// under one name) refuses by name once the endpoint is dropped**, and one sent
/// while it lived was accepted.
#[test]
fn a_pair_sender_refuses_by_name_once_the_endpoint_is_dropped() {
    let endpoint = plain(
        &unique_name("tutti-vend-gone"),
        VirtualProtocol::Midi2,
        |_| {},
    );
    let sender = endpoint.sender();
    let note = midi::note_on(MidiChannel::of(1), 60, 0x8000);
    assert!(sender.send(&note).is_ok());
    drop(endpoint);
    assert!(matches!(
        sender.send(&note),
        Err(MidiHardwareError::EndpointGone)
    ));
}

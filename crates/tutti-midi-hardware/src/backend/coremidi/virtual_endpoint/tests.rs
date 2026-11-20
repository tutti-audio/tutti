use super::super::CoreMidiEndpoints;
use super::*;
use crate::endpoints::MidiEndpoints;
use std::sync::mpsc;
use std::time::Duration;
use tutti_fixtures::{midi, unique_name};
use tutti_graph::Offset;
use tutti_ump::{DiscoveryFilter, Message, MidiChannel, Stream, Ump, UmpVersion, Utility};

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
    let _endpoint = UmpVirtualEndpoint::new(&name, VirtualProtocol::Midi2, move |ump| {
        let _ = tx.send(ump);
    })
    .expect("a virtual UMP endpoint");
    let backend = CoreMidiEndpoints::new();
    let id = backend
        .outputs()
        .into_iter()
        .find(|e| e.name.contains(&name))
        .expect("the virtual endpoint's destination is listed")
        .id;
    let out = backend.open_output(id).expect("opens");
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

/// A JR Timestamp is a single UMP Utility word, which has no MIDI 1.0 form: the
/// endpoint's sender carries it to the wire without error.
#[test]
fn a_jr_timestamp_word_sends() {
    let endpoint = UmpVirtualEndpoint::new(
        &unique_name("tutti-vend-jr"),
        VirtualProtocol::Midi2,
        |_| {},
    )
    .expect("creates");
    let jr = Ump::from(Message::Utility(Utility::JrTimestamp(0x1234)));
    assert!(endpoint.sender().send(&jr).is_ok());
}

/// A MIDI 2.0 channel-voice note on is two words, and the endpoint's sender carries
/// both.
#[test]
fn a_two_word_note_on_sends() {
    let endpoint = UmpVirtualEndpoint::new(
        &unique_name("tutti-vend-note"),
        VirtualProtocol::Midi2,
        |_| {},
    )
    .expect("creates");
    let note = midi::note_on(MidiChannel::of(1), 60, 0x8000);
    assert!(endpoint.sender().send(&note).is_ok());
}

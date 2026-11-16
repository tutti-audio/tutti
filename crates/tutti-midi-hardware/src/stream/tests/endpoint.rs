//! The session as a UMP Endpoint (M2-104 §7.1) and as a MIDI-CI Device under
//! the identity it presents, against a fake peer: the wire is the fake's sink
//! and the input's producer.

use super::ci::{GROUP_0, ci_sent, midi1_running, polled};
use super::*;
use crate::MidiIdentity;
use tutti_midi_device::{CiMessage, ProductInstanceId};
use tutti_ump::{BlockFilter, BlockSelect, DiscoveryFilter, UmpVersion};

const ASKING: UmpVersion = UmpVersion { major: 1, minor: 1 };

/// A UMP Endpoint wire, settled, that presents `who` to its device.
fn presenting(who: Option<crate::Presented>) -> Wire {
    let mut wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Duplex,
        offering_both(),
    );
    wire.session.present(who);
    wire.open();
    settle(&mut wire);
    // What the negotiation said is not under test.
    let _ = received(&wire);
    wire
}

fn presented_over(groups: u8) -> crate::Presented {
    crate::Presented {
        groups: GroupSpan::new(groups).unwrap(),
        ..presented(identity())
    }
}

/// The Stream messages among what the peer received.
fn stream_sent(wire: &Wire) -> Vec<Stream> {
    received(wire)
        .into_iter()
        .filter(|ump| ump.message_type() == 0xF)
        .filter_map(|ump| match ump.decode() {
            Ok(Message::Stream(stream)) => Some(stream),
            _ => None,
        })
        .collect()
}

fn ask_everything() -> Stream {
    Stream::EndpointDiscovery {
        version: ASKING,
        filter: DiscoveryFilter {
            endpoint_info: true,
            device_identity: true,
            endpoint_name: true,
            product_instance_id: true,
            stream_configuration: true,
        },
    }
}

fn request(protocol: Protocol, rx_jr: bool, tx_jr: bool) -> Stream {
    Stream::StreamConfigurationRequest(StreamConfig {
        protocol,
        rx_jr,
        tx_jr,
    })
}

/// Ticks the wire at `ms` after the peer said `stream`, and returns what the
/// session answered.
fn answered(wire: &mut Wire, stream: Stream, ms: u64) -> Vec<Stream> {
    send_ump(wire, stream, ms);
    let _ = wire.poll(ms + 1);
    stream_sent(wire)
}

/// M2-104 §7.1.2: an Endpoint Discovery asking for the endpoint info is
/// answered with an Endpoint Info Notification: UMP 1.1, both Protocols, JR
/// Timestamps neither way, the Function Blocks static and one. The Device
/// Identity, the Endpoint Name and the Stream Configuration the filter asks
/// for follow, in the order of §7.1.1.
/// Spec: m2-104/7.1.2#1
#[test]
fn a_peers_endpoint_discovery_is_answered_with_the_presented_endpoint() {
    let mut wire = presenting(Some(presented_over(4)));
    let sent = answered(&mut wire, ask_everything(), 100);
    let [
        Stream::EndpointInfo(info),
        Stream::DeviceIdentity(who),
        Stream::EndpointName(_, name),
        Stream::StreamConfigurationNotification(config),
    ] = sent.as_slice()
    else {
        panic!("Info, Identity, Name and Configuration, in that order: {sent:?}");
    };
    assert_eq!(info.version, ASKING);
    assert!(info.static_function_blocks);
    assert_eq!(info.function_blocks.count().map(|c| c.get()), Some(1));
    assert!(info.midi1 && info.midi2);
    assert!(!info.rx_jr && !info.tx_jr);
    assert_eq!(*who, identity());
    assert_eq!(&name[..7], b"Session");
    assert_eq!(config.protocol, Protocol::Midi1);
    assert!(!config.rx_jr && !config.tx_jr);
}

/// A Product Instance Id the identity carries is answered when asked for.
#[test]
fn the_product_instance_id_is_answered_when_the_identity_has_one() {
    let id = ProductInstanceId::new("SN-0042").unwrap();
    let mut who = presented(identity());
    who.identity.instance = Some(id);
    let mut wire = presenting(Some(who));
    let ask = Stream::EndpointDiscovery {
        version: ASKING,
        filter: DiscoveryFilter {
            endpoint_info: false,
            device_identity: false,
            endpoint_name: false,
            product_instance_id: true,
            stream_configuration: false,
        },
    };
    let sent = answered(&mut wire, ask, 100);
    let [Stream::ProductInstanceId(_, text)] = sent.as_slice() else {
        panic!("only the Product Instance Id: {sent:?}");
    };
    assert_eq!(&text[..7], b"SN-0042");
}

/// M2-104 §7.1.8: a Function Block Discovery for block 0 is answered with its
/// Function Block Info: the one block, bidirectional, active, over the Groups
/// the session was given from the first, MIDI-CI not declared.
/// Spec: m2-104/7.1.8#1
#[test]
fn a_function_block_discovery_is_answered_with_the_one_block_over_the_sessions_groups() {
    let mut wire = presenting(Some(presented_over(4)));
    let ask = Stream::FunctionBlockDiscovery {
        block: BlockSelect::One(FunctionBlockId::new(0).unwrap()),
        filter: BlockFilter {
            info: true,
            name: false,
        },
    };
    let sent = answered(&mut wire, ask, 100);
    let [Stream::FunctionBlockInfo(block)] = sent.as_slice() else {
        panic!("one Function Block Info: {sent:?}");
    };
    assert!(block.active);
    assert_eq!(block.id.value(), FunctionBlockId::new(0));
    assert_eq!(block.direction, BlockDirection::Bidirectional);
    assert_eq!(block.first_group.value(), FirstGroup::new(0));
    assert_eq!(block.groups_spanned.value(), GroupSpan::new(4));
    assert_eq!(block.midi_ci_version, 0);
}

/// A session that presents nothing is not an Endpoint to its peers: a
/// discovery is not answered, and nothing is sent.
#[test]
fn a_session_that_presents_nothing_answers_no_discovery() {
    let mut wire = presenting(None);
    let sent = answered(&mut wire, ask_everything(), 100);
    assert!(sent.is_empty(), "{sent:?}");
}

/// M2-104 §7.1.6.1 step 3: a Stream Configuration Request is answered by a
/// Notification of what is now in force, within the Endpoint's capabilities:
/// a Protocol it supports is taken, and JR Timestamps it does not support
/// either way are refused, the Notification saying so.
#[test]
fn a_stream_configuration_request_is_granted_within_what_the_endpoint_declares() {
    let mut wire = presenting(Some(presented(identity())));
    let sent = answered(&mut wire, request(Protocol::Midi2, false, false), 100);
    assert_eq!(sent, [notification(Protocol::Midi2, false)]);

    let sent = answered(&mut wire, request(Protocol::Midi2, true, true), 200);
    assert_eq!(
        sent,
        [notification(Protocol::Midi2, false)],
        "JR Timestamps are neither received nor sent"
    );

    let sent = answered(&mut wire, request(Protocol::Midi1, false, false), 300);
    assert_eq!(sent, [notification(Protocol::Midi1, false)]);
}

/// The request the session makes of a device, and the one a device makes of
/// the session, agree: after the device asked for MIDI 2.0 and JR both ways
/// and was told what was granted, the session asks the device for nothing it
/// has not already said and sends it no further request.
#[test]
fn answering_a_request_starts_no_exchange_of_requests() {
    let mut wire = presenting(Some(presented(identity())));
    let _ = answered(&mut wire, request(Protocol::Midi2, true, true), 100);
    for ms in [200, 1_000, 5_000] {
        let _ = wire.tick(ms);
        let sent = stream_sent(&wire);
        assert!(
            sent.iter()
                .all(|s| !matches!(s, Stream::StreamConfigurationRequest(_))),
            "{sent:?}"
        );
    }
}

/// M2-101 §5.9: a new Device Identity ends the MIDI-CI Device that ran under
/// the old (an Invalidate MUID from and for its own MUID) and starts one under
/// the new, with a fresh MUID, in the next poll; and the Endpoint says who it
/// is now.
#[test]
fn a_new_identity_invalidates_the_old_muid_and_starts_discovery_as_the_new() {
    let (mut wire, old) = midi1_running();
    let other = model_identity(9);
    wire.session.present(Some(presented(other)));
    let _ = polled(&mut wire, 100);
    let sent = ci_sent(&wire);
    assert!(
        matches!(
            sent.first(),
            Some((g, CiMessage::InvalidateMuid { header, target }))
                if *g == GROUP_0 && header.source == old && *target == old
        ),
        "the old MUID is invalidated first: {sent:?}"
    );
    let [_, (g, CiMessage::Discovery { header, data })] = sent.as_slice() else {
        panic!("an Invalidate MUID, then one Discovery: {sent:?}");
    };
    assert_eq!(*g, GROUP_0);
    assert_ne!(header.source, old, "a fresh MUID");
    assert_eq!(data.identity, other);
}

/// Presenting nothing after presenting an identity ends MIDI-CI the same way:
/// an Invalidate MUID, and no Discovery after it.
#[test]
fn presenting_nothing_invalidates_the_muid_and_starts_nothing() {
    let (mut wire, old) = midi1_running();
    wire.session.present(None);
    let _ = polled(&mut wire, 100);
    let _ = polled(&mut wire, 200);
    let sent = ci_sent(&wire);
    assert!(
        matches!(
            sent.as_slice(),
            [(_, CiMessage::InvalidateMuid { target, .. })] if *target == old
        ),
        "{sent:?}"
    );
}

/// A change of the name alone is the Endpoint's: the peer is told the new
/// name, and the MIDI-CI Device keeps its MUID (nothing of MIDI-CI is sent).
#[test]
fn a_new_name_is_announced_and_leaves_midi_ci_alone() {
    let mut wire = presenting(Some(presented(identity())));
    let mut renamed = presented(identity());
    renamed.identity = MidiIdentity {
        name: EndpointName::new("Renamed").unwrap(),
        ..renamed.identity
    };
    wire.session.present(Some(renamed));
    let _ = wire.poll(100);
    let sent = received(&wire);
    let stream: Vec<_> = sent
        .iter()
        .filter(|ump| ump.message_type() == 0xF)
        .filter_map(|ump| match ump.decode() {
            Ok(Message::Stream(stream)) => Some(stream),
            _ => None,
        })
        .collect();
    assert!(
        stream
            .iter()
            .any(|s| matches!(s, Stream::EndpointName(_, text) if &text[..7] == b"Renamed")),
        "{stream:?}"
    );
    assert!(
        sent.iter().all(|ump| ump.message_type() != 3),
        "no MIDI-CI message: {sent:?}"
    );
}

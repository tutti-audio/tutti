//! The session as a UMP Endpoint (M2-104 §7.1) and as a MIDI-CI Device under
//! the identity it presents, against a fake peer: the wire is the fake's sink
//! and the input's producer.

use super::ci::{GROUP_0, ci_sent, peer, peer_declaring, polled};
use super::*;
use crate::{CiChange, Polled, ProfilesChange};
use tutti_midi_ci::CiCategories;
use tutti_midi_ci::CiMessage;
use tutti_midi_device::MidiIdentity;
use tutti_ump::ProductInstanceId;
use tutti_ump::{BlockFilter, BlockSelect, DiscoveryFilter, Form, MessageType, UmpVersion};

const ASKING: UmpVersion = UmpVersion { major: 1, minor: 1 };

/// A UMP Endpoint wire, settled, that presents `who` to its device.
fn presenting(who: Option<crate::Presented>) -> Wire {
    presenting_timed(who, InputTiming::Arrival)
}

/// As [`presenting`], with inputs timed by `timing`: under
/// [`InputTiming::JitterReduction`] the device (which offers JR both ways) is
/// asked to send JR Timestamps and settles with them on.
fn presenting_timed(who: Option<crate::Presented>, timing: InputTiming) -> Wire {
    let mut wire = session_over_timing(midi2(), PortDirection::Duplex, offering_both(), timing);
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
        .filter(|ump| ump.message_type() == MessageType::Stream)
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
/// Timestamps sent but not received (arrival timing), the Function Blocks
/// static and one. The Device
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
        Stream::ProductInstanceId(..),
        Stream::ProductInstanceId(..),
        Stream::ProductInstanceId(..),
        Stream::StreamConfigurationNotification(config),
    ] = sent.as_slice()
    else {
        panic!(
            "Info, Identity, Name, Product Instance Id and Configuration, in that order: {sent:?}"
        );
    };
    assert_eq!(info.version, ASKING);
    assert!(info.static_function_blocks);
    assert_eq!(info.function_blocks.count().map(|c| c.get()), Some(1));
    assert!(info.midi1 && info.midi2);
    assert!(!info.rx_jr && info.tx_jr);
    assert_eq!(*who, identity());
    assert_eq!(&name[..7], b"Session");
    assert_eq!(
        config.protocol,
        Protocol::Midi2,
        "the Protocol the negotiation settled on, which the output sends in"
    );
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

fn ask_instance_id() -> Stream {
    Stream::EndpointDiscovery {
        version: ASKING,
        filter: DiscoveryFilter {
            endpoint_info: false,
            device_identity: false,
            endpoint_name: false,
            product_instance_id: true,
            stream_configuration: false,
        },
    }
}

/// M2-104 §7.1.5: an identity with no Product Instance Id is declared with one
/// the session made, and a rebuild of the Endpoint (a change of Groups) answers
/// the same one, so the Id re-identifies the device; the session says what it
/// is, so an app can keep it.
/// Spec: m2-104/7.1.5#1, m2-104/7.1.5#2
#[test]
fn a_generated_product_instance_id_is_the_same_through_every_rebuild() {
    let mut wire = presenting(Some(presented(identity())));
    let id_packets = |sent: Vec<Stream>| -> Vec<Stream> {
        sent.into_iter()
            .filter(|s| matches!(s, Stream::ProductInstanceId(..)))
            .collect()
    };
    let first = id_packets(answered(&mut wire, ask_instance_id(), 100));
    assert_eq!(first.len(), 3, "32 bytes: a Start, a Continue and an End");
    wire.session.present(Some(presented_over(2)));
    let second = id_packets(answered(&mut wire, ask_instance_id(), 200));
    wire.session.present(Some(presented_over(3)));
    let third = id_packets(answered(&mut wire, ask_instance_id(), 300));
    // Each rebuild announces the Endpoint again and then answers: the same Id
    // twice, and the same as before.
    for after in [second, third] {
        assert!(!after.is_empty() && after.len() % 3 == 0);
        assert!(
            after.chunks(3).all(|id| id == first.as_slice()),
            "{after:?}"
        );
    }
    let kept = wire.session.instance_id().expect("the session made one");
    let mut text = Vec::new();
    for packet in &first {
        let Stream::ProductInstanceId(_, bytes) = packet else {
            panic!("{packet:?}");
        };
        text.extend_from_slice(bytes);
    }
    assert!(text.starts_with(kept.as_str().as_bytes()));
}

/// An Id the identity carries is declared as it is, and the session makes none.
#[test]
fn a_given_product_instance_id_is_kept_through_a_rebuild() {
    let mut who = presented(identity());
    who.identity.instance = Some(ProductInstanceId::new("SN-0042").unwrap());
    let mut wire = presenting(Some(who));
    wire.session.present(Some(crate::Presented {
        groups: GroupSpan::new(2).unwrap(),
        ..who
    }));
    let sent = answered(&mut wire, ask_instance_id(), 100);
    let ids: Vec<_> = sent
        .iter()
        .filter(|s| matches!(s, Stream::ProductInstanceId(..)))
        .collect();
    assert!(!ids.is_empty());
    for id in ids {
        let Stream::ProductInstanceId(form, text) = id else {
            unreachable!()
        };
        assert_eq!((*form, &text[..7]), (Form::Complete, &b"SN-0042"[..]));
    }
    assert_eq!(wire.session.instance_id(), who.identity.instance);
}

/// `instance_id` is the Id in force: a given one as given, the generated one
/// when the identity has none, and the generated one is kept across a switch to
/// a given Id and back.
#[test]
fn the_session_reports_the_product_instance_id_in_force() {
    let mut wire = presenting(Some(presented(identity())));
    let generated = wire.session.instance_id().expect("the session made one");
    let mut given = presented(identity());
    let id = ProductInstanceId::new("SN-0042").unwrap();
    given.identity.instance = Some(id);
    wire.session.present(Some(given));
    assert_eq!(wire.session.instance_id(), Some(id));
    wire.session.present(Some(presented(identity())));
    assert_eq!(wire.session.instance_id(), Some(generated));
    wire.session.present(None);
    assert_eq!(wire.session.instance_id(), Some(generated));
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
        [Stream::StreamConfigurationNotification(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: false,
            tx_jr: true,
        })],
        "JR Timestamps are sent, and not received under arrival timing"
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
    let (mut wire, old) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    let other = model_identity(9);
    wire.session.present(Some(presented(other)));
    let reports = polled(&mut wire, 100);
    assert!(
        reports
            .iter()
            .any(|p| matches!(p, Polled::Ci(_, who, CiChange::Lost) if who.muid == peer())),
        "the peer of the old MUID is lost: {reports:?}"
    );
    assert!(
        reports.iter().any(
            |p| matches!(p, Polled::Profile(_, who, ProfilesChange::Dropped) if who.muid == peer())
        ),
        "and its Profiles dropped: {reports:?}"
    );
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
    let (mut wire, old) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    wire.session.present(None);
    let mut reports = polled(&mut wire, 100);
    reports.extend(polled(&mut wire, 200));
    assert!(
        reports
            .iter()
            .any(|p| matches!(p, Polled::Ci(_, who, CiChange::Lost) if who.muid == peer())),
        "{reports:?}"
    );
    assert!(
        reports
            .iter()
            .any(|p| matches!(p, Polled::Profile(_, _, ProfilesChange::Dropped))),
        "{reports:?}"
    );
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
        .filter(|ump| ump.message_type() == MessageType::Stream)
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
        sent.iter()
            .all(|ump| ump.message_type() != MessageType::Sysex7),
        "no MIDI-CI message: {sent:?}"
    );
}

/// M2-104 §7.1.6.2: after a Stream Configuration Request is granted, all data
/// uses the selected Protocol. The one stream state serves both directions: the
/// Notification that said MIDI 1.0 is the Protocol the output then sends in,
/// and the reverse.
/// Spec: m2-104/7.1.6.2#2
#[test]
fn a_granted_request_moves_the_output_to_the_protocol_asked() {
    let mut wire = presenting(Some(presented(identity())));
    wire.connect_output();
    let note = [(Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 0xFFFF))];
    wire.send(&note);
    assert_eq!(
        received(&wire)[0].message_type(),
        MessageType::Midi2ChannelVoice,
        "negotiated MIDI 2.0"
    );

    let _ = answered(&mut wire, request(Protocol::Midi1, false, false), 100);
    wire.send(&note);
    assert_eq!(
        received(&wire)[0].message_type(),
        MessageType::Midi1ChannelVoice,
        "granted MIDI 1.0"
    );
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().config().protocol,
        Protocol::Midi1
    );

    let _ = answered(&mut wire, request(Protocol::Midi2, false, false), 200);
    wire.send(&note);
    assert_eq!(
        received(&wire)[0].message_type(),
        MessageType::Midi2ChannelVoice,
        "granted MIDI 2.0"
    );
}

/// What the session asks of a device after its peer's request moved the
/// Protocol never undoes it: when the device sends JR Timestamps the input did
/// not ask for, the request to stop them names the Protocol in force.
#[test]
fn the_request_to_stop_jr_keeps_the_protocol_a_peers_request_chose() {
    let mut wire = presenting(Some(presented(identity())));
    let _ = answered(&mut wire, request(Protocol::Midi1, false, false), 100);
    let jr_on = Stream::StreamConfigurationNotification(StreamConfig {
        protocol: Protocol::Midi1,
        rx_jr: false,
        tx_jr: true,
    });
    send_ump(&wire, jr_on, 200);
    let _ = wire.poll(201);
    let sent = stream_sent(&wire);
    assert_eq!(
        sent,
        [
            request(Protocol::Midi1, false, false),
            // The Endpoint receives what the device sends, until it stops.
            Stream::StreamConfigurationNotification(StreamConfig {
                protocol: Protocol::Midi1,
                rx_jr: true,
                tx_jr: false,
            }),
        ],
        "one request, to stop JR, in the Protocol in force, and the JR in force told"
    );
}

/// The Stream Configuration Notification follows the Protocol in force once the
/// device has been told one: a Notification the device sends moving the
/// Protocol is followed by the session's own, so both ends read one state.
#[test]
fn a_change_of_the_protocol_in_force_is_told_to_a_device_that_was_told() {
    let mut wire = presenting(Some(presented(identity())));
    let sent = answered(&mut wire, ask_everything(), 100);
    assert!(
        sent.iter().any(
            |s| matches!(s, Stream::StreamConfigurationNotification(c) if c.protocol == Protocol::Midi2)
        ),
        "{sent:?}"
    );
    send_ump(&wire, notification(Protocol::Midi1, false), 200);
    let _ = wire.poll(201);
    assert_eq!(stream_sent(&wire), [notification(Protocol::Midi1, false)]);
}

/// A rebuilt Endpoint over other Groups announces its Function Block again
/// (M2-104 §7.1.8), after the Endpoint Info, so a peer learns the new span.
#[test]
fn a_change_of_the_groups_announces_the_function_block() {
    let mut wire = presenting(Some(presented_over(2)));
    wire.session.present(Some(presented_over(4)));
    let _ = wire.poll(100);
    let sent = stream_sent(&wire);
    let block = sent.iter().find_map(|s| match s {
        Stream::FunctionBlockInfo(block) => Some(block),
        _ => None,
    });
    assert_eq!(
        block.and_then(|b| b.groups_spanned.value()),
        GroupSpan::new(4),
        "{sent:?}"
    );
    assert!(
        sent.iter().any(|s| matches!(s, Stream::EndpointInfo(_))),
        "{sent:?}"
    );
}

fn jr_wire() -> Wire {
    let wire = presenting_timed(Some(presented(identity())), InputTiming::JitterReduction);
    let config = wire.session.stream(&wire.id).unwrap().config();
    assert!(config.tx_jr, "the device sends JR Timestamps to the input");
    wire
}

/// M2-104 §7.1.6.3: the Stream Configuration Notification reports the
/// Timestamps in force. Under JR timing the device sends them to the session,
/// so the Endpoint receives them (RXJR), and says so, in the Notification and
/// in the Endpoint Info it declares.
#[test]
fn under_jitter_reduction_the_notification_says_the_session_receives_jr() {
    let mut wire = jr_wire();
    let sent = answered(&mut wire, ask_everything(), 100);
    let notification = sent.iter().find_map(|s| match s {
        Stream::StreamConfigurationNotification(config) => Some(*config),
        _ => None,
    });
    assert_eq!(
        notification,
        Some(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: false,
        }),
        "{sent:?}"
    );
    assert!(
        sent.iter()
            .any(|s| matches!(s, Stream::EndpointInfo(info) if info.rx_jr && info.tx_jr)),
        "{sent:?}"
    );
}

/// A request that keeps receiving JR on is granted with it on, and the input
/// still reads the device's JR Timestamps; one that asks it off is honoured
/// (§7.1.6.1 step 3: JR off is always possible), the device is no longer asked
/// and the input is timed by arrival, with the Notification saying so.
#[test]
fn a_request_for_jr_on_keeps_the_input_on_jr_and_one_for_off_turns_it_off() {
    let mut wire = jr_wire();
    let sent = answered(&mut wire, request(Protocol::Midi2, true, false), 100);
    assert_eq!(sent, [notification(Protocol::Midi2, true)]);
    let config = wire.session.stream(&wire.id).unwrap().config();
    assert!(config.tx_jr && config.protocol == Protocol::Midi2);

    let sent = answered(&mut wire, request(Protocol::Midi2, false, false), 200);
    assert_eq!(sent, [notification(Protocol::Midi2, false)]);
    let config = wire.session.stream(&wire.id).unwrap().config();
    assert!(!config.tx_jr, "the stream agrees with the Notification");
}

/// With arrival timing the session does not receive JR Timestamps: a request to
/// receive them is granted with them off, and the stream agrees.
#[test]
fn under_arrival_timing_a_request_to_receive_jr_is_granted_with_it_off() {
    let mut wire = presenting(Some(presented(identity())));
    let sent = answered(&mut wire, request(Protocol::Midi2, true, false), 100);
    assert_eq!(sent, [notification(Protocol::Midi2, false)]);
    let config = wire.session.stream(&wire.id).unwrap().config();
    assert!(!config.tx_jr && !config.rx_jr);
}

/// A peer that asks the session to send JR Timestamps (the Endpoint's TXJR) is
/// granted: the Notification says so, the stream's RXJR is on, and the output
/// then carries a JR Clock it is handed.
#[test]
fn a_request_for_the_session_to_send_jr_is_granted_and_the_output_stamps() {
    let mut wire = presenting(Some(presented(identity())));
    wire.connect_output();
    let clock = Ump::from(Message::Utility(tutti_ump::Utility::JrClock(7)));
    let note = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    let events = [(Offset::ZERO, clock), (Offset::ZERO, note)];
    wire.send(&events);
    assert_eq!(received(&wire), [note], "no JR before the grant");

    let sent = answered(&mut wire, request(Protocol::Midi2, false, true), 100);
    assert_eq!(
        sent,
        [Stream::StreamConfigurationNotification(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: false,
            tx_jr: true,
        })]
    );
    assert!(wire.session.stream(&wire.id).unwrap().config().rx_jr);
    wire.send(&events);
    assert_eq!(received(&wire), [clock, note], "JR after the grant");
}

/// A device that never answered is the OS's Endpoint, which offers no JR. A
/// peer's Stream Configuration Request the session grants (the session sends
/// JR Timestamps) is a fact about the link, so the device's configuration
/// records it (the device receives them) and the stream settles: the same
/// request again changes nothing.
#[test]
fn a_granted_jr_request_is_recorded_for_an_os_described_device_and_settles() {
    let silent = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        ..Capabilities::default()
    }));
    let mut wire = session_over(midi2(), PortDirection::Duplex, silent);
    wire.session.present(Some(presented(identity())));
    wire.open();
    let step = STEP_TIMEOUT.as_millis() as u64;
    let mut settled = Vec::new();
    for ms in [0, step, 2 * step, 3 * step + 1] {
        settled.extend(streams(wire.poll(ms)));
    }
    let [(_, stream)] = settled.as_slice() else {
        panic!("settled: {settled:?}");
    };
    assert_eq!(stream.basis(), Basis::Unanswered);
    assert!(!stream.config().rx_jr);
    let _ = received(&wire);

    let at = 4 * step;
    send_ump(&wire, request(Protocol::Midi2, false, true), at);
    let moved = streams(wire.poll(at + 1));
    let [(_, moved)] = moved.as_slice() else {
        panic!("the grant moves the stream: {moved:?}");
    };
    assert!(
        moved.config().rx_jr,
        "the device receives what the session sends"
    );

    send_ump(&wire, request(Protocol::Midi2, false, true), at + 10);
    assert!(
        streams(wire.poll(at + 11)).is_empty(),
        "the stream has settled"
    );
}

//! MIDI-CI on the wire against a fake device, through
//! the session and the fake backend: what is sent is the fake's sink, what the
//! device says is the input's producer.

use super::*;
use crate::{Answer, CiChange, PeerId, Polled, ReportEnd};
use tutti_midi_ci::Initiator;
use tutti_midi_ci::Muid;
use tutti_midi_ci::{
    CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiMessage, Decoded, Destination, DeviceBody,
    DiscoveryData, MaxSysexSize, ReplyData, ReplyVersion2,
};
use tutti_midi_device::INQUIRY_TIMEOUT;
use tutti_ump::{
    CiVersion, MidiGroup, MpeChannels, MpeOverlap, MpeZoneSpan, Sysex8StreamCount, U7,
};

pub(super) const GROUP_0: MidiGroup = MidiGroup::of(0);

/// The MIDI-CI messages among what the fake device received, with their Group.
pub(super) fn ci_sent(wire: &Wire) -> Vec<(MidiGroup, CiMessage)> {
    payloads(&received(wire))
        .into_iter()
        .filter_map(|(group, payload)| match CiMessage::decode(&payload) {
            Decoded::Message(message) => Some((group, message)),
            _ => None,
        })
        .collect()
}

/// The device says `message` on `group` at `ms`.
fn say(wire: &Wire, group: MidiGroup, message: &CiMessage, ms: u64) {
    reply_at(wire, message.packets(group).unwrap().collect(), ms);
}

/// A peer of ours on the wire, and what it declares.
/// The peer of the tests, on Group 0: the one every change names.
fn peer_id() -> PeerId {
    PeerId {
        group: GROUP_0,
        muid: peer(),
    }
}

pub(super) fn peer() -> Muid {
    Muid::of(0x0012_3456)
}

fn peer_data() -> ReplyData {
    peer_data_of(CiCategories::empty())
}

fn peer_data_of(categories: CiCategories) -> ReplyData {
    ReplyData::new(DeviceBody::new(
        identity(),
        categories,
        MaxSysexSize::of(256),
    ))
}

pub(super) fn polled(wire: &mut Wire, ms: u64) -> Vec<Polled> {
    wire.session.poll(wire.at(ms)).collect()
}

/// A block of one Group at `first_group` that declares MIDI-CI `version`.
fn ci_block(id: u8, first_group: u8, version: u8) -> FunctionBlock {
    let builder = FunctionBlock::builder(
        FunctionBlockId::new(id).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(first_group).unwrap(),
        GroupSpan::new(1).unwrap(),
    );
    let builder = match CiVersion::new(version) {
        Some(version) => builder.midi_ci(version, Sysex8StreamCount::new(0)),
        None => builder,
    };
    builder.name(BlockName::new("B").unwrap()).build().unwrap()
}

fn endpoint_of(blocks: &[FunctionBlock]) -> Endpoint {
    let device = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    }));
    blocks.iter().cloned().fold(device, Endpoint::block)
}

/// A negotiated UMP Endpoint with `blocks`, and MIDI-CI started by the poll
/// that settled it.
fn endpoint_wire(blocks: &[FunctionBlock]) -> Wire {
    let mut wire = session_over_ci(
        midi2(),
        PortDirection::Duplex,
        endpoint_of(blocks),
        Some(identity()),
    );
    wire.open();
    // One Function Block Discovery per declared block takes a few ticks.
    let settled = (0..32).any(|ms| !wire.tick(ms).1.is_empty());
    assert!(settled, "the negotiation settled");
    let _ = polled(&mut wire, 100);
    wire
}

/// A MIDI 1.0 port whose Identity Request was answered and whose MIDI-CI has
/// started, with the MUID it drew; what was sent so far is read off.
pub(super) fn midi1_running() -> (Wire, Muid) {
    let mut wire = midi1_wire_ci(Some(identity()));
    let _ = polled(&mut wire, 0);
    reply_at(&wire, identity_reply(0), 10);
    // The poll that ends the inquiry starts MIDI-CI.
    let _ = polled(&mut wire, 20);
    let sent = ci_sent(&wire);
    let [(group, CiMessage::Discovery { source, .. })] = sent.as_slice() else {
        panic!("one Discovery expected");
    };
    assert_eq!(*group, GROUP_0);
    let source = *source;
    (wire, source)
}

/// M2-101 §3.2.2 and Table 6: a UMP Endpoint's Function Block that declares
/// MIDI-CI is a MIDI-CI Device of its own, on the block's first Group, and
/// opens with a Discovery to the Broadcast MUID from the Function Block's
/// Device ID (`7F`): the bytes of Table 6. A block that declares none (M2-104
/// §7.1.8) is sent nothing.
/// Spec: m2-101/4.1#1, m2-101/5.5#1, m2-101/5.5.3#2
#[test]
fn a_function_block_that_declares_midi_ci_is_sent_discovery_on_its_first_group() {
    let wire = endpoint_wire(&[ci_block(0, 2, 2), ci_block(1, 4, 0)]);
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one Discovery, on the block that declares MIDI-CI: {sent:?}");
    };
    assert_eq!(*group, MidiGroup::of(2), "the block's first Group");
    // F0 7E <device ID> 0D 70 <version> <source MUID> <destination MUID> ...
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x70]);
    assert_eq!(payload[4], CI_VERSION.get());
    assert_eq!(payload[9..13], [0x7F; 4], "the Broadcast MUID");
    assert_eq!(payload.len(), 13 + 17);
    let Decoded::Message(CiMessage::Discovery {
        device_id, data, ..
    }) = CiMessage::decode(payload)
    else {
        panic!("a Discovery");
    };
    assert_eq!(device_id, CiDeviceId::FunctionBlock);
    // The body (Table 6): exactly the app's identity (`identity()`: manufacturer
    // `41 00 00`, family 1 2, model 3 4, revision 5 6 7 8), Profile Configuration,
    // Property Exchange and Process Inquiry as the categories (bits 2, 3 and 4 of
    // Table 7: the session initiates all three), 512 as four 7-bit bytes, Output
    // Path 0.
    assert_eq!(payload[13..16], [0x41, 0x00, 0x00]);
    assert_eq!(payload[16..24], [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(data.body.identity, identity());
    assert_eq!(payload[24], 0b1_1100);
    assert_eq!(
        data.body.categories,
        CiCategories::PROFILE_CONFIGURATION
            | CiCategories::PROPERTY_EXCHANGE
            | CiCategories::PROCESS_INQUIRY
    );
    assert_eq!(payload[25..29], [0x00, 0x04, 0x00, 0x00]);
    assert_eq!(payload[29], 0);
    assert_eq!(data.body.max_sysex_size, MaxSysexSize::of(512));
}

/// An Endpoint whose blocks all declare no MIDI-CI runs none: nothing is sent
/// however long it goes on, and the port does not copy its SysEx.
#[test]
fn an_endpoint_whose_blocks_declare_no_midi_ci_runs_none() {
    let mut wire = endpoint_wire(&[ci_block(0, 0, 0), ci_block(1, 3, 0)]);
    let slot = wire.session.ports().list_input_ports()[0].id;
    for ms in [200, 5_000, 20_000] {
        let _ = polled(&mut wire, ms);
    }
    assert!(received(&wire).is_empty());
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 20_001);
    assert_eq!(wire.session.ports().drain_stream(slot).count(), 0);
}

/// §3.2.2: with no Function Block to say otherwise, the Endpoint's Group 0 is
/// one MIDI-CI Device. That holds for an Endpoint that answered with no blocks
/// and for one that never answered the Stream negotiation.
#[test]
fn an_endpoint_with_no_blocks_or_no_answer_gets_discovery_on_group_0() {
    // Answered, no blocks.
    let wire = endpoint_wire(&[]);
    let sent = payloads(&received(&wire));
    assert!(
        matches!(sent.as_slice(), [(g, _)] if *g == GROUP_0),
        "{sent:?}"
    );

    // Never answered: the Stream Discoveries are sent and dropped.
    let mut wire = session_over_ci(
        midi2(),
        PortDirection::Duplex,
        Endpoint::new(Supports::of(Capabilities {
            midi1: true,
            ..Capabilities::default()
        })),
        Some(identity()),
    );
    wire.open();
    let step = STEP_TIMEOUT.as_millis() as u64;
    for ms in [0, step, 2 * step, 3 * step + 1, 3 * step + 2] {
        let _ = polled(&mut wire, ms);
    }
    let sent = payloads(&received(&wire));
    assert!(
        matches!(sent.as_slice(), [(g, _)] if *g == GROUP_0),
        "{sent:?}"
    );
}

/// M2-101 routes by Group: a Discovery on the Group of a block is answered, one
/// on a Group with no MIDI-CI Device is not.
/// Spec: m2-101/5.2.1#3
#[test]
fn messages_are_routed_by_group() {
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2)]);
    let _ = received(&wire);
    let discovery = CiMessage::Discovery {
        device_id: CiDeviceId::FunctionBlock,
        source: peer(),
        data: DiscoveryData::new(peer_data().body),
    };
    say(&wire, MidiGroup::of(5), &discovery, 200);
    let _ = polled(&mut wire, 210);
    assert!(ci_sent(&wire).is_empty(), "no device on Group 5");
    say(&wire, MidiGroup::of(2), &discovery, 220);
    let _ = polled(&mut wire, 230);
    let sent = ci_sent(&wire);
    assert!(
        matches!(sent.as_slice(), [(g, CiMessage::DiscoveryReply { .. })] if *g == MidiGroup::of(2)),
        "{sent:?}"
    );
}

/// A MIDI 1.0 port is asked its identity first, and MIDI-CI starts only when
/// that has ended, answered or given up, so the two never read the SysEx
/// stream at once: every Identity Request precedes the Discovery.
#[test]
fn a_midi1_port_is_sent_discovery_only_after_the_identity_inquiry_ended() {
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    for answered in [true, false] {
        let mut wire = midi1_wire_ci(Some(identity()));
        let mut order = String::new();
        for step in 0..8 {
            if answered && step == 1 {
                reply_at(&wire, identity_reply(0), step * ms - 1);
            }
            let _ = polled(&mut wire, step * ms);
            for (_, payload) in payloads(&received(&wire)) {
                match CiMessage::decode(&payload) {
                    Decoded::Message(CiMessage::Discovery { .. }) => order.push('D'),
                    _ if payload == [0x7E, 0x7F, 0x06, 0x01] => order.push('R'),
                    other => panic!("sent {other:?}"),
                }
            }
        }
        assert_eq!(
            order,
            if answered { "RD" } else { "RRD" },
            "answered: {answered}"
        );
    }
}

/// A Reply to Discovery addressed to the device makes its sender a peer
/// (§5.6), found in the session's read and reported once by `poll`.
#[test]
fn a_reply_to_discovery_makes_the_peer_appear_and_is_reported_once() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let found = polled(&mut wire, 50);
    let [Polled::Ci(device, who, CiChange::Found(declared))] = found.as_slice() else {
        panic!("one peer found, got {found:?}");
    };
    assert_eq!(*device, wire.id);
    assert_eq!(*who, peer_id());
    assert_eq!(declared.discovery(), &peer_data());
    let listed = wire.session.ci_peers(&wire.id);
    let [info] = &listed[..] else {
        panic!("one peer listed, got {listed:?}");
    };
    assert_eq!(info.id(), *who);
    assert_eq!((info.group(), info.muid()), (GROUP_0, peer()));
    assert_eq!(info.peer(), declared);
    assert!(polled(&mut wire, 60).is_empty(), "reported once");
}

/// Appendix E: a Responder answers Discovery from another device at any time,
/// long after its own window closed, with a Reply to that device that echoes
/// the Output Path ID (§5.6.1) and declares no Function Block (§5.6.2). A
/// Discovery is not a peer found.
/// Spec: m2-101/4.1#2, m2-101/5.6#1
#[test]
fn a_discovery_a_peer_sends_is_answered_long_after_the_window_closed() {
    let (mut wire, ours) = midi1_running();
    let _ = polled(&mut wire, 10_000);
    let data = DiscoveryData {
        body: peer_data().body,
        output_path_id: Some(U7::of(5)),
    };
    say(
        &wire,
        GROUP_0,
        &CiMessage::Discovery {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            data,
        },
        10_001,
    );
    let none = polled(&mut wire, 10_002);
    assert!(none.is_empty(), "a Discovery finds no peer: {none:?}");
    let sent = ci_sent(&wire);
    let [
        (
            group,
            CiMessage::DiscoveryReply {
                source, to, data, ..
            },
        ),
    ] = sent.as_slice()
    else {
        panic!("one Reply to Discovery, got {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    assert_eq!((*source, *to), (ours, peer()));
    assert_eq!(
        data.version_2,
        Some(ReplyVersion2 {
            output_path_id: U7::of(5),
            function_block: None
        })
    );
    assert_eq!(data.body.identity, identity());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// §5.9: an Invalidate MUID naming a found peer discards it, and `poll` reports
/// the loss.
/// Spec: m2-101/5.9#3
#[test]
fn an_invalidate_muid_of_a_found_peer_removes_it_and_is_reported() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1);
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(Muid::of(0x00AB_CDEF)),
            target: peer(),
        },
        60,
    );
    let lost = polled(&mut wire, 70);
    assert!(
        matches!(
            lost.as_slice(),
            [Polled::Ci(_, who, CiChange::Lost)] if *who == peer_id()
        ),
        "{lost:?}"
    );
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// A SysEx that is not MIDI-CI (General MIDI System On, M1 p.52) is ignored:
/// nothing is answered and no peer appears.
#[test]
fn a_sysex_that_is_not_midi_ci_is_ignored() {
    let (mut wire, _) = midi1_running();
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 40);
    assert!(polled(&mut wire, 50).is_empty());
    assert!(received(&wire).is_empty());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// What MIDI-CI sends goes out on the input's own output and never the
/// session's MIDI output, and the SysEx it hears still reaches the graph: the
/// port copies it, it does not divert it.
#[test]
fn nothing_midi_ci_reaches_the_sessions_output_and_the_graph_still_hears_sysex() {
    use crate::testing::Rig;
    let mut wire = midi1_wire_ci(Some(identity()));
    let other = EndpointId::from_raw(8);
    wire.counters.plug(
        crate::test_support::endpoint(8, "Other"),
        PortDirection::Output,
    );
    let other_id = wire
        .session
        .outputs()
        .into_iter()
        .find(|d| d.name == "Other")
        .unwrap()
        .id;
    wire.session
        .connect_output(crate::testing::started(), &other_id)
        .unwrap();

    let mut rig = Rig::over(wire.session.ports().inlets(), wire.start);
    rig.run(1);
    rig.attach(wire.inlet.take().expect("the wire connected its input"));

    let _ = polled(&mut wire, 0);
    reply_at(&wire, identity_reply(0), 1);
    let _ = polled(&mut wire, 2);
    let _ = polled(&mut wire, 3);
    let sent = ci_sent(&wire);
    let [(_, CiMessage::Discovery { source, .. })] = sent.as_slice() else {
        panic!("the Discovery reached the device, got {sent:?}");
    };
    assert!(
        wire.counters.take_sent(other).is_empty(),
        "and not the session's output"
    );
    let reply = CiMessage::DiscoveryReply {
        device_id: CiDeviceId::FunctionBlock,
        source: peer(),
        to: *source,
        data: peer_data(),
    };
    say(&wire, GROUP_0, &reply, 5);
    let found = polled(&mut wire, 6);
    assert!(matches!(
        found.as_slice(),
        [Polled::Ci(_, _, CiChange::Found(_))]
    ));
    assert!(wire.counters.take_sent(other).is_empty());

    rig.run(40);
    let heard: Vec<_> = rig.take().into_iter().map(|(_, _, ump)| ump).collect();
    let said: Vec<_> = [identity_reply(0), reply.packets(GROUP_0).unwrap().collect()].concat();
    assert_eq!(heard, said, "the graph hears every packet MIDI-CI heard");
}

/// MIDI-CI "closes with the input": once it is disconnected its peers are
/// gone and nothing more is sent, whatever the poll's time.
#[test]
fn the_inputs_disconnect_ends_midi_ci() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1, "alive");

    wire.session
        .disconnect_input(&wire.id.clone())
        .expect("the input is open");
    // The close announced the MUID invalid, once; nothing follows it.
    assert_eq!(ci_sent(&wire).len(), 1);
    for ms in [60, 5_000, 60_000] {
        let _ = polled(&mut wire, ms);
    }
    assert!(received(&wire).is_empty());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// A Destination the Reply to Discovery is for is one device, never Broadcast
/// (§3.3.3), as sent.
/// Spec: m2-101/3.3.3#3
#[test]
fn a_reply_to_discovery_is_addressed_to_one_device() {
    let (mut wire, _) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::Discovery {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            data: DiscoveryData::new(peer_data().body),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    let sent = ci_sent(&wire);
    assert_eq!(
        sent.first().map(|(_, m)| m.destination()),
        Some(Destination::Muid(peer()))
    );
}

/// Once the inquiry has given up on a silent device, an enabled MIDI-CI takes over the
/// port's copy of the device's SysEx7 packets (it stays on, and the graph still
/// hears them, the next test), and no Identity Request is sent again: what
/// goes out is MIDI-CI's.
#[test]
fn after_giving_up_the_sysex_copy_stays_on_for_midi_ci_and_no_request_is_sent() {
    use tutti_midi_device::INQUIRY_ATTEMPTS;
    let mut wire = midi1_wire_ci(Some(identity()));
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    // The request, its one retry, then the poll that gives up, then the one
    // that starts MIDI-CI.
    for step in 0..=u64::from(INQUIRY_ATTEMPTS) + 1 {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
    }
    let slot = wire.session.ports().list_input_ports()[0].id;
    let _ = received(&wire);
    // Whatever the ring holds from the inquiry is read off.
    let _ = wire.session.ports().drain_stream(slot).count();

    reply_at(&wire, identity_reply(0), 10 * ms);
    assert_eq!(
        wire.session.ports().drain_stream(slot).count(),
        identity_reply(0).len(),
        "the copy is on while MIDI-CI runs"
    );
    for step in 10..13 {
        let _ = wire.session.poll(wire.at(step * ms)).count();
    }
    assert_eq!(identity_requests(&received(&wire)), 0);
}

/// MIDI-CI is off until the app gives an identity: a settled block that declares
/// MIDI-CI and a MIDI 1.0 port are sent no Discovery, and the port's SysEx copy
/// is off after the Identity Request (which is sent all the same).
#[test]
fn midi_ci_is_off_by_default() {
    let mut wire = session_over(
        midi2(),
        PortDirection::Duplex,
        endpoint_of(&[ci_block(0, 2, 2)]),
    );
    wire.open();
    let settled = (0..32).any(|ms| !wire.tick(ms).1.is_empty());
    assert!(settled);
    for ms in [100, 5_000, 20_000] {
        let _ = polled(&mut wire, ms);
    }
    assert!(
        received(&wire).is_empty(),
        "a block that declares MIDI-CI, unasked"
    );
    assert_eq!(wire.session.presented(), None);

    use tutti_midi_device::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT};
    let mut wire = midi1_wire();
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    let mut requests = 0;
    for step in 0..=u64::from(INQUIRY_ATTEMPTS) + 2 {
        let _ = polled(&mut wire, step * ms);
        let sent = received(&wire);
        requests += identity_requests(&sent);
        assert_eq!(
            payloads(&sent).len(),
            identity_requests(&sent),
            "nothing but requests"
        );
    }
    assert_eq!(requests, usize::from(INQUIRY_ATTEMPTS));
    let slot = wire.session.ports().list_input_ports()[0].id;
    let _ = wire.session.ports().drain_stream(slot).count();
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 20 * ms);
    assert_eq!(
        wire.session.ports().drain_stream(slot).count(),
        0,
        "the copy is off"
    );
}

/// The peers read are a snapshot: a list held across a change keeps what it
/// read, a list read after it shows the change, and two reads with no change
/// between them share one list (a read copies nothing).
#[test]
fn the_peers_read_are_a_snapshot_shared_until_they_change() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    let held = wire.session.ci_peers(&wire.id);
    assert_eq!(held.len(), 1);
    assert!(std::sync::Arc::ptr_eq(
        &held,
        &wire.session.ci_peers(&wire.id)
    ));

    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(peer()),
            target: peer(),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    assert_eq!(held.len(), 1, "a held list keeps what it read");
    assert_eq!(held[0].muid(), peer());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// A message is cut where the stream ring overflowed: the packets before the gap
/// and the ones after it are not one message. The payload is a valid Reply to
/// Discovery with padding after it (bytes past the last field are ignored), cut
/// in the padding, so joined it would be read and its sender found.
#[test]
fn a_gap_in_the_stream_ring_never_joins_a_midi_ci_message() {
    let (mut wire, ours) = midi1_running();
    let reply = |source: Muid| {
        let mut payload = CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source,
            to: ours,
            data: peer_data(),
        }
        .encode();
        payload.extend([0u8; 24]);
        tutti_ump::Sysex7Packets::from_payload(GROUP_0, &payload)
            .unwrap()
            .collect::<Vec<_>>()
    };
    let cut = reply(peer());
    assert!(cut.len() >= 9, "{}", cut.len());
    let filler = Ump::from(Message::Stream(Stream::EndpointDiscovery {
        version: tutti_midi_device::UMP_VERSION,
        filter: tutti_ump::DiscoveryFilter {
            endpoint_info: true,
            device_identity: false,
            endpoint_name: false,
            product_instance_id: false,
            stream_configuration: false,
        },
    }));
    // The first six packets (the whole Reply, 36 bytes), then the ring is
    // filled to overflowing, so the next two are lost ...
    let (head, rest) = cut.split_at(6);
    reply_at(&wire, head.to_vec(), 40);
    for _ in 0..crate::port::async_port::STREAM_CAPACITY + 8 {
        reply_at(&wire, vec![filler], 41);
    }
    reply_at(&wire, rest[..2].to_vec(), 42);
    let _ = polled(&mut wire, 50);
    // ... and the End arrives after the gap.
    reply_at(&wire, rest[2..].to_vec(), 60);
    let spliced = polled(&mut wire, 70);
    assert!(
        spliced.is_empty(),
        "a message with a hole is no message: {spliced:?}"
    );
    assert!(wire.session.ci_peers(&wire.id).is_empty());

    // A whole message after it is read.
    let other = Muid::of(0x0065_4321);
    reply_at(&wire, reply(other), 80);
    let found = polled(&mut wire, 90);
    assert!(
        matches!(found.as_slice(), [Polled::Ci(_, who, CiChange::Found(_))] if who.muid == other),
        "{found:?}"
    );
}

/// The device tells the session its block `id` is now on `first_group`, one
/// Group wide, declaring MIDI-CI `version`.
fn block_info(wire: &Wire, id: u8, first_group: u8, version: u8, ms: u64) {
    send_ump(
        wire,
        Stream::FunctionBlockInfo(tutti_ump::FunctionBlockInfo {
            active: true,
            id: FunctionBlockId::new(id).unwrap().into(),
            direction: BlockDirection::Bidirectional,
            ui_hint: tutti_ump::UiHint::Both,
            midi1: tutti_ump::Midi1Port::No,
            first_group: FirstGroup::new(first_group).unwrap().into(),
            groups_spanned: GroupSpan::new(1).unwrap().into(),
            midi_ci_version: CiVersion::new(version).map(Into::into),
            max_sysex8_streams: Sysex8StreamCount::new(0),
        }),
        ms,
    );
}

/// The MUID and Group of each Discovery among what was sent.
fn discoveries(wire: &Wire) -> Vec<(MidiGroup, Muid)> {
    discoveries_of(ci_sent(wire))
}

fn discoveries_of(sent: Vec<(MidiGroup, CiMessage)>) -> Vec<(MidiGroup, Muid)> {
    sent.into_iter()
        .filter_map(|(group, m)| match m {
            CiMessage::Discovery { source, .. } => Some((group, source)),
            _ => None,
        })
        .collect()
}

/// A MIDI 1.0 port whose peer declared `categories` and whose first inquiry
/// went out; the wire's clock is at 50 ms. What was sent is read off.
pub(super) fn peer_declaring(categories: CiCategories) -> (Wire, Muid) {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data_of(categories),
        },
        40,
    );
    let found = polled(&mut wire, 50);
    assert!(
        matches!(found.as_slice(), [Polled::Ci(_, _, CiChange::Found(_))]),
        "{found:?}"
    );
    let _ = received(&wire);
    (wire, ours)
}

/// A peer found on `group` by the device whose MUID is `ours`.
fn find_peer(wire: &mut Wire, group: MidiGroup, ours: Muid, ms: u64) {
    say(
        wire,
        group,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data(),
        },
        ms,
    );
    let found = polled(wire, ms + 1);
    assert!(
        matches!(found.as_slice(), [Polled::Ci(_, _, CiChange::Found(_))]),
        "{found:?}"
    );
}

/// M2-101 §3.3.2: a block that starts declaring MIDI-CI after the settle is a
/// MIDI-CI Device from then on: its Group gets a Discovery, and the Group that
/// already had a device keeps it (nothing is sent for it).
#[test]
fn a_block_that_starts_declaring_midi_ci_after_the_settle_is_sent_discovery() {
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2), ci_block(1, 4, 0)]);
    let _ = received(&wire);
    block_info(&wire, 1, 4, 2, 200);
    let _ = polled(&mut wire, 210);
    let _ = polled(&mut wire, 220);
    let sent = discoveries(&wire);
    assert!(
        matches!(sent.as_slice(), [(g, _)] if *g == MidiGroup::of(4)),
        "one Discovery, on the new Group: {sent:?}"
    );
}

/// §3.3.2 and §5.9: a block that stops declaring MIDI-CI ends its Group's
/// device (an Invalidate MUID from and for its own MUID, its peers `Lost`),
/// while another Group's device and peers are untouched.
#[test]
fn a_block_that_stops_declaring_midi_ci_ends_its_groups_device_and_no_other() {
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2), ci_block(1, 4, 2)]);
    let muids: std::collections::HashMap<_, _> = discoveries(&wire).into_iter().collect();
    assert_eq!(muids.len(), 2);
    let (on2, on4) = (muids[&MidiGroup::of(2)], muids[&MidiGroup::of(4)]);
    find_peer(&mut wire, MidiGroup::of(2), on2, 200);
    find_peer(&mut wire, MidiGroup::of(4), on4, 220);
    let _ = received(&wire);

    block_info(&wire, 0, 2, 0, 300);
    // The poll that hears the change recomputes the Groups; the next acts.
    let mut all = polled(&mut wire, 310);
    all.extend(polled(&mut wire, 320));
    let lost: Vec<_> = all
        .iter()
        .filter_map(|p| match p {
            Polled::Ci(_, who, CiChange::Lost) => Some((who.group, who.muid)),
            _ => None,
        })
        .collect();
    assert_eq!(lost, [(MidiGroup::of(2), peer())]);
    let sent = ci_sent(&wire);
    assert!(
        matches!(
            sent.as_slice(),
            [(g, CiMessage::InvalidateMuid { header, target })]
                if *g == MidiGroup::of(2) && header.source == on2 && *target == on2
        ),
        "{sent:?}"
    );
    let peers = wire.session.ci_peers(&wire.id);
    assert_eq!(
        peers
            .iter()
            .map(|p| (p.group(), p.muid()))
            .collect::<Vec<_>>(),
        [(MidiGroup::of(4), peer())],
        "Group 4's peer is untouched"
    );
}

/// A block that moves to another Group is, by Group, an end and a start: the
/// old Group's device is invalidated and the new Group gets a Discovery under a
/// fresh MUID.
/// Spec: m2-101/3.3.2#2, m2-101/3.3.2#3
#[test]
fn a_block_that_moves_ends_the_old_groups_device_and_starts_the_new() {
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2)]);
    let old = discoveries(&wire)[0].1;
    let _ = received(&wire);
    block_info(&wire, 0, 6, 2, 200);
    let _ = polled(&mut wire, 210);
    let _ = polled(&mut wire, 220);
    let sent = ci_sent(&wire);
    assert!(
        sent.iter().any(|(g, m)| *g == MidiGroup::of(2)
            && matches!(m, CiMessage::InvalidateMuid { target, .. } if *target == old)),
        "{sent:?}"
    );
    let started = discoveries_of(sent);
    assert!(
        matches!(started.as_slice(), [(g, muid)] if *g == MidiGroup::of(6) && *muid != old),
        "{started:?}"
    );
}

/// §5.9 lists a shutdown among the reasons for an Invalidate MUID: closing an
/// input sends one for each running device's own MUID, on that device's
/// output, and reports no peer lost (the app closed it).
#[test]
fn disconnecting_an_input_sends_one_invalidate_muid_from_our_muid() {
    let (mut wire, ours) = midi1_running();
    find_peer(&mut wire, GROUP_0, ours, 40);
    let _ = received(&wire);
    wire.session
        .disconnect_input(&wire.id.clone())
        .expect("the input is open");
    let sent = ci_sent(&wire);
    assert!(
        matches!(
            sent.as_slice(),
            [(g, CiMessage::InvalidateMuid { header, target })]
                if *g == GROUP_0 && header.source == ours && *target == ours
        ),
        "{sent:?}"
    );
    assert!(
        polled(&mut wire, 100).is_empty(),
        "no Lost for a closed input"
    );
}

/// Every input is closed the same way, by `disconnect_all_inputs`.
#[test]
fn disconnecting_all_inputs_invalidates_too() {
    let (mut wire, ours) = midi1_running();
    wire.session.disconnect_all_inputs();
    let sent = ci_sent(&wire);
    assert!(
        matches!(sent.as_slice(), [(_, CiMessage::InvalidateMuid { target, .. })] if *target == ours),
        "{sent:?}"
    );
}

/// An input whose MIDI-CI never ran has no MUID to invalidate: closing it
/// sends nothing, enabled or not.
#[test]
fn closing_an_input_without_midi_ci_sends_nothing() {
    let mut wire = midi1_wire();
    let _ = polled(&mut wire, 0);
    let _ = received(&wire);
    wire.session
        .disconnect_input(&wire.id.clone())
        .expect("the input is open");
    assert!(ci_sent(&wire).is_empty());
}

/// The device's own word (here its Identity Reply) is its identity; a MIDI-CI
/// peer fills only what that left, so its Product Instance Id (M2-101
/// §5.8.3.1, learned by Endpoint Inquiry) is added and its Discovery identity,
/// which speaks for a Function Block, never replaces the device's.
#[test]
fn a_ci_peers_product_instance_id_fills_what_the_device_left() {
    use tutti_midi_ci::EndpointInfo;
    use tutti_ump::ProductInstanceId;

    let (mut wire, ours) = midi1_running();
    let mut data = peer_data();
    data.body.identity = model_identity(9);
    data.version_2 = Some(ReplyVersion2 {
        output_path_id: U7::of(0),
        function_block: Some(tutti_ump::FunctionBlockId::new(0).unwrap()),
    });
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data,
        },
        40,
    );
    assert!(matches!(
        polled(&mut wire, 50).as_slice(),
        [Polled::Ci(_, _, CiChange::Found(_))]
    ));
    let learned = wire.session.learned(&wire.id);
    assert_eq!(learned.identity(), Some(&identity()), "the device's own");
    assert_eq!(learned.instance_id(), None, "not asked yet");

    let serial = ProductInstanceId::new("SN-9").unwrap();
    say(
        &wire,
        GROUP_0,
        &CiMessage::EndpointReply {
            header: CiHeader::new(peer()),
            to: ours,
            info: EndpointInfo::ProductInstanceId(serial),
        },
        60,
    );
    assert!(matches!(
        polled(&mut wire, 70).as_slice(),
        [Polled::Ci(_, _, CiChange::Updated(_))]
    ));
    let learned = wire.session.learned(&wire.id);
    assert_eq!(learned.identity(), Some(&identity()));
    assert_eq!(
        learned.instance_id().map(ProductInstanceId::as_str),
        Some("SN-9")
    );
}

// --- Profile Configuration (M2-101 section 7, M2-102 section 2) -----------

use crate::{ChannelCount, ProfileError, ProfilesChange, Switcher};
use tutti_midi_ci::Whole;
use tutti_midi_ci::{
    PEER_REPLY_TIMEOUT, ProfileChange, ProfileChannels, ProfileFormat, ProfileId, ProfileList,
    ProfileLists, ProfileOutcome, ProfileRequest, ProfileState, ProfileTarget, StandardProfile,
};
use tutti_ump::MidiChannel;

fn profile(number: u8) -> ProfileId {
    ProfileId::Standard(StandardProfile::new(
        U7::of(0x21),
        U7::of(number),
        U7::of(1),
        U7::of(1),
    ))
}

fn channel(n: u8) -> CiDeviceId {
    CiDeviceId::Channel(MidiChannel::of(n))
}

fn on_channel(n: u8, channels: ProfileChannels) -> ProfileTarget {
    ProfileTarget::Channel {
        channel: MidiChannel::of(n),
        channels,
    }
}

/// The peer's Reply to Profile Inquiry at `address`.
fn reply_at_address(
    wire: &Wire,
    ours: Muid,
    address: CiDeviceId,
    enabled: Vec<ProfileId>,
    disabled: Vec<ProfileId>,
    ms: u64,
) {
    say(
        wire,
        GROUP_0,
        &CiMessage::ProfileReply {
            header: CiHeader {
                device_id: address,
                ..CiHeader::new(peer())
            },
            to: ours,
            profiles: ProfileLists::new(
                ProfileList::try_from(enabled).unwrap(),
                ProfileList::try_from(disabled).unwrap(),
            ),
        },
        ms,
    );
}

/// Channel 1 lists `enabled` and `disabled`, the Group lists `group`, and the
/// final Reply on the Function Block ends the inquiry; the wire's clock is at
/// 100 ms.
fn known_profiles(
    wire: &mut Wire,
    ours: Muid,
    enabled: Vec<ProfileId>,
    disabled: Vec<ProfileId>,
    group: Vec<ProfileId>,
) {
    reply_at_address(wire, ours, channel(0), enabled, disabled, 60);
    reply_at_address(wire, ours, CiDeviceId::Group, vec![], group, 70);
    reply_at_address(wire, ours, CiDeviceId::FunctionBlock, vec![], vec![], 80);
    let changes = polled(wire, 100);
    assert!(
        matches!(
            changes.as_slice(),
            [Polled::Profile(_, _, ProfilesChange::Known)]
        ),
        "{changes:?}"
    );
}

/// How the requests ended, in order, each naming the peer.
fn request_answers(polled: &[Polled]) -> Vec<&Answer> {
    polled
        .iter()
        .filter_map(|p| match p {
            Polled::Answer(_, who, answer) => {
                assert_eq!(*who, peer_id(), "every answer names the peer");
                Some(answer)
            }
            _ => None,
        })
        .collect()
}

fn profile_events(polled: &[Polled]) -> Vec<&ProfilesChange> {
    polled
        .iter()
        .filter_map(|p| match p {
            Polled::Profile(_, who, change) => {
                assert_eq!(*who, peer_id(), "every change names the peer");
                Some(change)
            }
            _ => None,
        })
        .collect()
}

/// M2-101 section 7.2: the Initiator asks a peer that declared Profile
/// Configuration for its Profiles, with a Profile Inquiry (Sub-ID#2 `0x20`) to
/// the Function Block (`0x7F`), from its own MUID to the peer's. A peer that
/// declared none is not asked.
/// Spec: m2-101/7.2#1, m2-101/2.2#4
#[test]
fn a_peer_that_declares_profile_configuration_is_sent_a_profile_inquiry() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data_of(CiCategories::PROFILE_CONFIGURATION),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one message, the Profile Inquiry: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x20]);
    assert_eq!(payload[5..9], ours.to_bytes(), "from our MUID");
    assert_eq!(payload[9..13], peer().to_bytes(), "to the peer's");

    // Another peer, which declares no category, is sent nothing.
    let other = Muid::of(0x0023_4567);
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: other,
            to: ours,
            data: peer_data(),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    assert!(ci_sent(&wire).is_empty());
    assert_eq!(wire.session.profiles(&wire.id).len(), 1);
}

/// M2-102 section 2.4: the peer answers a Function Block Profile Inquiry with a
/// Reply per address, and a final one on `0x7F` with no Profiles; that final
/// Reply makes the table the peer's whole declaration. Each Profile is listed
/// at the address it replied on, enabled or disabled as its list says, and a
/// Channel's is a handle that asks for a Channel count where a Group's has none.
#[test]
fn the_replies_to_profile_inquiry_become_the_peers_profiles_once_the_final_one_arrives() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    reply_at_address(
        &wire,
        ours,
        channel(0),
        vec![profile(1)],
        vec![profile(2)],
        60,
    );
    let _ = polled(&mut wire, 65);
    assert!(
        !wire.session.profiles(&wire.id).iter().any(|p| p.is_known()),
        "not known before the final Reply"
    );
    reply_at_address(&wire, ours, CiDeviceId::Group, vec![], vec![profile(3)], 70);
    reply_at_address(&wire, ours, CiDeviceId::FunctionBlock, vec![], vec![], 80);
    let changes = polled(&mut wire, 100);
    assert_eq!(profile_events(&changes), [&ProfilesChange::Known]);
    let peers = wire.session.profiles(&wire.id);
    let [held] = &peers[..] else {
        panic!("one peer: {peers:?}");
    };
    assert!(held.is_known());
    assert_eq!(
        (held.at().peer().group, held.at().peer().muid),
        (GROUP_0, peer())
    );
    let listed: Vec<_> = held.listed().collect();
    let [
        Switcher::Channel(one),
        Switcher::Channel(two),
        Switcher::Whole(three),
    ] = listed.as_slice()
    else {
        panic!("two on Channel 1 and one on the Group: {listed:?}");
    };
    assert_eq!(one.channel(), MidiChannel::of(0));
    assert_eq!(
        (listed[0].profile(), listed[0].state()),
        (profile(1), ProfileState::Enabled)
    );
    assert_eq!(
        (listed[1].profile(), listed[1].state()),
        (profile(2), ProfileState::Disabled)
    );
    assert_eq!(two.channel(), MidiChannel::of(0));
    assert_eq!(three.whole(), Whole::Group);
    assert_eq!(listed[2].state(), ProfileState::Disabled);
}

/// M2-101 section 7.8 and Table 24: a Set Profile On to a Channel goes to that
/// Channel's Device ID with Number Channels Requested (Message Version 2, two
/// bytes, LSB first); the Profile Enabled Report of section 7.10 is its
/// acknowledgement, resolving the request with the state and Channel count the
/// peer reports and updating what the session holds.
/// Spec: m2-101/7.8#1
#[test]
fn a_set_profile_on_names_the_channel_and_its_enabled_report_updates_the_table() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let [Switcher::Channel(c)] = listed.as_slice() else {
        panic!("one Channel Profile: {listed:?}");
    };
    let _ = received(&wire);
    wire.session
        .set_profile(c.enable(ChannelCount::new(3).unwrap()))
        .unwrap();
    assert!(received(&wire).is_empty(), "sent at the next poll");
    let none = polled(&mut wire, 110);
    assert!(none.is_empty(), "{none:?}");
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one Set Profile On: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    // F0 7E <Channel 1> 0D 22 <version> <source> <destination> <Profile Id> <count LSB first>
    assert_eq!(payload[..4], [0x7E, 0x00, 0x0D, 0x22]);
    assert_eq!(payload[5..9], ours.to_bytes());
    assert_eq!(payload[9..13], peer().to_bytes());
    assert_eq!(payload[13..18], profile(2).to_bytes());
    assert_eq!(payload[18..], [3, 0]);

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileEnabled {
            source: peer(),
            format: ProfileFormat::current(on_channel(0, ProfileChannels::of(3))),
            profile: profile(2),
        },
        120,
    );
    let changes = polled(&mut wire, 130);
    assert_eq!(
        profile_events(&changes),
        [&ProfilesChange::Changed {
            address: channel(0),
            profile: profile(2),
            change: ProfileChange::Enabled,
        },]
    );
    assert_eq!(
        request_answers(&changes),
        [&Answer::Switch {
            request: ProfileRequest::SetOn {
                address: channel(0),
                profile: profile(2)
            },
            outcome: ProfileOutcome::Reported {
                state: ProfileState::Enabled,
                channels: Some(ProfileChannels::of(3)),
            },
        }]
    );
    let peers = wire.session.profiles(&wire.id);
    let entry = peers[0].table().get(channel(0), profile(2)).unwrap();
    assert_eq!(entry.state(), ProfileState::Enabled);
    assert_eq!(entry.channels(), Some(ProfileChannels::of(3)));
}

/// M2-120 section 3.2 (Table 4) names the MPE Profile, and the peer's Enabled
/// Report (appendix A.4) gives the Number of Channels, the Manager and its
/// Members: that is the zone the session reads. A disabled Profile, one
/// enabled with a single Channel (a Manager with no Member), and one on the
/// Group are no zone.
#[test]
fn an_enabled_mpe_profile_is_a_zone_of_the_channels_its_report_gave() {
    use tutti_midi_protocol::{PitchBendSensitivity, ProfileZone};
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![ProfileId::MPE], vec![]);
    let zones = |wire: &Wire| wire.session.profiles(&wire.id)[0].mpe_zones();
    assert_eq!(zones(&wire), None, "supported, not enabled");
    for (channels, zone) in [
        (
            5,
            Some(ProfileZone::new(
                MpeZoneSpan::new(MidiChannel::of(0), MpeChannels::of(5)).unwrap(),
                PitchBendSensitivity::MPE_PROFILE_DEFAULT,
            )),
        ),
        (1, None),
        (
            16,
            Some(ProfileZone::new(
                MpeZoneSpan::new(MidiChannel::of(0), MpeChannels::of(16)).unwrap(),
                PitchBendSensitivity::MPE_PROFILE_DEFAULT,
            )),
        ),
    ] {
        say(
            &wire,
            GROUP_0,
            &CiMessage::ProfileEnabled {
                source: peer(),
                format: ProfileFormat::current(on_channel(0, ProfileChannels::of(channels))),
                profile: ProfileId::MPE,
            },
            120,
        );
        let _ = polled(&mut wire, 130);
        assert_eq!(
            zones(&wire),
            zone.map(tutti_midi_protocol::ProfileZones::single),
            "{channels} channels"
        );
    }
    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileDisabled {
            source: peer(),
            format: ProfileFormat::current(on_channel(0, ProfileChannels::of(16))),
            profile: ProfileId::MPE,
        },
        140,
    );
    let _ = polled(&mut wire, 150);
    assert_eq!(zones(&wire), None, "disabled again");
}

/// **An output sends MPE to a device once its Enabled Report makes a zone, and
/// stops at its Disabled Report** (M2-120 section 3.3), whether the output was
/// connected before or after. A note on Channel 1, the Manager, goes out on
/// Channel 2, the first Member.
#[test]
fn an_output_sends_mpe_while_its_device_has_the_profile_enabled() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![ProfileId::MPE], vec![]);
    let format = || ProfileFormat::current(on_channel(0, ProfileChannels::of(3)));
    let note_channel = |wire: &mut Wire| {
        let _ = received(wire);
        let note = tutti_fixtures::midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
        let off = tutti_fixtures::midi::note_off(MidiChannel::FIRST, 60, 0);
        wire.send(&[(Offset::ZERO, note), (Offset::ZERO, off)]);
        received(wire)
            .iter()
            .find_map(|ump| match ump.decode() {
                Ok(Message::Midi1(a, tutti_ump::Voice1::NoteOn { .. })) => Some(a.channel.get()),
                _ => None,
            })
            .expect("a Note On")
    };
    wire.connect_output();
    assert_eq!(note_channel(&mut wire), 0, "not enabled");
    let enabled = CiMessage::ProfileEnabled {
        source: peer(),
        format: format(),
        profile: ProfileId::MPE,
    };
    say(&wire, GROUP_0, &enabled, 120);
    let _ = wire.poll(130);
    assert_eq!(note_channel(&mut wire), 1, "enabled");
    wire.connect_output();
    assert_eq!(note_channel(&mut wire), 1, "connected while enabled");
    let disabled = CiMessage::ProfileDisabled {
        source: peer(),
        format: format(),
        profile: ProfileId::MPE,
    };
    // A device that left MPE and is in it again stopped the note it played.
    let off = tutti_fixtures::midi::note_off(MidiChannel::FIRST, 60, 0);
    let on = tutti_fixtures::midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    wire.send(&[(Offset::ZERO, on)]);
    say(&wire, GROUP_0, &disabled, 140);
    say(&wire, GROUP_0, &enabled, 141);
    let _ = wire.poll(145);
    let _ = received(&wire);
    wire.send(&[(Offset::ZERO, off)]);
    assert_eq!(received(&wire), [], "its note is forgotten");
    say(&wire, GROUP_0, &disabled, 146);
    let _ = wire.poll(150);
    assert_eq!(note_channel(&mut wire), 0, "disabled");
}

/// M2-120 section 3.4: a zone that shares a Channel with a lower one is not a
/// zone of the peer, and the session says so rather than leaving it out in
/// silence: a change names the zone and why.
#[test]
fn an_mpe_zone_sharing_channels_with_a_lower_one_is_reported_refused() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![ProfileId::MPE], vec![]);
    let enable = |wire: &Wire, manager: u8, channels: u8, ms: u64| {
        say(
            wire,
            GROUP_0,
            &CiMessage::ProfileEnabled {
                source: peer(),
                format: ProfileFormat::current(on_channel(
                    manager,
                    ProfileChannels::of(channels.into()),
                )),
                profile: ProfileId::MPE,
            },
            ms,
        );
    };
    enable(&wire, 0, 5, 120);
    let first = polled(&mut wire, 130);
    let refused = |polled: &[Polled]| {
        polled
            .iter()
            .filter_map(|p| match p {
                Polled::Profile(_, _, ProfilesChange::MpeZoneRefused(overlap)) => Some(*overlap),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(refused(&first), [], "one zone is refused by none");
    enable(&wire, 3, 4, 140);
    let second = polled(&mut wire, 150);
    let span = |manager: u8, channels: u8| {
        MpeZoneSpan::new(MidiChannel::of(manager), MpeChannels::of(channels)).unwrap()
    };
    assert_eq!(
        refused(&second),
        [MpeOverlap {
            held: span(0, 5),
            added: span(3, 4)
        }]
    );
}

/// M2-120 section 3.6.3: the Channel Response Type Notification is Profile
/// Specific Data of the MPE Profile on the zone's Manager Channel, one byte.
/// It reaches the app as a change, read as a [`ChannelResponse`]; the same
/// data of another Profile, or one that is not a notification, is not one.
#[test]
fn a_channel_response_notification_is_read_off_the_specific_data() {
    use tutti_midi_ci::{Destination, ProfileSpecific, SpecificData};
    use tutti_midi_protocol::ChannelResponse;
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![ProfileId::MPE], vec![], vec![]);
    let data = |profile: ProfileId, bytes: &[u8]| {
        CiMessage::ProfileSpecificData(
            ProfileSpecific::new(
                CiHeader {
                    device_id: channel(3),
                    ..CiHeader::new(peer())
                },
                Destination::Muid(ours),
                profile,
                SpecificData::new(bytes.to_vec()).unwrap(),
            )
            .unwrap(),
        )
    };
    let mut read = Vec::new();
    for (ms, message) in [
        (120, data(ProfileId::MPE, &[0x01])),
        (130, data(ProfileId::MPE, &[0x00])),
        (140, data(ProfileId::MPE, &[0x02])),
        (150, data(profile(2), &[0x01])),
    ] {
        say(&wire, GROUP_0, &message, ms);
        let changes = polled(&mut wire, ms + 5);
        let events = profile_events(&changes);
        let [change] = events.as_slice() else {
            panic!("one change: {events:?}");
        };
        assert!(matches!(change, ProfilesChange::SpecificData { .. }));
        read.push(change.mpe_channel_response());
    }
    assert_eq!(
        read,
        [
            Some((MidiChannel::of(3), ChannelResponse::Monophonic)),
            Some((MidiChannel::of(3), ChannelResponse::Polyphonic)),
            None,
            None
        ]
    );
}

/// M2-101 section 7.8's note and 7.9: on the Group or the Function Block the
/// Number Channels Requested is 0x0000 (the width of what is addressed), and a
/// Set Profile Off (Sub-ID#2 `0x23`, Table 25) names only the Profile; the Disabled
/// Report of section 7.11 resolves it.
/// Spec: m2-101/7.8#1, m2-101/7.9#1
#[test]
fn a_set_profile_on_a_group_asks_for_no_channels_and_off_is_resolved_by_the_disabled_report() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![], vec![profile(3)]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let [Switcher::Whole(whole)] = listed.as_slice() else {
        panic!("one Group Profile: {listed:?}");
    };
    let _ = received(&wire);
    wire.session.set_profile(whole.enable()).unwrap();
    let _ = polled(&mut wire, 110);
    let sent = payloads(&received(&wire));
    let [(_, on)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(on[..4], [0x7E, 0x7E, 0x0D, 0x22], "to the Group");
    assert_eq!(on[18..], [0, 0], "no Channels requested");

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileEnabled {
            source: peer(),
            format: ProfileFormat::current(ProfileTarget::Group),
            profile: profile(3),
        },
        120,
    );
    let _ = polled(&mut wire, 130);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    wire.session.set_profile(listed[0].disable()).unwrap();
    let _ = polled(&mut wire, 140);
    let sent = payloads(&received(&wire));
    let [(_, off)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(off[..4], [0x7E, 0x7E, 0x0D, 0x23]);
    assert_eq!(off[13..18], profile(3).to_bytes());
    assert_eq!(off[18..], [0, 0], "the two reserved bytes of Version 2");

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileDisabled {
            source: peer(),
            format: ProfileFormat::current(ProfileTarget::Group),
            profile: profile(3),
        },
        150,
    );
    let changes = polled(&mut wire, 160);
    assert!(
        request_answers(&changes).iter().any(|a| matches!(
            a,
            Answer::Switch {
                request: ProfileRequest::SetOff { .. },
                outcome: ProfileOutcome::Reported {
                    state: ProfileState::Disabled,
                    ..
                },
            }
        )),
        "{changes:?}"
    );
    assert_eq!(
        wire.session.profiles(&wire.id)[0]
            .table()
            .get(CiDeviceId::Group, profile(3))
            .unwrap()
            .state(),
        ProfileState::Disabled
    );
}

/// M2-101 sections 7.4 and 7.5: a Profile Added Report lists a Profile (disabled)
/// and a Removed Report takes it out, each reported. A handle made before the
/// removal is then refused, since the peer no longer lists what it names.
/// Spec: m2-102/2.4#5, m2-102/2.4#6
#[test]
fn added_and_removed_reports_change_the_list_and_a_stale_handle_is_refused() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let stale: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileAdded {
            header: CiHeader {
                device_id: channel(0),
                ..CiHeader::new(peer())
            },
            profile: profile(4),
        },
        110,
    );
    let changes = polled(&mut wire, 120);
    assert!(
        matches!(
            profile_events(&changes).as_slice(),
            [ProfilesChange::Changed { profile: p, change: ProfileChange::Added, .. }] if *p == profile(4)
        ),
        "{changes:?}"
    );
    assert_eq!(wire.session.profiles(&wire.id)[0].listed().count(), 2);

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileRemoved {
            header: CiHeader {
                device_id: channel(0),
                ..CiHeader::new(peer())
            },
            profile: profile(2),
        },
        130,
    );
    let changes = polled(&mut wire, 140);
    assert!(
        matches!(
            profile_events(&changes).as_slice(),
            [ProfilesChange::Changed { profile: p, change: ProfileChange::Removed, .. }] if *p == profile(2)
        ),
        "{changes:?}"
    );
    let Switcher::Channel(old) = &stale[0] else {
        panic!("a Channel Profile");
    };
    assert_eq!(
        wire.session.set_profile(old.enable(ChannelCount::ONE)),
        Err(ProfileError::NotListed)
    );
}

/// M2-101 section 5.9: an Invalidate MUID for the peer discards it, and with it
/// its Initiator and the Profiles it held: they are reported dropped, the
/// session holds none, and a handle made before is refused, as is a switch
/// queued before it that the next poll would have sent.
/// Spec: m2-101/5.9#3
#[test]
fn an_invalidate_muid_drops_the_peers_profiles_and_refuses_its_handles() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let Switcher::Channel(c) = &listed[0] else {
        panic!("a Channel Profile");
    };
    wire.session
        .set_profile(c.enable(ChannelCount::ONE))
        .unwrap();
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(Muid::of(0x00AB_CDEF)),
            target: peer(),
        },
        110,
    );
    let changes = polled(&mut wire, 120);
    assert!(
        profile_events(&changes).contains(&&ProfilesChange::Dropped),
        "{changes:?}"
    );
    assert!(wire.session.profiles(&wire.id).is_empty());
    assert!(
        payloads(&received(&wire)).iter().all(|(_, p)| p[3] != 0x22),
        "the queued switch was not sent to a peer that is gone"
    );
    assert_eq!(
        wire.session.set_profile(c.enable(ChannelCount::ONE)),
        Err(ProfileError::Peer(PeerRequestError::NoSuchPeer))
    );
}

/// M2-102 gives an Initiator no time limit; tutti's is [`PEER_REPLY_TIMEOUT`]. A
/// Set Profile On the peer never answers ends, and is reported, once that long
/// has passed since it was sent and not before; the table is left as it was.
#[test]
fn a_set_profile_nothing_answers_times_out_after_the_profile_timeout() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let Switcher::Channel(c) = &listed[0] else {
        panic!("a Channel Profile");
    };
    wire.session
        .set_profile(c.enable(ChannelCount::ONE))
        .unwrap();
    let sent_at = 200;
    let _ = polled(&mut wire, sent_at);
    let wait = PEER_REPLY_TIMEOUT.as_millis() as u64;
    let early = polled(&mut wire, sent_at + wait - 1);
    assert!(request_answers(&early).is_empty(), "{early:?}");
    let ended = polled(&mut wire, sent_at + wait);
    assert!(
        matches!(
            request_answers(&ended).as_slice(),
            [Answer::Switch {
                outcome: ProfileOutcome::TimedOut,
                ..
            }]
        ),
        "{ended:?}"
    );
    assert_eq!(
        wire.session.profiles(&wire.id)[0]
            .table()
            .get(channel(0), profile(2))
            .unwrap()
            .state(),
        ProfileState::Disabled
    );
}

/// A peer that answers only part of the Profile Inquiry, never the final Reply,
/// is given up on after [`PEER_REPLY_TIMEOUT`]: it is reported, the table keeps
/// what was heard, and it is not known to be whole.
#[test]
fn an_inquiry_without_a_final_reply_times_out_keeping_what_was_heard() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    reply_at_address(&wire, ours, channel(0), vec![profile(1)], vec![], 60);
    let _ = polled(&mut wire, 70);
    let wait = PEER_REPLY_TIMEOUT.as_millis() as u64;
    let ended = polled(&mut wire, 50 + wait);
    assert_eq!(profile_events(&ended), [&ProfilesChange::InquiryTimedOut]);
    let peers = wire.session.profiles(&wire.id);
    assert!(!peers[0].is_known());
    assert_eq!(peers[0].listed().count(), 1);
}

/// A second request for a Profile at an address whose first is still waiting is
/// not sent: it has no answer of its own, and only one Set Profile On went out.
#[test]
fn a_request_while_one_for_that_profile_waits_is_not_sent() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let Switcher::Channel(c) = &listed[0] else {
        panic!("a Channel Profile");
    };
    let _ = received(&wire);
    for _ in 0..2 {
        wire.session
            .set_profile(c.enable(ChannelCount::ONE))
            .unwrap();
    }
    let changes = polled(&mut wire, 110);
    assert!(changes.is_empty(), "{changes:?}");
    let sent = payloads(&received(&wire));
    let [(_, payload)] = sent.as_slice() else {
        panic!("one Set Profile On: {sent:?}");
    };
    assert_eq!(payload[..4], [0x7E, 0x00, 0x0D, 0x22]);
    assert_eq!(payload[13..18], profile(2).to_bytes());
    assert_eq!(payload[18..], [1, 0], "one Channel, as the first asked");
}

/// A count of Channels is at least 1 and fits 14 bits (M2-101 section 7.8).
#[test]
fn a_channel_count_is_at_least_one_and_fits_fourteen_bits() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: ChannelCount::new,
        get: ChannelCount::get,
        of: Some(ChannelCount::of),
        valid: &[1, 3, 16_383],
        invalid: &[0, 16_384, u16::MAX],
    });
    assert_eq!(ChannelCount::new(1), Some(ChannelCount::ONE));
}

use crate::{PeerRequestError, ProcessChange};
use tutti_midi_ci::{
    ChannelControllers, DataControl, Nak, NoteData, ProcessFeatures, ReportRequest, ReportScope,
    SUB_ID2_MESSAGE_REPORT_INQUIRY, SystemMessages,
};

fn process_events(polled: &[Polled]) -> Vec<&ProcessChange> {
    polled
        .iter()
        .filter_map(|p| match p {
            Polled::Process(_, who, change) => {
                assert_eq!(*who, peer_id(), "every change names the peer");
                Some(change)
            }
            _ => None,
        })
        .collect()
}

/// The peer answers the capabilities inquiry with `features`; the clock is at
/// 70 ms.
fn answer_capabilities(wire: &mut Wire, ours: Muid, features: ProcessFeatures) {
    say(
        wire,
        GROUP_0,
        &CiMessage::ProcessCapabilitiesReply {
            source: peer(),
            ci_version: CI_VERSION,
            to: ours,
            features,
        },
        60,
    );
    let changes = polled(wire, 70);
    assert_eq!(
        process_events(&changes),
        [&ProcessChange::Capabilities(features)]
    );
}

fn function_block_request() -> ReportRequest {
    ReportRequest::new(
        DataControl::NonDefault,
        ReportScope::FunctionBlock {
            system: SystemMessages::SONG_POSITION,
            controllers: ChannelControllers::CONTROL_CHANGE,
            notes: NoteData::NOTES,
        },
    )
}

/// M2-101 section 9.2 and Table 7: the Initiator asks a peer that declared
/// Process Inquiry for its capabilities, with an Inquiry: Process Inquiry
/// Capabilities (Sub-ID#2 `0x40`) to the Function Block (`0x7F`), from its own
/// MUID to the peer's. A peer that declared none is not asked.
/// Spec: m2-101/9.2#1
#[test]
fn a_peer_that_declares_process_inquiry_is_sent_a_capabilities_inquiry() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data_of(CiCategories::PROCESS_INQUIRY),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one message, the capabilities inquiry: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    // F0 7E 7F 0D 40 <version> <source> <destination> F7
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x40]);
    assert_eq!(payload[5..9], ours.to_bytes(), "from our MUID");
    assert_eq!(payload[9..13], peer().to_bytes(), "to the peer's");
    assert_eq!(payload.len(), 13);
    assert!(
        wire.session.report_peers(&wire.id).is_empty(),
        "not yet answered"
    );

    // A peer that declares no category is sent nothing.
    let other = Muid::of(0x0023_4567);
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: other,
            to: ours,
            data: peer_data(),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    assert!(ci_sent(&wire).is_empty());
}

/// Table 42: a reply without D0, MIDI Message Report, leaves the peer one a report
/// cannot be asked of: it has no handle.
#[test]
fn a_peer_whose_capabilities_lack_midi_message_report_has_no_handle() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::empty());
    assert!(wire.session.report_peers(&wire.id).is_empty());
}

/// M2-101 section 9.5: the Inquiry: MIDI Message Report (Sub-ID#2 `0x42`) carries
/// the Message Data Control and the bitmaps of Table 43, on the Device ID of its
/// scope. Its Reply (`0x43`) and End (`0x44`) are reported as they arrive, and
/// the transaction is over.
#[test]
fn a_report_asks_with_table_43_and_ends_with_the_peers_end() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let listed = wire.session.report_peers(&wire.id);
    let [handle] = &listed[..] else {
        panic!("one reporter: {listed:?}");
    };
    assert_eq!(
        (handle.at().peer().group, handle.at().peer().muid),
        (GROUP_0, peer())
    );
    let _ = received(&wire);

    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    assert!(received(&wire).is_empty(), "sent at the next poll");
    let none = polled(&mut wire, 110);
    assert!(none.is_empty(), "{none:?}");
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one Inquiry: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x42]);
    assert_eq!(payload[5..9], ours.to_bytes());
    assert_eq!(payload[9..13], peer().to_bytes());
    // Control 0x01, system D1, the reserved byte, controllers D1, notes D0.
    assert_eq!(payload[13..], [0x01, 0b10, 0x00, 0b10, 0b1]);

    let declared = ReportScope::FunctionBlock {
        system: SystemMessages::empty(),
        controllers: ChannelControllers::CONTROL_CHANGE,
        notes: NoteData::NOTES,
    };
    say(
        &wire,
        GROUP_0,
        &CiMessage::MessageReportReply {
            source: peer(),
            ci_version: CI_VERSION,
            to: ours,
            scope: declared,
        },
        120,
    );
    let began = polled(&mut wire, 130);
    assert!(began.is_empty(), "the Reply is not an answer: {began:?}");
    say(
        &wire,
        GROUP_0,
        &CiMessage::MessageReportEnd {
            header: CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ..CiHeader::new(peer())
            },
            to: ours,
        },
        140,
    );
    let ended = polled(&mut wire, 150);
    assert_eq!(request_answers(&ended), [&Answer::Report(ReportEnd::Ended)]);
    // The transaction is over: another may be asked for.
    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    let again = polled(&mut wire, 160);
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(payloads(&received(&wire)).len(), 1);
}

/// M2-101 section 9.8.1: a NAK ends a report even with no End.
/// Spec: m2-101/9.8.1#2
#[test]
fn a_nak_of_a_report_ends_it_and_is_reported() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    let _ = polled(&mut wire, 110);
    let nak = Nak::new(
        U7::of(SUB_ID2_MESSAGE_REPORT_INQUIRY),
        tutti_midi_ci::NakStatus::Terminate(tutti_midi_ci::PeTerminator::Responder),
    );
    say(
        &wire,
        GROUP_0,
        &CiMessage::Nak {
            header: CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ..CiHeader::new(peer())
            },
            to: ours,
            nak: nak.clone(),
        },
        120,
    );
    let ended = polled(&mut wire, 130);
    assert_eq!(
        request_answers(&ended),
        [&Answer::Report(ReportEnd::Refused(nak))]
    );
}

/// A report whose Reply never comes ends once the process timeout has passed
/// since it was sent, and not before.
#[test]
fn a_report_nothing_answers_times_out_after_the_process_timeout() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    let sent_at = 200;
    let _ = polled(&mut wire, sent_at);
    let wait = PEER_REPLY_TIMEOUT.as_millis() as u64;
    let early = polled(&mut wire, sent_at + wait - 1);
    assert!(request_answers(&early).is_empty(), "{early:?}");
    let ended = polled(&mut wire, sent_at + wait);
    assert_eq!(
        request_answers(&ended),
        [&Answer::Report(ReportEnd::TimedOut)]
    );
}

/// A second request while one waits is not sent: it has no answer of its own,
/// and only one Inquiry went out.
#[test]
fn a_report_while_one_waits_is_not_sent() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    let _ = received(&wire);
    for _ in 0..2 {
        wire.session
            .report(handle.ask(function_block_request()))
            .unwrap();
    }
    let changes = polled(&mut wire, 110);
    assert!(changes.is_empty(), "{changes:?}");
    let sent = payloads(&received(&wire));
    let [(_, payload)] = sent.as_slice() else {
        panic!("one Inquiry: {sent:?}");
    };
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x42]);
    assert_eq!(payload[13..], [0x01, 0b10, 0x00, 0b10, 0b1]);
}

/// A lost peer's handle is refused, and the peer's Initiator is reported dropped.
#[test]
fn a_lost_peer_drops_its_process_inquiry_and_refuses_its_handles() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(peer()),
            target: peer(),
        },
        80,
    );
    let changes = polled(&mut wire, 90);
    assert!(
        process_events(&changes).contains(&&ProcessChange::Dropped),
        "{changes:?}"
    );
    assert!(wire.session.report_peers(&wire.id).is_empty());
    assert_eq!(
        wire.session.report(handle.ask(function_block_request())),
        Err(PeerRequestError::NoSuchPeer)
    );
}

/// M2-101 section 9.3 and 9.6: the session declares Process Inquiry, and answers a
/// capabilities inquiry with D0, and a report of the Function Block with an empty
/// Reply (`43`, all bitmaps 0) and the End (`44`) on the same Device ID.
/// Spec: m2-101/9.3#1, m2-101/9.6#1, m2-101/9.8#1
#[test]
fn the_session_answers_a_process_inquiry_as_a_device_with_nothing_to_report() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::ProcessCapabilitiesInquiry {
            source: peer(),
            ci_version: CI_VERSION,
            to: ours,
        },
        40,
    );
    say(
        &wire,
        GROUP_0,
        &CiMessage::MessageReportInquiry {
            source: peer(),
            ci_version: CI_VERSION,
            to: ours,
            control: DataControl::Full,
            scope: ReportScope::Group {
                controllers: ChannelControllers::all(),
                notes: NoteData::all(),
            },
        },
        45,
    );
    let _ = polled(&mut wire, 50);
    let sent = payloads(&received(&wire));
    let [(_, caps), (_, begin), (_, end)] = sent.as_slice() else {
        panic!("a reply, a Reply and an End: {sent:?}");
    };
    assert_eq!(caps[..4], [0x7E, 0x7F, 0x0D, 0x41]);
    assert_eq!(caps[13..], [0x01]);
    assert_eq!(begin[..4], [0x7E, 0x7E, 0x0D, 0x43]);
    assert_eq!(begin[9..13], peer().to_bytes());
    assert_eq!(begin[13..], [0, 0, 0, 0]);
    assert_eq!(end[..4], [0x7E, 0x7E, 0x0D, 0x44]);
    assert_eq!(end.len(), 13);
}

// ---- Property Exchange ----

use crate::{PropertyCall, PropertyChange};
use tutti_midi_ci::{
    BodyLimit, BufferLimit, PE_TIMEOUT, PeCapabilities, PeCapabilitiesFailure, PeCiMessage, PeData,
    PeLocalOn, PeReassemblyLimits, PeSettings, ResourceEntry, ResourceEvent, ResourceGet,
    ResourceInitiator, ResourceResponder, ResourceStore, ResourceTarget, ResponderSettings,
    SimultaneousRequests, StandardResource, StreamLimit,
};

fn property_events(polled: &[Polled]) -> Vec<&PropertyChange> {
    polled
        .iter()
        .filter_map(|p| match p {
            Polled::Property(_, who, change) => {
                assert_eq!(*who, peer_id(), "every change names the peer");
                Some(change)
            }
            _ => None,
        })
        .collect()
}

fn property_answers(polled: &[Polled]) -> Vec<&Answer> {
    polled
        .iter()
        .filter_map(|p| match p {
            Polled::Answer(_, who, answer) => {
                assert_eq!(*who, peer_id(), "every answer names the peer");
                Some(answer)
            }
            _ => None,
        })
        .collect()
}

fn pe_limits() -> PeReassemblyLimits {
    PeReassemblyLimits {
        streams: StreamLimit::of(8),
        bytes: BufferLimit::of(1 << 16),
    }
}

fn local_on(on: bool) -> ResourceStore {
    let mut store = ResourceStore::new();
    store.declare(
        ResourceEntry::standard(StandardResource::LocalOn)
            .build()
            .unwrap(),
    );
    store
        .put(None, PeData::LocalOn(PeLocalOn(on)))
        .expect("declared");
    store
}

/// The far end of a Property Exchange: a Responder serving `store`, and, when
/// the test needs one, an Initiator that asks the session.
struct Far {
    resp: ResourceResponder,
    store: ResourceStore,
    init: Option<ResourceInitiator>,
    events: Vec<ResourceEvent>,
}

impl Far {
    fn new(ours: Muid, store: ResourceStore, asks: Option<Instant>) -> (Self, Vec<CiMessage>) {
        let mut resp = ResourceResponder::new(
            peer(),
            ResponderSettings {
                capabilities: PeCapabilities::new(SimultaneousRequests::of(4)),
                reassembly: pe_limits(),
                body_limit: BodyLimit::of(1 << 16),
            },
        );
        resp.accept(ours, MaxSysexSize::of(512));
        let mut first = Vec::new();
        let init = asks.map(|now| {
            let (init, actions) = ResourceInitiator::new(
                peer(),
                ours,
                PeSettings {
                    capabilities: PeCapabilities::new(SimultaneousRequests::of(4)),
                    peer_max_sysex: MaxSysexSize::of(512),
                    reassembly: pe_limits(),
                    body_limit: BodyLimit::of(1 << 16),
                },
                now,
            );
            first = Self::sends_of(actions);
            init
        });
        (
            Self {
                resp,
                store,
                init,
                events: Vec::new(),
            },
            first,
        )
    }

    fn sends_of(actions: Vec<tutti_midi_ci::ResourceAction>) -> Vec<CiMessage> {
        actions
            .into_iter()
            .filter_map(|a| match a {
                tutti_midi_ci::ResourceAction::Send(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    /// What the far end says to a message of the session.
    fn hear(&mut self, message: &CiMessage, now: Instant) -> Vec<CiMessage> {
        let decoded = Decoded::Message(message.clone());
        let mut said: Vec<CiMessage> = self
            .resp
            .on_message(&mut self.store, &decoded, now)
            .into_iter()
            .filter_map(|a| match a {
                tutti_midi_ci::ResponderAction::Send(m) => Some(m),
                _ => None,
            })
            .collect();
        if let Some(init) = &mut self.init {
            let actions = init.on_message(&decoded, now);
            for action in &actions {
                if let tutti_midi_ci::ResourceAction::Event(e) = action {
                    self.events.push((**e).clone());
                }
            }
            said.extend(Self::sends_of(actions));
        }
        said
    }
}

/// The far end and the session trade messages until neither has more to say,
/// from `ms` on in steps of 2 ms; every poll's reports are returned.
fn pump(wire: &mut Wire, far: &mut Far, mut said: Vec<CiMessage>, mut ms: u64) -> Vec<Polled> {
    let mut all = Vec::new();
    for _ in 0..40 {
        for message in said.drain(..) {
            say(wire, GROUP_0, &message, ms);
        }
        ms += 2;
        all.extend(polled(wire, ms));
        let now = wire.at(ms);
        for (_, message) in ci_sent(wire) {
            said.extend(far.hear(&message, now));
        }
        if said.is_empty() {
            break;
        }
    }
    all
}

/// A peer that declared Property Exchange, found at 40 ms; the Capabilities
/// Inquiry the session sent is returned for the far end to hear.
fn property_peer() -> (Wire, Muid, Vec<CiMessage>) {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: ours,
            data: peer_data_of(CiCategories::PROPERTY_EXCHANGE),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    let sent = ci_sent(&wire).into_iter().map(|(_, m)| m).collect();
    (wire, ours, sent)
}

/// The far end hears what the session sent, and what it answers is returned.
fn answers(far: &mut Far, heard: &[CiMessage], ms: u64, wire: &Wire) -> Vec<CiMessage> {
    heard
        .iter()
        .flat_map(|message| far.hear(message, wire.at(ms)))
        .collect()
}

/// M2-101 section 8.5: a peer that declared Property Exchange (Table 7, D3) is
/// sent Inquiry: Property Exchange Capabilities (Sub-ID#2 `0x30`) from our MUID
/// to its own, and has no handle until it answers.
/// Spec: m2-101/8.5#1
#[test]
fn a_peer_that_declares_property_exchange_is_sent_a_capabilities_inquiry() {
    let (wire, ours, sent) = property_peer();
    let [
        CiMessage::PropertyExchange {
            header,
            to,
            message,
        },
    ] = sent.as_slice()
    else {
        panic!("one message, the capabilities inquiry: {sent:?}");
    };
    assert_eq!(header.source, ours);
    assert_eq!(*to, peer());
    assert!(matches!(message, PeCiMessage::CapabilitiesInquiry(_)));
    assert!(wire.session.property_peers(&wire.id).is_empty());
}

/// The session declares Property Exchange, so a peer's Capabilities Inquiry is
/// answered with the Capabilities Reply (`0x31`) and not refused.
/// Spec: m2-101/8.6#1
#[test]
fn the_session_answers_a_property_exchange_capabilities_inquiry_and_does_not_nak_it() {
    let (mut wire, ours, _) = property_peer();
    say(
        &wire,
        GROUP_0,
        &CiMessage::PropertyExchange {
            header: CiHeader::new(peer()),
            to: ours,
            message: PeCiMessage::CapabilitiesInquiry(PeCapabilities::new(
                SimultaneousRequests::of(2),
            )),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    let sent = ci_sent(&wire);
    assert!(
        matches!(
            sent.as_slice(),
            [(
                _,
                CiMessage::PropertyExchange {
                    message: PeCiMessage::CapabilitiesReply(_),
                    ..
                }
            )]
        ),
        "{sent:?}"
    );
}

/// M2-103 section 3.2: after the Capabilities Reply the Initiator gets the
/// ResourceList, and the peer is listed; a Get of a Resource it lists is
/// answered with the data the peer holds.
#[test]
fn the_session_initiates_property_exchange_with_a_peer_that_declared_it() {
    let (mut wire, ours, inquiry) = property_peer();
    let (mut far, _) = Far::new(ours, local_on(false), None);
    let said = answers(&mut far, &inquiry, 60, &wire);
    let all = pump(&mut wire, &mut far, said, 60);
    let changes = property_events(&all);
    assert!(
        changes.iter().any(
            |c| matches!(c, PropertyChange::Event(e) if matches!(**e, ResourceEvent::Ready(_)))
        ),
        "{changes:?}"
    );
    let list = changes.iter().find_map(|c| match c {
        PropertyChange::Event(e) => match &**e {
            ResourceEvent::ResourceList(list) => Some(list.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(list, Some(far.store.resource_list()));

    let peers = wire.session.property_peers(&wire.id);
    let [handle] = &*peers else {
        panic!("the peer is listed: {peers:?}");
    };
    let get = ResourceGet::new(ResourceTarget::try_from(StandardResource::LocalOn).unwrap());
    wire.session
        .ask_property(handle.ask(PropertyCall::Get(get)))
        .unwrap();
    let mut all = polled(&mut wire, 300);
    let said = answers(
        &mut far,
        &ci_sent(&wire)
            .into_iter()
            .map(|(_, m)| m)
            .collect::<Vec<_>>(),
        300,
        &wire,
    );
    all.extend(pump(&mut wire, &mut far, said, 302));
    let got = property_answers(&all).into_iter().find_map(|a| match a {
        Answer::Got { target, result } => Some((target.clone(), result.clone())),
        _ => None,
    });
    assert_eq!(
        got,
        Some((
            ResourceTarget::try_from(StandardResource::LocalOn).unwrap(),
            Ok(PeData::LocalOn(PeLocalOn(false)))
        ))
    );
}

/// The session serves what the app gave: a peer's Initiator gets the
/// ResourceList and the data of the store.
#[test]
fn the_session_serves_its_store_to_a_peer_that_declared_property_exchange() {
    let (mut wire, ours, _) = property_peer();
    wire.session.serve_properties(local_on(true));
    let _ = polled(&mut wire, 55);
    let (mut far, first) = Far::new(ours, ResourceStore::new(), Some(wire.at(60)));
    let _ = pump(&mut wire, &mut far, first, 60);
    let list = far.events.iter().find_map(|e| match e {
        ResourceEvent::ResourceList(list) => Some(list.clone()),
        _ => None,
    });
    assert_eq!(list, Some(local_on(true).resource_list()));

    let get = ResourceGet::new(ResourceTarget::try_from(StandardResource::LocalOn).unwrap());
    let init = far.init.as_mut().unwrap();
    let (_, actions) = init.get(&get, wire.at(300)).unwrap();
    let said = Far::sends_of(actions);
    let _ = pump(&mut wire, &mut far, said, 300);
    let got = far.events.iter().find_map(|e| match e {
        ResourceEvent::Got { result, .. } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(got, Some(Ok(PeData::LocalOn(PeLocalOn(true)))));
}

/// M2-103 section 12.2: a Capabilities Inquiry nothing answers ends after
/// [`PE_TIMEOUT`], on the session's own poll.
#[test]
fn an_unanswered_property_exchange_inquiry_fails_after_the_property_exchange_timeout() {
    let (mut wire, _, _) = property_peer();
    let timeout = PE_TIMEOUT.as_millis() as u64;
    let early = polled(&mut wire, 50 + timeout - 10);
    assert!(property_events(&early).is_empty());
    let late = polled(&mut wire, 50 + timeout + 10);
    let changes = property_events(&late);
    assert!(
        matches!(
            changes.as_slice(),
            [PropertyChange::Event(e)]
                if matches!(**e, ResourceEvent::CapabilitiesFailed(PeCapabilitiesFailure::TimedOut))
        ),
        "{changes:?}"
    );
}

/// A lost peer takes its Property Exchange with it and its handle goes stale.
#[test]
fn a_lost_peer_drops_its_property_exchange() {
    let (mut wire, ours, inquiry) = property_peer();
    let (mut far, _) = Far::new(ours, ResourceStore::new(), None);
    let said = answers(&mut far, &inquiry, 60, &wire);
    let _ = pump(&mut wire, &mut far, said, 60);
    assert_eq!(wire.session.property_peers(&wire.id).len(), 1);
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(peer()),
            target: peer(),
        },
        400,
    );
    let gone = polled(&mut wire, 410);
    assert!(property_events(&gone).contains(&&PropertyChange::Dropped));
    assert!(wire.session.property_peers(&wire.id).is_empty());
}

fn subscribable() -> ResourceStore {
    let mut store = ResourceStore::new();
    store.declare(
        ResourceEntry::standard(StandardResource::LocalOn)
            .can_subscribe(true)
            .build()
            .unwrap(),
    );
    store
        .put(None, PeData::LocalOn(PeLocalOn(true)))
        .expect("declared");
    store
}

/// A peer with Property Exchange whose Capabilities and ResourceList have come.
fn ready_property_peer(store: ResourceStore) -> (Wire, Muid, Far) {
    let (mut wire, ours, inquiry) = property_peer();
    let (mut far, _) = Far::new(ours, store, None);
    let said = answers(&mut far, &inquiry, 60, &wire);
    let _ = pump(&mut wire, &mut far, said, 60);
    assert_eq!(wire.session.property_peers(&wire.id).len(), 1);
    (wire, ours, far)
}

/// The app asks the first listed peer for `call` at `ms`; the far end and the
/// session trade messages until quiet.
fn ask_and_pump(wire: &mut Wire, far: &mut Far, call: PropertyCall, ms: u64) -> Vec<Polled> {
    let peers = wire.session.property_peers(&wire.id);
    wire.session.ask_property(peers[0].ask(call)).unwrap();
    let mut all = polled(wire, ms);
    let sent: Vec<CiMessage> = ci_sent(wire).into_iter().map(|(_, m)| m).collect();
    let said = answers(far, &sent, ms, wire);
    all.extend(pump(wire, far, said, ms + 2));
    all
}

/// M2-103 section 11: a Subscription the peer allows starts with its Reply and ends
/// with the Initiator's End.
/// Spec: m2-103/11#4, m2-103/11.5#1
#[test]
fn a_subscription_starts_and_ends_through_the_session() {
    let (mut wire, _, mut far) = ready_property_peer(subscribable());
    let get = ResourceGet::new(ResourceTarget::try_from(StandardResource::LocalOn).unwrap());
    let all = ask_and_pump(&mut wire, &mut far, PropertyCall::Subscribe(get), 300);
    let answers = property_answers(&all);
    let Some(id) = answers.iter().find_map(|a| match a {
        Answer::Subscribed { subscribe_id, .. } => Some(subscribe_id.clone()),
        _ => None,
    }) else {
        panic!("a Subscription started: {answers:?}");
    };
    assert_eq!(far.resp.subscriptions().count(), 1);

    let all = ask_and_pump(&mut wire, &mut far, PropertyCall::Unsubscribe(id), 600);
    let answers = property_answers(&all);
    assert!(
        answers
            .iter()
            .any(|a| matches!(a, Answer::Unsubscribed { result: Ok(_), .. })),
        "{answers:?}"
    );
    assert_eq!(far.resp.subscriptions().count(), 0);
}

/// The statuses of the Subscription replies the session sent.
fn subscription_replies(wire: &Wire) -> Vec<tutti_midi_ci::PeStatus> {
    use tutti_midi_ci::{PeHeader, PeMessageKind};
    ci_sent(wire)
        .into_iter()
        .filter_map(|(_, message)| match message {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            } if kind == PeMessageKind::SUBSCRIPTION_REPLY => {
                match PeHeader::try_from(chunk.header()) {
                    Ok(PeHeader::Reply(reply)) => Some(reply.status),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

/// A Subscription inquiry of the peer: a header and its data, in `total` chunks
/// of which this is the `at`th.
fn subscription_chunk(
    ours: Muid,
    request: u8,
    total: u16,
    at: u16,
    header: &str,
    data: &str,
) -> CiMessage {
    use tutti_midi_ci::{ChunkNo, ChunkPosition, ChunkTotal, PeChunk, PeMessageKind, PeRequestId};
    CiMessage::PropertyExchange {
        header: CiHeader::new(peer()),
        to: ours,
        message: PeCiMessage::Chunk {
            kind: PeMessageKind::SUBSCRIPTION_INQUIRY,
            chunk: PeChunk::new(
                PeRequestId::of(request),
                ChunkTotal::Known(ChunkNo::of(total)),
                ChunkPosition::Number(ChunkNo::of(at)),
                header.as_bytes().to_vec(),
                data.as_bytes().to_vec(),
            )
            .expect("a chunk"),
        },
    }
}

/// A subscribed far end whose Subscription the session's Initiator holds.
fn held_subscription() -> (Wire, Muid, Far, tutti_midi_ci::SubscribeId) {
    let (mut wire, ours, mut far) = ready_property_peer(subscribable());
    let get = ResourceGet::new(ResourceTarget::try_from(StandardResource::LocalOn).unwrap());
    let all = ask_and_pump(&mut wire, &mut far, PropertyCall::Subscribe(get), 300);
    let id = property_answers(&all)
        .into_iter()
        .find_map(|a| match a {
            Answer::Subscribed { subscribe_id, .. } => Some(subscribe_id.clone()),
            _ => None,
        })
        .expect("a Subscription started");
    (wire, ours, far, id)
}

/// M2-103 section 11.1: an update of a Subscription the session holds is answered
/// once, by its Initiator, with 200; the Responder, which would refuse it as
/// something only a Responder sends, hears nothing of it.
#[test]
fn an_update_of_a_held_subscription_is_answered_once_with_200_and_not_by_the_responder() {
    let (mut wire, _, mut far, _) = held_subscription();
    let _ = ci_sent(&wire);
    let now = wire.at(600);
    let target = ResourceTarget::try_from(StandardResource::LocalOn).unwrap();
    let actions = far
        .resp
        .publish(
            &mut far.store,
            &target,
            PeData::LocalOn(PeLocalOn(false)),
            now,
        )
        .expect("publishes");
    for action in actions {
        if let tutti_midi_ci::ResponderAction::Send(message) = action {
            say(&wire, GROUP_0, &message, 600);
        }
    }
    let _ = polled(&mut wire, 610);
    assert_eq!(subscription_replies(&wire), [tutti_midi_ci::PeStatus::Ok]);
}

/// An update in several chunks is the Initiator's from its first chunk to its last:
/// the session answers it once, with 200, and the Responder says nothing for any
/// chunk.
#[test]
fn a_chunked_update_is_answered_once_by_the_initiator_and_not_by_the_responder() {
    let (mut wire, ours, _far, id) = held_subscription();
    let _ = ci_sent(&wire);
    let header = format!(r#"{{"command":"full","subscribeId":"{}"}}"#, id.as_str());
    say(
        &wire,
        GROUP_0,
        &subscription_chunk(ours, 9, 2, 1, &header, "tr"),
        600,
    );
    let _ = polled(&mut wire, 610);
    assert!(
        subscription_replies(&wire).is_empty(),
        "nothing answers the first of two chunks"
    );
    say(
        &wire,
        GROUP_0,
        &subscription_chunk(ours, 9, 2, 2, "", "ue"),
        620,
    );
    let _ = polled(&mut wire, 630);
    assert_eq!(subscription_replies(&wire), [tutti_midi_ci::PeStatus::Ok]);
}

/// M2-103 section 11.3: the End of a Subscription the session holds, sent by the
/// peer, reaches the Initiator, which reports it and answers 200; the Responder
/// does not answer it as the end of a Subscription it never had.
#[test]
fn the_peers_end_of_a_held_subscription_reaches_the_initiator() {
    let (mut wire, _, mut far, id) = held_subscription();
    let _ = ci_sent(&wire);
    let now = wire.at(600);
    for action in far.resp.end_subscription(&mut far.store, &id, now) {
        if let tutti_midi_ci::ResponderAction::Send(message) = action {
            say(&wire, GROUP_0, &message, 600);
        }
    }
    let all = polled(&mut wire, 610);
    let ended = property_events(&all).into_iter().any(|c| {
        matches!(c, PropertyChange::Event(e)
            if matches!(**e, ResourceEvent::SubscriptionEnded { .. }))
    });
    assert!(ended, "the Initiator reported the end");
    assert_eq!(subscription_replies(&wire), [tutti_midi_ci::PeStatus::Ok]);
}

/// The End of a Subscription the session does not hold is the Responder's: it
/// answers it 404, as it does the end of a Subscription it never started.
#[test]
fn the_end_of_an_unheld_subscription_reaches_the_responder_and_is_answered_404() {
    let (mut wire, ours, _far, _) = held_subscription();
    let _ = ci_sent(&wire);
    say(
        &wire,
        GROUP_0,
        &subscription_chunk(
            ours,
            11,
            1,
            1,
            r#"{"command":"end","subscribeId":"unheld"}"#,
            "",
        ),
        600,
    );
    let _ = polled(&mut wire, 610);
    assert_eq!(
        subscription_replies(&wire),
        [tutti_midi_ci::PeStatus::NotFound]
    );
}

/// A call the peer's ResourceList does not allow is refused when it is started,
/// with the reason, and nothing is sent.
#[test]
fn a_call_the_peers_resource_list_refuses_is_a_not_sent_answer() {
    let (mut wire, _, mut far) = ready_property_peer(subscribable());
    let get = ResourceGet::new(ResourceTarget::try_from(StandardResource::DeviceInfo).unwrap());
    let all = ask_and_pump(&mut wire, &mut far, PropertyCall::Get(get), 300);
    let answers = property_answers(&all);
    assert!(
        matches!(answers.as_slice(), [Answer::PropertyNotSent { .. }]),
        "{answers:?}"
    );
}

/// A handle outlives its peer: asking from it is refused with the reason, whether
/// the input closed or the peer went, and a call queued before the peer went is
/// reported as not sent at the poll.
#[test]
fn a_call_for_a_peer_that_is_gone_is_refused_or_reported_not_sent() {
    let (mut wire, _, _) = ready_property_peer(subscribable());
    let handle = wire.session.property_peers(&wire.id)[0].clone();
    let get = || {
        PropertyCall::Get(ResourceGet::new(
            ResourceTarget::try_from(StandardResource::LocalOn).unwrap(),
        ))
    };

    // Queued, then the peer is invalidated before the poll that would send it.
    wire.session.ask_property(handle.ask(get())).unwrap();
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(peer()),
            target: peer(),
        },
        300,
    );
    let all = polled(&mut wire, 310);
    let answers = property_answers(&all);
    assert!(
        answers
            .iter()
            .any(|a| matches!(a, Answer::PropertyLost { .. })),
        "{answers:?}"
    );
    assert_eq!(
        wire.session.ask_property(handle.ask(get())),
        Err(PeerRequestError::NoSuchPeer)
    );

    wire.session.disconnect_input(&wire.id.clone()).unwrap();
    assert_eq!(
        wire.session.ask_property(handle.ask(get())),
        Err(PeerRequestError::NotOpen)
    );
}

/// M2-101 section 5.11: a Property Exchange inquiry of a peer that declared no
/// Property Exchange is NAKed as a message not supported, not left unanswered.
#[test]
fn an_inquiry_of_a_peer_that_declared_no_property_exchange_is_nakked() {
    let (mut wire, ours) = midi1_running();
    find_peer(&mut wire, GROUP_0, ours, 40);
    let _ = ci_sent(&wire);
    say(
        &wire,
        GROUP_0,
        &CiMessage::PropertyExchange {
            header: CiHeader::new(peer()),
            to: ours,
            message: PeCiMessage::CapabilitiesInquiry(PeCapabilities::new(
                SimultaneousRequests::of(2),
            )),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    let sent = ci_sent(&wire);
    let [(_, CiMessage::Nak { to, nak, .. })] = sent.as_slice() else {
        panic!("one NAK: {sent:?}");
    };
    assert_eq!(*to, peer());
    assert_eq!(nak.nak_sub_id2.get(), 0x30);
    assert_eq!(nak.status, tutti_midi_ci::NakStatus::MessageNotSupported);
}

/// M2-101 section 5.9: when the device's MUID changes its Responder is for the
/// new one: an inquiry of the new MUID is answered from it.
#[test]
fn a_new_muid_renews_the_responder() {
    let (mut wire, ours, _) = property_peer();
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(peer()),
            target: ours,
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    let sent = ci_sent(&wire);
    let Some(new) = sent.iter().find_map(|(_, m)| match m {
        CiMessage::Discovery { source, .. } => Some(*source),
        _ => None,
    }) else {
        panic!("a new round of Discovery: {sent:?}");
    };
    assert_ne!(new, ours);
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            device_id: CiDeviceId::FunctionBlock,
            source: peer(),
            to: new,
            data: peer_data_of(CiCategories::PROPERTY_EXCHANGE),
        },
        80,
    );
    let _ = polled(&mut wire, 90);
    let _ = ci_sent(&wire);
    say(
        &wire,
        GROUP_0,
        &CiMessage::PropertyExchange {
            header: CiHeader::new(peer()),
            to: new,
            message: PeCiMessage::CapabilitiesInquiry(PeCapabilities::new(
                SimultaneousRequests::of(2),
            )),
        },
        100,
    );
    let _ = polled(&mut wire, 110);
    let replies: Vec<_> = ci_sent(&wire)
        .into_iter()
        .filter_map(|(_, m)| match m {
            CiMessage::PropertyExchange {
                header,
                message: PeCiMessage::CapabilitiesReply(_),
                ..
            } => Some(header.source),
            _ => None,
        })
        .collect();
    assert_eq!(replies, [new]);
}

/// A session that serves a store whose entry says it can be subscribed to
/// lists it as not subscribable (the session cannot tell a subscriber of a
/// change), and refuses a Subscription Start with the status for a Resource that
/// is not subscribable (status 405).
#[test]
fn a_served_resource_is_not_subscribable_and_a_subscription_start_is_refused() {
    use tutti_midi_ci::{PeAction, PeCall, PeGet, PeInitiator, PeOutcome, PeStatus};
    let (mut wire, ours, _) = property_peer();
    wire.session.serve_properties(subscribable());
    let _ = polled(&mut wire, 55);

    // The ResourceList the peer reads says canSubscribe is false.
    let (mut far, first) = Far::new(ours, ResourceStore::new(), Some(wire.at(60)));
    let _ = pump(&mut wire, &mut far, first, 60);
    let list = far.events.iter().find_map(|e| match e {
        ResourceEvent::ResourceList(list) => Some(list.clone()),
        _ => None,
    });
    let list = list.expect("the ResourceList came");
    let entry = list
        .entry(&StandardResource::LocalOn)
        .expect("LocalOn is listed");
    assert!(!entry.can_subscribe());

    // A Start sent anyway, past the Initiator's own check, gets the refusal.
    let (mut init, actions) = PeInitiator::new(
        peer(),
        ours,
        PeSettings {
            capabilities: PeCapabilities::new(SimultaneousRequests::of(4)),
            peer_max_sysex: MaxSysexSize::of(512),
            reassembly: pe_limits(),
            body_limit: BodyLimit::of(1 << 16),
        },
        wire.at(300),
    );
    let mut pending = actions;
    let mut status = None;
    let mut started = false;
    for round in 0..40 {
        let ms = 300 + 2 * round;
        for action in std::mem::take(&mut pending) {
            match action {
                PeAction::Send(message) => say(&wire, GROUP_0, &message, ms),
                PeAction::Capabilities(_) if !started => {
                    started = true;
                    let name = StandardResource::LocalOn.name();
                    let (_, sends) = init
                        .start(PeCall::Subscribe(PeGet::new(name)), wire.at(ms))
                        .unwrap();
                    pending.extend(sends);
                }
                PeAction::Finished {
                    outcome: PeOutcome::Failed(response),
                    ..
                } => status = Some(response.status()),
                _ => {}
            }
        }
        let _ = polled(&mut wire, ms + 1);
        for (_, message) in ci_sent(&wire) {
            pending.extend(init.on_message(&Decoded::Message(message), wire.at(ms + 1)));
        }
        if status.is_some() {
            break;
        }
    }
    assert_eq!(status, Some(PeStatus::NotAllowed));
}

// ---- The three ways a first inquiry ends ----

/// How the first inquiry of a category (or the peer itself) ended, as the change
/// the session reports for it.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    Refused,
    TimedOut,
    Dropped,
}

/// The endings `polled` reports for the one category of `peer_declares`.
fn endings(polled: &[Polled], category: CiCategories) -> Vec<Ending> {
    if category == CiCategories::PROFILE_CONFIGURATION {
        profile_events(polled)
            .into_iter()
            .filter_map(|c| match c {
                ProfilesChange::InquiryRefused => Some(Ending::Refused),
                ProfilesChange::InquiryTimedOut => Some(Ending::TimedOut),
                ProfilesChange::Dropped => Some(Ending::Dropped),
                _ => None,
            })
            .collect()
    } else if category == CiCategories::PROCESS_INQUIRY {
        process_events(polled)
            .into_iter()
            .filter_map(|c| match c {
                ProcessChange::CapabilitiesRefused => Some(Ending::Refused),
                ProcessChange::CapabilitiesTimedOut => Some(Ending::TimedOut),
                ProcessChange::Dropped => Some(Ending::Dropped),
                _ => None,
            })
            .collect()
    } else {
        property_events(polled)
            .into_iter()
            .filter_map(|c| match c {
                PropertyChange::Event(event) => match **event {
                    ResourceEvent::CapabilitiesFailed(PeCapabilitiesFailure::Refused(_)) => {
                        Some(Ending::Refused)
                    }
                    ResourceEvent::CapabilitiesFailed(PeCapabilitiesFailure::TimedOut) => {
                        Some(Ending::TimedOut)
                    }
                    _ => None,
                },
                PropertyChange::Dropped => Some(Ending::Dropped),
                _ => None,
            })
            .collect()
    }
}

/// A peer that NAKs the first inquiry of a category, one that never answers it
/// and one that is lost before it does are three different changes, in each of
/// Profile Configuration, Process Inquiry and Property Exchange: the Initiators
/// share one driver, which must not blur them.
#[test]
fn a_nak_a_silence_and_a_lost_peer_are_three_endings_of_every_first_inquiry() {
    use tutti_midi_ci::{
        Nak, NakStatus, SUB_ID2_PE_CAPABILITIES_INQUIRY, SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
        SUB_ID2_PROFILE_INQUIRY,
    };
    let silence = PEER_REPLY_TIMEOUT.max(PE_TIMEOUT).as_millis() as u64 + 100;
    for (category, inquiry) in [
        (CiCategories::PROFILE_CONFIGURATION, SUB_ID2_PROFILE_INQUIRY),
        (
            CiCategories::PROCESS_INQUIRY,
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
        ),
        (
            CiCategories::PROPERTY_EXCHANGE,
            SUB_ID2_PE_CAPABILITIES_INQUIRY,
        ),
    ] {
        let (mut wire, ours) = peer_declaring(category);
        say(
            &wire,
            GROUP_0,
            &CiMessage::Nak {
                header: CiHeader {
                    device_id: CiDeviceId::FunctionBlock,
                    ..CiHeader::new(peer())
                },
                to: ours,
                nak: Nak::new(U7::of(inquiry), NakStatus::MessageNotSupported),
            },
            60,
        );
        let refused = polled(&mut wire, 70);
        assert_eq!(
            endings(&refused, category),
            [Ending::Refused],
            "{category:?}"
        );

        let (mut wire, _) = peer_declaring(category);
        let early = polled(&mut wire, silence - 200);
        assert_eq!(endings(&early, category), [], "{category:?}");
        let silent = polled(&mut wire, 50 + silence);
        assert_eq!(
            endings(&silent, category),
            [Ending::TimedOut],
            "{category:?}"
        );

        let (mut wire, _) = peer_declaring(category);
        say(
            &wire,
            GROUP_0,
            &CiMessage::InvalidateMuid {
                header: CiHeader::new(peer()),
                target: peer(),
            },
            60,
        );
        let lost = polled(&mut wire, 70);
        assert_eq!(endings(&lost, category), [Ending::Dropped], "{category:?}");
    }
}

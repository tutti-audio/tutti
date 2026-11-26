use std::time::Duration;

use tutti_midi_ci::{CiCategories, CiDeviceId, DeviceBody, DiscoveryData};
use tutti_midi_device::{EndpointId, PortDirection};

use super::*;
use crate::MidiSession;
use crate::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_fixtures::midi::device_identity_one_byte_manufacturer as identity;

/// The output of a fake device, and the counters of what it was sent.
fn output() -> (OpenOutput, FakeCounters) {
    output_taking(usize::MAX)
}

/// As [`output`], the device taking `room` packets in all.
fn output_taking(room: usize) -> (OpenOutput, FakeCounters) {
    let (backend, counters) = FakeBackend::build_with(vec![], vec![], room);
    let inlets =
        tutti_midi_runtime::InputInlets::new(tutti_graph::EventCapacity::of(64), Duration::ZERO);
    let session = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        crate::HardwareMidiInputs::new(inlets),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    counters.plug(endpoint(7, "Wire"), PortDirection::Duplex);
    let id = session.outputs().first().expect("the device").id.clone();
    (
        session
            .connect_output(crate::testing::started(), &id)
            .unwrap(),
        counters,
    )
}

// A MIDI-CI message is SysEx7 packets, and a device with room for some of
// them takes none: a SysEx7 whose end the device never gets is no message.
#[test]
fn a_ci_message_the_device_has_no_room_for_whole_is_not_sent_in_part() {
    let message = CiMessage::Discovery {
        device_id: CiDeviceId::FunctionBlock,
        source: Muid::of(0x0012_3456),
        data: DiscoveryData::new(DeviceBody::new(
            identity(),
            CiCategories::empty(),
            MaxSysexSize::of(256),
        )),
    };
    let packets = message.packets(G0).unwrap().count();
    assert!(packets >= 2, "a message of several packets");
    let (mut out, counters) = output_taking(packets - 1);
    assert_eq!(send(&mut out, &[(G0, message.clone())]), Losses::refused(1));
    assert!(counters.take_sent(EndpointId::from_raw(7)).is_empty());

    let (mut out, counters) = output_taking(packets);
    assert_eq!(send(&mut out, &[(G0, message)]), Losses::NONE);
    assert_eq!(counters.take_sent(EndpointId::from_raw(7)).len(), packets);
}

/// An entropy source that refuses draws `from..to` (counted from 1) and
/// gives every other, with the count it keeps.
fn refusing(from: usize, to: usize) -> impl FnMut() -> Result<Entropy, getrandom::Error> {
    let mut draws = 0;
    move || {
        draws += 1;
        if (from..to).contains(&draws) {
            Err(getrandom::Error::UNSUPPORTED)
        } else {
            entropy()
        }
    }
}

fn heard(packets: &[Ump]) -> Heard<'_> {
    Heard {
        packets,
        lost_before: false,
        lost_during: false,
    }
}

fn group_of(ump: &Ump) -> u8 {
    ((ump.0[0] >> 24) & 0xF) as u8
}

const G0: MidiGroup = MidiGroup::of(0);
const G1: MidiGroup = MidiGroup::of(1);
const B0: Block = Block::single(G0);
const B1: Block = Block::single(G1);

// A Group whose device cannot start is reported once, however many polls
// retry it, and the retries do not rebuild the reader, so a message open on
// a Group that did start is finished; the Group starts when the source gives.
#[test]
fn a_group_that_cannot_start_is_reported_once_and_retried_without_dropping_a_message() {
    let (mut out, counters) = output();
    let raw = EndpointId::from_raw(7);
    // Group 0 takes the first draw; Group 1 is refused the next three.
    let mut source = refusing(2, 5);
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);

    let (mut running, first) = Running::start(&[B0, B1], identity(), &mut source, at(0), &mut out);
    assert_eq!(first.unstarted, [(G1, getrandom::Error::UNSUPPORTED)]);
    let _ = counters.take_sent(raw);

    // A peer's Discovery arrives on Group 0 in two polls; Group 1 is refused
    // again in both and is not reported again.
    let message = CiMessage::Discovery {
        device_id: CiDeviceId::FunctionBlock,
        source: Muid::of(0x0012_3456),
        data: DiscoveryData::new(DeviceBody::new(
            identity(),
            CiCategories::empty(),
            MaxSysexSize::of(256),
        )),
    };
    let packets: Vec<Ump> = message.packets(G0).unwrap().collect();
    assert!(packets.len() >= 2);
    let (head, tail) = packets.split_at(packets.len() / 2);
    for (ms, part) in [(1, head), (2, tail)] {
        let mut changes = CiChanges::default();
        running.reconcile(&[B0, B1], &mut source, at(ms), &mut out, &mut changes);
        running.step(
            &mut ResourceStore::new(),
            at(ms),
            &heard(part),
            &mut out,
            &mut changes,
        );
        assert!(changes.unstarted.is_empty(), "reported once");
        if ms == 1 {
            assert!(
                counters.take_sent(raw).is_empty(),
                "the message is not whole yet"
            );
        }
    }
    assert!(
        !counters.take_sent(raw).is_empty(),
        "the message survived the retries and was answered"
    );

    // The next draw succeeds: Group 1 starts and sends its Discovery.
    let mut changes = CiChanges::default();
    running.reconcile(&[B0, B1], &mut source, at(3), &mut out, &mut changes);
    assert!(changes.unstarted.is_empty());
    assert!(running.unstarted.is_empty());
    let sent = counters.take_sent(raw);
    assert!(!sent.is_empty() && sent.iter().all(|u| group_of(u) == 1));
}

// Every Group refused: each is reported once over several polls, the set
// stays not idle (what keeps `Ci::poll` from dropping it), and the retry goes
// on: the first draw that succeeds starts both Groups.
#[test]
fn when_every_group_is_refused_each_is_reported_once_and_the_retry_goes_on() {
    let (mut out, _counters) = output();
    // Two draws at the start and two on each of three polls are refused.
    let mut source = refusing(1, 9);
    let now = Instant::now();
    let (mut running, first) = Running::start(&[B0, B1], identity(), &mut source, now, &mut out);
    assert_eq!(
        first.unstarted,
        [
            (G0, getrandom::Error::UNSUPPORTED),
            (G1, getrandom::Error::UNSUPPORTED)
        ]
    );
    assert!(!running.is_idle(), "the Groups still wait to start");
    for _ in 0..3 {
        let mut changes = CiChanges::default();
        running.reconcile(&[B0, B1], &mut source, now, &mut out, &mut changes);
        assert!(changes.unstarted.is_empty(), "reported once");
        assert!(!running.is_idle());
        assert!(running.members.is_empty());
    }
    // The next draws succeed: both Groups start, and nothing is reported.
    let mut changes = CiChanges::default();
    running.reconcile(&[B0, B1], &mut source, now, &mut out, &mut changes);
    assert!(changes.unstarted.is_empty());
    assert_eq!(running.members.len(), 2);
    assert!(running.unstarted.is_empty());
    // No Group is called for: nothing waits and nothing runs.
    running.reconcile(&[], &mut source, now, &mut out, &mut changes);
    assert!(running.is_idle());
}

// A Group that leaves the Groups while it is refused and returns is a new
// failure: it is reported again.
#[test]
fn a_group_that_leaves_and_returns_while_refused_is_reported_again() {
    let (mut out, _counters) = output();
    let mut source = refusing(1, 100);
    let now = Instant::now();
    let (mut running, first) = Running::start(&[B0], identity(), &mut source, now, &mut out);
    assert_eq!(first.unstarted.len(), 1);
    let mut changes = CiChanges::default();
    running.reconcile(&[B1], &mut source, now, &mut out, &mut changes);
    assert_eq!(changes.unstarted, [(G1, getrandom::Error::UNSUPPORTED)]);
    let mut changes = CiChanges::default();
    running.reconcile(&[B0, B1], &mut source, now, &mut out, &mut changes);
    assert_eq!(changes.unstarted, [(G0, getrandom::Error::UNSUPPORTED)]);
}

// A CI message the output cannot take is counted, not dropped without a
// trace: the first Discovery of each of two Groups goes to an output that
// takes nothing, so two messages are refused; an output that takes them
// counts none.
#[test]
fn a_ci_message_the_output_refuses_is_counted_and_one_it_takes_is_not() {
    let now = Instant::now();
    let (mut full, _counters) = output_taking(0);
    let (_, changes) = Running::start(&[B0, B1], identity(), &mut entropy, now, &mut full);
    assert_eq!(changes.refused, Losses::refused(2));

    let (mut roomy, counters) = output();
    let (_, changes) = Running::start(&[B0, B1], identity(), &mut entropy, now, &mut roomy);
    assert_eq!(changes.refused, Losses::NONE);
    assert!(counters.sent() > 0, "the messages were sent");
}

// A message the output has room for only in part is not sent in part: it
// is refused whole, and counted.
#[test]
fn a_ci_message_the_output_has_room_for_only_in_part_is_counted() {
    let now = Instant::now();
    let (mut short, _counters) = output_taking(1);
    let (_, changes) = Running::start(&[B0], identity(), &mut entropy, now, &mut short);
    assert_eq!(changes.refused, Losses::refused(1));
}

/// The initiator `init` and the Responder `server` trade messages until
/// neither has more to say; the initiator's events are returned.
fn trade(
    init: &mut tutti_midi_ci::ResourceInitiator,
    server: &mut ResourceResponder,
    store: &mut ResourceStore,
    first: Vec<tutti_midi_ci::ResourceAction>,
    now: Instant,
) -> Vec<tutti_midi_ci::ResourceEvent> {
    use tutti_midi_ci::{ResourceAction, ResponderAction};
    let mut events = Vec::new();
    let mut actions = first;
    for _ in 0..40 {
        let mut to_server = Vec::new();
        for action in actions.drain(..) {
            match action {
                ResourceAction::Send(message) => to_server.push(message),
                ResourceAction::Event(event) => events.push(*event),
                _ => {}
            }
        }
        if to_server.is_empty() {
            break;
        }
        for message in to_server {
            let replies = server.on_message(store, &Decoded::Message(message), now);
            for reply in replies {
                if let ResponderAction::Send(message) = reply {
                    actions.extend(init.on_message(&Decoded::Message(message), now));
                }
            }
        }
    }
    events
}

// Every Group of an input serves the one store the app gave: a Set through
// one Group is read through another, and by the app.
#[test]
fn a_set_through_one_group_is_read_through_another_and_by_the_app() {
    use tutti_midi_ci::{
        PeCapabilities, PeData, PeLocalOn, PeReassemblyLimits, PeSettings, ResourceEntry,
        ResourceEvent, ResourceGet, ResourceInitiator, ResourceSet, ResourceTarget,
        SimultaneousRequests, StandardResource,
    };
    let (mut out, _counters) = output();
    let now = Instant::now();
    let mut store = ResourceStore::new();
    store.declare(
        ResourceEntry::standard(StandardResource::LocalOn)
            .build()
            .unwrap(),
    );
    store
        .put(None, PeData::LocalOn(PeLocalOn(true)))
        .expect("declared");
    let (mut running, _) = Running::start(&[B0, B1], identity(), &mut entropy, now, &mut out);

    let peer = Muid::of(0x0012_3456);
    let settings = |max| PeSettings {
        capabilities: PeCapabilities::new(SimultaneousRequests::of(2)),
        peer_max_sysex: max,
        reassembly: PeReassemblyLimits {
            streams: tutti_midi_ci::StreamLimit::of(4),
            bytes: tutti_midi_ci::BufferLimit::of(1 << 16),
        },
        body_limit: tutti_midi_ci::BodyLimit::of(1 << 16),
    };
    let target = ResourceTarget::try_from(StandardResource::LocalOn).unwrap();
    let mut ends = Vec::new();
    for member in &mut running.members {
        member.server.accept(peer, MAX_SYSEX);
        let (init, first) =
            ResourceInitiator::new(peer, member.device.muid(), settings(MAX_SYSEX), now);
        ends.push((init, first));
    }
    let [(a, first_a), (b, first_b)] = &mut ends[..] else {
        panic!("two Groups");
    };
    let [ma, mb] = &mut running.members[..] else {
        panic!("two Groups");
    };
    let _ = trade(a, &mut ma.server, &mut store, std::mem::take(first_a), now);
    let _ = trade(b, &mut mb.server, &mut store, std::mem::take(first_b), now);

    let set = ResourceSet::full(target.clone(), PeData::LocalOn(PeLocalOn(false))).unwrap();
    let (_, actions) = a.set(&set, now).unwrap();
    let _ = trade(a, &mut ma.server, &mut store, actions, now);

    let (_, actions) = b.get(&ResourceGet::new(target.clone()), now).unwrap();
    let events = trade(b, &mut mb.server, &mut store, actions, now);
    let got = events.iter().find_map(|e| match e {
        ResourceEvent::Got { result, .. } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(got, Some(Ok(PeData::LocalOn(PeLocalOn(false)))));
    assert_eq!(
        store.get(&target),
        Some(&PeData::LocalOn(PeLocalOn(false))),
        "the app's store sees it"
    );
}

/// The id of a fake device.
fn device_id() -> MidiDeviceId {
    let (backend, counters) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let inlets =
        tutti_midi_runtime::InputInlets::new(tutti_graph::EventCapacity::of(64), Duration::ZERO);
    let session = MidiSession::open_over(
        &tutti_platform::Executor::new(),
        backend,
        crate::HardwareMidiInputs::new(inlets),
        crate::MidiHardwareConfig::new(),
    )
    .unwrap();
    counters.plug(endpoint(7, "Wire"), PortDirection::Duplex);
    session.outputs().first().expect("the device").id.clone()
}

// A request whose peer is gone when the poll would send it is reported with
// the reason, never dropped: for a Group with no device and for a peer the
// device no longer has, and for each of Profile, Process and Property Exchange.
#[test]
fn a_request_for_a_peer_that_is_gone_is_reported_not_dropped() {
    use crate::properties::PropertyCall;
    use tutti_midi_ci::{
        CiDeviceId, DataControl, ProfileId, ReportRequest, ReportScope, ResourceGet, ResourceTarget,
    };
    let device = device_id();
    let (mut out, _counters) = output();
    let now = Instant::now();
    let (mut running, _) = Running::start(&[B0], identity(), &mut entropy, now, &mut out);
    let muid = Muid::of(0x0012_3456);
    let profile = ProfileId::from_bytes([0x7E, 1, 0, 1, 1]).unwrap();
    for group in [G1, G0] {
        let id = PeerId { group, muid };
        let mut changes = CiChanges::default();
        running.start_switches(
            &mut vec![ProfileSwitch {
                at: PeerRef::new(device.clone(), id),
                profile,
                switch: Switch::Off(CiDeviceId::Group),
            }],
            now,
            &mut out,
            &mut changes,
        );
        assert_eq!(
            changes.answers,
            [(
                id,
                Answer::SwitchLost {
                    request: tutti_midi_ci::ProfileRequest::SetOff {
                        address: CiDeviceId::Group,
                        profile
                    }
                }
            )],
            "{group:?}"
        );
        let peer = Reporter {
            at: PeerRef::new(device.clone(), id),
        };
        let request = ReportRequest::new(
            DataControl::Full,
            ReportScope::Group {
                controllers: tutti_midi_ci::ChannelControllers::all(),
                notes: tutti_midi_ci::NoteData::all(),
            },
        );
        running.start_reports(
            &mut vec![ReportAsk { peer, request }],
            now,
            &mut out,
            &mut changes,
        );
        assert_eq!(
            changes.answers[1..2],
            [(id, Answer::ReportLost)],
            "{group:?}"
        );
        let call = PropertyCall::Get(ResourceGet::new(
            ResourceTarget::try_from(tutti_midi_ci::StandardResource::LocalOn).unwrap(),
        ));
        running.start_properties(
            &mut vec![PropertyAsk {
                peer: PropertyPeer {
                    at: PeerRef::new(device.clone(), id),
                    resources: Arc::default(),
                },
                call: call.clone(),
            }],
            now,
            &mut out,
            &mut changes,
        );
        assert_eq!(
            changes.answers[2..],
            [(
                id,
                Answer::PropertyLost {
                    call: Box::new(call)
                }
            )],
            "{group:?}"
        );
    }
}

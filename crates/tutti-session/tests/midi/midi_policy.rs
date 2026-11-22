//! `MidiPolicy`: who the session is on the wire, through the session and the
//! fake OS. The fake peer is the wire's other end: what the session sends is
//! the fake's sink, what the peer says is the input's producer.

use std::time::Duration;

use tutti_platform::Instant;

use crate::common::OpenPorts as _;
use crate::common::{MF, blocks, note, play};
use tutti_graph::Losses;
use tutti_midi_ci::Muid;
use tutti_midi_ci::{CiCategories, CiHeader, CiMessage, Decoded, DiscoveryData, MaxSysexSize};
use tutti_midi_device::{
    Capabilities, Endpoint, EndpointId, MidiIdentity, PortDirection, Supports,
};
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_midi_hardware::{MidiHardwareConfig, ProfilesChange};
use tutti_midi_runtime::MidiClipNode;
use tutti_session::Peer as _;
use tutti_session::{
    EventInputPort, MidiEvent, MidiPolicy, MidiPolicyError, PHASES, PhaseCx, Session, SessionEvent,
    SetRefusal, ValueErrorKind,
};
use tutti_ump::EndpointName;
use tutti_ump::{
    DeviceIdentity, DiscoveryFilter, ManufacturerId, Message, MessageType, MidiGroup, Protocol,
    Stream, StreamConfig, SysexAssembler, SysexStep, U7, Ump, UmpVersion,
};
use tutti_units::SampleRate;
use tutti_units::{Beat, BeatDuration};

const RAW: u64 = 7;
const PEER: Muid = Muid::of(0x0012_3456);

fn device(model: u8) -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::NON_COMMERCIAL,
        family: [U7::of(1); 2],
        model: [U7::of(model); 2],
        revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
    }
}

fn policy(model: u8, name: &str) -> MidiPolicy {
    MidiPolicy {
        identity: Some(MidiIdentity {
            device: device(model),
            name: EndpointName::new(name).unwrap(),
            instance: None,
        }),
    }
}

/// A session over a fake OS with one duplex UMP device, "Wire", connected.
fn wired(last_group: u8) -> (Session, FakeCounters, Ports) {
    wired_with(last_group, MidiHardwareConfig::new())
}

/// As [`wired`], with the session opened with `config`.
fn wired_with(last_group: u8, config: MidiHardwareConfig) -> (Session, FakeCounters, Ports) {
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, config)
        .midi_groups(MidiGroup::of(last_group))
        .sample_rate(SampleRate::of(48_000.0))
        .build()
        .expect("builds");
    fake.plug(endpoint(RAW, "Wire"), PortDirection::Duplex);
    let _ = s.update();
    let wire = s
        .midi()
        .devices()
        .into_iter()
        .find(|d| d.name == "Wire")
        .expect("Wire is listed");
    let ports = Ports {
        input: s.open_input(wire.clone()).expect("opens"),
        output: s.open_output(wire).expect("opens"),
    };
    (s, fake, ports)
}

/// The two ports of the device `Wire`; they stay open while this is held.
struct Ports {
    input: tutti_session::MidiInput,
    output: tutti_session::MidiOutput,
}

fn update_at(s: &mut Session, now: Instant) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
    }
    out
}

/// The Stream messages among `sent`.
fn streams(sent: &[Ump]) -> Vec<Stream> {
    sent.iter()
        .filter_map(|ump| match ump.decode() {
            Ok(Message::Stream(stream)) => Some(stream),
            _ => None,
        })
        .collect()
}

/// The MIDI-CI messages among `sent`.
fn ci(sent: &[Ump]) -> Vec<CiMessage> {
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let mut found = Vec::new();
    for &ump in sent {
        assembler.begin_block();
        if let SysexStep::Complete(done) = assembler.push(ump)
            && let Decoded::Message(message) = CiMessage::decode(assembler.message(done).payload())
        {
            found.push(message);
        }
    }
    found
}

fn ask_everything() -> Ump {
    Ump::from(Message::Stream(Stream::EndpointDiscovery {
        version: UmpVersion { major: 1, minor: 1 },
        filter: DiscoveryFilter {
            endpoint_info: true,
            device_identity: true,
            endpoint_name: true,
            product_instance_id: true,
            stream_configuration: true,
        },
    }))
}

/// The wire's clock: one update per millisecond from a fixed start.
struct Clock(Instant, u64, Vec<SessionEvent>);

impl Clock {
    fn new() -> Self {
        Self(Instant::now(), 0, Vec::new())
    }

    /// Runs `ms` updates, answering the session's own Stream negotiation as a
    /// device that offers both Protocols would. Returns the MIDI-CI messages
    /// the session sent meanwhile.
    fn run(&mut self, s: &mut Session, fake: &FakeCounters, ms: u64) -> Vec<CiMessage> {
        let mut peer = Endpoint::new(Supports::of(Capabilities {
            midi1: true,
            midi2: true,
            rx_jr: true,
            tx_jr: true,
        }));
        let mut sent_ci = Vec::new();
        for _ in 0..ms {
            self.1 += 1;
            let now = self.0 + Duration::from_millis(self.1);
            self.2.extend(update_at(s, now));
            let sent = fake.take_sent(EndpointId::from_raw(RAW));
            let mut answers: Vec<Ump> = Vec::new();
            for message in ci(&sent) {
                // The peer of the wire answers each Discovery, declaring
                // Profile Configuration.
                if let CiMessage::Discovery { header, .. } = &message {
                    let data = DiscoveryData::new(
                        device(1),
                        CiCategories::PROFILE_CONFIGURATION,
                        MaxSysexSize::of(256),
                    );
                    answers.extend(
                        CiMessage::DiscoveryReply {
                            header: CiHeader::new(PEER),
                            to: header.source,
                            data,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                }
                if matches!(
                    message,
                    CiMessage::Discovery { .. } | CiMessage::InvalidateMuid { .. }
                ) {
                    sent_ci.push(message);
                }
            }
            for stream in streams(&sent) {
                let _ = peer.reply(Message::Stream(stream), &mut answers);
            }
            for ump in answers {
                assert!(fake.send_input(EndpointId::from_raw(RAW), ump, now));
            }
        }
        sent_ci
    }

    /// The profile changes the updates reported since the last call.
    fn profile_changes(&mut self) -> Vec<ProfilesChange> {
        std::mem::take(&mut self.2)
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::Midi(MidiEvent::CiProfiles { change, .. }) => Some(change),
                _ => None,
            })
            .collect()
    }

    /// The peer says `ump` and the session updates once; what it sent back.
    fn say(&mut self, s: &mut Session, fake: &FakeCounters, ump: Ump) -> Vec<Ump> {
        self.1 += 1;
        let now = self.0 + Duration::from_millis(self.1);
        let _ = fake.take_sent(EndpointId::from_raw(RAW));
        assert!(fake.send_input(EndpointId::from_raw(RAW), ump, now));
        self.1 += 1;
        let _ = update_at(s, self.0 + Duration::from_millis(self.1));
        fake.take_sent(EndpointId::from_raw(RAW))
    }
}

/// M2-104 §7.1.1: the session is the Endpoint it was told to be, once it was
/// told, and none before: the same Endpoint Discovery is unanswered under the
/// default policy and answered in the update after the policy is set, with the
/// identity, the name and one Function Block over the Groups the session
/// hears.
#[test]
fn a_peers_endpoint_discovery_is_answered_only_once_the_policy_names_an_identity() {
    let (mut s, fake, _ports) = wired(3);
    let mut clock = Clock::new();
    clock.run(&mut s, &fake, 5);
    let sent = clock.say(&mut s, &fake, ask_everything());
    assert!(streams(&sent).is_empty(), "no identity, no Endpoint");

    s.set(s.policy::<MidiPolicy>().to(policy(2, "My App")))
        .expect("the session has an OS MIDI layer");
    let _ = clock.run(&mut s, &fake, 5);
    let sent = streams(&clock.say(&mut s, &fake, ask_everything()));
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
        panic!("Info, Identity, Name, Product Instance Id, Configuration: {sent:?}");
    };
    assert_eq!(
        config.protocol,
        Protocol::Midi2,
        "the Protocol the negotiation with the device settled on"
    );
    assert_eq!(info.function_blocks.count().map(|c| c.get()), Some(1));
    assert!(info.midi1 && info.midi2 && !info.rx_jr && info.tx_jr);
    assert_eq!(*who, device(2));
    assert_eq!(&name[..6], b"My App");

    let ask = Ump::from(Message::Stream(Stream::FunctionBlockDiscovery {
        block: tutti_ump::BlockSelect::All,
        filter: tutti_ump::BlockFilter {
            info: true,
            name: false,
        },
    }));
    let sent = streams(&clock.say(&mut s, &fake, ask));
    let [Stream::FunctionBlockInfo(block)] = sent.as_slice() else {
        panic!("one block: {sent:?}");
    };
    assert_eq!(
        block.groups_spanned.value().map(|g| g.get()),
        Some(4),
        "the Groups the session hears: 1 through 4"
    );
}

/// M2-104 §7.1.6.1 step 3: a Stream Configuration Request is granted within
/// the capabilities the Endpoint declared (both Protocols, JR sent and, under arrival timing, not received),
/// and the session, as a discoverer of the same device, sends no request back.
#[test]
fn a_stream_configuration_request_is_granted_within_the_declared_capabilities() {
    let (mut s, fake, _ports) = wired(0);
    s.set(s.policy::<MidiPolicy>().to(policy(2, "My App")))
        .unwrap();
    let mut clock = Clock::new();
    clock.run(&mut s, &fake, 50);
    let asked = |protocol| {
        Ump::from(Message::Stream(Stream::StreamConfigurationRequest(
            StreamConfig {
                protocol,
                rx_jr: true,
                tx_jr: true,
            },
        )))
    };
    let sent = clock.say(&mut s, &fake, asked(Protocol::Midi2));
    assert_eq!(
        streams(&sent),
        [Stream::StreamConfigurationNotification(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: false,
            tx_jr: true,
        })]
    );
    let sent = clock.say(&mut s, &fake, asked(Protocol::Midi1));
    assert_eq!(
        streams(&sent),
        [Stream::StreamConfigurationNotification(StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: true,
        })]
    );
}

/// M2-101 §5.9: a new identity in the policy acts in the next update: the MIDI-CI Device
/// that ran sends Invalidate MUID for its MUID, and the new identity starts
/// Discovery under a fresh one. Clearing the identity ends MIDI-CI the same
/// way and starts nothing.
#[test]
fn a_changed_identity_invalidates_the_old_muid_and_starts_discovery_as_the_new() {
    let (mut s, fake, ports) = wired(0);
    let mut clock = Clock::new();
    s.set(s.policy::<MidiPolicy>().to(policy(2, "My App")))
        .unwrap();
    let first = clock.run(&mut s, &fake, 400);
    let [CiMessage::Discovery { header, data }] = first.as_slice() else {
        panic!("one Discovery as the first identity: {first:?}");
    };
    let (old, who) = (header.source, data.identity);
    assert_eq!(who, device(2));
    let port = &ports.input;
    assert_eq!(port.info().peers().len(), 1, "the peer was found");
    let old_id = port.info().peers()[0].id();
    let _ = clock.profile_changes();

    s.set(s.policy::<MidiPolicy>().to(policy(9, "My App")))
        .unwrap();
    let then = clock.run(&mut s, &fake, 400);
    assert!(
        clock.profile_changes().contains(&ProfilesChange::Dropped),
        "the peer's Profiles went with its MUID"
    );
    let [
        CiMessage::InvalidateMuid {
            header: from,
            target,
        },
        CiMessage::Discovery { header, data },
    ] = then.as_slice()
    else {
        panic!("Invalidate MUID, then Discovery: {then:?}");
    };
    assert_eq!((from.source, *target), (old, old));
    assert_ne!(header.source, old, "a fresh MUID");
    assert_eq!(data.identity, device(9));
    let newer = header.source;

    assert_eq!(
        port.info().peers().len(),
        1,
        "found again under the new MUID"
    );
    assert_ne!(
        port.info().peers()[0].id(),
        old_id,
        "a peer that was dropped and is found again is listed anew, with a new id"
    );
    let _ = clock.profile_changes();
    s.set(s.policy::<MidiPolicy>().to(MidiPolicy::default()))
        .unwrap();
    let then = clock.run(&mut s, &fake, 400);
    assert!(
        clock.profile_changes().contains(&ProfilesChange::Dropped),
        "the peer's Profiles were dropped"
    );
    assert!(
        port.info().peers().is_empty(),
        "no peer without an identity"
    );
    assert!(
        matches!(
            then.as_slice(),
            [CiMessage::InvalidateMuid { target, .. }] if *target == newer
        ),
        "{then:?}"
    );
}

/// **A MIDI-CI message the device's output refuses counts on the input port**:
/// the input owns the conversation, so the refused Discovery is its `refused`
/// loss, and the output port, which never carries it, counts nothing.
#[test]
fn a_ci_send_the_device_refuses_counts_on_the_input_port() {
    let (mut s, fake, ports) = wired(0);
    let mut clock = Clock::new();
    let _ = clock.run(&mut s, &fake, 100);
    let (input, output) = (&ports.input, &ports.output);
    assert_eq!(input.losses(), Losses::NONE);
    fake.refuse_sends(true);
    s.set(s.policy::<MidiPolicy>().to(policy(2, "My App")))
        .unwrap();
    let _ = clock.run(&mut s, &fake, 400);
    assert_eq!(input.losses().refused, 1, "the one Discovery");
    assert_eq!(input.losses().lost, 0);
    assert_eq!(output.losses(), Losses::NONE);
}

/// An identity on a session with no OS MIDI layer is refused where it is set,
/// and the policy in force is kept; presenting nothing is always accepted.
#[test]
fn an_identity_is_refused_on_a_session_with_no_os_midi_layer() {
    let s = Session::builder()
        .sample_rate(SampleRate::of(48_000.0))
        .build()
        .expect("builds");
    assert_eq!(
        s.set(s.policy::<MidiPolicy>().to(policy(2, "My App")))
            .unwrap_err()
            .kind,
        ValueErrorKind::Refused(SetRefusal::MidiPolicy(MidiPolicyError::NoHardware(
            tutti_session::NoHardware
        )))
    );
    assert_eq!(s.get(s.policy::<MidiPolicy>()).unwrap().identity, None);
    assert_eq!(
        s.set(s.policy::<MidiPolicy>().to(MidiPolicy::default())),
        Ok(())
    );
}

/// M2-104 §7.1.6.2: after a Stream Configuration Request is granted, the
/// session's output to that device sends in the Protocol granted, the
/// Notification and the output reading one stream state. The output was
/// MIDI 2.0 from the session's own negotiation, so a note it sends after the
/// grant of MIDI 1.0 is a MIDI 1.0 message.
#[test]
fn a_granted_request_moves_the_sessions_output_to_the_protocol_asked() {
    let (mut s, fake, ports) = wired(0);
    s.set(s.policy::<MidiPolicy>().to(policy(2, "My App")))
        .unwrap();
    let mut clock = Clock::new();
    clock.run(&mut s, &fake, 50);
    let out = &ports.output;
    assert_eq!(out.info().config().protocol, Protocol::Midi2);

    let ask = Ump::from(Message::Stream(Stream::StreamConfigurationRequest(
        StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: false,
        },
    )));
    let sent = clock.say(&mut s, &fake, ask);
    assert_eq!(
        streams(&sent),
        [Stream::StreamConfigurationNotification(StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: false,
        })]
    );

    let clip = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));
    s.graph()
        .set_sources(EventInputPort::new(out.node(), 0), [clip.event_output(0)])
        .expect("the port's node takes events");
    let _ = s.update();
    play(&s);
    blocks(&mut s, 2);
    let _ = s.update();
    let sent = fake.take_sent(EndpointId::from_raw(RAW));
    let notes: Vec<_> = sent
        .iter()
        .filter(|u| u.note_key_event().is_some())
        .map(|u| u.message_type())
        .collect();
    assert_eq!(
        notes,
        [MessageType::Midi1ChannelVoice],
        "the note on, as MIDI 1.0"
    );
}

/// M2-104 §7.1.6.3: under JR timing the device sends JR Timestamps to the
/// session, so the Notification the session sends reports that it receives them.
#[test]
fn under_jitter_reduction_the_endpoint_reports_that_it_receives_jr() {
    let (mut s, fake, _ports) = wired_with(
        0,
        MidiHardwareConfig::new().input_timing(tutti_midi_hardware::InputTiming::JitterReduction),
    );
    s.set(s.policy::<MidiPolicy>().to(policy(2, "My App")))
        .unwrap();
    let mut clock = Clock::new();
    clock.run(&mut s, &fake, 50);
    let sent = streams(&clock.say(&mut s, &fake, ask_everything()));
    let config = sent.iter().find_map(|s| match s {
        Stream::StreamConfigurationNotification(c) => Some(*c),
        _ => None,
    });
    assert_eq!(
        config,
        Some(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: false,
        }),
        "{sent:?}"
    );
}

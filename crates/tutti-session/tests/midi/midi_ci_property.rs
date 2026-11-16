//! Property Exchange with a MIDI-CI peer as typed jobs (M2-103): a Get and a Set
//! of a Resource the peer's ResourceList names, each driven over the fake OS by a
//! peer that serves a store, refuses or stays silent, with the session's clock set
//! by the test.

use std::time::{Duration, Instant};

use tutti_midi_device::{BodyLimit, BufferLimit, PeCiMessage, PeMessageKind};
use tutti_midi_device::{
    CallError, CiCategories, CiHeader, CiMessage, Decoded, DiscoveryData, EndpointId, MaxSysexSize,
    PE_TIMEOUT, PeCapabilities, PeData, PeHeader, PeLocalOn, PeReassemblyLimits, PeResource,
    PeStatus, PortDirection, RequestRefused, ResourceEntry, ResourceResponder, ResourceStore,
    ResourceTarget, ResponderAction, ResponderSettings, SimultaneousRequests, StreamLimit,
};
use tutti_midi_hardware::test_support::FakeCounters;
use tutti_session::Peer as _;
use tutti_session::{
    Job, JobKind, JobStatus, Landed, ListedResource, MidiInput, PeFailure, PeerError, Session,
    SessionEvent,
};
use tutti_ump::{DeviceIdentity, ManufacturerId, MidiGroup, U7, Ump};

use crate::common::{blocks, play};
use crate::midi::{connect, hot_plug_session_ci, plug, update_at};

const RAW: u64 = 9;

/// What the fake peer was asked, in the order it arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Asked {
    Get(String),
    Set(String),
}

/// How the fake peer answers a Get or a Set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    /// As the Responder of its store does.
    Serve,
    /// Nothing.
    Silent,
}

/// A MIDI-CI peer that declares Property Exchange and serves a store of one
/// Resource, LocalOn, off, and one declared with no data, DeviceInfo.
struct PePeer {
    device: tutti_midi_device::Endpoint,
    assembler: tutti_ump::SysexAssembler<Box<[u8]>>,
    muid: tutti_midi_device::Muid,
    responder: ResourceResponder,
    reply: Reply,
    asked: Vec<Asked>,
}

impl PePeer {
    fn new(store: ResourceStore) -> Self {
        use tutti_midi_device::{Capabilities, Endpoint, FunctionBlock, Supports};
        use tutti_ump::{Direction, FirstGroup, FunctionBlockId, GroupSpan, SysexAssembler};
        let block = FunctionBlock::builder(
            FunctionBlockId::new(0).unwrap(),
            Direction::Bidirectional,
            FirstGroup::new(0).unwrap(),
            GroupSpan::new(1).unwrap(),
        )
        .midi_ci(2, 0)
        .name(tutti_midi_device::BlockName::new("Main").unwrap())
        .build()
        .unwrap();
        let muid = tutti_midi_device::Muid::of(0x0012_3457);
        let room = SysexAssembler::room_for(1024, 1, 16);
        Self {
            device: Endpoint::new(Supports::of(Capabilities {
                midi1: true,
                midi2: true,
                ..Capabilities::default()
            }))
            .with_function_block(block),
            assembler: SysexAssembler::new(vec![0; room].into_boxed_slice(), 1024, 16),
            muid,
            responder: ResourceResponder::new(
                muid,
                ResponderSettings {
                    capabilities: PeCapabilities::new(SimultaneousRequests::of(4)),
                    reassembly: PeReassemblyLimits {
                        streams: StreamLimit::of(8),
                        bytes: BufferLimit::of(1 << 16),
                    },
                    body_limit: BodyLimit::of(1 << 16),
                },
                store,
            ),
            reply: Reply::Serve,
            asked: Vec::new(),
        }
    }

    /// What the session sent the peer, answered as the peer would.
    fn answer(&mut self, fake: &FakeCounters, now: Instant) {
        use tutti_ump::SysexStep;
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(EndpointId::from_raw(RAW)) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                let _ = self.device.reply(message, &mut answers);
                continue;
            }
            self.assembler.begin_block();
            let SysexStep::Complete(done) = self.assembler.push(ump) else {
                continue;
            };
            let decoded = CiMessage::decode(self.assembler.message(done).payload());
            if let Decoded::Message(CiMessage::Discovery { header, .. }) = &decoded {
                self.responder.accept(header.source, MaxSysexSize::of(512));
                let data = DiscoveryData::new(
                    DeviceIdentity {
                        manufacturer: ManufacturerId::of(0x41),
                        family: [U7::of(1); 2],
                        model: [U7::of(2); 2],
                        revision: [U7::of(3); 4],
                    },
                    CiCategories::PROPERTY_EXCHANGE,
                    MaxSysexSize::of(1024),
                );
                answers.extend(
                    CiMessage::DiscoveryReply {
                        header: CiHeader::new(self.muid),
                        to: header.source,
                        data,
                    }
                    .packets(MidiGroup::FIRST),
                );
                continue;
            }
            if let Decoded::Message(CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            }) = &decoded
                && !chunk.header().is_empty()
                && let Ok(PeHeader::Request(request)) = PeHeader::parse(chunk.header())
            {
                let name = request.resource.as_str().to_owned();
                // The ResourceList is the session's own, not the app's.
                if name != "ResourceList" {
                    if *kind == PeMessageKind::GET_INQUIRY {
                        self.asked.push(Asked::Get(name));
                    } else if *kind == PeMessageKind::SET_INQUIRY {
                        self.asked.push(Asked::Set(name));
                    }
                    if self.reply == Reply::Silent {
                        continue;
                    }
                }
            }
            for action in self.responder.on_message(&decoded, now) {
                if let ResponderAction::Send(message) = action {
                    answers.extend(message.packets(MidiGroup::FIRST));
                }
            }
        }
        for ump in answers {
            assert!(fake.send_input(EndpointId::from_raw(RAW), ump, now));
        }
    }
}

fn store() -> ResourceStore {
    let mut store = ResourceStore::new();
    store.declare(ResourceEntry::new(PeResource::LocalOn).unwrap());
    store.declare(ResourceEntry::new(PeResource::DeviceInfo).unwrap());
    store
        .put(None, PeData::LocalOn(PeLocalOn(false)))
        .expect("declared");
    store
}

/// A session over a fake OS with one Property Exchange peer on the input
/// `"wire"` whose ResourceList has arrived, and the session's clock.
struct Wire {
    s: Session,
    fake: FakeCounters,
    port: MidiInput,
    peer: PePeer,
    start: Instant,
    ms: u64,
    landed: Vec<Landed>,
}

impl Wire {
    fn new(store: ResourceStore) -> Self {
        let (mut s, fake) = hot_plug_session_ci(DeviceIdentity {
            manufacturer: ManufacturerId::NON_COMMERCIAL,
            family: [U7::of(1); 2],
            model: [U7::of(2); 2],
            revision: [U7::of(3); 4],
        });
        plug(&fake, RAW, "Wire", PortDirection::Duplex);
        let _ = s.update();
        connect(&mut s, &["wire"]);
        let port = s.midi_input(&"wire").expect("loaded");
        play(&s);
        let mut wire = Self {
            s,
            fake,
            port,
            peer: PePeer::new(store),
            start: Instant::now(),
            ms: 0,
            landed: Vec::new(),
        };
        wire.run(120);
        wire
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms {
            self.step(1);
        }
    }

    fn step(&mut self, by: u64) {
        self.ms += by;
        let now = self.start + Duration::from_millis(self.ms);
        for event in update_at(&mut self.s, now) {
            if let SessionEvent::Landed(landed) = event {
                self.landed.push(landed);
            }
        }
        self.peer.answer(&self.fake, now);
        blocks(&self.s, 1);
    }

    fn result<J: JobKind>(&mut self, job: &Job<J>) -> Option<Result<J::Output, J::Error>> {
        self.landed.iter_mut().find_map(|landed| landed.take(job))
    }

    /// The Resource the peer's ResourceList names `resource`, as the port's info
    /// reads now.
    fn listed(&self, resource: &PeResource) -> ListedResource {
        let info = self.port.info();
        let [peer] = info.resource_peers() else {
            panic!("one peer lists Resources: {:?}", info.resource_peers());
        };
        peer.listed()
            .find(|listed| listed.resource() == resource)
            .unwrap_or_else(|| panic!("{resource:?} is listed"))
    }
}

fn on(value: bool) -> PeData {
    PeData::LocalOn(PeLocalOn(value))
}

/// M2-103 section 14: the peer's ResourceList is what the input's info lists, one
/// handle for each entry, in the order the peer gave them.
#[test]
fn the_info_lists_the_resources_of_the_peers_resource_list() {
    let wire = Wire::new(store());
    let info = wire.port.info();
    let [peer] = info.resource_peers() else {
        panic!("one peer: {:?}", info.resource_peers());
    };
    let listed: Vec<_> = peer.listed().map(|l| l.resource().clone()).collect();
    assert_eq!(listed, [PeResource::LocalOn, PeResource::DeviceInfo]);
}

/// M2-103 section 8: a Get lands the Property Data the peer replied with, and the
/// peer was asked once, by the Resource's name.
#[test]
fn a_fetch_lands_the_peers_property_data() {
    let mut wire = Wire::new(store());
    let job = wire.listed(&PeResource::LocalOn).fetch();
    wire.run(60);
    assert_eq!(wire.result(&job), Some(Ok(on(false))));
    assert_eq!(wire.peer.asked, [Asked::Get("LocalOn".to_owned())]);
}

/// M2-103 section 10: a Set lands the status of the reply, 200, and the peer
/// holds the data afterward.
#[test]
fn a_write_lands_status_200_and_the_peer_holds_the_data() {
    let mut wire = Wire::new(store());
    let job = wire.listed(&PeResource::LocalOn).write(on(true));
    wire.run(60);
    let Some(Ok(status)) = wire.result(&job) else {
        panic!("a status: {:?}", wire.landed);
    };
    assert_eq!(status.code(), 200);
    assert_eq!(wire.peer.asked, [Asked::Set("LocalOn".to_owned())]);
    let target = ResourceTarget::new(PeResource::LocalOn).unwrap();
    assert_eq!(wire.peer.responder.store().get(&target), Some(&on(true)));
}

/// M2-103 section 7.4.1: a Resource the peer lists with no data replies 404, which
/// lands as the peer's status with its header, and is not a NAK.
#[test]
fn a_fetch_the_peer_answers_404_is_a_status_not_a_nak() {
    let mut wire = Wire::new(store());
    let job = wire.listed(&PeResource::DeviceInfo).fetch();
    wire.run(60);
    let Some(Err(PeerError::Refused(PeFailure::Status(status, header)))) = wire.result(&job) else {
        panic!("a status: {:?}", wire.landed);
    };
    assert_eq!(status.code(), 404);
    assert_eq!(status, PeStatus::NotFound);
    assert_eq!(header.status, PeStatus::NotFound);
}

/// M2-103 section 12.2: a Get no one answers lands `TimedOut` after the Property
/// Exchange timeout since it was sent, and not before.
#[test]
fn a_fetch_nothing_answers_times_out_after_the_property_exchange_timeout() {
    let mut wire = Wire::new(store());
    wire.peer.reply = Reply::Silent;
    let job = wire.listed(&PeResource::LocalOn).fetch();
    wire.run(5);
    let wait = PE_TIMEOUT.as_millis() as u64;
    wire.step(wait - 20);
    assert_eq!(wire.result(&job), None, "still waiting");
    wire.step(40);
    wire.run(1);
    assert_eq!(wire.result(&job), Some(Err(PeerError::TimedOut)));
}

/// A peer that leaves before it answers ends the call as `Disconnected`.
#[test]
fn a_fetch_whose_peer_leaves_before_it_answers_is_disconnected() {
    let mut wire = Wire::new(store());
    wire.peer.reply = Reply::Silent;
    let job = wire.listed(&PeResource::LocalOn).fetch();
    wire.run(5);
    let muid = wire.peer.muid;
    for ump in (CiMessage::InvalidateMuid {
        header: CiHeader::new(muid),
        target: muid,
    })
    .packets(MidiGroup::FIRST)
    {
        let now = wire.start + Duration::from_millis(wire.ms);
        assert!(wire.fake.send_input(EndpointId::from_raw(RAW), ump, now));
    }
    wire.run(5);
    assert_eq!(wire.result(&job), Some(Err(PeerError::Disconnected)));
}

/// An identical Get is not sent again: both jobs land with its answer.
#[test]
fn an_identical_repeated_fetch_shares_the_answer_and_is_sent_once() {
    let mut wire = Wire::new(store());
    let res = wire.listed(&PeResource::LocalOn);
    let first = res.fetch();
    let second = res.fetch();
    wire.run(60);
    assert_eq!(wire.peer.asked, [Asked::Get("LocalOn".to_owned())]);
    assert_eq!(wire.result(&first), Some(Ok(on(false))));
    assert_eq!(wire.result(&second), Some(Ok(on(false))));
}

/// A Set asked while a Get of the same Resource is out waits behind it: the peer is
/// asked the Get and then the Set, in that order, and a Get after the Set reads
/// what it wrote.
#[test]
fn a_write_behind_a_fetch_is_sent_after_it_and_a_later_fetch_reads_it() {
    let mut wire = Wire::new(store());
    let res = wire.listed(&PeResource::LocalOn);
    let get = res.fetch();
    let set = res.write(on(true));
    let again = res.fetch();
    wire.run(120);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Get("LocalOn".to_owned()),
            Asked::Set("LocalOn".to_owned()),
            Asked::Get("LocalOn".to_owned())
        ]
    );
    assert_eq!(wire.result(&get), Some(Ok(on(false))));
    assert!(matches!(wire.result(&set), Some(Ok(_))));
    assert_eq!(wire.result(&again), Some(Ok(on(true))));
}

/// Data of another Resource is not a Set of this one: the job ends at the call
/// and nothing is sent.
#[test]
fn a_write_of_another_resources_data_ends_at_the_call_and_sends_nothing() {
    let mut wire = Wire::new(store());
    let job = wire.listed(&PeResource::DeviceInfo).write(on(true));
    assert_eq!(job.status(), JobStatus::Done);
    wire.run(30);
    let Some(Err(PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused))))) =
        wire.result(&job)
    else {
        panic!("not sent: {:?}", wire.landed);
    };
    assert!(
        matches!(refused, RequestRefused::DataOfAnotherResource { .. }),
        "{refused:?}"
    );
    assert!(wire.peer.asked.is_empty());
}

/// A request of a port that is lost fails at the call: the job is ended before any
/// update, and lands `Disconnected` in the next one.
#[test]
fn a_request_on_a_lost_port_fails_at_the_call() {
    let mut wire = Wire::new(store());
    let res = wire.listed(&PeResource::LocalOn);
    wire.fake.unplug(EndpointId::from_raw(RAW));
    wire.run(2);
    let get = res.fetch();
    let set = res.write(on(true));
    assert_eq!(get.status(), JobStatus::Done);
    assert_eq!(set.status(), JobStatus::Done);
    wire.run(1);
    assert_eq!(wire.result(&get), Some(Err(PeerError::Disconnected)));
    assert_eq!(wire.result(&set), Some(Err(PeerError::Disconnected)));
}

/// A request pending when the entry is released ends as `Released`, and one made
/// after it ends at the call.
#[test]
fn a_request_pending_when_the_port_is_released_ends_released() {
    let mut wire = Wire::new(store());
    wire.peer.reply = Reply::Silent;
    let res = wire.listed(&PeResource::LocalOn);
    let pending = res.fetch();
    wire.run(5);
    wire.s
        .pool(Vec::<
            tutti_session::Declared<&str, tutti_session::MidiInputEntry>,
        >::new())
        .expect("declares nothing");
    wire.run(3);
    assert_eq!(wire.result(&pending), Some(Err(PeerError::Released)));
    let late = res.fetch();
    assert_eq!(late.status(), JobStatus::Done, "ended at the call");
    wire.run(1);
    assert_eq!(wire.result(&late), Some(Err(PeerError::Released)));
}

/// A session that ends with a call pending ends the job, and a handle that
/// outlives the session ends what it asks at the call.
#[test]
fn a_session_dropped_with_a_call_pending_ends_the_job() {
    let mut wire = Wire::new(store());
    wire.peer.reply = Reply::Silent;
    let res = wire.listed(&PeResource::LocalOn);
    let pending = res.fetch();
    wire.run(5);
    let queued = res.write(on(true));
    drop(wire);
    assert_eq!(pending.status(), JobStatus::Done);
    assert_eq!(queued.status(), JobStatus::Done);
    assert_eq!(res.fetch().status(), JobStatus::Done);
}

/// M2-103 section 8.5: a Resource whose entries are named by a `resId` is not
/// fetched without one: the job ends at the call as not sent, and the peer hears
/// nothing.
#[test]
fn a_fetch_of_a_resource_that_needs_a_res_id_is_not_sent() {
    let mut states = ResourceStore::new();
    states.declare(ResourceEntry::new(PeResource::State).unwrap());
    let mut wire = Wire::new(states);
    let job = wire.listed(&PeResource::State).fetch();
    assert_eq!(job.status(), JobStatus::Done);
    wire.run(30);
    let Some(Err(PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused))))) =
        wire.result(&job)
    else {
        panic!("not sent: {:?}", wire.landed);
    };
    assert!(
        matches!(refused, RequestRefused::ResIdRequired(_)),
        "{refused:?}"
    );
    assert!(wire.peer.asked.is_empty());
}

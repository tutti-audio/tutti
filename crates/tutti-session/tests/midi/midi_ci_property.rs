//! Property Exchange with a MIDI-CI peer as typed jobs (M2-103): a Get and a Set
//! of a Resource the peer's ResourceList names, each driven over the fake OS by a
//! peer that serves a store, refuses or stays silent, with the session's clock set
//! by the test.

use std::time::Duration;

use tutti_platform::Instant;

use tutti_midi_ci::{BodyLimit, BufferLimit, PeCiMessage, PeMessageKind};
use tutti_midi_ci::{
    CallError, CiCategories, CiHeader, CiMessage, Decoded, DiscoveryData, JsonPointer,
    MaxSysexSize, PE_TIMEOUT, PartialUpdate, PartialValue, PeCapabilities, PeData, PeExternalSync,
    PeHeader, PeJson, PeLocalOn, PeReassemblyLimits, PeResource, PeStatus, PeSubscription,
    RequestRefused, ResourceEntry, ResourceResponder, ResourceStore, ResourceTarget,
    ResponderAction, ResponderSettings, SimultaneousRequests, StreamLimit,
};
use tutti_midi_device::{EndpointId, PortDirection};
use tutti_midi_hardware::test_support::FakeCounters;
use tutti_session::Peer as _;
use tutti_session::{
    Job, JobKind, JobOutput, JobStatus, ListedResource, MidiInput, PeFailure, PeerError, Presence,
    Resource as _, ResourceFact, ResourceId, Session, SessionEvent, Subscription,
    SubscriptionCause,
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
    Subscribe(String),
    Unsubscribe,
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
    muid: tutti_midi_ci::Muid,
    responder: ResourceResponder,
    store: ResourceStore,
    reply: Reply,
    asked: Vec<Asked>,
}

impl PePeer {
    fn new(store: ResourceStore, requests: u8) -> Self {
        use tutti_midi_device::{Capabilities, Endpoint, FunctionBlock, Supports};
        use tutti_ump::{BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, SysexAssembler};
        let block = FunctionBlock::builder(
            FunctionBlockId::new(0).unwrap(),
            BlockDirection::Bidirectional,
            FirstGroup::new(0).unwrap(),
            GroupSpan::new(1).unwrap(),
        )
        .midi_ci(2, 0)
        .name(tutti_ump::BlockName::new("Main").unwrap())
        .build()
        .unwrap();
        let muid = tutti_midi_ci::Muid::of(0x0012_3457);
        let room = SysexAssembler::room_for(1024, 1, 16);
        Self {
            device: Endpoint::new(Supports::of(Capabilities {
                midi1: true,
                midi2: true,
                ..Capabilities::default()
            }))
            .block(block),
            assembler: SysexAssembler::new(vec![0; room].into_boxed_slice(), 1024, 16),
            muid,
            responder: ResourceResponder::new(
                muid,
                ResponderSettings {
                    capabilities: PeCapabilities::new(SimultaneousRequests::of(requests)),
                    reassembly: PeReassemblyLimits {
                        streams: StreamLimit::of(8),
                        bytes: BufferLimit::of(1 << 16),
                    },
                    body_limit: BodyLimit::of(1 << 16),
                },
            ),
            store,
            reply: Reply::Serve,
            asked: Vec::new(),
        }
    }

    /// What the session sent the peer, answered as the peer would.
    /// Sends what the Responder made of an update the test asked for.
    fn send(&self, fake: &FakeCounters, actions: Vec<ResponderAction>, now: Instant) {
        for action in actions {
            if let ResponderAction::Send(message) = action {
                for ump in message.packets(MidiGroup::FIRST).unwrap() {
                    assert!(fake.send_input(EndpointId::from_raw(RAW), ump, now));
                }
            }
        }
    }

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
                        revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
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
                    .packets(MidiGroup::FIRST)
                    .unwrap(),
                );
                continue;
            }
            if let Decoded::Message(CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            }) = &decoded
                && *kind == PeMessageKind::SUBSCRIPTION_INQUIRY
                && !chunk.header().is_empty()
                && let Ok(PeHeader::Subscription(subscription)) = PeHeader::try_from(chunk.header())
            {
                match subscription {
                    PeSubscription::Start(request) => {
                        self.asked
                            .push(Asked::Subscribe(request.resource.as_str().to_owned()));
                    }
                    PeSubscription::End { .. } => self.asked.push(Asked::Unsubscribe),
                    _ => {}
                }
            }
            if let Decoded::Message(CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            }) = &decoded
                && !chunk.header().is_empty()
                && let Ok(PeHeader::Request(request)) = PeHeader::try_from(chunk.header())
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
            for action in self.responder.on_message(&mut self.store, &decoded, now) {
                if let ResponderAction::Send(message) = action {
                    answers.extend(message.packets(MidiGroup::FIRST).unwrap());
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
    store.declare(ResourceEntry::builder(PeResource::LocalOn).build().unwrap());
    store.declare(
        ResourceEntry::builder(PeResource::DeviceInfo)
            .build()
            .unwrap(),
    );
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
    /// The port's handle, which keeps it open until it is taken.
    port: Option<MidiInput>,
    peer: PePeer,
    start: Instant,
    ms: u64,
    outputs: Vec<JobOutput>,
    /// What the updates noticed of resources, in order.
    changed: Vec<(ResourceId, ResourceFact)>,
}

impl Wire {
    /// The port, while its handle is held.
    fn port(&self) -> &MidiInput {
        self.port.as_ref().expect("the port is held")
    }

    fn new(store: ResourceStore) -> Self {
        Self::taking(store, 4)
    }

    /// A peer that takes `requests` requests at once.
    fn taking(store: ResourceStore, requests: u8) -> Self {
        let (mut s, fake) = hot_plug_session_ci(DeviceIdentity {
            manufacturer: ManufacturerId::NON_COMMERCIAL,
            family: [U7::of(1); 2],
            model: [U7::of(2); 2],
            revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
        });
        plug(&fake, RAW, "Wire", PortDirection::Duplex);
        let _ = s.update();
        let port = connect(&mut s, &["wire"]).remove(0);
        play(&s);
        let mut wire = Self {
            s,
            fake,
            port: Some(port),
            peer: PePeer::new(store, requests),
            start: Instant::now(),
            ms: 0,
            outputs: Vec::new(),
            changed: Vec::new(),
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
            match event {
                SessionEvent::JobDone(output) => self.outputs.push(output),
                SessionEvent::ResourceChanged { id, fact } => self.changed.push((id, fact)),
                _ => {}
            }
        }
        self.peer.answer(&self.fake, now);
        blocks(&self.s, 1);
    }

    fn result<J: JobKind>(&mut self, job: &Job<J>) -> Option<Result<J::Output, J::Error>> {
        self.outputs.iter_mut().find_map(|output| output.take(job))
    }

    /// The Resource the peer's ResourceList names `resource`, as the port's info
    /// reads now.
    fn listed(&self, resource: &PeResource) -> ListedResource {
        let info = self.port().info();
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
    let info = wire.port().info();
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
        panic!("a status: {:?}", wire.outputs);
    };
    assert_eq!(status.code(), 200);
    assert_eq!(wire.peer.asked, [Asked::Set("LocalOn".to_owned())]);
    let target = ResourceTarget::try_from(PeResource::LocalOn).unwrap();
    assert_eq!(wire.peer.store.get(&target), Some(&on(true)));
}

/// M2-103 section 7.4.1: a Resource the peer lists with no data replies 404, which
/// lands as the peer's status with its header, and is not a NAK.
#[test]
fn a_fetch_the_peer_answers_404_is_a_status_not_a_nak() {
    let mut wire = Wire::new(store());
    let job = wire.listed(&PeResource::DeviceInfo).fetch();
    wire.run(60);
    let Some(Err(PeerError::Refused(PeFailure::Status(status, header)))) = wire.result(&job) else {
        panic!("a status: {:?}", wire.outputs);
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
    .unwrap()
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
        panic!("not sent: {:?}", wire.outputs);
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

/// A request pending when the port's last handle is dropped ends as `Released`, and one made
/// after it ends at the call.
#[test]
fn a_request_pending_when_the_port_is_released_ends_released() {
    let mut wire = Wire::new(store());
    wire.peer.reply = Reply::Silent;
    let res = wire.listed(&PeResource::LocalOn);
    let pending = res.fetch();
    wire.run(5);
    drop(wire.port.take());
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
    states.declare(ResourceEntry::builder(PeResource::State).build().unwrap());
    let mut wire = Wire::new(states);
    let job = wire.listed(&PeResource::State).fetch();
    assert_eq!(job.status(), JobStatus::Done);
    wire.run(30);
    let Some(Err(PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused))))) =
        wire.result(&job)
    else {
        panic!("not sent: {:?}", wire.outputs);
    };
    assert!(
        matches!(refused, RequestRefused::ResIdRequired(_)),
        "{refused:?}"
    );
    assert!(wire.peer.asked.is_empty());
}

/// A second store Resource with data, for the tests of two Resources.
fn synced() -> ResourceStore {
    let mut store = store();
    store.declare(
        ResourceEntry::builder(PeResource::ExternalSync)
            .build()
            .unwrap(),
    );
    store
        .put(None, PeData::ExternalSync(PeExternalSync(true)))
        .expect("declared");
    store
}

/// M2-103 section 3.1: past the requests the peer's Capabilities say it takes at
/// once, a call waits for room instead of failing: with a peer that takes one,
/// two Gets of two Resources are both answered, and reach the wire one after the
/// other in the order asked.
#[test]
fn calls_past_the_peers_simultaneous_requests_wait_for_room_and_all_land() {
    let mut wire = Wire::taking(synced(), 1);
    let first = wire.listed(&PeResource::LocalOn).fetch();
    let second = wire.listed(&PeResource::ExternalSync).fetch();
    let third = wire.listed(&PeResource::LocalOn).write(on(true));
    wire.run(200);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Get("LocalOn".to_owned()),
            Asked::Get("ExternalSync".to_owned()),
            Asked::Set("LocalOn".to_owned())
        ]
    );
    assert_eq!(wire.result(&first), Some(Ok(on(false))));
    assert_eq!(
        wire.result(&second),
        Some(Ok(PeData::ExternalSync(PeExternalSync(true))))
    );
    assert!(matches!(wire.result(&third), Some(Ok(_))));
}

/// M2-103 section 14.2: a Set of a Resource the peer's entry says is read only is
/// refused by the Initiator before anything is sent, and lands as not sent with
/// the rule it broke.
#[test]
fn a_write_of_a_read_only_resource_is_not_sent_with_the_rule_it_broke() {
    let table_6 = tutti_midi_ci::PeJson::try_from(
        br#"{"version":"1.0","manufacturerId":[125,0,0],"manufacturer":"Educational Use",
        "familyId":[0,0],"family":"Test Range","modelId":[48,0],"model":"Workbench",
        "versionId":[0,0,1,0]}"#
            .as_slice(),
    )
    .expect("json");
    let info = PeData::decode(&PeResource::DeviceInfo, &table_6).expect("DeviceInfo");
    let mut wire = Wire::new(store());
    let job = wire.listed(&PeResource::DeviceInfo).write(info);
    assert_eq!(job.status(), JobStatus::Pending, "refused by the Initiator");
    wire.run(30);
    let Some(Err(PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused))))) =
        wire.result(&job)
    else {
        panic!("not sent: {:?}", wire.outputs);
    };
    assert!(
        matches!(refused, RequestRefused::CannotSet { .. }),
        "{refused:?}"
    );
    assert!(wire.peer.asked.is_empty());
}

/// An identical Set joins the Set in flight: one is sent and both land.
#[test]
fn an_identical_repeated_write_shares_the_answer_and_is_sent_once() {
    let mut wire = Wire::new(store());
    let res = wire.listed(&PeResource::LocalOn);
    let first = res.write(on(true));
    let second = res.write(on(true));
    wire.run(60);
    assert_eq!(wire.peer.asked, [Asked::Set("LocalOn".to_owned())]);
    assert!(matches!(wire.result(&first), Some(Ok(_))));
    assert!(matches!(wire.result(&second), Some(Ok(_))));
}

/// A Get identical to one that waits behind a Set joins the waiting one: the
/// peer is asked the Set and the Get once each, and both Gets read what the Set
/// wrote.
#[test]
fn a_fetch_joins_an_identical_fetch_still_waiting_behind_a_write() {
    let mut wire = Wire::new(store());
    let res = wire.listed(&PeResource::LocalOn);
    let set = res.write(on(true));
    let first = res.fetch();
    let second = res.fetch();
    wire.run(100);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Set("LocalOn".to_owned()),
            Asked::Get("LocalOn".to_owned())
        ]
    );
    assert!(matches!(wire.result(&set), Some(Ok(_))));
    assert_eq!(wire.result(&first), Some(Ok(on(true))));
    assert_eq!(wire.result(&second), Some(Ok(on(true))));
}

/// A call pending when the device goes ends as `Disconnected`, and the port takes
/// calls again once the device returns and its peer is found.
#[test]
fn a_call_pending_across_a_replug_ends_and_the_returned_port_takes_new_ones() {
    let mut wire = Wire::new(store());
    wire.peer.reply = Reply::Silent;
    let pending = wire.listed(&PeResource::LocalOn).fetch();
    wire.run(5);
    wire.fake.unplug(EndpointId::from_raw(RAW));
    wire.run(3);
    assert_eq!(wire.result(&pending), Some(Err(PeerError::Disconnected)));
    plug(&wire.fake, RAW, "Wire", PortDirection::Duplex);
    wire.peer.reply = Reply::Serve;
    wire.run(200);
    let again = wire.listed(&PeResource::LocalOn).fetch();
    wire.run(60);
    assert_eq!(wire.result(&again), Some(Ok(on(false))));
}

// --- Subscriptions (M2-103 section 11) ---

fn device_info(model: &str) -> PeData {
    let json = format!(
        r#"{{"version":"1.0","manufacturerId":[125,0,0],"manufacturer":"Educational Use",
        "familyId":[0,0],"family":"Test Range","modelId":[48,0],"model":"{model}",
        "versionId":[0,0,1,0]}}"#
    );
    let json = PeJson::try_from(json.as_bytes()).expect("json");
    PeData::decode(&PeResource::DeviceInfo, &json).expect("DeviceInfo")
}

/// A store whose LocalOn (on) and DeviceInfo ("Workbench") can be subscribed to.
fn subscribable() -> ResourceStore {
    let mut store = ResourceStore::new();
    store.declare(
        ResourceEntry::builder(PeResource::LocalOn)
            .can_subscribe(true)
            .build()
            .unwrap(),
    );
    store.declare(
        ResourceEntry::builder(PeResource::DeviceInfo)
            .can_subscribe(true)
            .build()
            .unwrap(),
    );
    store.put(None, on(true)).expect("declared");
    store.put(None, device_info("Workbench")).expect("declared");
    store
}

impl Wire {
    fn now(&self) -> Instant {
        self.start + Duration::from_millis(self.ms)
    }

    /// What the live Subscriptions hold of `resource`, as the port's info reads now.
    fn held(&self, resource: &PeResource) -> Option<PeData> {
        let info = self.port().info();
        let listed = self.listed(resource);
        info.resource_peers()[0].property(&listed).cloned()
    }

    /// Subscribes to `resource` and runs until the lease and its first Property
    /// Data have come.
    fn subscribed(&mut self, resource: &PeResource) -> Subscription {
        let job = self.listed(resource).subscribe();
        self.run(80);
        match self.result(&job) {
            Some(Ok(lease)) => lease,
            other => panic!("a lease: {other:?}"),
        }
    }

    fn target(resource: PeResource) -> ResourceTarget {
        ResourceTarget::try_from(resource).expect("a Resource with no entries")
    }

    /// How many times the peer was told of a change of the info of the port.
    fn info_notices(&self) -> usize {
        let id = self.port().id();
        self.changed
            .iter()
            .filter(|c| **c == (id, ResourceFact::Info))
            .count()
    }
}

/// M2-103 section 11: a Subscription lands a live lease once the peer accepts, the
/// peer is asked to start it and then to send the Resource, and the peer's record
/// holds the Property Data the Get returned.
#[test]
fn a_subscription_lands_live_and_the_record_holds_the_resources_data() {
    let mut wire = Wire::new(subscribable());
    assert_eq!(wire.held(&PeResource::LocalOn), None);
    let lease = wire.subscribed(&PeResource::LocalOn);
    assert_eq!(lease.presence(), Presence::Live);
    assert_eq!(*lease.resource(), PeResource::LocalOn);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Subscribe("LocalOn".to_owned()),
            Asked::Get("LocalOn".to_owned())
        ]
    );
    assert_eq!(wire.held(&PeResource::LocalOn), Some(on(true)));
    assert!(
        wire.info_notices() >= 1,
        "the record changed: {:?}",
        wire.changed
    );
}

/// M2-103 section 11.1: a `full` update replaces the data the record holds, and the
/// change is noticed as a change of the info.
#[test]
fn a_full_update_replaces_the_data_the_record_holds() {
    let mut wire = Wire::new(subscribable());
    let _lease = wire.subscribed(&PeResource::LocalOn);
    let before = wire.info_notices();
    let now = wire.now();
    let actions = wire
        .peer
        .responder
        .publish(
            &mut wire.peer.store,
            &Wire::target(PeResource::LocalOn),
            on(false),
            now,
        )
        .expect("publishes");
    wire.peer.send(&wire.fake, actions, now);
    wire.run(40);
    assert_eq!(wire.held(&PeResource::LocalOn), Some(on(false)));
    assert_eq!(wire.info_notices(), before + 1);
}

/// M2-103 section 11.1: a `partial` update is merged into the data held, and what it
/// does not name is kept.
#[test]
fn a_partial_update_is_merged_into_the_data_the_record_holds() {
    let mut wire = Wire::new(subscribable());
    let _lease = wire.subscribed(&PeResource::DeviceInfo);
    let now = wire.now();
    let update = PartialUpdate::new().with(
        JsonPointer::new("/model").expect("a pointer"),
        PartialValue::String("Renamed".to_owned()),
    );
    let actions = wire
        .peer
        .responder
        .change(
            &mut wire.peer.store,
            &Wire::target(PeResource::DeviceInfo),
            update,
            now,
        )
        .expect("changes");
    wire.peer.send(&wire.fake, actions, now);
    wire.run(40);
    let Some(PeData::DeviceInfo(info)) = wire.held(&PeResource::DeviceInfo) else {
        panic!("DeviceInfo is held");
    };
    assert_eq!(info.model.as_str(), "Renamed");
    assert_eq!(info.family.as_str(), "Test Range");
}

/// M2-103 section 11.1.1: an update too large for one chunk is a `notify`, and the
/// session Gets the Resource: the record holds what the Get returned.
#[test]
fn a_notify_makes_the_session_get_the_resource_and_the_record_holds_the_result() {
    let mut wire = Wire::new(subscribable());
    let _lease = wire.subscribed(&PeResource::DeviceInfo);
    let long = "x".repeat(600);
    let now = wire.now();
    let actions = wire
        .peer
        .responder
        .publish(
            &mut wire.peer.store,
            &Wire::target(PeResource::DeviceInfo),
            device_info(&long),
            now,
        )
        .expect("publishes");
    wire.peer.send(&wire.fake, actions, now);
    let gets = |wire: &Wire| {
        wire.peer
            .asked
            .iter()
            .filter(|a| **a == Asked::Get("DeviceInfo".to_owned()))
            .count()
    };
    let before = gets(&wire);
    wire.run(80);
    assert_eq!(gets(&wire), before + 1, "the session Got after the notify");
    let Some(PeData::DeviceInfo(info)) = wire.held(&PeResource::DeviceInfo) else {
        panic!("DeviceInfo is held");
    };
    assert_eq!(info.model.as_str(), long);
}

/// Dropping the lease ends the Subscription with the peer, through the port's queue,
/// and the record forgets the data.
#[test]
fn dropping_the_lease_ends_the_subscription_with_the_peer() {
    let mut wire = Wire::new(subscribable());
    let lease = wire.subscribed(&PeResource::LocalOn);
    assert_eq!(wire.peer.responder.subscriptions().count(), 1);
    drop(lease);
    wire.run(60);
    assert_eq!(wire.peer.asked.last(), Some(&Asked::Unsubscribe));
    assert_eq!(wire.peer.responder.subscriptions().count(), 0);
    assert_eq!(wire.held(&PeResource::LocalOn), None);
}

/// A job dropped before the peer accepts lands nowhere, and the Subscription it
/// started ends as the lease it would have carried drops.
#[test]
fn a_dropped_subscribe_job_does_not_leave_a_subscription_with_the_peer() {
    let mut wire = Wire::new(subscribable());
    drop(wire.listed(&PeResource::LocalOn).subscribe());
    wire.run(120);
    assert_eq!(
        wire.peer.asked.first(),
        Some(&Asked::Subscribe("LocalOn".to_owned()))
    );
    assert_eq!(wire.peer.asked.last(), Some(&Asked::Unsubscribe));
    assert_eq!(wire.peer.responder.subscriptions().count(), 0);
}

/// M2-103 section 11.3: the peer ending the Subscription is the lease's presence,
/// noticed with the lease's own id, and the record forgets the data.
#[test]
fn the_peer_ending_the_subscription_is_the_leases_presence() {
    let mut wire = Wire::new(subscribable());
    let lease = wire.subscribed(&PeResource::LocalOn);
    let id = wire
        .peer
        .responder
        .subscriptions()
        .next()
        .expect("held")
        .0
        .clone();
    let now = wire.now();
    let actions = wire
        .peer
        .responder
        .end_subscription(&mut wire.peer.store, &id, now);
    wire.peer.send(&wire.fake, actions, now);
    wire.run(40);
    assert_eq!(
        lease.presence(),
        Presence::Lost(SubscriptionCause::EndedByPeer)
    );
    assert!(wire.changed.contains(&(lease.id(), ResourceFact::Presence)));
    assert_eq!(wire.held(&PeResource::LocalOn), None);
}

/// A port that closes under a Subscription ends it: the lease reads `PortLost`,
/// noticed with its own id, and dropping it afterward sends nothing.
#[test]
fn a_subscription_whose_port_is_lost_reads_port_lost() {
    let mut wire = Wire::new(subscribable());
    let lease = wire.subscribed(&PeResource::LocalOn);
    wire.fake.unplug(EndpointId::from_raw(RAW));
    wire.run(3);
    assert_eq!(
        lease.presence(),
        Presence::Lost(SubscriptionCause::PortLost)
    );
    assert!(wire.changed.contains(&(lease.id(), ResourceFact::Presence)));
    let asked = wire.peer.asked.len();
    drop(lease);
    wire.run(3);
    assert_eq!(wire.peer.asked.len(), asked);
}

/// A Subscription counts against the peer's simultaneous requests like a Get: with
/// a peer that takes one, a Get and two Subscriptions all land, in the order asked.
#[test]
fn subscriptions_past_the_peers_simultaneous_requests_wait_for_room() {
    let mut wire = Wire::taking(subscribable(), 1);
    let get = wire.listed(&PeResource::DeviceInfo).fetch();
    let first = wire.listed(&PeResource::LocalOn).subscribe();
    let second = wire.listed(&PeResource::DeviceInfo).subscribe();
    wire.run(300);
    assert_eq!(wire.result(&get), Some(Ok(device_info("Workbench"))));
    assert!(matches!(wire.result(&first), Some(Ok(_))));
    assert!(matches!(wire.result(&second), Some(Ok(_))));
    let at = |wanted: Asked| {
        wire.peer
            .asked
            .iter()
            .position(|a| *a == wanted)
            .unwrap_or_else(|| panic!("{wanted:?} was asked: {:?}", wire.peer.asked))
    };
    assert_eq!(at(Asked::Get("DeviceInfo".to_owned())), 0);
    assert!(
        at(Asked::Subscribe("LocalOn".to_owned())) < at(Asked::Subscribe("DeviceInfo".to_owned()))
    );
}

/// M2-103 section 14.2: a Resource whose entry does not allow a Subscription is not
/// asked: the job lands as not sent, and the peer hears nothing.
#[test]
fn a_subscribe_to_a_resource_the_entry_forbids_is_not_sent() {
    let mut wire = Wire::new(store());
    let job = wire.listed(&PeResource::LocalOn).subscribe();
    wire.run(30);
    let Some(Err(PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused))))) =
        wire.result(&job)
    else {
        panic!("not sent: {:?}", wire.outputs);
    };
    assert!(
        matches!(refused, RequestRefused::CannotSubscribe(_)),
        "{refused:?}"
    );
    assert!(wire.peer.asked.is_empty());
}

/// A peer that leaves MIDI-CI takes its Subscriptions with it: the lease reads
/// `EndedByPeer`, noticed with its own id, and the peer is no longer listed.
#[test]
fn a_peer_that_leaves_ends_its_leases_as_ended_by_the_peer() {
    let mut wire = Wire::new(subscribable());
    let lease = wire.subscribed(&PeResource::LocalOn);
    let muid = wire.peer.muid;
    for ump in (CiMessage::InvalidateMuid {
        header: CiHeader::new(muid),
        target: muid,
    })
    .packets(MidiGroup::FIRST)
    .unwrap()
    {
        let now = wire.now();
        assert!(wire.fake.send_input(EndpointId::from_raw(RAW), ump, now));
    }
    wire.run(10);
    assert_eq!(
        lease.presence(),
        Presence::Lost(SubscriptionCause::EndedByPeer)
    );
    assert!(wire.changed.contains(&(lease.id(), ResourceFact::Presence)));
    assert!(wire.port().info().resource_peers().is_empty());
}

/// A lease whose port was unplugged stays lost across the device's return, with its
/// one notice, and the returned port takes a new Subscription.
#[test]
fn a_lease_lost_with_its_port_stays_lost_across_a_replug() {
    let mut wire = Wire::new(subscribable());
    let lease = wire.subscribed(&PeResource::LocalOn);
    wire.fake.unplug(EndpointId::from_raw(RAW));
    wire.run(3);
    assert_eq!(
        lease.presence(),
        Presence::Lost(SubscriptionCause::PortLost)
    );
    plug(&wire.fake, RAW, "Wire", PortDirection::Duplex);
    wire.run(200);
    assert_eq!(
        lease.presence(),
        Presence::Lost(SubscriptionCause::PortLost)
    );
    let notices = wire
        .changed
        .iter()
        .filter(|c| **c == (lease.id(), ResourceFact::Presence))
        .count();
    assert_eq!(notices, 1);
    let again = wire.subscribed(&PeResource::LocalOn);
    assert_eq!(again.presence(), Presence::Live);
}

/// A Get after a `notify` that the peer never answers leaves the data held out of
/// date, so the record drops it rather than read it as current.
#[test]
fn a_sync_that_fails_clears_the_data_the_record_holds() {
    let mut wire = Wire::new(subscribable());
    let _lease = wire.subscribed(&PeResource::DeviceInfo);
    assert!(wire.held(&PeResource::DeviceInfo).is_some());
    wire.peer.reply = Reply::Silent;
    let long = "x".repeat(600);
    let now = wire.now();
    let actions = wire
        .peer
        .responder
        .publish(
            &mut wire.peer.store,
            &Wire::target(PeResource::DeviceInfo),
            device_info(&long),
            now,
        )
        .expect("publishes");
    wire.peer.send(&wire.fake, actions, now);
    wire.run(20);
    assert!(
        wire.held(&PeResource::DeviceInfo).is_some(),
        "the old data is held while the Get is out"
    );
    wire.step(PE_TIMEOUT.as_millis() as u64 + 100);
    wire.run(5);
    assert_eq!(wire.held(&PeResource::DeviceInfo), None);
}

/// The end of a Subscription is a request like another: past the peer's count it
/// waits for room and then goes out.
#[test]
fn an_unsubscribe_past_the_peers_simultaneous_requests_waits_and_goes_out() {
    let mut wire = Wire::taking(subscribable(), 1);
    let lease = wire.subscribed(&PeResource::LocalOn);
    wire.peer.reply = Reply::Silent;
    let get = wire.listed(&PeResource::DeviceInfo).fetch();
    wire.run(10);
    drop(lease);
    wire.run(10);
    assert_ne!(wire.peer.asked.last(), Some(&Asked::Unsubscribe));
    wire.peer.reply = Reply::Serve;
    wire.step(PE_TIMEOUT.as_millis() as u64 + 100);
    wire.run(60);
    assert_eq!(wire.result(&get), Some(Err(PeerError::TimedOut)));
    assert_eq!(wire.peer.asked.last(), Some(&Asked::Unsubscribe));
    assert_eq!(wire.peer.responder.subscriptions().count(), 0);
}

/// A lease that outlives its released port drops without sending and without
/// hanging.
#[test]
fn a_lease_dropped_after_its_port_was_released_sends_nothing() {
    let mut wire = Wire::new(subscribable());
    let lease = wire.subscribed(&PeResource::LocalOn);
    drop(wire.port.take());
    wire.run(5);
    assert_eq!(
        lease.presence(),
        Presence::Lost(SubscriptionCause::PortLost)
    );
    let asked = wire.peer.asked.len();
    drop(lease);
    wire.run(5);
    assert_eq!(wire.peer.asked.len(), asked);
}

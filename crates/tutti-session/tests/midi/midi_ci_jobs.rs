//! A request to a MIDI-CI peer is a typed job (M2-101 sections 7 and 9): a
//! Profile switch and a MIDI Message Report, each driven over the fake OS by a
//! peer that answers, refuses or stays silent, with the session's clock set by
//! the test.

use std::time::Duration;

use tutti_platform::Instant;

use tutti_midi_ci::{
    ChannelControllers, CiHeader, CiMessage, DataControl, NakStatus, NoteData, PEER_REPLY_TIMEOUT,
    PeTerminator, ProfileChannels, ProfileState, ReportRequest, ReportScope, SystemMessages,
};
use tutti_midi_device::{EndpointId, PortDirection};
use tutti_midi_hardware::ChannelCount;
use tutti_midi_hardware::test_support::FakeCounters;
use tutti_session::Peer as _;
use tutti_session::{
    ChannelProfile, Job, JobKind, JobOutput, JobStatus, ListedProfile, MidiInput, MidiRouteRule,
    PeerError, ReportPeer, Session, SessionEvent,
};
use tutti_ump::{DeviceIdentity, ManufacturerId, Message, MidiGroup, U7, Voice2};

use crate::common::{Seen, blocks, play, tap};
use crate::midi::{Asked, MpePeer, Reply, connect, hot_plug_session_ci, plug, update_at};

/// A session over a fake OS with one MIDI-CI peer on the input `"wire"`, whose
/// Profiles and Process Inquiry are known, and the session's clock.
struct Wire {
    s: Session,
    fake: FakeCounters,
    port: MidiInput,
    peer: MpePeer,
    seen: Seen,
    start: Instant,
    ms: u64,
    outputs: Vec<JobOutput>,
}

impl Wire {
    fn new(switch: Reply, report: Reply) -> Self {
        let (mut s, _bg, fake) = hot_plug_session_ci(DeviceIdentity {
            manufacturer: ManufacturerId::NON_COMMERCIAL,
            family: [U7::of(1); 2],
            model: [U7::of(2); 2],
            revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
        });
        let (synth, seen) = tap(&mut s);
        s.midi()
            .route(MidiRouteRule::any_channel().to(synth))
            .expect("routes");
        plug(&fake, MpePeer::RAW, "Wire", PortDirection::Duplex);
        let _ = s.update();
        connect(&mut s, &["wire"]);
        let port = s.midi_input(&"wire").expect("loaded");
        play(&s);
        let mut peer = MpePeer::new();
        peer.switch = switch;
        peer.report = report;
        let mut wire = Self {
            s,
            fake,
            port,
            peer,
            seen,
            start: Instant::now(),
            ms: 0,
            outputs: Vec::new(),
        };
        wire.run(60);
        wire
    }

    /// Runs the session and the peer for `ms` milliseconds, one at a time.
    fn run(&mut self, ms: u64) {
        for _ in 0..ms {
            self.step(1);
        }
    }

    /// One update `by` milliseconds on, and what the peer then answers.
    fn step(&mut self, by: u64) {
        self.ms += by;
        let now = self.start + Duration::from_millis(self.ms);
        for event in update_at(&mut self.s, now) {
            if let SessionEvent::JobDone(output) = event {
                self.outputs.push(output);
            }
        }
        self.peer.answer(&self.fake, now);
        blocks(&self.s, 1);
    }

    /// What `job` landed with, once.
    fn result<J: JobKind>(&mut self, job: &Job<J>) -> Option<Result<J::Output, J::Error>> {
        self.outputs.iter_mut().find_map(|output| output.take(job))
    }

    /// The Profile the peer lists on Channel 1, as the port's info reads now.
    fn profile(&self) -> ChannelProfile {
        let info = self.port.info();
        let [peer] = info.profiles() else {
            panic!("one peer lists Profiles: {:?}", info.profiles());
        };
        match peer.listed().next() {
            Some(ListedProfile::Channel(profile)) => profile,
            other => panic!("one Profile on a Channel: {other:?}"),
        }
    }

    fn reporter(&self) -> ReportPeer {
        let info = self.port.info();
        let [peer] = info.report_peers() else {
            panic!("one peer can report: {:?}", info.report_peers());
        };
        peer.clone()
    }

    fn state(&self) -> ProfileState {
        let info = self.port.info();
        info.profiles()[0].listed().next().expect("listed").state()
    }
}

fn four() -> ChannelCount {
    ChannelCount::new(4).expect("a count")
}

fn everything() -> ReportRequest {
    ReportRequest::new(
        DataControl::Full,
        ReportScope::FunctionBlock {
            system: SystemMessages::empty(),
            controllers: ChannelControllers::CONTROL_CHANGE,
            notes: NoteData::NOTES,
        },
    )
}

/// The peer's Enabled Report lands the switch with the state it reports, and
/// the port's list holds it.
#[test]
fn a_switch_the_peer_enables_lands_enabled() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    assert_eq!(wire.state(), ProfileState::Disabled);
    let job = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.result(&job), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.state(), ProfileState::Enabled);
}

/// The peer's Disabled Report lands the switch with the state it reports.
#[test]
fn a_switch_the_peer_disables_lands_disabled() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let enable = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.result(&enable), Some(Ok(ProfileState::Enabled)));
    let job = wire.profile().disable();
    wire.run(30);
    assert_eq!(wire.result(&job), Some(Ok(ProfileState::Disabled)));
    assert_eq!(wire.state(), ProfileState::Disabled);
}

/// A NAK of the switch is `Refused`, with the peer's NAK, and leaves the
/// Profile as it was.
#[test]
fn a_switch_the_peer_naks_is_refused_with_its_nak() {
    let mut wire = Wire::new(Reply::Refuse, Reply::Answer);
    let job = wire.profile().enable(four());
    wire.run(30);
    let Some(Err(PeerError::Refused(nak))) = wire.result(&job) else {
        panic!("a refusal: {:?}", wire.outputs);
    };
    assert_eq!(nak.status, NakStatus::ProfileNotSupported);
    assert_eq!(wire.state(), ProfileState::Disabled);
}

/// A switch no one answers lands `TimedOut` once the peer reply timeout has
/// passed since it was sent, and not before.
#[test]
fn a_switch_nothing_answers_times_out_after_the_peer_reply_timeout() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let job = wire.profile().enable(four());
    wire.run(5);
    let wait = PEER_REPLY_TIMEOUT.as_millis() as u64;
    wire.step(wait - 20);
    assert_eq!(wire.result(&job), None, "still waiting");
    wire.step(20);
    wire.run(1);
    assert_eq!(wire.result(&job), Some(Err(PeerError::TimedOut)));
}

/// A peer that leaves before it answers ends the switch as `Disconnected`.
#[test]
fn a_switch_whose_peer_leaves_before_it_answers_is_disconnected() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let job = wire.profile().enable(four());
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
        assert!(
            wire.fake
                .send_input(EndpointId::from_raw(MpePeer::RAW), ump, now)
        );
    }
    wire.run(5);
    assert_eq!(wire.result(&job), Some(Err(PeerError::Disconnected)));
}

/// A request of a port that is lost fails at the call: the job is ended before
/// any update, and lands `Disconnected` in the next one.
#[test]
fn a_request_on_a_lost_port_fails_at_the_call() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let profile = wire.profile();
    let reporter = wire.reporter();
    wire.fake.unplug(EndpointId::from_raw(MpePeer::RAW));
    wire.run(2);
    let switch = profile.enable(four());
    let report = reporter.request(everything());
    assert_eq!(switch.status(), JobStatus::Done);
    assert_eq!(report.status(), JobStatus::Done);
    wire.run(1);
    assert_eq!(wire.result(&switch), Some(Err(PeerError::Disconnected)));
    assert_eq!(wire.result(&report), Some(Err(PeerError::Disconnected)));
}

/// A job that is dropped lands nothing, and the peer's answer still moves the
/// Profile.
#[test]
fn a_dropped_switch_lands_nothing_and_the_peers_answer_still_applies() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    drop(wire.profile().enable(four()));
    wire.run(30);
    assert!(wire.outputs.is_empty(), "{:?}", wire.outputs);
    assert_eq!(wire.state(), ProfileState::Enabled);
}

/// A report lands at the peer's End, and the messages it carried reached the
/// graph as the input's MIDI.
#[test]
fn a_report_lands_at_the_peers_end_and_its_messages_reach_the_graph() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let job = wire.reporter().request(everything());
    wire.run(30);
    assert_eq!(wire.result(&job), Some(Ok(())));
    let reported = wire
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, event)| {
            matches!(
                event.decode(),
                Ok(Message::Midi2(_, Voice2::NoteOn { note, .. }))
                    if note.get() == MpePeer::REPORTED_NOTE
            )
        })
        .count();
    assert_eq!(reported, 1, "the report's one Note On reached the graph");
}

/// A NAK of the report is `Refused`, with the peer's NAK.
#[test]
fn a_report_the_peer_naks_is_refused_with_its_nak() {
    let mut wire = Wire::new(Reply::Answer, Reply::Refuse);
    let job = wire.reporter().request(everything());
    wire.run(30);
    let Some(Err(PeerError::Refused(nak))) = wire.result(&job) else {
        panic!("a refusal: {:?}", wire.outputs);
    };
    assert_eq!(nak.status, NakStatus::Terminate(PeTerminator::Responder));
}

/// A report no one answers lands `TimedOut` after the peer reply timeout.
#[test]
fn a_report_nothing_answers_times_out_after_the_peer_reply_timeout() {
    let mut wire = Wire::new(Reply::Answer, Reply::Silent);
    let job = wire.reporter().request(everything());
    wire.run(5);
    wire.step(PEER_REPLY_TIMEOUT.as_millis() as u64);
    wire.run(1);
    assert_eq!(wire.result(&job), Some(Err(PeerError::TimedOut)));
}

/// A peer that leaves while its report is out ends the job as `Disconnected`.
#[test]
fn a_report_whose_peer_leaves_before_its_end_is_disconnected() {
    let mut wire = Wire::new(Reply::Answer, Reply::Silent);
    let job = wire.reporter().request(everything());
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
        assert!(
            wire.fake
                .send_input(EndpointId::from_raw(MpePeer::RAW), ump, now)
        );
    }
    wire.run(5);
    assert_eq!(wire.result(&job), Some(Err(PeerError::Disconnected)));
}

/// A report with another scope than `everything`.
fn notes_only() -> ReportRequest {
    ReportRequest::new(
        DataControl::Full,
        ReportScope::FunctionBlock {
            system: SystemMessages::empty(),
            controllers: ChannelControllers::empty(),
            notes: NoteData::NOTES,
        },
    )
}

fn scope(request: ReportRequest) -> ReportScope {
    request.scope
}

/// A request identical to the one in flight is not sent again: both jobs land
/// with its answer.
#[test]
fn an_identical_repeated_switch_shares_the_answer_and_is_sent_once() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let first = wire.profile().enable(four());
    let second = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.peer.asked, [Asked::On(ProfileChannels::of(4))]);
    assert_eq!(wire.result(&first), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&second), Some(Ok(ProfileState::Enabled)));
}

/// A disable asked while an enable is out waits behind it: the peer is asked
/// Set On and then Set Off, in that order, and each job lands the state of its
/// own answer.
#[test]
fn a_disable_behind_an_enable_is_sent_after_it_and_lands_its_own_state() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let enable = wire.profile().enable(four());
    let disable = wire.profile().disable();
    wire.run(40);
    assert_eq!(
        wire.peer.asked,
        [Asked::On(ProfileChannels::of(4)), Asked::Off]
    );
    assert_eq!(wire.result(&enable), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&disable), Some(Ok(ProfileState::Disabled)));
    assert_eq!(wire.state(), ProfileState::Disabled);
}

/// An enable with another Channel count is a different request: it waits behind
/// the first and is sent with its own count.
#[test]
fn an_enable_with_another_count_waits_behind_and_is_sent_with_its_own() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let two = wire.profile().enable(ChannelCount::new(2).unwrap());
    let three = wire.profile().enable(ChannelCount::new(3).unwrap());
    wire.run(40);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::On(ProfileChannels::of(2)),
            Asked::On(ProfileChannels::of(3))
        ]
    );
    assert_eq!(wire.result(&two), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&three), Some(Ok(ProfileState::Enabled)));
}

/// An identical report request shares the report in flight.
#[test]
fn an_identical_repeated_report_shares_the_answer_and_is_sent_once() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let first = wire.reporter().request(everything());
    let second = wire.reporter().request(everything());
    wire.run(30);
    assert_eq!(wire.peer.asked, [Asked::Report(scope(everything()))]);
    assert_eq!(wire.result(&first), Some(Ok(())));
    assert_eq!(wire.result(&second), Some(Ok(())));
}

/// A report that asks for something else waits behind the one in flight, and is
/// sent when it has ended.
#[test]
fn a_report_of_another_kind_waits_behind_the_one_in_flight() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let first = wire.reporter().request(everything());
    let second = wire.reporter().request(notes_only());
    wire.run(40);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Report(scope(everything())),
            Asked::Report(scope(notes_only()))
        ]
    );
    assert_eq!(wire.result(&first), Some(Ok(())));
    assert_eq!(wire.result(&second), Some(Ok(())));
}

/// A request pending when the entry is released ends as `Released`, and one made
/// after it ends at the call.
#[test]
fn a_request_pending_when_the_port_is_released_ends_released() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let profile = wire.profile();
    let pending = profile.enable(four());
    wire.run(5);
    wire.s
        .pool(Vec::<
            tutti_session::Declared<&str, tutti_session::MidiInputEntry>,
        >::new())
        .expect("declares nothing");
    wire.run(3);
    assert_eq!(wire.result(&pending), Some(Err(PeerError::Released)));
    let late = profile.enable(four());
    assert_eq!(late.status(), JobStatus::Done, "ended at the call");
    wire.run(1);
    assert_eq!(wire.result(&late), Some(Err(PeerError::Released)));
}

/// A request pending when the device goes ends as `Disconnected`, and the port
/// takes requests again once the device returns and its peer is found.
#[test]
fn a_request_pending_across_a_replug_ends_and_the_returned_port_takes_new_ones() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let pending = wire.profile().enable(four());
    wire.run(5);
    let raw = EndpointId::from_raw(MpePeer::RAW);
    wire.fake.unplug(raw);
    wire.run(3);
    assert_eq!(wire.result(&pending), Some(Err(PeerError::Disconnected)));
    plug(&wire.fake, MpePeer::RAW, "Wire", PortDirection::Duplex);
    wire.peer.switch = Reply::Answer;
    wire.run(80);
    let again = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.result(&again), Some(Ok(ProfileState::Enabled)));
}

/// A session that ends with a request pending ends the job, and a handle that
/// outlives the session ends what it asks at the call: neither is left waiting.
#[test]
fn a_session_dropped_with_a_request_pending_ends_the_job() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let profile = wire.profile();
    let pending = profile.enable(four());
    wire.run(5);
    let queued = profile.disable();
    drop(wire);
    assert_eq!(pending.status(), JobStatus::Done);
    assert_eq!(queued.status(), JobStatus::Done);
    assert_eq!(profile.enable(four()).status(), JobStatus::Done);
}

/// A job lands in the update after the one whose poll read the answer, not in
/// that one.
#[test]
fn a_job_lands_in_the_update_after_the_poll_that_read_the_answer() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let job = wire.profile().enable(four());
    for _ in 0..30 {
        wire.step(1);
        if job.status() == JobStatus::Done {
            break;
        }
    }
    assert_eq!(job.status(), JobStatus::Done, "the poll read the answer");
    assert_eq!(wire.result(&job), None, "not yet in an event");
    wire.step(1);
    assert_eq!(wire.result(&job), Some(Ok(ProfileState::Enabled)));
}

/// An identical request that follows a different one is its own: enable, disable,
/// enable sends all three in order, and each job lands the state of its own
/// answer.
#[test]
fn an_enable_after_a_disable_is_sent_again_and_lands_enabled() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let first = wire.profile().enable(four());
    let off = wire.profile().disable();
    let last = wire.profile().enable(four());
    wire.run(60);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::On(ProfileChannels::of(4)),
            Asked::Off,
            Asked::On(ProfileChannels::of(4))
        ]
    );
    assert_eq!(wire.result(&first), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&off), Some(Ok(ProfileState::Disabled)));
    assert_eq!(wire.result(&last), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.state(), ProfileState::Enabled);
}

/// A report A, a report B and a report A again are three asks, in order.
#[test]
fn a_report_after_another_kind_is_asked_again() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let a1 = wire.reporter().request(everything());
    let b = wire.reporter().request(notes_only());
    let a2 = wire.reporter().request(everything());
    wire.run(60);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Report(scope(everything())),
            Asked::Report(scope(notes_only())),
            Asked::Report(scope(everything()))
        ]
    );
    for job in [&a1, &b, &a2] {
        assert_eq!(wire.result(job), Some(Ok(())));
    }
}

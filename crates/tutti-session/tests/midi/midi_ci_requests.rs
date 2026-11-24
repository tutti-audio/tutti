//! A request to a MIDI-CI peer is a future (M2-101 sections 7 and 9): a
//! Profile switch and a MIDI Message Report, each driven over the fake OS by a
//! peer that answers, refuses or stays silent, with the session's clock set by
//! the test.

use std::future::Future;
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
    ChannelProfile, ListedProfile, MidiInput, MidiRouteRule, PeerError, ReportPeer, Session,
};
use tutti_ump::{DeviceIdentity, ManufacturerId, Message, MidiGroup, U7, Voice2};

use crate::common::{Seen, blocks, play, tap};
use crate::midi::{Asked, MpePeer, Reply, connect, hot_plug_session_ci, plug, polled, update_at};

/// A session over a fake OS with one MIDI-CI peer on the input `"wire"`, whose
/// Profiles and Process Inquiry are known, and the session's clock.
struct Wire {
    s: Session,
    fake: FakeCounters,
    /// The port's handle, which keeps it open until it is taken.
    port: Option<MidiInput>,
    peer: MpePeer,
    seen: Seen,
    start: Instant,
    ms: u64,
}

impl Wire {
    /// The port, while its handle is held.
    fn port(&self) -> &MidiInput {
        self.port.as_ref().expect("the port is held")
    }

    fn new(switch: Reply, report: Reply) -> Self {
        let (mut s, fake) = hot_plug_session_ci(DeviceIdentity {
            manufacturer: ManufacturerId::NON_COMMERCIAL,
            family: [U7::of(1); 2],
            model: [U7::of(2); 2],
            revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
        });
        let (synth, seen) = tap(&mut s);
        s.midi()
            .route(MidiRouteRule::new().to(synth))
            .expect("routes");
        plug(&fake, MpePeer::RAW, "Wire", PortDirection::Duplex);
        let _ = s.update();
        let port = connect(&mut s, &["wire"]).remove(0);
        play(&s);
        let mut peer = MpePeer::new();
        peer.switch = switch;
        peer.report = report;
        let mut wire = Self {
            s,
            fake,
            port: Some(port),
            peer,
            seen,
            start: Instant::now(),
            ms: 0,
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
        let _ = update_at(&mut self.s, now);
        self.peer.answer(&self.fake, now);
        blocks(&mut self.s, 1);
    }

    /// What `request` resolved to, or `None` while it is pending: polled without
    /// an update.
    fn result<F: Future + Unpin>(&mut self, request: &mut F) -> Option<F::Output> {
        polled(request)
    }

    /// The Profile the peer lists on Channel 1, as the port's info reads now.
    fn profile(&self) -> ChannelProfile {
        let info = self.port().info();
        let [listed] = info.peers() else {
            panic!("one peer: {:?}", info.peers());
        };
        let peer = listed.profiles().expect("the peer lists Profiles");
        match peer.listed().next() {
            Some(ListedProfile::Channel(profile)) => profile,
            other => panic!("one Profile on a Channel: {other:?}"),
        }
    }

    fn reporter(&self) -> ReportPeer {
        let info = self.port().info();
        let [listed] = info.peers() else {
            panic!("one peer: {:?}", info.peers());
        };
        listed.report().expect("the peer can report").clone()
    }

    fn state(&self) -> ProfileState {
        let info = self.port().info();
        info.peers()[0]
            .profiles()
            .expect("the peer lists Profiles")
            .listed()
            .next()
            .expect("listed")
            .state()
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

impl Wire {
    /// Awaits `request` while a thread plays the device and nothing updates the
    /// session: the answer reaches the future from the session's MIDI-CI task.
    /// The bound only guards a hang.
    fn awaited<F: Future>(&mut self, request: F) -> F::Output {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let (fake, now) = (
            self.fake.clone(),
            self.start + Duration::from_millis(self.ms),
        );
        let mut peer = std::mem::replace(&mut self.peer, MpePeer::new());
        let stop = Arc::new(AtomicBool::new(false));
        let executor = tutti_platform::Executor::new();
        let device = {
            let stop = Arc::clone(&stop);
            executor
                .spawn_dedicated("device", None, async move {
                    while !stop.load(Ordering::Relaxed) {
                        peer.answer(&fake, now);
                        std::thread::yield_now();
                    }
                    peer
                })
                .expect("a thread")
        };
        let out = tutti_fixtures::block_on_within(request, Duration::from_secs(120));
        stop.store(true, Ordering::Relaxed);
        self.peer = tutti_platform::join(device).expect("the device ended");
        out.expect("the request resolved with no update")
    }
}

/// **A future resolves when the device answers, with no update**: the
/// session's MIDI-CI task reads the Enabled Report and wakes the switch with
/// the state it reports.
#[test]
fn a_switch_resolves_when_the_device_answers_with_no_update() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let request = wire.profile().enable(four());
    assert_eq!(wire.awaited(request), Ok(ProfileState::Enabled));
}

/// As a switch, a report resolves at the peer's End with no update.
#[test]
fn a_report_resolves_when_the_device_answers_with_no_update() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let request = wire.reporter().request(everything());
    assert_eq!(wire.awaited(request), Ok(()));
}

/// The peer's Enabled Report resolves the switch with the state it reports, and
/// the port's list holds it.
#[test]
fn a_switch_the_peer_enables_lands_enabled() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    assert_eq!(wire.state(), ProfileState::Disabled);
    let mut request = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.result(&mut request), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.state(), ProfileState::Enabled);
}

/// The peer's Disabled Report resolves the switch with the state it reports.
#[test]
fn a_switch_the_peer_disables_lands_disabled() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut enable = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.result(&mut enable), Some(Ok(ProfileState::Enabled)));
    let mut request = wire.profile().disable();
    wire.run(30);
    assert_eq!(wire.result(&mut request), Some(Ok(ProfileState::Disabled)));
    assert_eq!(wire.state(), ProfileState::Disabled);
}

/// A NAK of the switch is `Refused`, with the peer's NAK, and leaves the
/// Profile as it was.
#[test]
fn a_switch_the_peer_naks_is_refused_with_its_nak() {
    let mut wire = Wire::new(Reply::Refuse, Reply::Answer);
    let mut request = wire.profile().enable(four());
    wire.run(30);
    let Some(Err(PeerError::Refused(nak))) = wire.result(&mut request) else {
        panic!("a refusal");
    };
    assert_eq!(nak.status, NakStatus::ProfileNotSupported);
    assert_eq!(wire.state(), ProfileState::Disabled);
}

/// A switch no one answers resolves `TimedOut` once the peer reply timeout has
/// passed since it was sent, and not before.
#[test]
fn a_switch_nothing_answers_times_out_after_the_peer_reply_timeout() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let mut request = wire.profile().enable(four());
    wire.run(5);
    let wait = PEER_REPLY_TIMEOUT.as_millis() as u64;
    wire.step(wait - 20);
    assert_eq!(wire.result(&mut request), None, "still waiting");
    wire.step(20);
    // The session's MIDI-CI task ends the request once it has read a time past
    // the timeout: stepped until it has.
    assert_eq!(
        settled_by(&mut wire, &mut request),
        Err(PeerError::TimedOut)
    );
}

/// A peer that leaves before it answers ends the switch as `Disconnected`.
#[test]
fn a_switch_whose_peer_leaves_before_it_answers_is_disconnected() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let mut request = wire.profile().enable(four());
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
    assert_eq!(
        wire.result(&mut request),
        Some(Err(PeerError::Disconnected))
    );
}

/// A request of a port that is lost fails at the call: its future is resolved as
/// `Disconnected` before any update.
#[test]
fn a_request_on_a_lost_port_fails_at_the_call() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let profile = wire.profile();
    let reporter = wire.reporter();
    wire.fake.unplug(EndpointId::from_raw(MpePeer::RAW));
    wire.run(2);
    let mut switch = profile.enable(four());
    let mut report = reporter.request(everything());
    assert_eq!(wire.result(&mut switch), Some(Err(PeerError::Disconnected)));
    assert_eq!(wire.result(&mut report), Some(Err(PeerError::Disconnected)));
}

/// A switch whose future is dropped is still sent, and the peer's answer still
/// moves the Profile.
#[test]
fn a_dropped_switch_is_still_sent_and_the_peers_answer_still_applies() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    drop(wire.profile().enable(four()));
    wire.run(30);
    assert_eq!(wire.peer.asked, [Asked::On(ProfileChannels::of(4))]);
    assert_eq!(wire.state(), ProfileState::Enabled);
}

/// A report resolves at the peer's End, and the messages it carried reached the
/// graph as the input's MIDI.
#[test]
fn a_report_lands_at_the_peers_end_and_its_messages_reach_the_graph() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut request = wire.reporter().request(everything());
    wire.run(30);
    assert_eq!(wire.result(&mut request), Some(Ok(())));
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
    let mut request = wire.reporter().request(everything());
    wire.run(30);
    let Some(Err(PeerError::Refused(nak))) = wire.result(&mut request) else {
        panic!("a refusal");
    };
    assert_eq!(nak.status, NakStatus::Terminate(PeTerminator::Responder));
}

/// A report no one answers resolves `TimedOut` after the peer reply timeout.
#[test]
fn a_report_nothing_answers_times_out_after_the_peer_reply_timeout() {
    let mut wire = Wire::new(Reply::Answer, Reply::Silent);
    let mut request = wire.reporter().request(everything());
    wire.run(5);
    wire.step(PEER_REPLY_TIMEOUT.as_millis() as u64);
    assert_eq!(
        settled_by(&mut wire, &mut request),
        Err(PeerError::TimedOut)
    );
}

/// A peer that leaves while its report is out ends the request as `Disconnected`.
#[test]
fn a_report_whose_peer_leaves_before_its_end_is_disconnected() {
    let mut wire = Wire::new(Reply::Answer, Reply::Silent);
    let mut request = wire.reporter().request(everything());
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
    assert_eq!(
        wire.result(&mut request),
        Some(Err(PeerError::Disconnected))
    );
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

/// A request identical to the one in flight is not sent again: both requests resolve
/// with its answer.
#[test]
fn an_identical_repeated_switch_shares_the_answer_and_is_sent_once() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut first = wire.profile().enable(four());
    let mut second = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.peer.asked, [Asked::On(ProfileChannels::of(4))]);
    assert_eq!(wire.result(&mut first), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&mut second), Some(Ok(ProfileState::Enabled)));
}

/// A disable asked while an enable is out waits behind it: the peer is asked
/// Set On and then Set Off, in that order, and each request resolves the state of its
/// own answer.
#[test]
fn a_disable_behind_an_enable_is_sent_after_it_and_lands_its_own_state() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut enable = wire.profile().enable(four());
    let mut disable = wire.profile().disable();
    wire.run(40);
    assert_eq!(
        wire.peer.asked,
        [Asked::On(ProfileChannels::of(4)), Asked::Off]
    );
    assert_eq!(wire.result(&mut enable), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&mut disable), Some(Ok(ProfileState::Disabled)));
    assert_eq!(wire.state(), ProfileState::Disabled);
}

/// An enable with another Channel count is a different request: it waits behind
/// the first and is sent with its own count.
#[test]
fn an_enable_with_another_count_waits_behind_and_is_sent_with_its_own() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut two = wire.profile().enable(ChannelCount::new(2).unwrap());
    let mut three = wire.profile().enable(ChannelCount::new(3).unwrap());
    wire.run(40);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::On(ProfileChannels::of(2)),
            Asked::On(ProfileChannels::of(3))
        ]
    );
    assert_eq!(wire.result(&mut two), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&mut three), Some(Ok(ProfileState::Enabled)));
}

/// An identical report request shares the report in flight.
#[test]
fn an_identical_repeated_report_shares_the_answer_and_is_sent_once() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut first = wire.reporter().request(everything());
    let mut second = wire.reporter().request(everything());
    wire.run(30);
    assert_eq!(wire.peer.asked, [Asked::Report(scope(everything()))]);
    assert_eq!(wire.result(&mut first), Some(Ok(())));
    assert_eq!(wire.result(&mut second), Some(Ok(())));
}

/// A report that asks for something else waits behind the one in flight, and is
/// sent when it has ended.
#[test]
fn a_report_of_another_kind_waits_behind_the_one_in_flight() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut first = wire.reporter().request(everything());
    let mut second = wire.reporter().request(notes_only());
    wire.run(40);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Report(scope(everything())),
            Asked::Report(scope(notes_only()))
        ]
    );
    assert_eq!(wire.result(&mut first), Some(Ok(())));
    assert_eq!(wire.result(&mut second), Some(Ok(())));
}

/// A request pending when the port's last handle is dropped ends as `Released`, and one made
/// after it ends at the call.
#[test]
fn a_request_pending_when_the_port_is_released_ends_released() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let profile = wire.profile();
    let mut pending = profile.enable(four());
    wire.run(5);
    drop(wire.port.take());
    wire.run(3);
    assert_eq!(wire.result(&mut pending), Some(Err(PeerError::Released)));
    let mut late = profile.enable(four());
    assert_eq!(
        wire.result(&mut late),
        Some(Err(PeerError::Released)),
        "ended at the call"
    );
}

/// A request pending when the device goes ends as `Disconnected`, and the port
/// takes requests again once the device returns and its peer is found.
#[test]
fn a_request_pending_across_a_replug_ends_and_the_returned_port_takes_new_ones() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let mut pending = wire.profile().enable(four());
    wire.run(5);
    let raw = EndpointId::from_raw(MpePeer::RAW);
    wire.fake.unplug(raw);
    wire.run(3);
    assert_eq!(
        wire.result(&mut pending),
        Some(Err(PeerError::Disconnected))
    );
    plug(&wire.fake, MpePeer::RAW, "Wire", PortDirection::Duplex);
    wire.peer.switch = Reply::Answer;
    wire.run(80);
    let mut again = wire.profile().enable(four());
    wire.run(30);
    assert_eq!(wire.result(&mut again), Some(Ok(ProfileState::Enabled)));
}

/// A session that ends with a request pending resolves it to `Disconnected`, and
/// a handle that outlives the session ends what it asks at the call: neither is
/// left waiting.
#[test]
fn a_session_dropped_with_a_request_pending_resolves_disconnected() {
    let mut wire = Wire::new(Reply::Silent, Reply::Answer);
    let profile = wire.profile();
    let mut pending = profile.enable(four());
    wire.run(5);
    let mut queued = profile.disable();
    drop(wire);
    assert_eq!(polled(&mut pending), Some(Err(PeerError::Disconnected)));
    assert_eq!(polled(&mut queued), Some(Err(PeerError::Disconnected)));
    assert_eq!(
        polled(&mut profile.enable(four())),
        Some(Err(PeerError::Disconnected))
    );
}

/// **A future resolves in the session's updates, not by itself**: polled with no
/// update it stays pending, and the updates that send the request and read the
/// peer's answer make it ready.
#[test]
fn a_switch_is_pending_until_the_session_updates() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut switch = wire.profile().enable(four());
    for _ in 0..3 {
        assert_eq!(wire.result(&mut switch), None, "no update has run");
    }
    assert_eq!(
        settled_by(&mut wire, &mut switch),
        Ok(ProfileState::Enabled)
    );
}

/// As a switch, a report is pending until the session updates.
#[test]
fn a_report_is_pending_until_the_session_updates() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut report = wire.reporter().request(everything());
    for _ in 0..3 {
        assert_eq!(wire.result(&mut report), None, "no update has run");
    }
    assert_eq!(settled_by(&mut wire, &mut report), Ok(()));
}

/// Steps the wire until `request` resolves. The bound is a count of simulated
/// milliseconds: it guards a request that never resolves.
fn settled_by<F: Future + Unpin>(wire: &mut Wire, request: &mut F) -> F::Output {
    for _ in 0..2000 {
        if let Some(out) = wire.result(request) {
            return out;
        }
        wire.step(1);
    }
    panic!("the request never resolved");
}

/// An identical request that follows a different one is its own: enable, disable,
/// enable sends all three in order, and each request resolves the state of its own
/// answer.
#[test]
fn an_enable_after_a_disable_is_sent_again_and_lands_enabled() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut first = wire.profile().enable(four());
    let mut off = wire.profile().disable();
    let mut last = wire.profile().enable(four());
    wire.run(60);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::On(ProfileChannels::of(4)),
            Asked::Off,
            Asked::On(ProfileChannels::of(4))
        ]
    );
    assert_eq!(wire.result(&mut first), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.result(&mut off), Some(Ok(ProfileState::Disabled)));
    assert_eq!(wire.result(&mut last), Some(Ok(ProfileState::Enabled)));
    assert_eq!(wire.state(), ProfileState::Enabled);
}

/// A report A, a report B and a report A again are three asks, in order.
#[test]
fn a_report_after_another_kind_is_asked_again() {
    let mut wire = Wire::new(Reply::Answer, Reply::Answer);
    let mut a1 = wire.reporter().request(everything());
    let mut b = wire.reporter().request(notes_only());
    let mut a2 = wire.reporter().request(everything());
    wire.run(60);
    assert_eq!(
        wire.peer.asked,
        [
            Asked::Report(scope(everything())),
            Asked::Report(scope(notes_only())),
            Asked::Report(scope(everything()))
        ]
    );
    for request in [&mut a1, &mut b, &mut a2] {
        assert_eq!(wire.result(request), Some(Ok(())));
    }
}

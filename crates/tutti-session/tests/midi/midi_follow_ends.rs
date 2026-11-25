//! A follower whose input node is removed ends: the session raises
//! `MidiEvent::Unfollowed`, drops the follower, and publishes the maps it stepped
//! as the base again, so the tempo is the app's and another follower is accepted.
//!
//! The oracles are the specification's: a Flex Set Tempo of 90 bpm is 6.0e9 / 90
//! ten-nanosecond units per quarter note (M2-104 7.5.3), and 24 Timing Clocks make
//! a quarter note (M1 p.30).

use crate::common::OpenPorts as _;
use crate::common::blocks;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_graph::{EventCapacity, EventTapOptions, TapAlign};
use tutti_midi_device::EndpointId;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_midi_protocol::SysexDeviceId;
use tutti_midi_runtime::MidiInputPorts;
use tutti_session::{
    ClockKind, FollowClock, FollowKind, FollowMachineControl, FollowTempo, MidiError, MidiEvent,
    MidiInput, Session, SessionEvent, UnfollowCause,
};
use tutti_ump::{Flex, FlexTempo, Message, MidiGroup, System};
use tutti_units::{Bpm, SampleRate};

const G: MidiGroup = MidiGroup::FIRST;
const OURS: SysexDeviceId = SysexDeviceId::of(5);

struct Rig {
    s: Session,
    fake: FakeCounters,
    first: MidiInput,
    second: MidiInput,
}

fn rig() -> Rig {
    let (backend, fake) = FakeBackend::build_with(
        vec![endpoint(1, "Keys"), endpoint(2, "Pads")],
        vec![],
        usize::MAX,
    );
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(48_000.0))
        .build()
        .expect("builds");
    let _ = s.update();
    let mut devices = s.midi().devices();
    let second = s.open_input(devices.remove(1)).expect("opens");
    let first = s.open_input(devices.remove(0)).expect("opens");
    Rig {
        s,
        fake,
        first,
        second,
    }
}

/// One follower: how it follows the first input, what makes it hear, which
/// follower of the second input the session refused while it held the tempo
/// (none for one that owns no tempo), and whether it is still following.
struct Follower {
    name: &'static str,
    kind: FollowKind,
    follow: fn(&mut Rig),
    refused_meanwhile: Option<fn(&mut Rig) -> Result<(), MidiError>>,
    still_following: fn(&mut Session) -> bool,
}

const FOLLOWERS: [Follower; 3] = [
    Follower {
        name: "the Flex tempo follower",
        kind: FollowKind::Tempo,
        follow: |r| {
            let node = r.first.node();
            r.s.midi()
                .follow_tempo(FollowTempo::new(node, G))
                .expect("follows");
        },
        refused_meanwhile: Some(|r| {
            let node = r.second.node();
            r.s.midi()
                .follow_clock(FollowClock::new(node, G, ClockKind::BeatClock))
        }),
        still_following: |s| s.midi().unfollow_tempo().is_ok(),
    },
    Follower {
        name: "the Beat Clock follower",
        kind: FollowKind::Clock(ClockKind::BeatClock),
        follow: |r| {
            let node = r.first.node();
            r.s.midi()
                .follow_clock(FollowClock::new(node, G, ClockKind::BeatClock))
                .expect("follows");
        },
        refused_meanwhile: Some(|r| {
            let node = r.second.node();
            r.s.midi().follow_tempo(FollowTempo::new(node, G))
        }),
        still_following: |s| s.midi().unfollow_clock().is_ok(),
    },
    Follower {
        name: "the Machine Control follower",
        kind: FollowKind::MachineControl,
        follow: |r| {
            let node = r.first.node();
            r.s.midi()
                .follow_machine_control(FollowMachineControl::new(node, G, OURS))
                .expect("follows");
        },
        refused_meanwhile: None,
        still_following: |s| s.midi().unfollow_machine_control().is_ok(),
    },
];

fn midi_events(s: &mut Session) -> Vec<MidiEvent> {
    s.update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Midi(e) => Some(e),
            _ => None,
        })
        .collect()
}

/// **A follower whose input node is removed ends on the update after the
/// removal is applied.** It is
/// reported once with its kind and the cause, it no longer follows, a follower
/// that owned the tempo no longer refuses another, and the tempo is the app's
/// 120 bpm again, not the 90 bpm the source had set.
#[test]
fn a_follower_whose_input_is_gone_ends_and_releases_what_it_held() {
    for f in FOLLOWERS {
        let mut r = rig();
        (f.follow)(&mut r);
        // The engine's block clock must have a reading to place arrivals on frames.
        for _ in 0..8 {
            blocks(&mut r.s, 1);
            let _ = r.s.update();
        }

        // A tempo source steps the map to 90 bpm, so a release is visible.
        match f.kind {
            FollowKind::Tempo => {
                let field = FlexTempo::new((6.0e9 / 90.0) as u32);
                assert!(r.fake.send_input(
                    EndpointId::from_raw(1),
                    Message::Flex(G, Flex::SetTempo(field)).into(),
                    Instant::now()
                ));
            }
            FollowKind::Clock(_) => {
                let period = Duration::from_secs_f64(60.0 / (90.0 * 24.0));
                let start = Instant::now();
                for k in 0..60 {
                    let tick = Message::System(G, System::TimingClock).into();
                    assert!(
                        r.fake
                            .send_input(EndpointId::from_raw(1), tick, start + period * k)
                    );
                }
            }
            _ => {}
        }
        let mut stepped = f.refused_meanwhile.is_none();
        for _ in 0..400 {
            if stepped {
                break;
            }
            blocks(&mut r.s, 1);
            stepped = midi_events(&mut r.s)
                .iter()
                .any(|e| matches!(e, MidiEvent::TempoFollowed { .. }));
        }
        assert!(stepped, "{} heard its source", f.name);
        if let Some(other) = f.refused_meanwhile {
            assert!(
                matches!(other(&mut r), Err(MidiError::TempoSourceTaken)),
                "{} holds the tempo",
                f.name
            );
        }

        let node = r.first.node();
        let _ = r.s.remove(node);
        // The update sends the removal, the block applies it, the next update
        // finds the tap's source gone; later updates add nothing.
        let mut events = midi_events(&mut r.s);
        assert!(
            events.is_empty(),
            "{}: not before the removal is applied",
            f.name
        );
        blocks(&mut r.s, 1);
        events.extend(midi_events(&mut r.s));
        for _ in 0..4 {
            blocks(&mut r.s, 8);
            events.extend(midi_events(&mut r.s));
        }

        let ended: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, MidiEvent::Unfollowed { .. }))
            .collect();
        assert!(
            matches!(
                ended.as_slice(),
                [MidiEvent::Unfollowed { follower, cause: UnfollowCause::InputGone }]
                    if *follower == f.kind
            ),
            "{}: one end, named, with its cause; got {ended:?}",
            f.name
        );
        assert!(
            !(f.still_following)(&mut r.s),
            "{} no longer follows",
            f.name
        );
        if let Some(other) = f.refused_meanwhile {
            other(&mut r).expect("the tempo is free again");
        }
    }
}

/// The maps a tempo follower stepped are published as the app's base when it
/// ends: the effective tempo is the 120 bpm project tempo, not the followed 90.
#[test]
fn an_ended_tempo_follower_publishes_the_base_again() {
    let mut r = rig();
    (FOLLOWERS[0].follow)(&mut r);
    let _ = r.s.update();
    let field = FlexTempo::new((6.0e9 / 90.0) as u32);
    assert!(r.fake.send_input(
        EndpointId::from_raw(1),
        Message::Flex(G, Flex::SetTempo(field)).into(),
        Instant::now()
    ));
    let mut heard = false;
    for _ in 0..400 {
        blocks(&mut r.s, 1);
        heard = midi_events(&mut r.s)
            .iter()
            .any(|e| matches!(e, MidiEvent::TempoFollowed { .. }));
        if heard {
            break;
        }
    }
    assert!(heard);
    assert_ne!(
        crate::tempo::effective(&r.s).constant_tempo(),
        Some(Bpm::of(120.0)),
        "the follower stepped the map"
    );
    let node = r.first.node();
    let _ = r.s.remove(node);
    let _ = r.s.update();
    blocks(&mut r.s, 1);
    let _ = r.s.update();
    assert_eq!(
        crate::tempo::effective(&r.s).constant_tempo(),
        Some(Bpm::of(120.0)),
        "the app's tempo again"
    );
}

/// **What the tap held is applied before the follower ends.** A tempo source's
/// Set Tempo that reached the tap before the node was removed is followed, then
/// the follower ends, and the tempo is the app's again.
#[test]
fn a_follower_applies_what_its_tap_held_before_it_ends() {
    let mut r = rig();
    (FOLLOWERS[0].follow)(&mut r);
    let _ = r.s.update();
    let port = MidiInputPorts::FIRST_GROUP
        .channelless(r.first.node(), G)
        .expect("the group is heard");
    let mut witness =
        r.s.graph()
            .taps()
            .open_event(
                port,
                EventTapOptions::new(EventCapacity::of(16)).align(TapAlign::Raw),
            )
            .expect("the port exists");
    let _ = r.s.update();
    let field = FlexTempo::new((6.0e9 / 90.0) as u32);
    assert!(r.fake.send_input(
        EndpointId::from_raw(1),
        Message::Flex(G, Flex::SetTempo(field)).into(),
        Instant::now()
    ));
    // The witness hears the message once it is in the input's ring; the session
    // is not updated meanwhile, so the follower has not read it.
    let mut arrived = false;
    for _ in 0..400 {
        blocks(&mut r.s, 1);
        arrived = witness.drain().next().is_some();
        if arrived {
            break;
        }
    }
    assert!(arrived, "the message reached the input");
    let node = r.first.node();
    let _ = r.s.remove(node);
    let mut events = midi_events(&mut r.s);
    blocks(&mut r.s, 1);
    events.extend(midi_events(&mut r.s));
    let followed = events
        .iter()
        .position(|e| matches!(e, MidiEvent::TempoFollowed { .. }));
    let ended = events
        .iter()
        .position(|e| matches!(e, MidiEvent::Unfollowed { .. }));
    assert!(
        matches!((followed, ended), (Some(f), Some(e)) if f < e),
        "followed, then ended; got {events:?}"
    );
    assert_eq!(
        crate::tempo::effective(&r.s).constant_tempo(),
        Some(Bpm::of(120.0)),
        "the app's tempo again"
    );
}

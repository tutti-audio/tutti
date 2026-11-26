//! A follower keeps hearing its source after the graph is re-rated: the tap it
//! reads is re-opened at the new rate, and what the source sends next is still
//! followed.
//!
//! The oracles are the specification's: a Flex Set Tempo of 90 bpm is 6.0e9 / 90
//! ten-nanosecond units per quarter note (M2-104 7.5.3), 24 Timing Clocks make a quarter
//! note (M1 p.30), and an MMC PLAY (RP-013 p.20, `02`) rolls the transport.

use crate::common::OpenPorts as _;
use crate::common::{Keyed, blocks};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_graph::Fade;
use tutti_midi_device::EndpointId;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_midi_protocol::SysexDeviceId;
use tutti_midi_runtime::{MidiClipConfig, MidiClipNode};
use tutti_session::{
    ClockKind, FollowClock, FollowMachineControl, FollowTempo, MidiEvent, Replacement, Session,
    SessionEvent,
};
use tutti_ump::{Flex, FlexTempo, Message, MidiGroup, Sysex7Packets, System, Ump};
use tutti_units::{Bpm, SampleRate, Samples};

const RATE: f64 = 48_000.0;
const KEYS: u64 = 1;
const G: MidiGroup = MidiGroup::FIRST;
const OURS: SysexDeviceId = SysexDeviceId::of(5);

/// One follower: how a session starts following the rig's input, what the
/// source then sends, and whether an update's events and the transport show it
/// was heard.
struct Follower {
    name: &'static str,
    follow: fn(&mut Keyed),
    send: fn(&FakeCounters),
    heard: fn(&Session, &[MidiEvent]) -> bool,
}

fn rig() -> (Keyed, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![endpoint(KEYS, "Keys")], vec![], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let _ = s.update();
    let keys = s.midi().devices().remove(0);
    let keys = s.open_input(keys).expect("opens");
    (Keyed::new(s, keys), fake)
}

fn send(fake: &FakeCounters, ump: Ump) {
    assert!(fake.send_input(EndpointId::from_raw(KEYS), ump, Instant::now()));
}

fn update_events(s: &mut Session) -> Vec<MidiEvent> {
    s.update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Midi(e) => Some(e),
            _ => None,
        })
        .collect()
}

const FOLLOWERS: [Follower; 3] = [
    Follower {
        name: "the Flex tempo follower",
        follow: |s| {
            let input = s.input();
            s.midi()
                .follow_tempo(FollowTempo::new(input, G))
                .expect("follows");
        },
        send: |fake| {
            let field = FlexTempo::new((6.0e9 / 90.0) as u32);
            send(fake, Message::Flex(G, Flex::SetTempo(field)).into());
        },
        heard: |_, events| {
            events.iter().any(|e| {
                matches!(e, MidiEvent::TempoFollowed { tempo, .. }
                    if (tempo.get() - 90.0).abs() < 1e-3)
            })
        },
    },
    Follower {
        name: "the clock follower",
        follow: |s| {
            let input = s.input();
            s.midi()
                .follow_clock(FollowClock::new(input, G, ClockKind::BeatClock))
                .expect("follows");
        },
        // Timing Clocks at 90 bpm (24 to a quarter note, M1 p.30): the tempo
        // they fit reads each arrival's frame through the tap's rate, so a tap
        // left on 48 kHz in a 44.1 kHz graph fits 98 bpm.
        send: |fake| {
            let period = Duration::from_secs_f64(60.0 / (90.0 * 24.0));
            let start = Instant::now();
            for k in 0..60 {
                let tick = Message::System(G, System::TimingClock).into();
                assert!(fake.send_input(EndpointId::from_raw(KEYS), tick, start + period * k));
            }
        },
        heard: |_, events| {
            events.iter().any(|e| {
                matches!(e, MidiEvent::TempoFollowed { tempo, .. }
                    if !tempo.differs_from(Bpm::of(90.0), Bpm::of(0.5)))
            })
        },
    },
    Follower {
        name: "the Machine Control follower",
        follow: |s| {
            let input = s.input();
            s.midi()
                .follow_machine_control(FollowMachineControl::new(input, G, OURS))
                .expect("follows");
        },
        send: |fake| {
            let payload = [0x7F, 5, 0x06, 0x02];
            for ump in Sysex7Packets::from_payload(G, &payload).expect("a 7-bit payload") {
                send(fake, ump);
            }
        },
        heard: |s, _| s.playhead().playing(),
    },
];

/// **A follower still hears its source after the graph is re-rated.** The
/// re-open itself is pinned by the unit test
/// `reopen_answers_unchanged_opened_and_gone`; this is the product property,
/// with the clock case as its rate-sensitive witness. Each
/// follower follows, the graph moves from 48 kHz to 44.1 kHz so its tap must be
/// re-opened, and the source's next message (a Set Tempo of 90 bpm, Timing Clocks at
/// 90 bpm, a PLAY) is followed. A tap that was not re-opened is superseded and hears
/// nothing.
#[test]
fn a_follower_hears_its_source_after_a_re_rate() {
    for follower in FOLLOWERS {
        let (mut s, fake) = rig();
        (follower.follow)(&mut s);
        let _ = s.update();
        s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
        // The re-prepare's second half lands on a render, then the commit.
        // The old reader ends when its tap is re-opened; the follower does not.
        let mut all = update_events(&mut s);
        blocks(&mut s, 8);
        all.extend(update_events(&mut s));
        blocks(&mut s, 1);
        all.extend(update_events(&mut s));

        (follower.send)(&fake);
        let mut heard = false;
        for _ in 0..100 {
            blocks(&mut s, 1);
            let events = update_events(&mut s);
            heard = (follower.heard)(&s, &events);
            all.extend(events);
            if heard {
                break;
            }
        }
        assert!(heard, "{} did not hear its source", follower.name);
        assert!(
            !all.iter()
                .any(|e| matches!(e, MidiEvent::Unfollowed { .. })),
            "{} did not end at the re-rate; got {all:?}",
            follower.name
        );
    }
}

/// **A follower whose input node is gone stops editing the graph.** Each
/// follower follows, its input node is removed and the graph is re-rated: the
/// tap cannot be re-opened, and once the re-rate has settled no later update
/// commits for it.
#[test]
fn a_follower_whose_input_is_gone_commits_nothing() {
    for follower in FOLLOWERS {
        let (mut s, _fake) = rig();
        (follower.follow)(&mut s);
        let _ = s.update();
        let input = s.input();
        let _ = s.remove(input);
        let _ = s.update();
        s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
        for _ in 0..4 {
            let _ = s.update();
            blocks(&mut s, 8);
        }
        let settled = s.graph_ref().testing().commits_sent();
        for _ in 0..5 {
            let _ = s.update();
            blocks(&mut s, 1);
        }
        assert_eq!(
            s.graph_ref().testing().commits_sent(),
            settled,
            "{} edited the graph after its input went",
            follower.name
        );
    }
}

/// **A follower whose input node is replaced at its key does not end.** The
/// replacement is not a removal: the tap stays on the key, and no
/// `Unfollowed` is raised.
#[test]
fn a_follower_whose_input_is_replaced_does_not_end() {
    for follower in FOLLOWERS {
        let (mut s, _fake) = rig();
        (follower.follow)(&mut s);
        let _ = s.update();
        let input = s.input();
        s.graph()
            .replace(
                input,
                Replacement::new(MidiClipNode::from(MidiClipConfig::default()))
                    .fade(Fade::new(Samples::new(0))),
            )
            .expect("the input is there");
        let mut all = update_events(&mut s);
        for _ in 0..4 {
            blocks(&mut s, 1);
            all.extend(update_events(&mut s));
        }
        assert!(
            !all.iter()
                .any(|e| matches!(e, MidiEvent::Unfollowed { .. })),
            "{} ended when its input was replaced; got {all:?}",
            follower.name
        );
    }
}

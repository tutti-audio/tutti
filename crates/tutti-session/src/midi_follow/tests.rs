use super::*;
use crate::{MidiEvent, SessionEvent};
use tutti_music::MeterMap;
use tutti_units::{SampleRate, Samples};

const RATE: SampleRate = SampleRate::of(48_000.0);
/// `round(48 000 * 0.005)`: the declick in frames.
const DECLICK: u64 = 240;

fn at(frame: u64) -> Seconds {
    Frame::new(frame).to_seconds(RATE)
}

/// What a poll at `stand` places against: 120 bpm.
fn poll(stand: &Stand) -> Poll<'_> {
    static MAP: std::sync::LazyLock<TempoMap> =
        std::sync::LazyLock::new(|| TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
    Poll {
        stand,
        tempo: Bpm::of(120.0),
        map: &MAP,
        now: stand.seconds,
    }
}

/// A transport at 2 s (beat 4 at 120 bpm), rolling, whose next block starts at
/// frame 96 512.
fn rolling() -> Stand {
    Stand {
        rate: RATE,
        frame: Frame::new(96_000),
        span: Samples::new(512),
        seconds: Seconds::new(2.0),
        playing: true,
    }
}

fn map(bpm: f64) -> Arc<TempoMap> {
    Arc::new(TempoMap::constant(Bpm::of(bpm)).expect("a tempo"))
}

/// **A command is placed `CLOCK_LEAD` after its message's arrival frame.** The
/// lead is four declicks, `4 * 240 = 960` frames (20 ms). A message at frame
/// 48 000 takes effect at `48 960`, whatever the poll that read it, while that
/// is past the next block and a declick (`48 512 + 240 = 48 752`).
#[test]
fn a_command_lands_the_lead_after_its_arrival_frame() {
    let next = Frame::new(48_512);
    assert_eq!(placed_frame(at(48_000), RATE, next), Frame::new(48_960));
    // Two messages 480 frames (10 ms) apart keep their spacing.
    assert_eq!(placed_frame(at(48_480), RATE, next), Frame::new(49_440));
}

/// **A late poll lands a declick past the next block.** A message at frame
/// 48 000 read when the next block starts at 50 000 (past `48 960`) lands at
/// `50 000 + 240`, so the engine sees it one declick ahead.
#[test]
fn a_late_poll_lands_at_the_next_block_plus_a_declick() {
    assert_eq!(
        placed_frame(at(48_000), RATE, Frame::new(50_000)),
        Frame::new(50_000 + DECLICK)
    );
}

/// **A follower's commands are `At::Frame` at the placed frame, never
/// `NextBlock`.** Start at frame 96 000 (a Locate to beat 0 and a Play), then a Stop at
/// 96 480: the PlayFrom lands at `96 000 + 960` carrying beat
/// `0.02 s * 120 / 60 = 0.04` (the source rolled for the lead); the Stop at
/// `96 480 + 960`. A tempo of 90 bpm heard at 96 000 is stepped at the same frame.
#[test]
fn a_followed_command_is_placed_at_its_frame_not_the_next_block() {
    let stand = rolling();
    let mut out = Vec::new();
    let events = vec![
        BeatClockEvent::Clock(ClockEvent::Locate {
            position: Beat::new(0.0),
            at: at(96_000),
        }),
        BeatClockEvent::Clock(ClockEvent::Play),
        BeatClockEvent::Tempo {
            tempo: Bpm::of(90.0),
            at: at(96_000),
        },
        BeatClockEvent::Clock(ClockEvent::Stop { at: at(96_480) }),
    ];
    let mut tempos = Vec::new();
    let actions = lay::<BeatClockFollower>(
        events,
        ClockKind::BeatClock,
        false,
        &poll(&stand),
        &mut out,
        |event, _| {
            if let BeatClockEvent::Tempo { tempo, at: heard } = event {
                tempos.push((tempo, placed_frame(heard, RATE, stand.next())));
            }
        },
    );
    // The source ends stopped, so the locate is not advanced.
    assert_eq!(
        actions,
        [
            Placed {
                action: ClockAction::PlayFrom(Beat::new(0.0)),
                at: At::Frame(Frame::new(96_960)),
            },
            Placed {
                action: ClockAction::Stop,
                at: At::Frame(Frame::new(97_440)),
            },
        ]
    );
    assert_eq!(tempos, [(Bpm::of(90.0), Frame::new(96_960))]);
    assert!(out.is_empty());
}

/// **A locate while the source rolls is advanced by the lead**: beats at the
/// tempo in force for Beat Clock (`0.02 s * 120 / 60 = 0.04`), seconds through
/// the map for MTC (`10 s + 0.02 s` is beat `20.04` at 120 bpm).
#[test]
fn a_rolling_locate_is_advanced_by_the_time_to_its_frame() {
    let stand = rolling();
    let placed = |actions: Vec<Placed>| {
        let [
            Placed {
                action: ClockAction::Locate(beat),
                at: At::Frame(frame),
            },
        ] = actions[..]
        else {
            panic!("one placed locate: {actions:?}");
        };
        assert_eq!(frame, Frame::new(96_960));
        beat
    };
    let actions = lay::<BeatClockFollower>(
        vec![BeatClockEvent::Clock(ClockEvent::Locate {
            position: Beat::new(8.0),
            at: at(96_000),
        })],
        ClockKind::BeatClock,
        true,
        &poll(&stand),
        &mut Vec::new(),
        |_, _| {},
    );
    let beat = placed(actions);
    assert!((beat.get() - 8.04).abs() < 1e-9, "{beat:?}");
    let actions = lay::<MtcFollower>(
        vec![MtcEvent::Clock(ClockEvent::Locate {
            position: Seconds::new(10.0),
            at: at(96_000),
        })],
        ClockKind::Mtc,
        true,
        &poll(&stand),
        &mut Vec::new(),
        |_, _| {},
    );
    let beat = placed(actions);
    assert!((beat.get() - 20.04).abs() < 1e-9, "{beat:?}");
}

/// **A tempo step lands on the beat the transport reaches at its placed
/// frame.** The transport is at 2 s on frame 96 000 and rolling at 120 bpm; a
/// step placed at frame 96 960 is `0.02 s` on, 2.02 s, beat `2.02 * 2 = 4.04`,
/// and the published map holds 90 bpm from there and 120 before.
#[test]
fn a_tempo_step_lands_on_the_beat_at_its_placed_frame() {
    let stand = rolling();
    let beat = stand.beat_at(&map(120.0), Frame::new(96_960));
    assert!((beat.get() - 4.04).abs() < 1e-9, "{beat:?}");

    let settings = TransportSettings::new();
    let meter = ProjectMeter::new(MeterMap::default());
    let bases = Bases::of(&settings);
    let cx = FollowCx {
        settings: &settings,
        meter: &meter,
        bases: &bases,
        playhead: Playhead::default(),
    };
    let mut out = Vec::new();
    cx.apply_tempos(&[(Bpm::of(90.0), Frame::new(96_960))], &stand, &mut out);
    let published = settings.tempo_map();
    assert_eq!(published.tempo_at(Beat::new(4.03)), Bpm::of(120.0));
    assert_eq!(published.tempo_at(Beat::new(4.05)), Bpm::of(90.0));
    assert!(matches!(
        out[..],
        [SessionEvent::Midi(MidiEvent::TempoFollowed { beat, .. })] if (beat.get() - 4.04).abs() < 1e-9
    ));
}

/// **A stopped transport does not advance**: a frame ahead is the same time.
#[test]
fn a_stopped_transport_stands_where_it_is() {
    let stopped = Stand {
        playing: false,
        ..rolling()
    };
    assert_eq!(stopped.seconds_at(Frame::new(500_000)), Seconds::new(2.0));
}

/// **A reversed source is reported and stops the transport at its frame.**
/// `Reversed`, then a Stop heard at frame 96 000, give a `ClockReversed` report
/// and a Stop at `96 960`; a slip reports its error.
#[test]
fn a_reversed_source_is_reported_and_stopped() {
    let mut out = Vec::new();
    let actions = lay::<MtcFollower>(
        vec![
            MtcEvent::Reversed,
            MtcEvent::Clock(ClockEvent::Stop { at: at(96_000) }),
            MtcEvent::Clock(ClockEvent::Slipped {
                error: Seconds::new(0.1),
            }),
        ],
        ClockKind::Mtc,
        false,
        &poll(&rolling()),
        &mut out,
        |event, out| {
            if event == MtcEvent::Reversed {
                out.push(
                    MidiEvent::ClockReversed {
                        kind: ClockKind::Mtc,
                    }
                    .into(),
                );
            }
        },
    );
    assert_eq!(
        actions,
        [Placed {
            action: ClockAction::Stop,
            at: At::Frame(Frame::new(96_960)),
        }]
    );
    assert!(matches!(
        out[0],
        SessionEvent::Midi(MidiEvent::ClockReversed {
            kind: ClockKind::Mtc
        })
    ));
    assert!(matches!(
        out[1],
        SessionEvent::Midi(MidiEvent::ClockSlipped {
            kind: ClockKind::Mtc,
            error: ClockOffset::Time(_)
        })
    ));
}

/// A graph at 48 kHz with one node that has an event output, and a tap of
/// that output's port.
fn tapped() -> (crate::Graph, tutti_graph::NodeKey, FollowTap) {
    let mut graph = crate::Graph::new(
        2,
        RATE,
        tutti_buffers::MaxBlock::of(Samples::new(64)),
        None,
        #[cfg(feature = "plugin")]
        tutti_timeline::TempoReader::fixed(map(120.0)),
    );
    let node = tutti_graph::NodeKey::from(graph.add(tutti_midi_runtime::MidiClipNode::new([])));
    let reader = graph
        .taps()
        .open_event(
            crate::EventOutputPort::new(node, 0),
            tutti_graph::EventTapOptions::new(tutti_graph::EventCapacity::of(16))
                .align(tutti_graph::TapAlign::Raw),
        )
        .expect("the port exists");
    let _ = graph.commit();
    (graph, node, FollowTap::new(reader, "test"))
}

/// **`reopen` answers for what the graph did to the tap.** At the graph's own
/// rate the tap is `Unchanged` and the graph is not edited; after a re-rate it
/// is `Opened` on the new rate, then `Unchanged` again; once its node is gone
/// it is `Gone`, and asking again edits nothing.
#[test]
fn reopen_answers_unchanged_opened_and_gone() {
    let (mut graph, node, mut tap) = tapped();
    assert_eq!(tap.reopen(&mut graph), Reopened::Unchanged);
    assert!(
        !graph.has_uncommitted_edits(),
        "nothing was asked of the graph"
    );

    graph
        .set_sample_rate(SampleRate::SR_44K1)
        .expect("re-rates");
    let _ = graph.commit();
    assert_eq!(tap.reopen(&mut graph), Reopened::Opened);
    assert_eq!(tap.rate(), SampleRate::SR_44K1);
    let _ = graph.commit();
    assert_eq!(tap.reopen(&mut graph), Reopened::Unchanged);

    let _ = graph.remove(node).expect("the node is in the graph");
    graph.set_sample_rate(SampleRate::SR_48K).expect("re-rates");
    let _ = graph.commit();
    assert_eq!(tap.reopen(&mut graph), Reopened::Gone);
    let _ = graph.commit();
    assert_eq!(tap.reopen(&mut graph), Reopened::Gone);
    assert!(
        !graph.has_uncommitted_edits(),
        "a gone tap edits nothing on the second ask"
    );
}

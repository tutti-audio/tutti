use super::*;
use crate::{MidiEvent, SessionEvent};
use tutti_midi_runtime::ClockOffset;
use tutti_music::MeterMap;
use tutti_units::{SampleRate, Samples};

const RATE: SampleRate = SampleRate::of(48_000.0);
/// `round(48 000 * 0.005)`: the declick in frames.
const DECLICK: u64 = 240;

fn at(frame: u64) -> Seconds {
    Frame::new(frame).to_seconds(RATE)
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
        ClockEvent::Locate {
            position: ClockPosition::Beat(Beat::new(0.0)),
            at: at(96_000),
        },
        ClockEvent::Play,
        ClockEvent::Tempo {
            tempo: Bpm::of(90.0),
            at: at(96_000),
        },
        ClockEvent::Stop { at: at(96_480) },
    ];
    let (actions, tempos) = lay(
        events,
        ClockKind::BeatClock,
        false,
        &stand,
        Bpm::of(120.0),
        &map(120.0),
        &mut out,
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
    for (position, expected) in [
        (ClockPosition::Beat(Beat::new(8.0)), 8.04),
        (ClockPosition::Time(Seconds::new(10.0)), 20.04),
    ] {
        let (actions, _) = lay(
            vec![ClockEvent::Locate {
                position,
                at: at(96_000),
            }],
            ClockKind::BeatClock,
            true,
            &stand,
            Bpm::of(120.0),
            &map(120.0),
            &mut Vec::new(),
        );
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
        assert!(
            (beat.get() - expected).abs() < 1e-9,
            "{position:?}: {beat:?}"
        );
    }
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
    let cx = FollowCx {
        settings: &settings,
        meter: &meter,
        playhead: Playhead::default(),
    };
    let mut overlay = Overlay::new(map(120.0), Arc::new(MeterMap::default()));
    let mut out = Vec::new();
    overlay.apply_tempos(
        &[(Bpm::of(90.0), Frame::new(96_960))],
        &stand,
        &cx,
        &mut out,
    );
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
    let (actions, _) = lay(
        vec![
            ClockEvent::Reversed,
            ClockEvent::Stop { at: at(96_000) },
            ClockEvent::Slipped {
                error: ClockOffset::Time(Seconds::new(0.1)),
            },
        ],
        ClockKind::Mtc,
        false,
        &rolling(),
        Bpm::of(120.0),
        &map(120.0),
        &mut out,
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

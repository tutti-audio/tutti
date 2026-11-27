//! Modulation on control lanes, through the session (doc 014 §8, §12.5).
//!
//! Every
//! test here declares through [`Session::mods`],
//! runs the session's updates, and asserts on what the graph renders: the
//! nodes are [`ProbeUnit`]s, which write the value their param port reads to
//! their output, so a render is the delivered value frame by frame. The audio
//! side is taken (`Graph::take_audio_side`) and rendered between updates, as
//! a device would.
//!
//! Tests that exist in
//! `modulation.rs` (the §8.1 render, a lane in the plan of its node's update
//! publishing its latency, a target added an update late) are not repeated
//! here.

use crate::common;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{InterleavedMut, PlanesMut};
use tutti_fixtures::WavePlanes;
#[cfg(feature = "export")]
use tutti_session::ToMemory;

use std::sync::Arc;

use tutti_ump::{MidiChannel, NoteNumber, U7};

use tutti_graph::NodeKey;
use tutti_graph::{Cx, Event, Fade, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_modulation::{Adsr, Follower, Lfo, NoteEnvelope, Polarity};
use tutti_nodes::automation::{Breakpoint, RecordMode, Trajectory};
use tutti_nodes::{ModDelayConfig, PhaserConfig, Stagger};
use tutti_session::{
    AudioSide, AutomationTake, DepthTarget, DetachedSession, MainOutput, MixRef, ModRouteId,
    ModSourceId, ModTarget, NoSuchModRoute, NoSuchModSource, OutputPort, ParamRef, ParamTarget,
    PolarityTarget, Problem, Replacement, RouteSpec, Scale, ScaleError, Session, SessionEvent,
    SourceName, TargetName,
};
use tutti_units::{
    Amplitude, Beat, Bipolar, Bpm, Drive, Frame, Hz, SampleRate, Samples, Seconds, UnitParam,
};

use common::probe::{ProbeUnit, Pulse};
use tutti_ump::{Message, Voice1};

const RATE: f64 = 48_000.0;
const TEMPO: Bpm = Bpm::of(120.0);
/// A lane's control step (`tutti_modulation::CONTROL_GRID`).
const STEP: usize = 32;
/// A port's connect crossfade: `DECLICK` at 48 kHz.
const DECLICK: usize = 240;

/// A detached session of `outputs` channels at 48 kHz, its audio side taken:
/// the executor on this side, played in device-sized blocks.
pub(crate) fn rig(outputs: usize) -> (DetachedSession, AudioSide) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::clamped(outputs))
        .build_detached(SampleRate::DEFAULT)
        .expect("builds");
    s.graph().set_sample_rate(SampleRate::of(RATE)).unwrap();
    let side = s.graph().take_audio_side();
    (s, side)
}

/// Output channel `c` from `nodes[c]`'s port 0.
pub(crate) fn main_output(s: &mut DetachedSession, nodes: &[NodeKey]) {
    for (c, &n) in nodes.iter().enumerate() {
        s.graph()
            .set_sources(MainOutput::channel(c), [OutputPort::new(n, 0)])
            .unwrap();
    }
}

/// Updates until nothing is left to resolve: nodes committed, then lanes.
pub(crate) fn settle(s: &mut DetachedSession) {
    for _ in 0..4 {
        let _ = s.update();
    }
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "the declarations apply"
    );
}

/// `frames` frames of `channels` outputs, stopped, in `block`-frame blocks.
fn render_n(side: &mut AudioSide, channels: usize, frames: usize, block: usize) -> Vec<Vec<f32>> {
    let mut out = vec![vec![0.0; frames]; channels];
    side.set_transport(tutti_graph::TransportPosition::new(
        false,
        TEMPO,
        Beat::new(0.0),
        None,
    ));
    side.render(block, PlanesMut::from_vecs(&mut out).unwrap());
    out
}

/// [`render_n`] of one output.
pub(crate) fn render(side: &mut AudioSide, frames: usize, block: usize) -> Vec<Vec<f32>> {
    render_n(side, 1, frames, block)
}

/// Doc 014 §5.3's bound for a linear sine at `rate` on the 32-frame grid,
/// times the swing, plus `f32` rounding of values near 1000.
fn lfo_tolerance(depth: f64, rate: f64) -> f64 {
    let h = std::f64::consts::TAU * rate * STEP as f64 / RATE;
    h * h / 8.0 * 1000.0 * depth + 1e-3
}

/// The value probe 0 settles to, past the connect's declick: frames
/// `DECLICK + STEP..` of a 1024-frame render must all read it.
fn assert_holds(out: &[f32], want: f32, what: &str) {
    for (f, &v) in out.iter().enumerate().skip(DECLICK + STEP) {
        assert!(
            (v - want).abs() < 1e-3,
            "{what}: frame {f} reads {v}, not {want}"
        );
    }
}

/// A note-on (note 60, velocity 100) at executor frame `on`, never
/// released, on its one event output.
#[derive(Clone)]
struct Held {
    on: u64,
}

impl Held {
    const SHAPE: Shape = Shape::EMPTY.events(0, 1);
    const OUT: tutti_graph::OutEvents = Self::SHAPE
        .event_output(0)
        .expect("one event output is declared");
}

impl Node for Held {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(o) = cx.env.offset_of(Frame::new(self.on)) {
            io.event_out(Self::OUT)
                .push(Event::midi(
                    o,
                    Message::midi1(
                        MidiChannel::FIRST,
                        Voice1::note_on(NoteNumber::of(60), U7::of(100)),
                    ),
                ))
                .expect("room");
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A source declaring `LATE` frames of latency: a look-ahead's bookkeeping.
#[derive(Clone)]
struct Late;

const LATE: usize = 100;

impl Node for Late {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .latency(tutti_units::Latency::new(Samples::new(LATE)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

// --------------------------------------------------------- export --

/// **Export == live, on a native param.** A 3 Hz sine on one probe's cutoff
/// and a 5 Hz triangle on another's volume, their knobs moved after the lanes
/// are built (a knob is the node's cell, which a fork reads); live plays from
/// a stop into rolling ragged blocks, the export (buffers, over the fork of
/// the graph) renders from its first frame. Equal bit for bit from the play
/// start.
#[cfg(feature = "export")]
#[test]
fn an_export_is_the_live_render_on_a_native_param() {
    use crate::bounced::bounced;
    use tutti_session::{Bounce, BounceLatency, BounceTail, ExportClock, Range};
    use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};

    let (mut s, mut side) = rig(2);
    let a = s.graph().add(ProbeUnit::new(500.0));
    let b = s.graph().add(ProbeUnit::of(UnitParam::Volume, 0.5));
    main_output(&mut s, &[a.erase(), b.erase()]);
    let cutoff = ParamRef::unit(a, UnitParam::Cutoff);
    let volume = ParamRef::unit(b, UnitParam::Volume);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    m.range_raw(volume, 0.0, 1.0).unwrap();
    let sine = m.lfo(Lfo::sine().rate(Hz::new(3.0)));
    let tri = m.lfo(Lfo::triangle().rate(Hz::new(5.0)));
    m.route(RouteSpec::new(sine, cutoff, Bipolar::of(0.3)))
        .unwrap();
    m.route(RouteSpec::new(tri, volume, Bipolar::of(0.2)))
        .unwrap();
    settle(&mut s);
    // Past the connect's declick, stopped; then the knobs move.
    render_n(&mut side, 2, 1_024, 128);
    s.set(a.cutoff().to(Hz::new(450.0))).unwrap();
    let _ = s.update();
    // The same write lands on the live side before it plays.
    render_n(&mut side, 2, 128, 128);

    let total = 24_000;
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat::new(0.0),
        tempo_map: Arc::new(tutti_tempo::TempoMap::constant(TEMPO).expect("a tempo")),
        sample_rate: SampleRate::of(RATE),
        loop_range: None,
    }));
    let request = Bounce::new()
        .main_output(
            ToMemory::new().channels(tutti_buffers::ChannelSet::default_for(
                ChannelLayout::STEREO,
            )),
        )
        .range(Range::Frames(
            tutti_units::Frame::new(0),
            tutti_units::Frame::new(total as u64),
        ))
        .sample_rate(SampleRate::of(RATE))
        .clock(ExportClock::timeline(timeline))
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None);
    let export = bounced(&mut s, request)
        .expect("the export runs")
        .into_main_output()
        .to_planes();

    // Live: rolling from beat 0 (the play clock restarts there), in
    // ragged blocks.
    let mut live = [Vec::new(), Vec::new()];
    let mut done = 0;
    for n in [1usize, 128, 7, 64, 127, 33, 2, 100].iter().cycle() {
        if done >= total {
            break;
        }
        let n = (*n).min(total - done);
        let beat = done as f64 * TEMPO.get() / 60.0 / RATE;
        side.set_transport(tutti_graph::TransportPosition::new(
            true,
            TEMPO,
            Beat::new(beat),
            None,
        ));
        let mut out = vec![vec![0.0; n]; 2];
        side.render(n, PlanesMut::from_vecs(&mut out).unwrap());
        for (l, o) in live.iter_mut().zip(&out) {
            l.extend_from_slice(o);
        }
        done += n;
    }

    for (c, (l, e)) in live.iter().zip(&export).enumerate() {
        assert!(e.len() >= total, "channel {c}: {} frames", e.len());
        assert!(l.iter().any(|&v| v != l[0]), "channel {c} moves");
        for f in 0..total {
            assert_eq!(
                l[f].to_bits(),
                e[f].to_bits(),
                "channel {c}, frame {f}: live {} vs export {}",
                l[f],
                e[f]
            );
        }
    }
    // The knob written after the lanes were built is what both play.
    let want = 450.0 + 300.0 * (std::f64::consts::TAU * 3.0 * 12_000.0 / RATE).sin();
    assert!((f64::from(export[0][12_000]) - want).abs() < lfo_tolerance(0.3, 3.0));
}

// ------------------------------------------ removal and the knob --

/// **A removed source leaves no node and no layer.** A follower and an LFO
/// on one cutoff; the follower's source is removed (and the route from it).
/// Its node leaves the graph, the lane's program reads no input and keeps the
/// LFO's layer, and the session holds nothing for it.
#[test]
fn a_removed_source_leaves_no_node_and_no_layer() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let kick = s.graph().add(ForkByClone(Pulse {
        from: 0,
        to: 48_000,
        level: 0.5,
    }));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    let follower = m.follower(Follower::default(), kick);
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.1)))
        .unwrap();
    m.route(RouteSpec::new(follower, cutoff, Bipolar::of(-0.2)))
        .unwrap();
    settle(&mut s);
    render(&mut side, 1_024, 128);
    let node = s
        .mod_state()
        .source_node(follower)
        .expect("the follower has a node");
    assert!(s.graph_ref().contains(node));
    let (_, lane) = s.mod_state().lane(cutoff.key()).expect("a lane");
    assert_eq!(
        lane.program().inputs().len(),
        1,
        "precondition: it reads the follower"
    );

    s.mods_mut().remove_source(follower).unwrap();
    assert_eq!(
        s.mods_mut().remove_source(follower),
        Err(NoSuchModSource(follower)),
        "a removed source is not there"
    );
    settle(&mut s);
    render(&mut side, 1_024, 128);

    assert_eq!(
        s.mod_state().source_node(follower),
        None,
        "nothing held for it"
    );
    assert!(!s.graph_ref().contains(node), "its node is gone");
    let (_, lane) = s
        .mod_state()
        .lane(cutoff.key())
        .expect("the LFO keeps the lane");
    let program = lane.program();
    assert!(program.inputs().is_empty(), "no input layer");
    assert_eq!(program.layers().len(), 1, "the LFO's layer");
}

/// **A removed feed node leaves its source unfed, not every apply refused.**
/// A follower's feed node is removed through the graph after a steady state:
/// the follower is reported `Problem::Unfed` and nothing else, its node goes,
/// the LFO keeps the lane and an unrelated lane keeps its figure (`0.1 + 0.5`
/// of a macro of 1 over `0..=1`). A note envelope whose MIDI feed node is
/// removed keeps its node and reports nothing: it is fed nothing.
#[test]
fn a_removed_feed_node_leaves_its_source_unfed() {
    let (mut s, mut side) = rig(2);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let other = s.graph().add(ProbeUnit::new(0.1));
    let kick = s.graph().add(ForkByClone(Pulse {
        from: 0,
        to: 48_000,
        level: 0.5,
    }));
    main_output(&mut s, &[probe.erase(), other.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let steady = ParamRef::unit(other, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    m.range_raw(steady, 0.0, 1.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    let follower = m.follower(Follower::default(), kick);
    let one = m.macro_(1.0);
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.1)))
        .unwrap();
    m.route(RouteSpec::new(follower, cutoff, Bipolar::of(-0.2)))
        .unwrap();
    m.route(RouteSpec::new(one, steady, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);
    render_n(&mut side, 2, 1_024, 128);
    let node = s.mod_state().source_node(follower).expect("fed and built");

    s.remove(kick).unwrap();
    let mut events = Vec::new();
    for _ in 0..4 {
        events.extend(reported(s.update()));
        render_n(&mut side, 2, 1_024, 128);
    }
    assert_eq!(events, [], "nothing is refused");
    let unfed = Problem::Unfed {
        source: SourceName::Own {
            id: follower,
            fed_by: Some(kick.erase()),
        },
    };
    assert_eq!(s.mod_state().problems().collect::<Vec<_>>(), [&unfed]);
    assert_eq!(
        s.mod_state().source_node(follower),
        None,
        "the follower went"
    );
    assert!(!s.graph_ref().contains(node), "its node is gone");
    assert!(s.mod_state().lane(cutoff.key()).is_some(), "the LFO's lane");
    let got = render_n(&mut side, 2, 1_024, 128);
    assert!(
        (got[1][1_023] - 0.6).abs() < 1e-6,
        "the other lane runs: {}",
        got[1][1_023]
    );

    // A note envelope whose MIDI feed node is removed keeps its node.
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(0.2));
    let clip = s.graph().add(ForkByClone(Held { on: 10 }));
    main_output(&mut s, &[probe.erase()]);
    let c = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(c, 0.0, 1.0).unwrap();
    let env = m.note_envelope(NoteEnvelope::new(Adsr::default()), clip, 0);
    m.route(RouteSpec::new(env, c, Bipolar::of(0.5))).unwrap();
    settle(&mut s);
    assert!(s.mod_state().source_node(env).is_some(), "precondition");
    s.remove(clip).unwrap();
    let mut events = Vec::new();
    for _ in 0..4 {
        events.extend(reported(s.update()));
        render(&mut side, 1_024, 128);
    }
    assert_eq!(events, [], "nothing is refused");
    assert!(
        !s.graph_ref().contains(clip.erase()),
        "the feed node is gone"
    );
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "unfed, not a problem"
    );
    assert!(
        s.mod_state().source_node(env).is_some(),
        "the envelope stays"
    );
}

/// **A knob written after a route edit is the knob.** The cutoff's knob is
/// the node's own cell, written through `Session::set`; a depth edit
/// writes the lane's depth cell and no knob, so the knob written before it and
/// the one written after it are each the base the LFO rides on.
#[test]
fn a_knob_written_after_a_route_edit_is_the_knob() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.3)))
        .unwrap();
    settle(&mut s);
    render(&mut side, 1_024, 128);

    s.set(probe.cutoff().to(Hz::new(300.0))).unwrap();
    let _ = s.update();
    s.set(DepthTarget::of(lfo, cutoff).to(Bipolar::of(0.2)))
        .unwrap();
    let _ = s.update();
    let a = render(&mut side, 2_048, 128);
    s.set(probe.cutoff().to(Hz::new(700.0))).unwrap();
    let _ = s.update();
    let b = render(&mut side, 2_048, 128);

    assert_eq!(
        s.mod_state().lane(cutoff.key()).expect("a lane").1.value(),
        Some(700.0)
    );
    let tol = lfo_tolerance(0.2, 2.0);
    for (base, from, got) in [(300.0, 1_024, &a[0]), (700.0, 3_072, &b[0])] {
        for (i, &v) in got.iter().enumerate().skip(STEP) {
            let p = (from + i) as f64;
            let want = base + 200.0 * (std::f64::consts::TAU * 2.0 * p / RATE).sin();
            assert!(
                (f64::from(v) - want).abs() < tol,
                "knob {base}, frame {p}: {v} vs {want}"
            );
        }
    }
}

// ------------------------------------------------------- envelope --

/// A note held from frame 10 into a note envelope (sustain 0.4) routed to
/// probe 1 (`0..=1`, knob 0.2, depth 0.5); probe 2 (knob 0.1) has a lane
/// reading a macro of 0. Rendered into the sustain: probe 1 reads 0.4, probe 2
/// its knob. Returns the session, its audio side, the two cutoffs and the
/// envelope.
fn held_envelope_scene() -> (DetachedSession, AudioSide, ParamRef, ParamRef, ModSourceId) {
    let (mut s, mut side) = rig(2);
    let one = s.graph().add(ProbeUnit::new(0.2));
    let two = s.graph().add(ProbeUnit::new(0.1));
    let clip = s.graph().add(ForkByClone(Held { on: 10 }));
    main_output(&mut s, &[one.erase(), two.erase()]);
    let (c1, c2) = (
        ParamRef::unit(one, UnitParam::Cutoff),
        ParamRef::unit(two, UnitParam::Cutoff),
    );
    let adsr = Adsr {
        attack: Seconds::new(0.002),
        decay: Seconds::new(0.003),
        sustain: Amplitude::new(0.4),
        release: Seconds::new(0.2),
    };
    let mut m = s.mods_mut();
    m.range_raw(c1, 0.0, 1.0).unwrap();
    m.range_raw(c2, 0.0, 1.0).unwrap();
    let env = m.note_envelope(NoteEnvelope::new(adsr), clip, 0);
    let zero = m.macro_(0.0);
    m.route(RouteSpec::new(env, c1, Bipolar::of(0.5))).unwrap();
    m.route(RouteSpec::new(zero, c2, Bipolar::of(0.5))).unwrap();
    settle(&mut s);
    let got = render_n(&mut side, 2, 4_096, 128);
    assert!(
        (got[0][4_095] - 0.4).abs() < 1e-6,
        "probe 1 in the sustain: {}",
        got[0][4_095]
    );
    assert!((got[1][4_095] - 0.1).abs() < 1e-7, "probe 2 at its knob");
    (s, side, c1, c2, env)
}

/// **A held note envelope newly routed reads its level at once.** A route
/// from the envelope to probe 2 is added in the sustain: from the first grid
/// piece after the frame it lands in, probe 2 reads `0.1 + 0.5 · 0.4`, not its
/// knob until the next note.
#[test]
fn a_held_note_envelope_newly_routed_reads_its_level_at_once() {
    let (mut s, mut side, _, c2, env) = held_envelope_scene();
    s.mods_mut()
        .route(RouteSpec::new(env, c2, Bipolar::of(0.5)))
        .unwrap();
    let _ = s.update();
    assert_eq!(
        s.mod_state().pending_resyncs(),
        0,
        "the resync went with the update's commit"
    );
    let got = render_n(&mut side, 2, 512, 128);
    for (f, &v) in got[1].iter().enumerate().skip(STEP) {
        assert!((v - 0.3).abs() < 1e-6, "frame {f}: {v}");
    }
}

/// **A route dropped and added back reads the held envelope at once too.** The
/// route from the envelope to probe 2 is added, dropped (the lane frees the
/// envelope's input slot: probe 2 is at its knob again) and added back: the
/// lane's program newly reads the envelope, so it is owed a resync, and probe 2
/// reads `0.1 + 0.5 · 0.4` from the first grid piece, not its knob until the
/// next note.
#[test]
fn a_route_dropped_and_added_back_reads_the_held_envelope_at_once() {
    let (mut s, mut side, _, c2, env) = held_envelope_scene();
    let route = s
        .mods_mut()
        .route(RouteSpec::new(env, c2, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);
    let got = render_n(&mut side, 2, 1_024, 128);
    assert!(
        (got[1][1_023] - 0.3).abs() < 1e-6,
        "routed: {}",
        got[1][1_023]
    );

    s.mods_mut().remove_route(route).unwrap();
    settle(&mut s);
    let got = render_n(&mut side, 2, 1_024, 128);
    assert!(
        (got[1][1_023] - 0.1).abs() < 1e-7,
        "the lane dropped the input: {}",
        got[1][1_023]
    );

    s.mods_mut()
        .route(RouteSpec::new(env, c2, Bipolar::of(0.5)))
        .unwrap();
    let _ = s.update();
    assert_eq!(s.mod_state().pending_resyncs(), 0, "sent with the commit");
    let got = render_n(&mut side, 2, 512, 128);
    for (f, &v) in got[1].iter().enumerate().skip(STEP) {
        assert!((v - 0.3).abs() < 1e-6, "frame {f}: {v}");
    }
}

/// **A rebuilt source resyncs.** The envelope's declaration is edited, so its
/// node is rebuilt, a fresh node that has sent nothing: its lane would hold the
/// old node's `0.4` until the new one sent. The update's commit schedules a
/// resync for it, the rebuilt envelope sends where it stands (0), and probe 1
/// reads its knob from the first grid piece.
#[test]
fn a_rebuilt_source_resyncs_so_its_lane_does_not_hold_the_old_value() {
    let (mut s, mut side, _, _, env) = held_envelope_scene();
    let adsr = Adsr {
        attack: Seconds::new(0.004),
        decay: Seconds::new(0.003),
        sustain: Amplitude::new(0.4),
        release: Seconds::new(0.2),
    };
    let before = s.mod_state().source_node(env).expect("a node");
    s.mods_mut()
        .set_source(env, NoteEnvelope::new(adsr))
        .unwrap();
    let _ = s.update();
    assert_eq!(s.mod_state().pending_resyncs(), 0, "sent with the commit");
    assert_eq!(s.mod_state().source_node(env), Some(before), "at its key");
    let got = render_n(&mut side, 2, 512, 128);
    for (f, &v) in got[0].iter().enumerate().skip(STEP) {
        assert!((v - 0.2).abs() < 1e-6, "frame {f}: {v}");
    }
}

/// **A follower is owed no resync**: it sends on every grid point while it is
/// not silent, so a route from one leaves nothing pending.
#[test]
fn a_follower_route_owes_no_resync() {
    let (mut s, _side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let other = s.graph().add(ProbeUnit::new(0.1));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let follower = m.follower(Follower::default(), other);
    m.route(RouteSpec::new(follower, cutoff, Bipolar::of(0.5)))
        .unwrap();
    let _ = s.update();
    assert!(s.mod_state().source_node(follower).is_some(), "built");
    assert_eq!(s.mod_state().pending_resyncs(), 0);
}

// -------------------------------------------------------- refusals --

/// What one update reports of modulation.
fn reported(events: Vec<SessionEvent>) -> Vec<Problem> {
    events
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Modulation(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// **A route onto a param the node does not declare is refused by target.**
/// The probe declares a cutoff; a route onto its Q names a param no port has.
/// One report, held while it lasts, nothing built.
#[test]
fn a_param_the_node_does_not_declare_is_refused_by_target() {
    let (mut s, _side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let q = ParamRef::unit(probe, UnitParam::Q);
    let mut m = s.mods_mut();
    m.range_raw(q, 0.0, 1.0).unwrap();
    let source = m.macro_(1.0);
    m.route(RouteSpec::new(source, q, Bipolar::of(0.5)))
        .unwrap();
    let refusal = Problem::NoSuchParam {
        target: TargetName::Own(q.into()),
    };
    assert_eq!(reported(s.update()), vec![refusal.clone()]);
    assert_eq!(s.mod_state().problems().collect::<Vec<_>>(), [&refusal]);
    assert!(s.mod_state().lane(q.key()).is_none(), "nothing built");
    assert_eq!(reported(s.update()), [], "reported once");
}

/// **A plugin parameter on a node with no event input is refused by target**:
/// the lane would feed an input the node does not have.
#[test]
fn a_plugin_parameter_on_a_node_with_no_event_input_is_refused() {
    let (mut s, _side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let param = ParamRef::plugin(probe, tutti_units::ParamId::new(7));
    let mut m = s.mods_mut();
    m.range_raw(param, 0.0, 1.0).unwrap();
    let source = m.macro_(1.0);
    m.route(RouteSpec::new(source, param, Bipolar::of(0.5)))
        .unwrap();
    assert_eq!(
        reported(s.update()),
        [Problem::NoSuchEventInput {
            target: TargetName::Own(param.into()),
        }]
    );
    assert!(s.mod_state().lane(param.key()).is_none(), "nothing built");
}

/// **A note envelope fed from an event output its node does not have is
/// refused by source**, naming the node it was declared on.
#[test]
fn a_note_envelope_fed_from_a_missing_event_output_is_refused() {
    let (mut s, _side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let adsr = Adsr {
        attack: Seconds::new(0.002),
        decay: Seconds::new(0.003),
        sustain: Amplitude::new(0.4),
        release: Seconds::new(0.2),
    };
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1.0).unwrap();
    let env = m.note_envelope(NoteEnvelope::new(adsr), probe, 3);
    m.route(RouteSpec::new(env, cutoff, Bipolar::of(0.5)))
        .unwrap();
    assert_eq!(
        reported(s.update()),
        [Problem::WrongFeed {
            source: SourceName::Own {
                id: env,
                fed_by: Some(probe.erase()),
            },
        }]
    );
    assert_eq!(s.mod_state().source_node(env), None, "nothing built");
}

// ----------------------------------------------------------- takes --

/// **A take suspends the automation, then restores it.** A cutoff (knob 300)
/// automated to 800: the automation plays. A touch take opens on the
/// automation's target: the knob plays (the gesture drives the param). The
/// take records 650 and is released: its trajectory becomes the automation, and
/// 650 plays.
#[test]
fn a_take_suspends_the_automation_then_restores_it() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(300.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let cutoff_key = probe.cutoff();
    let mut lane = Trajectory::new().with_range(Hz::new(0.0), Hz::new(1_000.0));
    lane.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(800.0)));
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    assert!(m.automate(cutoff, Arc::new(lane)));
    settle(&mut s);
    let last = |out: &[Vec<f32>]| *out[0].last().expect("rendered");
    assert_eq!(
        last(&render(&mut side, 1_024, 128)),
        800.0,
        "the automation plays"
    );

    let mut m = s.mods_mut();
    m.record(
        cutoff_key,
        AutomationTake::record(RecordMode::Touch, Hz::new(0.0), Hz::new(1_000.0)),
    )
    .unwrap();
    m.take_mut(cutoff_key)
        .expect("the take is open")
        .recorder_mut()
        .touch(Beat::new(0.0), Hz::new(650.0));
    let _ = s.update();
    assert_eq!(
        last(&render(&mut side, 512, 128)),
        300.0,
        "the knob plays under the take"
    );

    s.mods_mut()
        .take_mut(cutoff_key)
        .expect("still open")
        .recorder_mut()
        .release(Beat::new(1.0), Hz::new(650.0));
    let _ = s.update();
    assert!(
        s.mods_mut().take_mut(cutoff_key).is_none(),
        "the take closed"
    );
    assert_eq!(
        last(&render(&mut side, 512, 128)),
        650.0,
        "the take's trajectory plays"
    );
}

/// **A latch take ends only when it is stopped.** The recorder is touched and
/// released: a latch take stays open (it holds the released value), and
/// `AutomationTake::stop` makes its trajectory the automation on the next
/// update, so 650 plays.
#[test]
fn a_latch_take_becomes_the_automation_when_stopped() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(300.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let cutoff_key = probe.cutoff();
    let mut lane = Trajectory::new().with_range(Hz::new(0.0), Hz::new(1_000.0));
    lane.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(800.0)));
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    assert!(m.automate(cutoff, Arc::new(lane)));
    m.record(
        cutoff_key,
        AutomationTake::record(RecordMode::Latch, Hz::new(0.0), Hz::new(1_000.0)),
    )
    .unwrap();
    let take = m.take_mut(cutoff_key).expect("the take is open");
    take.recorder_mut().touch(Beat::new(0.0), Hz::new(650.0));
    take.recorder_mut().release(Beat::new(1.0), Hz::new(650.0));
    let _ = s.update();
    assert!(
        s.mods_mut().take_mut(cutoff_key).is_some(),
        "a released latch take stays open"
    );

    s.mods_mut().take_mut(cutoff_key).unwrap().stop();
    let _ = s.update();
    assert!(s.mods_mut().take_mut(cutoff_key).is_none(), "stopped");
    let last = *render(&mut side, 512, 128)[0].last().expect("rendered");
    assert_eq!(last, 650.0, "the take's trajectory plays");
}

// ---------------------------------------------------------- refusal --

/// The refusals of one `update`, in order.
fn refusals(events: Vec<SessionEvent>) -> Vec<Problem> {
    events
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Modulation(p @ Problem::Cycle { .. }) => Some(p),
            _ => None,
        })
        .collect()
}

/// **A follower downstream of its target is refused by route, and the error
/// is surfaced.** A follower of the probe's own output routed into the
/// probe's cutoff would be a cycle: the declaration is refused with the
/// route, as a `SessionEvent::Modulation` and in
/// `ModState::problems`, and nothing is built. Dropping the route applies the
/// rest, and clears the refusal.
#[test]
fn a_follower_downstream_of_its_target_is_refused_by_route() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let offset = m.macro_(0.5);
    m.route(RouteSpec::new(offset, cutoff, Bipolar::of(0.2)))
        .unwrap();
    settle(&mut s);

    let mut m = s.mods_mut();
    let follower = m.follower(Follower::default(), probe);
    let route = m
        .route(RouteSpec::new(follower, cutoff, Bipolar::of(0.5)))
        .unwrap();
    let mut refused = refusals(s.update());
    refused.extend(refusals(s.update()));
    assert_eq!(refused.len(), 1, "one message for one refusal: {refused:?}");
    assert_eq!(refused[0].route(), Some(route), "refused by route");
    assert_eq!(
        refused[0],
        Problem::Cycle {
            route,
            at: Some(TargetName::Own(cutoff.into())),
        },
        "the problem names the route's target"
    );
    assert_eq!(
        s.mod_state().problems().next(),
        Some(&refused[0]),
        "held while it lasts"
    );
    assert_eq!(s.mod_state().source_node(follower), None, "nothing built");
    let (_, lane) = s
        .mod_state()
        .lane(cutoff.key())
        .expect("the lane before the refusal");
    assert!(lane.program().inputs().is_empty(), "unchanged");
    // The macro's `+100` still plays.
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 600.0);

    s.mods_mut().remove_route(route).unwrap();
    assert_eq!(
        s.mods_mut().remove_route(route),
        Err(NoSuchModRoute(route)),
        "a removed route is not there"
    );
    settle(&mut s);
    assert_eq!(s.mod_state().problems().next(), None);
}

// ----------------------------------------------------------- rebind --

/// **A route onto a param of a node added in the same update goes out in that
/// update's one plan, and moves the param by `depth * range` from its first
/// frame.** The probe (cutoff knob 500), the cutoff's range `0..=1000` and a
/// macro of 1 at depth 0.25 are declared before one update: the lane binds to
/// the range cell the probe made when it was staged, so exactly one plan is
/// sent, and the first block reads `500 + 0.25 * 1000`.
#[test]
fn a_route_onto_a_param_of_a_node_added_in_the_update_sends_one_plan() {
    let (mut s, mut side) = rig(1);
    settle(&mut s);
    let plans = s.graph_ref().testing().commits_sent();

    let probe = s.graph().add(ProbeUnit::new(500.0));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(probe, 0)])
        .unwrap()
        .unfaded();
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let offset = m.macro_(1.0);
    m.route(RouteSpec::new(offset, cutoff, Bipolar::of(0.25)))
        .unwrap();

    let _ = s.update();
    assert_eq!(s.mod_state().problems().next(), None, "the route resolved");
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        plans + 1,
        "one plan for the node and its lane"
    );
    let out = render(&mut side, 128, 128);
    assert_eq!(out[0][0], 750.0, "the first frame");
    assert_eq!(out[0][127], 750.0);
}

/// **A node swapped for one with another param list keeps modulating.** A
/// macro's `+250` on a cutoff probe (knob 500); the node is swapped, with a
/// zero fade (a fade over another shape is refused), for a probe that
/// declares the cutoff and a Q (another param list, knob 400).
/// The update that sends the swap sends one plan (the lane is bound to the new
/// unit's staged range cell), the new node plays `400 + 250`, and a range edit
/// after it lands on the new node (`+500`).
#[test]
fn a_node_swapped_for_another_param_list_keeps_modulating() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range(probe.cutoff(), Hz::new(0.0)..Hz::new(1_000.0))
        .unwrap();
    let offset = m.macro_(1.0);
    m.route(RouteSpec::new(offset, cutoff, Bipolar::of(0.25)))
        .unwrap();
    settle(&mut s);
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 750.0, "precondition: modulated");
    let plans = s.graph_ref().testing().commits_sent();

    s.graph()
        .replace(
            probe,
            Replacement::new(ProbeUnit::new(400.0).with_extra(UnitParam::Q))
                .fade(Fade::new(Samples::ZERO)),
        )
        .expect("the swap is accepted");
    // The update's one commit sends the node and the rebound lane: the first
    // block it plays is modulated.
    let _ = s.update();
    assert_eq!(s.mod_state().problems().next(), None, "the route resolved");
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        plans + 1,
        "one plan for the swap and the rebound lane"
    );
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 650.0, "the new node, modulated");
    settle(&mut s);
    let out = render(&mut side, 4_096, 128);
    assert_eq!(*out[0].last().unwrap(), 650.0, "the new node, modulated");

    s.set(probe.cutoff().range().to(Hz::new(0.0)..Hz::new(2_000.0)))
        .unwrap();
    let _ = s.update();
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 900.0, "a later range edit lands");
}

/// **A lane on a param with no typed key reaches its port.** The probe
/// declares `Balance` by `UnitParam` alone (the probe types key only `Cutoff`
/// and `Drive`): a macro of 1 at depth 0.25 over `-1..=1` renders
/// `0 + 0.25 * 1 * 2` = 0.5 at the port.
#[test]
fn a_lane_on_a_param_with_no_typed_key_reaches_its_port() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::of(UnitParam::Balance, 0.0));
    main_output(&mut s, &[probe.erase()]);
    let balance = ParamRef::unit(probe, UnitParam::Balance);
    let mut m = s.mods_mut();
    m.range_raw(balance, -1.0, 1.0).unwrap();
    let one = m.macro_(1.0);
    m.route(RouteSpec::new(one, balance, Bipolar::of(0.25)))
        .unwrap();
    settle(&mut s);
    let out = render(&mut side, 1_024, 128);
    assert_holds(&out[0], 0.5, "balance at the analytic figure");
}

/// **A replace that drops the targeted param refuses the target and leaves
/// nothing driving the new node.** A macro lane on a cutoff probe renders
/// `500 + 250`; the node is replaced, with a zero fade, by a probe that
/// declares `Q` alone (knob 400). The target is `NoSuchParam`, reported once,
/// and the new node renders its own knob.
#[test]
fn a_replace_that_drops_the_param_is_refused_and_leaves_the_knob() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let offset = m.macro_(1.0);
    m.route(RouteSpec::new(offset, cutoff, Bipolar::of(0.25)))
        .unwrap();
    settle(&mut s);
    let out = render(&mut side, 1_024, 128);
    assert_eq!(*out[0].last().unwrap(), 750.0, "precondition: modulated");
    assert!(s.mod_state().lane(cutoff.key()).is_some());

    s.graph()
        .replace(
            probe,
            Replacement::new(ProbeUnit::of(UnitParam::Q, 400.0)).fade(Fade::new(Samples::ZERO)),
        )
        .expect("the swap is accepted");
    let mut events = Vec::new();
    for _ in 0..4 {
        events.extend(reported(s.update()));
        render(&mut side, 1_024, 128);
    }
    let refusal = Problem::NoSuchParam {
        target: TargetName::Own(cutoff.into()),
    };
    assert_eq!(events, std::slice::from_ref(&refusal), "reported once");
    assert_eq!(s.mod_state().problems().collect::<Vec<_>>(), [&refusal]);
    let out = render(&mut side, 1_024, 128);
    assert_eq!(
        *out[0].last().unwrap(),
        400.0,
        "the new node plays its own knob: nothing drives it"
    );
}

// ------------------------------------------------- a host's commit --

/// **A host's staged edit committed on an update with an unresolved target
/// publishes its PDC figures on that update.** A host inserts, between
/// updates, a latent node on the outputs and a probe it then modulates: the
/// probe's target is unresolved when the update applies. The update's commit
/// sends the host's edit with the lanes, and the session's latency is that
/// plan's on that update, which also carries the target's lane.
#[test]
fn a_host_edit_committed_for_a_target_publishes_its_latency() {
    let (mut s, mut side) = rig(1);
    settle(&mut s);
    let late = s.graph().add(ForkByClone(Late));
    s.graph().connect(late, MainOutput).unwrap();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let offset = m.macro_(1.0);
    m.route(RouteSpec::new(offset, cutoff, Bipolar::of(0.1)))
        .unwrap();

    let _ = s.update();
    assert_eq!(
        s.latency(),
        Samples::new(LATE),
        "the plan the update sent, published"
    );
    render(&mut side, 128, 128);
    settle(&mut s);
    assert!(s.mod_state().lane(cutoff.key()).is_some());
}

// ------------------------------- properties of the declarations --
//
// Each is asserted on the lanes.

/// **Two routes onto one param sum, and removing one takes only its share.**
/// Two macros at 1 on a cutoff (knob 500 in `0..=1000`) at depths 0.1 and 0.2
/// add 100 and 200: 800. Removing the second route leaves 600 (the session
/// has no `enabled` flag: a disabled route declares nothing, which is this); declaring it again, 800.
#[test]
fn two_routes_on_one_param_sum_and_removing_one_takes_its_share() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let a = m.macro_(1.0);
    let b = m.macro_(1.0);
    m.route(RouteSpec::new(a, cutoff, Bipolar::of(0.1)))
        .unwrap();
    let second = m
        .route(RouteSpec::new(b, cutoff, Bipolar::of(0.2)))
        .unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "both routes");

    s.mods_mut().remove_route(second).unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 600.0, "one removed");

    s.mods_mut()
        .route(RouteSpec::new(b, cutoff, Bipolar::of(0.2)))
        .unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "declared again");
}

/// **Removing the last route returns the param to its knob.** A macro at
/// depth 0.1 on a cutoff (knob 500 in `0..=1000`) adds 100: 600. With the
/// route removed the target has nothing left to drive it, so its lane is gone
/// and the probe reads the knob, 500, past the disconnect's declick.
#[test]
fn removing_the_last_route_returns_the_param_to_its_knob() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let offset = m.macro_(1.0);
    let route = m
        .route(RouteSpec::new(offset, cutoff, Bipolar::of(0.1)))
        .unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 600.0, "the route");
    assert!(
        s.mod_state().lane(cutoff.key()).is_some(),
        "the route has its lane"
    );

    s.mods_mut().remove_route(route).unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 500.0, "no route left");
    assert!(s.mod_state().lane(cutoff.key()).is_none(), "and no lane");
}

/// **A route's polarity and curve are its own, declared with it and edited in
/// place.** A macro at 0.5, depth 0.4, on a cutoff (knob 500 in `0..=1000`):
/// bipolar linear adds `1000 * 0.4 * 0.5` = 200; unipolar maps 0.5 to
/// `(0.5 + 1) / 2` = 0.75 and adds 300; bipolar `Hold` is 0 below full scale
/// and adds nothing. The edits reach the lane without a new route.
#[test]
fn a_routes_polarity_and_curve_reach_the_render() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let knob = m.macro_(0.5);
    let route = m
        .route(RouteSpec::new(knob, cutoff, Bipolar::of(0.4)))
        .unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 700.0, "bipolar linear");

    s.set(route.polarity().to(tutti_modulation::Polarity::Unipolar))
        .unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 800.0, "unipolar");

    s.set((
        route.polarity().to(tutti_modulation::Polarity::Bipolar),
        route.curve().to(tutti_modulation::Law::Hold),
    ))
    .unwrap();
    settle(&mut s);
    assert_holds(&render(&mut side, 1_024, 128)[0], 500.0, "bipolar hold");

    // Declared with them: a second route says unipolar at creation.
    s.mods_mut().remove_route(route).unwrap();
    s.mods_mut()
        .route(
            RouteSpec::new(knob, cutoff, Bipolar::of(0.4))
                .polarity(tutti_modulation::Polarity::Unipolar),
        )
        .unwrap();
    settle(&mut s);
    assert_holds(
        &render(&mut side, 1_024, 128)[0],
        800.0,
        "declared unipolar",
    );
}

/// **A bus strip's volume and pan take lanes.** The strip declares its params
/// itself, so a route onto either resolves to a lane on its
/// port, with nothing refused.
#[test]
fn a_bus_strips_volume_and_pan_take_lanes() {
    let (mut s, _side) = rig(2);
    let strip = s.graph().add(tutti_nodes::BusStripNode::from(
        tutti_nodes::BusStripConfig::new(ChannelTopology::STEREO),
    ));
    let mut m = s.mods_mut();
    let offset = m.macro_(0.5);
    for param in [UnitParam::Volume, UnitParam::Balance] {
        let at = ParamRef::unit(strip, param);
        m.range_raw(at, 0.0, 1.0).unwrap();
        m.route(RouteSpec::new(offset, at, Bipolar::of(0.5)))
            .unwrap();
    }
    settle(&mut s);
    for param in [UnitParam::Volume, UnitParam::Balance] {
        assert!(
            s.mod_state()
                .lane(ParamRef::unit(strip, param).key())
                .is_some(),
            "{param:?} has its lane"
        );
    }
}

/// **A chorus's rate and a phaser's mix take lanes.** The chorus declares its
/// rate as a port, and every node's mix is a port at the
/// graph's reserved address (`MixRef`), so a route onto each resolves to a
/// lane, with nothing refused. What a lane on them sounds
/// like is pinned where the nodes live (`tutti-nodes`'
/// `a_lane_modulates_a_chorus_rate_around_its_own_cell` and
/// `each_fed_param_is_the_one_the_dsp_reads`).
#[test]
fn a_chorus_rate_and_a_phaser_mix_take_lanes() {
    let (mut s, _side) = rig(2);
    let chorus = s.graph().add(tutti_nodes::ModDelayNode::<f32>::from(
        ModDelayConfig::chorus(tutti_buffers::ChannelTopology::STEREO),
    ));
    let phaser = s.graph().add(tutti_nodes::PhaserNode::<f32>::from(
        PhaserConfig::new(
            ChannelTopology::STEREO,
            tutti_nodes::Stages::of(4),
            Stagger::NONE,
        )
        .unwrap()
        .controls(tutti_nodes::PhaserControls::DEFAULT),
    ));
    // The phaser's mix is the graph's (`MixRef`), not a param of the node.
    let targets = [
        ModTarget::from(ParamRef::unit(chorus, UnitParam::Rate)),
        ModTarget::from(MixRef::of(phaser)),
    ];
    let mut m = s.mods_mut();
    let offset = m.macro_(0.5);
    let keys = targets.map(|at| m.key(at));
    for at in targets {
        m.range_raw(at, 0.0, 1.0).unwrap();
        m.route(RouteSpec::new(offset, at, Bipolar::of(0.5)))
            .unwrap();
    }
    settle(&mut s);
    for (at, key) in targets.into_iter().zip(keys) {
        assert!(s.mod_state().lane(key).is_some(), "{at:?} has its lane");
    }
}

/// **A synth added as a graph node is a modulation target.** A `PolySynth`
/// goes in through `Graph::add` like any node; a route into its volume
/// resolves (nothing refused, nothing unresolved), and the volume's lane has
/// the synth's own volume cell as its knob.
#[cfg(feature = "synth")]
#[test]
fn an_added_synth_is_a_modulation_target() {
    use tutti_polysynth::{PolySynth, SynthConfig};

    let (mut s, _side) = rig(2);
    let node = PolySynth::new(SynthConfig::new());
    let synth = s.graph().add(node);
    main_output(&mut s, &[synth.erase(), synth.erase()]);
    let volume = ParamRef::unit(synth, UnitParam::Volume);
    let mut m = s.mods_mut();
    m.range_raw(volume, 0.0, 1.0).unwrap();
    let offset = m.macro_(1.0);
    m.route(RouteSpec::new(offset, volume, Bipolar::of(0.25)))
        .unwrap();
    settle(&mut s);
    let (_, lane) = s
        .mod_state()
        .lane(volume.key())
        .expect("the volume has a lane");
    // The knob is read from its cell: a write through the synth's volume is
    // what the lane reads, value for value.
    for v in [0.37, 0.81] {
        s.set(synth.volume().to(Amplitude::new(v))).unwrap();
        assert_eq!(lane.value(), Some(v), "its knob is the synth's volume cell");
    }
}

// ----------------------------------------------- what an edit sends --

/// The plan the graph sent last.
fn sent(s: &DetachedSession) -> Arc<tutti_graph::Plan> {
    s.graph_ref()
        .testing()
        .sent_plan()
        .expect("a plan was sent")
}

/// Nothing is staged: the last update's commit took every edit.
fn idle(s: &DetachedSession) -> bool {
    !s.graph_ref().has_uncommitted_edits()
}

/// One update, then a block rendered, as a device would between updates.
pub(crate) fn frame(s: &mut DetachedSession, side: &mut AudioSide) {
    let _ = s.update();
    render(side, 128, 128);
}

/// A cutoff probe (knob 500, range `0..=1000`) on output 0, a sine at `rate`
/// Hz routed into it at `depth`, settled. Returns the probe's param, the
/// source and the route's key.
fn lfo_scene(
    s: &mut DetachedSession,
    side: &mut AudioSide,
    rate: f64,
    depth: f64,
) -> (ParamTarget<Hz>, ModSourceId, ModRouteId) {
    let filter = s.graph().add(ProbeUnit::new(500.0));
    main_output(s, &[filter.erase()]);
    let cutoff = filter.cutoff();
    let mut m = s.mods_mut();
    m.range(cutoff, Hz::new(0.0)..Hz::new(1_000.0)).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(rate)));
    let route = m
        .route(RouteSpec::new(lfo, cutoff, Bipolar::of(depth)))
        .unwrap();
    for _ in 0..4 {
        frame(s, side);
    }
    assert_eq!(s.mod_state().problems().next(), None);
    assert!(
        s.mod_state().lane(ParamRef::from(cutoff).key()).is_some(),
        "precondition: a lane"
    );
    (cutoff, lfo, route)
}

/// **A range or depth edit sends no plan; a follower route sends one.** Doc
/// 014 §8.2: a program edit is a store into the lane, not a rebuilt node; a
/// follower is a node, so routing one is one commit, and the updates after
/// it send nothing more.
///
/// Each edit is checked to have reached the render, so "no plan" is not an
/// edit that did nothing.
#[test]
fn range_and_depth_edits_send_no_plan_and_a_follower_route_one_commit() {
    let (mut s, mut side) = rig(1);
    // A rate low enough that one block barely moves it: the render after an
    // edit is read against the edit, not the wave.
    let (cutoff, lfo, _) = lfo_scene(&mut s, &mut side, 0.5, 0.3);
    let settled = sent(&s);

    // The depth: 0.3 → 0.0, the knob alone.
    s.set(DepthTarget::of(lfo, cutoff).to(Bipolar::of(0.0)))
        .unwrap();
    for _ in 0..3 {
        frame(&mut s, &mut side);
        assert!(Arc::ptr_eq(&sent(&s), &settled), "a depth edit sent a plan");
        assert!(idle(&s), "a depth edit staged an edit");
    }
    let v = render(&mut side, 128, 128);
    assert!(
        v[0].iter().all(|&x| x == 500.0),
        "depth 0 is the knob: {v:?}"
    );

    // The range: clamped to 600..=700, the knob (500) reads 600.
    s.set(cutoff.range().to(Hz::new(600.0)..Hz::new(700.0)))
        .unwrap();
    for _ in 0..3 {
        frame(&mut s, &mut side);
        assert!(Arc::ptr_eq(&sent(&s), &settled), "a range edit sent a plan");
        assert!(idle(&s), "a range edit staged an edit");
    }
    let v = render(&mut side, 128, 128);
    assert!(v[0].iter().all(|&x| x == 600.0), "the range clamps: {v:?}");
    // And back to 0..=1000 (a route's depth is a fraction of the span).
    s.set(cutoff.range().to(Hz::new(0.0)..Hz::new(1_000.0)))
        .unwrap();
    frame(&mut s, &mut side);
    assert!(Arc::ptr_eq(&sent(&s), &settled), "a range edit sent a plan");

    // A follower on a pulse, routed in: the pulse node first, on its own.
    let pulse = s.graph().add(ForkByClone(Pulse {
        from: 0,
        to: u64::MAX,
        level: 0.8,
    }));
    for _ in 0..3 {
        frame(&mut s, &mut side);
    }
    let mut plans = vec![sent(&s)];
    let mut m = s.mods_mut();
    let kick = m.follower(Follower::default(), pulse);
    m.route(RouteSpec::new(kick, cutoff, Bipolar::of(0.15)))
        .unwrap();
    for _ in 0..6 {
        frame(&mut s, &mut side);
        let now = sent(&s);
        if !Arc::ptr_eq(plans.last().unwrap(), &now) {
            plans.push(now);
        }
    }
    assert_eq!(plans.len(), 2, "a follower route sends exactly one plan");
    assert!(idle(&s));
    // And it modulates: the knob 500 plus the follower's 0.8 · 0.15 · 1000,
    // 620, inside 600..=700.
    let v = render(&mut side, 4_800, 128);
    let last = *v[0].last().unwrap();
    assert!((last - 620.0).abs() < 0.5, "the follower raises it: {last}");
}

/// **A depth slider keeps the LFO's phase.** Dragging a route's depth is a
/// store into the lane's program: the wave goes on from where it was, at the
/// new depth, on the frame the next block starts; a rebuilt source node would
/// restart the wave.
///
/// The edit lands at a block boundary; every frame after it reads
/// `500 + 1000 · 0.1 · sin(2π · 4 · t)` with `t` counted from frame 0, within
/// the control grid's bound.
#[test]
fn a_depth_slider_keeps_phase() {
    let (mut s, mut side) = rig(1);
    let rate = 4.0;
    let (cutoff, lfo, _) = lfo_scene(&mut s, &mut side, rate, 0.3);
    let settled = sent(&s);
    // `lfo_scene` rendered its updates' blocks: the clock is past them.
    let start = 4 * 128;
    let before = render(&mut side, 2 * DECLICK, 128).swap_remove(0);
    let value = |depth: f64, f: usize| {
        500.0 + 1000.0 * depth * (std::f64::consts::TAU * rate * f as f64 / RATE).sin()
    };
    let h = std::f64::consts::TAU * rate * STEP as f64 / RATE;
    let tol = |depth: f64| h * h / 8.0 * 1000.0 * depth + 1e-3;
    for (i, &v) in before.iter().enumerate().skip(DECLICK) {
        let f = start + i;
        let want = value(0.3, f);
        assert!(
            (f64::from(v) - want).abs() < tol(0.3),
            "before, frame {f}: {v} vs {want}"
        );
    }

    s.set(DepthTarget::of(lfo, cutoff).to(Bipolar::of(0.1)))
        .unwrap();
    let _ = s.update();
    assert!(Arc::ptr_eq(&sent(&s), &settled), "a depth edit sent a plan");
    let after = render(&mut side, 12_000, 128).swap_remove(0);
    let start = start + before.len();
    for (i, &v) in after.iter().enumerate().skip(STEP) {
        let f = start + i;
        let want = value(0.1, f);
        assert!(
            (f64::from(v) - want).abs() < tol(0.1),
            "after, frame {f}: {v} vs {want}"
        );
    }
}

/// A macro of 1 at depth 0.2 on the cutoff probe's `0..=1000` (knob 500),
/// scaled by a macro of 0.5 at depth 1:
/// `1000 * 0.2 * (1 * 0.5)` = 100 over the knob, so 600. Settled; returns the
/// session, its audio side and the two macros.
fn scaled_scene() -> (
    DetachedSession,
    AudioSide,
    ParamRef,
    ModSourceId,
    ModSourceId,
) {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let full = m.macro_(1.0);
    let half = m.macro_(0.5);
    let scaled = m
        .route(RouteSpec::new(full, cutoff, Bipolar::of(0.2)))
        .unwrap();
    m.scale(scaled, Some(Scale::new(half, Bipolar::of(1.0))))
        .unwrap();
    settle(&mut s);
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 600.0);
    let _ = s.update();
    (s, side, cutoff, full, half)
}

/// **A depth write to a route that another route scales lands from the
/// cell.** From the scene's 600, writing the scaled route's depth to 0.6
/// sends no plan and republishes nothing, and the next block reads
/// `1000 * 0.6 * 0.5` = 300 over the knob, so 800.
#[test]
fn a_depth_write_to_a_scaled_route_lands_from_its_cell() {
    let (mut s, mut side, cutoff, full, _) = scaled_scene();
    let settled = sent(&s);
    s.set(DepthTarget::of(full, cutoff).to(Bipolar::of(0.6)))
        .unwrap();
    let _ = s.update();
    assert!(
        Arc::ptr_eq(&sent(&s), &settled),
        "a depth write sent a plan"
    );
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 800.0);
}

/// **A scale's own depth write is applied.** The scale's
/// depth is read when the program is compiled, so the write republishes the
/// cutoff's program with no plan: from the scene's scaled depth 0.2 and scale
/// `0.5 * 1`, writing the scale's depth to 0.5 reads `1000 * 0.2 * (0.5 *
/// 0.5)` = 50 over the knob, so 550.
#[test]
fn a_scales_own_depth_write_is_applied() {
    let (mut s, mut side, _, full, _) = scaled_scene();
    let scaled = s.mods().routes().find(|r| r.source == full).unwrap().id;
    let settled = sent(&s);
    s.set(DepthTarget::scale_of(scaled).to(Bipolar::of(0.5)))
        .unwrap();
    let _ = s.update();
    assert!(
        Arc::ptr_eq(&sent(&s), &settled),
        "a depth write sent a plan"
    );
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 550.0);
}

/// **One source can route into a target and scale a route into it.** A
/// macro of 0.5 routes into the cutoff (`0..=1000`, knob 500) at depth 0.1 and
/// scales the depth of a macro of 1's route at depth 0.2, itself at depth 1:
/// `1000 * 0.2 * 0.5` plus `1000 * 0.1 * 0.5` over the knob, so 650. Each of
/// the two depths has its own name; writing the scale's depth to 0.5 reads
/// `1000 * 0.2 * 0.25 + 50` over the knob, 600, and the route into the cutoff
/// to 0.3 reads `50 + 1000 * 0.3 * 0.5`, 700. A scale set again replaces the
/// first, with its depth.
#[test]
fn a_source_routes_into_a_target_and_scales_a_route_into_it() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let full = m.macro_(1.0);
    let half = m.macro_(0.5);
    let scaled = m
        .route(RouteSpec::new(full, cutoff, Bipolar::of(0.2)))
        .unwrap();
    m.route(RouteSpec::new(half, cutoff, Bipolar::of(0.1)))
        .unwrap();
    m.scale(scaled, Some(Scale::new(half, Bipolar::of(0.3))))
        .unwrap();
    m.scale(scaled, Some(Scale::new(half, Bipolar::of(1.0))))
        .expect("a scale is replaced");
    settle(&mut s);
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 650.0);

    s.set(DepthTarget::scale_of(scaled).to(Bipolar::of(0.5)))
        .unwrap();
    let _ = s.update();
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 600.0);
    s.set(DepthTarget::of(half, cutoff).to(Bipolar::of(0.3)))
        .unwrap();
    let _ = s.update();
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 700.0);
}

/// **Clearing a scale restores the unscaled figure, and so does removing the
/// scaling source.** From the scene's 600, `scale(route, None)` reads
/// `1000 * 0.2 * 1` = 200 over the knob, so 700; a scale set again reads 600,
/// and removing its source reads 700 with the route's `scale` empty. A scale
/// goes with its route, and one asked of a route or by a source that is gone
/// is refused at the call.
#[test]
fn clearing_a_scale_or_removing_its_source_restores_the_unscaled_figure() {
    let (mut s, mut side, _, full, half) = scaled_scene();
    let scaled = s.mods().routes().find(|r| r.source == full).unwrap().id;
    assert_eq!(s.mods().route(scaled).unwrap().scale, Some(half));
    s.mods_mut().scale(scaled, None).unwrap();
    let _ = s.update();
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 700.0);
    assert_eq!(s.mods().route(scaled).unwrap().scale, None);
    let scale = Some(Scale::new(half, Bipolar::of(1.0)));
    s.mods_mut().scale(scaled, scale).unwrap();
    let _ = s.update();
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 600.0);
    s.mods_mut().remove_source(half).unwrap();
    let _ = s.update();
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 700.0);
    assert_eq!(s.mods().route(scaled).unwrap().scale, None);
    assert_eq!(
        s.mods_mut().scale(scaled, scale),
        Err(ScaleError::NoSuchSource(NoSuchModSource(half)))
    );
    s.mods_mut().remove_route(scaled).unwrap();
    assert_eq!(
        s.mods_mut().scale(scaled, None),
        Err(ScaleError::NoSuchRoute(NoSuchModRoute(scaled)))
    );
}

/// **A scale's polarity is its own target.** From the scene's 600 (scale
/// `0.5` at depth 1, bipolar), a unipolar scale maps the macro's 0.5 to
/// `(0.5 + 1) / 2` = 0.75: `1000 * 0.2 * 0.75` = 150 over the knob, so 650.
/// The route's own polarity is not the scale's: the route's macro at 1 stays
/// 1 under unipolar, so writing it moves nothing.
#[test]
fn a_scales_polarity_shapes_the_scale_not_the_route() {
    let (mut s, mut side, _, full, _) = scaled_scene();
    let scaled = s.mods().routes().find(|r| r.source == full).unwrap().id;
    s.set(PolarityTarget::scale_of(scaled).to(Polarity::Unipolar))
        .unwrap();
    let _ = s.update();
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 650.0);
    s.set(scaled.polarity().to(Polarity::Unipolar)).unwrap();
    let _ = s.update();
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 650.0);
}

/// **A scale needs a time function on both sides.** A follower as the scale's
/// source, or as the scaled route's own source, is refused at the call and
/// nothing changes.
#[test]
fn a_scale_by_or_of_a_signal_source_is_refused() {
    let (mut s, _side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let follower = m.follower(Follower::default(), probe);
    let mac = m.macro_(0.5);
    let by_time = m
        .route(RouteSpec::new(mac, cutoff, Bipolar::of(0.2)))
        .unwrap();
    assert_eq!(
        m.scale(by_time, Some(Scale::new(follower, Bipolar::of(1.0)))),
        Err(ScaleError::SignalSource { route: by_time })
    );
    let by_signal = m
        .route(RouteSpec::new(follower, cutoff, Bipolar::of(0.2)))
        .unwrap();
    assert_eq!(
        m.scale(by_signal, Some(Scale::new(mac, Bipolar::of(1.0)))),
        Err(ScaleError::SignalSource { route: by_signal })
    );
    assert_eq!(m.view().route(by_time).unwrap().scale, None);
    assert_eq!(m.view().route(by_signal).unwrap().scale, None);
}

// ------------------------------------------------------------- fork --

/// **A fork renders a modulated param as live does**, a knob written under
/// its modulation and a knob left as it was built alike. The constraint
/// export by `Editor::fork` rests on: a modulated param's cell is its lane's
/// knob (one writer, doc 014 §8.2), and its lane is a graph node, so the fork
/// carries both. A macro at 1 by a fifth of `0..10` adds 2: the knob written
/// to 6 reads 8, the one built at 1 reads 3, live and forked.
///
/// (The unmodulated paths are `graph.rs`' test of the same name).
#[test]
fn a_fork_renders_a_modulated_param_as_live_does() {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::of(RATE))
        .expect("builds");
    let written = s.graph().add(ProbeUnit::of(UnitParam::Drive, 1.0));
    let built = s.graph().add(ProbeUnit::of(UnitParam::Drive, 1.0));
    main_output(&mut s, &[written.erase(), built.erase()]);
    let mut m = s.mods_mut();
    let offset = m.macro_(1.0);
    for node in [written, built] {
        let drive = ParamRef::unit(node, UnitParam::Drive);
        m.range_raw(drive, 0.0, 10.0).unwrap();
        m.route(RouteSpec::new(offset, drive, Bipolar::of(0.2)))
            .unwrap();
    }
    settle(&mut s);
    s.set(written.drive().to(Drive::new(6.0))).unwrap();
    let _ = s.update();

    // Past the lanes' connect declick (256 frames).
    let mut live = [0.0f32; 2];
    for _ in 0..512 {
        s.render_frame(InterleavedMut::one_frame(&mut live).unwrap());
    }
    let mut fork = s
        .graph_ref()
        .testing()
        .fork_live()
        .expect("every knob is forkable");
    let mut forked = [0.0f32; 2];
    fork.tick(InterleavedMut::one_frame(&mut forked).unwrap());
    assert_eq!(live[0], 8.0, "the knob written under its modulation, live");
    assert_eq!(forked[0], 8.0, "and in the fork");
    assert_eq!(live[1], 3.0, "the knob built under its modulation, live");
    assert_eq!(forked[1], 3.0, "and in the fork");
}

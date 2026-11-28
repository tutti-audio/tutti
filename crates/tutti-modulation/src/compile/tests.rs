//! `compile`'s properties, against analytic figures.

use super::*;
use crate::LfoShape;
use crate::shape::shape;
use crate::{Adsr, NoteEnvelope};
use tutti_curve::Exponent;
use tutti_curve::{Breakpoint, Trajectory};
use tutti_rt::Param;
use tutti_units::{Amplitude, Ballistics, Frame, Hz, ParamId, Seconds, UnitParam};

const SR: SampleRate = SampleRate::SR_48K;

fn now(epoch: u32, play: u64) -> PlayReading {
    PlayReading {
        epoch,
        play: Frame::new(play),
    }
}

fn target(node: u64) -> TargetKey {
    TargetKey::param(node, ParamAddr::Unit(UnitParam::Cutoff))
}

fn plugin(node: u64, id: u32) -> TargetKey {
    TargetKey::param(node, ParamAddr::Id(ParamId::new(id)))
}

fn ranged(min: f64, max: f64) -> TargetDecl {
    TargetDecl {
        min,
        max,
        automation: None,
    }
}

fn lfo(shape: LfoShape, clock: LayerClock) -> TimeSource {
    TimeSource::Lfo {
        wave: Waveform::Shape { shape, seed: 0 },
        clock,
    }
}

fn beats(per_cycle: f64) -> LayerClock {
    LayerClock::Beats {
        per_cycle: BeatsPerCycle::of(per_cycle),
        phase: PhaseIncrement::new(0.0),
    }
}

fn seconds(rate: f64) -> LayerClock {
    LayerClock::Seconds {
        rate: Hz::new(rate),
        phase: PhaseIncrement::new(0.0),
        anchor: None,
    }
}

fn route(source: u64, to: TargetKey, depth: f64) -> RouteDecl {
    RouteDecl {
        source: SourceKey(source),
        to,
        depth: Param::new(Bipolar::of(depth)),
        polarity: Polarity::Bipolar,
        curve: Law::Linear,
        scale: None,
    }
}

/// `route` with its depth scaled by `source` at `depth`, bipolar and linear.
fn scaled(route: RouteDecl, source: u64, depth: f64) -> RouteDecl {
    RouteDecl {
        scale: Some(Scale {
            source: SourceKey(source),
            depth: Param::new(Bipolar::of(depth)),
            polarity: Polarity::Bipolar,
            curve: Law::Linear,
        }),
        ..route
    }
}

fn to(node: u64) -> TargetKey {
    target(node)
}

fn follower() -> SignalKind {
    SignalKind::Follower(
        Ballistics::new(Seconds::new(0.01), Seconds::new(0.2)).expect("valid times"),
    )
}

fn adsr() -> Adsr {
    Adsr {
        attack: Seconds::new(0.01),
        decay: Seconds::new(0.1),
        sustain: Amplitude::new(0.6),
        release: Seconds::new(0.3),
    }
}

fn note_envelope() -> SignalKind {
    SignalKind::NoteEnvelope(NoteEnvelope::new(adsr()).expect("a valid ADSR"))
}

fn run(
    time: &[(SourceKey, TimeSource)],
    routes: &[(RouteKey, RouteDecl)],
    targets: &[(TargetKey, TargetDecl)],
) -> Result<Compiled, ProgramCompileError> {
    run_signals(time, &[], routes, targets)
}

fn run_signals(
    time: &[(SourceKey, TimeSource)],
    signals: &[(SourceKey, SignalKind)],
    routes: &[(RouteKey, RouteDecl)],
    targets: &[(TargetKey, TargetDecl)],
) -> Result<Compiled, ProgramCompileError> {
    compile(
        &Declarations {
            time,
            signals,
            routes,
            targets,
        },
        None,
        now(1, 0),
        SR,
    )
}

fn time(beat: f64, play: f64, epoch: u32) -> ProgramTime {
    ProgramTime {
        beat: Beat::new(beat),
        play: SamplePosition::new(play),
        play_epoch: epoch,
        rate: SR,
    }
}

/// A frozen trajectory: what a host hands as automation.
fn trajectory() -> Arc<dyn Curve> {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(100.0)));
    e.add_breakpoint(Breakpoint::new(Beat::new(4.0), Hz::new(900.0)));
    Arc::new(e)
}

/// A rig of every source kind: two LFOs (one scaled by a macro), a macro,
/// a follower and a note envelope, into three native targets and a plugin
/// parameter, one with automation.
struct Rig {
    time: Vec<(SourceKey, TimeSource)>,
    signals: Vec<(SourceKey, SignalKind)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    targets: Vec<(TargetKey, TargetDecl)>,
}

impl Rig {
    fn new() -> Self {
        let time = vec![
            (SourceKey(10), lfo(LfoShape::Sine, seconds(2.0))),
            (SourceKey(11), lfo(LfoShape::Triangle, beats(4.0))),
            (SourceKey(12), TimeSource::Macro { value: 0.5 }),
            (SourceKey(15), TimeSource::Macro { value: -0.25 }),
        ];
        let signals = vec![
            (SourceKey(13), follower()),
            (SourceKey(14), note_envelope()),
        ];
        let routes = vec![
            (RouteKey(100), route(10, to(1), 0.3)),
            (RouteKey(101), scaled(route(11, to(1), 0.1), 12, 1.0)),
            (RouteKey(102), route(12, to(2), 0.2)),
            (RouteKey(103), route(13, to(2), -0.6)),
            (RouteKey(104), route(14, plugin(4, 7), 0.5)),
            (RouteKey(106), route(15, to(3), 0.4)),
            (RouteKey(107), route(13, to(1), 0.25)),
        ];
        let targets = vec![
            (target(1), ranged(0.0, 1000.0)),
            (target(2), ranged(-1.0, 1.0)),
            (
                target(3),
                TargetDecl {
                    automation: Some(trajectory()),
                    ..ranged(20.0, 20_000.0)
                },
            ),
            (plugin(4, 7), ranged(0.0, 1.0)),
        ];
        Self {
            time,
            signals,
            routes,
            targets,
        }
    }

    fn compile(&self, prev: Option<&Compiled>, at: PlayReading) -> Compiled {
        compile(
            &Declarations {
                time: &self.time,
                signals: &self.signals,
                routes: &self.routes,
                targets: &self.targets,
            },
            prev,
            at,
            SR,
        )
        .expect("compiles")
    }

    fn route_mut(&mut self, key: u64) -> &mut RouteDecl {
        &mut self
            .routes
            .iter_mut()
            .find(|(k, _)| k.0 == key)
            .expect("a route")
            .1
    }

    fn source_mut(&mut self, key: u64) -> &mut TimeSource {
        &mut self
            .time
            .iter_mut()
            .find(|(k, _)| k.0 == key)
            .expect("a source")
            .1
    }
}

/// Equal declarations compile equal, in any order: reversed, rotated and
/// interleaved, every list; and the rig compiles to what it says (a program
/// per routed or automated target, a node per signal source, an edge per
/// signal route).
#[test]
fn equal_declarations_in_any_order_compile_equal() {
    let rig = Rig::new();
    let base = rig.compile(None, now(1, 0));
    assert_eq!(
        base.programs.keys().copied().collect::<Vec<_>>(),
        [target(1), target(2), target(3), plugin(4, 7)]
    );
    assert_eq!(base.nodes.len(), 2, "the follower and the note envelope");
    assert_eq!(
        base.edges.iter().copied().collect::<Vec<_>>(),
        [
            (SourceKey(13), target(1)),
            (SourceKey(13), target(2)),
            (SourceKey(14), plugin(4, 7)),
        ]
    );
    fn reorder<T: Clone>(how: usize, v: &mut Vec<T>) {
        match how {
            0 => v.reverse(),
            1 => v.rotate_left(1),
            _ => {
                let n = v.len();
                let (a, b) = v.split_at(n / 2);
                *v = b.iter().cloned().chain(a.iter().cloned()).collect();
            }
        }
    }
    for how in 0..3 {
        let mut r = Rig::new();
        reorder(how, &mut r.time);
        reorder(how, &mut r.signals);
        // A route's depth is a cell compared by identity: the clone shares it.
        let mut rt = rig.routes.clone();
        rt.reverse();
        r.routes = rt;
        // A `TargetDecl` is `Clone` (its curve an `Arc`, kept by identity).
        let mut ts = rig.targets.clone();
        ts.reverse();
        r.targets = ts;
        assert_eq!(r.compile(None, now(1, 0)), base);
    }
}

/// One depth convention: for every source kind (LFO, macro, follower, note envelope,
/// and an LFO scaled by a macro through `depth_by`), each polarity and
/// curve, the offset at raw ±1 is `shape(±1, depth, polarity, curve) ×
/// span`, and at raw ±0.5 likewise (where the curve bends: every curve
/// maps 1 to 1), on a native target and a plugin parameter alike: the compiled
/// program's `value_at` (the lane's evaluation) on knob 0 reads exactly
/// that.
#[test]
fn the_offset_at_raw_unit_is_shape_times_span_for_every_source_kind() {
    let curves = [
        Law::Linear,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
        Law::PowerInOut(Exponent::of(2.0)),
    ];
    let span = 800.0f64;
    for polarity in [Polarity::Bipolar, Polarity::Unipolar] {
        for curve in curves {
            for raw in [1.0f64, -1.0, 0.5, -0.5] {
                let depth = Bipolar::of(-0.35);
                let want = 0.0f64 + (-0.0 + shape(raw, depth, polarity, curve) * span);
                let r = |source, t| RouteDecl {
                    source: SourceKey(source),
                    to: (t),
                    depth: Param::new(depth),
                    polarity,
                    curve,
                    scale: None,
                };
                // A triangle on 4 beats is +1 at beat 1, −1 at beat 3, +0.5
                // at beat 0.5 and −0.5 at beat 2.5, exactly.
                let beat = match raw {
                    1.0 => 1.0,
                    -1.0 => 3.0,
                    0.5 => 0.5,
                    _ => 2.5,
                };
                let at = time(beat, 0.0, 1);
                let time = [
                    (SourceKey(1), lfo(LfoShape::Triangle, beats(4.0))),
                    (SourceKey(2), TimeSource::Macro { value: raw }),
                    (SourceKey(5), TimeSource::Macro { value: 1.0 }),
                ];
                let signals = [(SourceKey(3), follower()), (SourceKey(4), note_envelope())];
                let routes = [
                    (RouteKey(1), r(1, target(1))),
                    (RouteKey(2), r(2, target(2))),
                    (RouteKey(3), r(3, target(3))),
                    (RouteKey(4), r(4, plugin(4, 1))),
                    // Scaled by a macro at +1 through the unit amount: a scale
                    // of exactly 1.
                    (
                        RouteKey(5),
                        RouteDecl {
                            scale: Some(Scale {
                                source: SourceKey(5),
                                depth: Param::new(Bipolar::MAX),
                                polarity: Polarity::Unipolar,
                                curve: Law::Linear,
                            }),
                            ..r(1, plugin(5, 2))
                        },
                    ),
                ];
                let targets = [
                    (target(1), ranged(0.0, 800.0)),
                    (target(2), ranged(0.0, 800.0)),
                    (target(3), ranged(0.0, 800.0)),
                    (plugin(4, 1), ranged(0.0, 800.0)),
                    (plugin(5, 2), ranged(0.0, 800.0)),
                ];
                let c = run_signals(&time, &signals, &routes, &targets).expect("compiles");
                let at_unit = |t: TargetKey, inputs: &[Option<f64>]| {
                    c.programs[&t].value_at(None, Some(0.0), &at, span, inputs)
                };
                let case = format!("{polarity:?} {curve:?} raw {raw}");
                assert_eq!(at_unit(target(1), &[]), Some(want), "LFO, {case}");
                assert_eq!(at_unit(target(2), &[]), Some(want), "macro, {case}");
                assert_eq!(
                    at_unit(target(3), &[Some(raw)]),
                    Some(want),
                    "follower, {case}"
                );
                assert_eq!(
                    at_unit(plugin(4, 1), &[Some(raw)]),
                    Some(want),
                    "note envelope, {case}"
                );
                assert_eq!(
                    at_unit(plugin(5, 2), &[]),
                    Some(want),
                    "depth_by-scaled LFO, {case}"
                );
            }
        }
    }
}

/// A depth write changes no program, and keeps every id: the route's layer
/// reads its depth from the route's cell, so the compile is the same and the
/// program's depth snapshot is the written value.
#[test]
fn a_depth_write_changes_no_program_and_the_snapshot_reads_it() {
    let mut rig = Rig::new();
    let before = rig.compile(None, now(1, 0));
    let snapshot = before.programs[&target(1)].depth_snapshot();
    rig.route_mut(100).depth.store(Bipolar::of(0.7));
    let after = rig.compile(Some(&before), now(1, 5_000));
    assert_eq!(after, before);
    assert_ne!(
        after.programs[&target(1)].depth_snapshot(),
        snapshot,
        "the lane reads the written depth"
    );
}

/// A macro move changes exactly the programs it reaches: macro 12 goes to
/// target 2 directly and scales route 101 (to target 1); targets 3 and the
/// plugin parameter are equal; and the moved macro reads at its new value.
#[test]
fn a_macro_move_changes_exactly_the_programs_it_reaches() {
    let mut rig = Rig::new();
    let before = rig.compile(None, now(1, 0));
    *rig.source_mut(12) = TimeSource::Macro { value: -0.75 };
    let after = rig.compile(Some(&before), now(1, 0));
    let changed: Vec<TargetKey> = after
        .programs
        .iter()
        .filter(|(t, p)| before.programs[*t] != **p)
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(changed, [target(1), target(2)]);
    let at = time(0.0, 0.0, 1);
    assert_eq!(
        after.programs[&target(2)].value_at(None, Some(0.0), &at, 2.0, &[]),
        Some(-0.0 + shape(-0.75f64, Bipolar::of(0.2), Polarity::Bipolar, Law::Linear) * 2.0),
        "depth 0.2 of a span of 2, at the macro's −0.75"
    );
}

/// A rate edit anchors continuously from the previous program: a 2 Hz sine
/// edited to 5 Hz at play 10 000 (epoch 7) takes effect
/// `RATE_ANCHOR_MARGIN` frames on, where the new program's phase is the
/// old one's exactly; before it the new program is the old one (within
/// 1e-9 cycles), from it the phase moves at 5 Hz; and the value has no
/// step at the anchor larger than one frame's slope at the new rate.
#[test]
fn a_rate_edit_anchors_continuously() {
    let mut rig = Rig::new();
    let before = rig.compile(None, now(7, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(5.0));
    let edit = now(7, 10_000);
    let after = rig.compile(Some(&before), edit);
    let at = edit.after(RATE_ANCHOR_MARGIN).play;
    let id = layer_id(RouteKey(100), None);
    let clock = |c: &Compiled| {
        c.programs[&target(1)]
            .layers()
            .iter()
            .find(|l| l.id == id)
            .expect("the layer")
            .clock
    };
    let LayerClock::Seconds {
        anchor: Some(a), ..
    } = clock(&after)
    else {
        panic!("anchored: {:?}", clock(&after));
    };
    assert_eq!((a.epoch, a.at, a.prev_rate), (7, at, Hz::new(2.0)));
    let cyc = |c: &Compiled, play: f64| clock(c).cycles(&time(0.0, play, 7)).expect("a phase");
    let t = at.get() as f64;
    assert_eq!(
        cyc(&after, t),
        cyc(&before, t),
        "phase-continuous at the anchor"
    );
    for p in [10_000.0, 15_000.0, t - 1.0] {
        assert!((cyc(&after, p) - cyc(&before, p)).abs() < 1e-9, "at {p}");
    }
    let slope = |p: f64| (cyc(&after, p + 480.0) - cyc(&after, p)) / 480.0 * SR.get();
    assert!((slope(t) - 5.0).abs() < 1e-6, "5 Hz from the anchor");
    assert!((slope(t - 960.0) - 2.0).abs() < 1e-6, "2 Hz before it");
    // The value through the lane's evaluation: no step at the anchor.
    let v = |play: f64| {
        let p = &after.programs[&target(1)];
        p.value_at(None, Some(0.0), &time(0.0, play, 7), 1000.0, &[None])
            .expect("a value")
    };
    // 0.3 of a span of 1000 at 5 Hz: at most 300 · 2π · 5 / 48 000 a frame.
    let frame_slope = 300.0 * std::f64::consts::TAU * 5.0 / SR.get();
    assert!((v(t) - v(t - 1.0)).abs() <= frame_slope * 1.01);
}

/// What follows a rate edit keeps it: a depth edit republished while the
/// anchor is in force carries it unchanged (else the phase would jump back
/// to `rate · play`); a second rate edit while the anchor is still
/// `ANCHOR_REUSE_MIN` or more ahead keeps the anchor's frame (the near case
/// is `a_second_rate_edit_near_its_anchor_reanchors`); a play start (a new
/// epoch) drops it; and a phase edit moves a carried anchor's phase by the
/// edit.
#[test]
fn an_anchor_is_carried_until_a_play_start() {
    let id = layer_id(RouteKey(100), None);
    let anchor = |c: &Compiled| match c.programs[&target(1)]
        .layers()
        .iter()
        .find(|l| l.id == id)
        .expect("the layer")
        .clock
    {
        LayerClock::Seconds { anchor, .. } => anchor,
        LayerClock::Beats { .. } => None,
    };
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(7, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(5.0));
    let c1 = rig.compile(Some(&c0), now(7, 10_000));
    let a1 = anchor(&c1).expect("anchored");

    rig.route_mut(100).depth.store(Bipolar::of(0.9));
    let c2 = rig.compile(Some(&c1), now(7, 30_000));
    assert_eq!(anchor(&c2), Some(a1), "a depth edit carries it");

    let mut pending = Rig::new();
    *pending.source_mut(10) = lfo(LfoShape::Sine, seconds(3.0));
    let c3 = pending.compile(Some(&c1), now(7, 12_000));
    let a3 = anchor(&c3).expect("anchored");
    assert_eq!(
        (a3.at, a3.phase, a3.prev_rate),
        (a1.at, a1.phase, a1.prev_rate),
        "a second edit before the anchor lands on its frame"
    );

    let c4 = rig.compile(Some(&c2), now(8, 0));
    assert_eq!(anchor(&c4), None, "a play start drops it");

    *rig.source_mut(10) = TimeSource::Lfo {
        wave: Waveform::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        clock: LayerClock::Seconds {
            rate: Hz::new(5.0),
            phase: PhaseIncrement::new(0.25),
            anchor: None,
        },
    };
    let c5 = rig.compile(Some(&c2), now(7, 40_000));
    let a5 = anchor(&c5).expect("carried");
    assert_eq!((a5.at, a5.prev_rate), (a1.at, a1.prev_rate));
    assert_eq!(a5.phase, a1.phase + 0.25, "the phase edit moves it");
    // Equal inputs, equal outputs: compiling again changes nothing.
    assert_eq!(rig.compile(Some(&c2), now(7, 40_000)), c5);
}

/// A signal source keeps its `SourceId` across compiles: adding a follower
/// with a lower key does not renumber the existing ones (the new one takes
/// the next id never given). Removing one frees nothing: no later compile
/// gives its id again, so no lane can read a removed source's last segment
/// as a new source's.
#[test]
fn a_signal_source_keeps_its_id() {
    let rig = Rig::new();
    let c0 = rig.compile(None, now(1, 0));
    assert_eq!(c0.nodes[&SourceKey(13)].id, SourceId::new(ParamId::new(0)));
    assert_eq!(c0.nodes[&SourceKey(14)].id, SourceId::new(ParamId::new(1)));
    let mut more = Rig::new();
    more.signals.push((SourceKey(1), follower()));
    let c1 = more.compile(Some(&c0), now(1, 0));
    assert_eq!(c1.nodes[&SourceKey(13)].id, SourceId::new(ParamId::new(0)));
    assert_eq!(c1.nodes[&SourceKey(14)].id, SourceId::new(ParamId::new(1)));
    assert_eq!(c1.nodes[&SourceKey(1)].id, SourceId::new(ParamId::new(2)));
    assert_eq!(c1.nodes[&SourceKey(14)].kind, note_envelope());

    // Remove follower 13 (and its routes) and add follower 2 in one compile.
    let mut swap = more;
    swap.signals.retain(|(k, _)| k.0 != 13);
    swap.routes.retain(|(_, r)| r.source != SourceKey(13));
    swap.signals.push((SourceKey(2), follower()));
    let c2 = swap.compile(Some(&c1), now(1, 0));
    assert!(!c2.nodes.contains_key(&SourceKey(13)));
    assert_eq!(
        c2.nodes[&SourceKey(2)].id,
        SourceId::new(ParamId::new(3)),
        "not 13's id"
    );
    // Nor in any later compile.
    swap.signals.push((SourceKey(3), follower()));
    let c3 = swap.compile(Some(&c2), now(1, 0));
    assert_eq!(c3.nodes[&SourceKey(2)].id, SourceId::new(ParamId::new(3)));
    assert_eq!(
        c3.nodes[&SourceKey(3)].id,
        SourceId::new(ParamId::new(4)),
        "never 13's id"
    );
    assert_eq!(c3.next_source, SourceId::new(ParamId::new(5)));
}

/// Each refusal fires, by name.
#[test]
fn each_refusal_fires_by_name() {
    let t1 = [(target(1), ranged(0.0, 1.0))];
    let one_lfo = [(SourceKey(1), lfo(LfoShape::Sine, beats(1.0)))];

    // Past MAX_LANE_LAYERS: 17 routes from one LFO into one target.
    let routes: Vec<_> = (0..17)
        .map(|k| (RouteKey(k), route(1, to(1), 0.1)))
        .collect();
    assert_eq!(
        run(&one_lfo, &routes, &t1),
        Err(ProgramCompileError::TooManyLayers {
            target: target(1),
            count: 17
        })
    );
    // Past MAX_LANE_INPUTS: 9 follower routes.
    let f = [(SourceKey(1), follower())];
    let routes: Vec<_> = (0..9)
        .map(|k| (RouteKey(k), route(1, to(1), 0.1)))
        .collect();
    assert_eq!(
        run_signals(&[], &f, &routes, &t1),
        Err(ProgramCompileError::TooManyInputs {
            target: target(1),
            count: 9
        })
    );
    let r1 = [(RouteKey(1), route(1, to(1), 0.5))];
    // A NaN range bound.
    assert_eq!(
        run(&one_lfo, &r1, &[(target(1), ranged(f64::NAN, 1.0))]),
        Err(ProgramCompileError::NanRange(target(1)))
    );
    // A circular target's range is an arc, min at most max: 170..-170 would
    // read the opposite arc once ordered. A line takes crossed bounds as ever,
    // and an arc across ±180 is 170..190.
    let bearing = TargetKey::param(1, ParamAddr::Unit(UnitParam::Azimuth));
    assert_eq!(
        run(&one_lfo, &[], &[(bearing, ranged(170.0, -170.0))]),
        Err(ProgramCompileError::CrossedArc(bearing))
    );
    assert!(run(&one_lfo, &[], &[(bearing, ranged(170.0, 190.0))]).is_ok());
    let height = TargetKey::param(1, ParamAddr::Unit(UnitParam::Elevation));
    assert!(run(&one_lfo, &[], &[(height, ranged(170.0, -170.0))]).is_ok());
    // An automation curve that is not frozen.
    struct Live;
    impl Curve for Live {
        fn value_at(&self, _: Beat) -> Option<f64> {
            Some(0.0)
        }
        fn frozen(&self) -> Option<Arc<dyn Curve>> {
            Some(Arc::new(Live))
        }
    }
    let live = [(
        target(1),
        TargetDecl {
            automation: Some(Arc::new(Live)),
            ..ranged(0.0, 1.0)
        },
    )];
    assert_eq!(
        run(&one_lfo, &r1, &live),
        Err(ProgramCompileError::LiveCurve(target(1)))
    );
    // A scale from a signal source, and of a route from one.
    let time = [(SourceKey(1), lfo(LfoShape::Sine, beats(1.0)))];
    let signals = [(SourceKey(2), follower())];
    let from_signal = [(RouteKey(1), scaled(route(1, to(1), 0.5), 2, 1.0))];
    assert_eq!(
        run_signals(&time, &signals, &from_signal, &t1),
        Err(ProgramCompileError::DepthBySignal { route: RouteKey(1) })
    );
    let of_signal = [(RouteKey(1), scaled(route(2, to(1), 0.5), 1, 1.0))];
    assert_eq!(
        run_signals(&time, &signals, &of_signal, &t1),
        Err(ProgramCompileError::DepthBySignal { route: RouteKey(1) })
    );
    // Unknown references.
    assert_eq!(
        run(&[], &r1, &t1),
        Err(ProgramCompileError::UnknownSource { route: RouteKey(1) })
    );
    assert_eq!(
        run(
            &one_lfo,
            &[(RouteKey(2), scaled(route(1, to(1), 1.0), 9, 1.0))],
            &t1
        ),
        Err(ProgramCompileError::UnknownSource { route: RouteKey(2) })
    );
    assert_eq!(
        run(&one_lfo, &r1, &[]),
        Err(ProgramCompileError::UndeclaredTarget { route: RouteKey(1) })
    );
    // Duplicate keys.
    assert_eq!(
        run(&[one_lfo[0].clone(), one_lfo[0].clone()], &r1, &t1),
        Err(ProgramCompileError::DuplicateSource(SourceKey(1)))
    );
    assert_eq!(
        run(&one_lfo, &[r1[0].clone(), r1[0].clone()], &t1),
        Err(ProgramCompileError::DuplicateRoute(RouteKey(1)))
    );
    assert_eq!(
        run(&one_lfo, &r1, &[t1[0].clone(), t1[0].clone()]),
        Err(ProgramCompileError::DuplicateTarget(target(1)))
    );
    assert_eq!(
        run_signals(&one_lfo, &[(SourceKey(1), follower())], &r1, &t1),
        Err(ProgramCompileError::DuplicateSource(SourceKey(1))),
        "one key names one source across both lists"
    );
    // Time sources out of their domain (a follower's and a note envelope's are
    // refused where `Ballistics` and `NoteEnvelope` are built), and a declared anchor.
    let bad_sources = [
        TimeSource::Macro { value: f64::NAN },
        lfo(LfoShape::Sine, seconds(f64::NAN)),
        TimeSource::Lfo {
            wave: Waveform::Const(f64::NAN),
            clock: beats(1.0),
        },
    ];
    for s in bad_sources {
        assert_eq!(
            run(&[(SourceKey(1), s.clone())], &r1, &t1),
            Err(ProgramCompileError::BadSource(SourceKey(1))),
            "{s:?}"
        );
    }
    let anchored = TimeSource::Lfo {
        wave: Waveform::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        clock: LayerClock::Seconds {
            rate: Hz::new(1.0),
            phase: PhaseIncrement::new(0.0),
            anchor: Some(RateAnchor {
                epoch: 1,
                at: Frame::new(0),
                phase: 0.0,
                prev_rate: Hz::new(1.0),
            }),
        },
    };
    assert_eq!(
        run(&[(SourceKey(1), anchored)], &r1, &t1),
        Err(ProgramCompileError::AnchorDeclared(SourceKey(1)))
    );
    // At the caps, it compiles.
    let routes: Vec<_> = (0..16)
        .map(|k| (RouteKey(k), route(1, to(1), 0.1)))
        .collect();
    assert!(run(&one_lfo, &routes, &t1).is_ok());
}

/// Every declaration a host can make compiles with no runtime (no driver, no
/// target), each feature as a declaration:
///
/// - an LFO at a seconds clock or a beat clock with a phase offset, every shape
///   (the random ones as hashed waves);
/// - a route with a depth, polarity and curve to a native param or a plugin
///   parameter (`ParamAddr::Id`), every one a lane; a disabled one is left
///   out;
/// - a target's range (its base is the knob, a cell, not a declaration);
/// - a plugin parameter's automation curve, and an LFO route into a plugin
///   parameter: automation and a layer.
///
/// A source with no closed form declares one with `declare() -> Source`.
///
/// It pins that the declarations compile, which the type checker and the run assert.
/// The no-runtime half is `cargo check -p tutti-modulation`.
#[test]
fn every_declaration_a_host_can_make_compiles_without_a_runtime() {
    let mut sources = Vec::new();
    let mut routes = Vec::new();
    for (k, &shape) in LfoShape::all().iter().enumerate() {
        let k = k as u64;
        let phase = PhaseIncrement::new(0.25);
        let free = LayerClock::Seconds {
            rate: Hz::new(1.5),
            phase,
            anchor: None,
        };
        let synced = LayerClock::Beats {
            per_cycle: BeatsPerCycle::of(2.0),
            phase,
        };
        sources.push((
            SourceKey(2 * k),
            TimeSource::Lfo {
                wave: Waveform::Shape { shape, seed: 0 },
                clock: free,
            },
        ));
        sources.push((
            SourceKey(2 * k + 1),
            TimeSource::Lfo {
                wave: Waveform::Shape { shape, seed: 0 },
                clock: synced,
            },
        ));
        for (j, (polarity, curve)) in [
            (Polarity::Bipolar, Law::Linear),
            (Polarity::Unipolar, Law::PowerIn(Exponent::of(2.0))),
        ]
        .into_iter()
        .enumerate()
        {
            let s = 2 * k + j as u64;
            routes.push((
                RouteKey(100 + s),
                RouteDecl {
                    source: SourceKey(s),
                    to: (target(1 + k % 2)),
                    depth: Param::new(Bipolar::of(0.3)),
                    polarity,
                    curve,
                    scale: None,
                },
            ));
        }
    }
    routes.push((RouteKey(999), route(0, plugin(5, 3), 0.2)));
    let targets = [
        (target(1), ranged(20.0, 20_000.0)),
        (target(2), ranged(0.0, 1.0)),
        (
            plugin(5, 3),
            TargetDecl {
                automation: Some(trajectory()),
                ..ranged(0.0, 1.0)
            },
        ),
    ];
    let c = run(&sources, &routes, &targets).expect("compiles");
    assert_eq!(c.programs.len(), 3);
    assert!(c.nodes.is_empty() && c.edges.is_empty(), "no signal source");
    assert_eq!(
        c.programs[&target(1)].layers().len() + c.programs[&target(2)].layers().len(),
        LfoShape::all().len() * 2
    );
}

/// The clock of `route`'s layer in `target` (`depth_by` its scaler's id).
fn layer_clock(c: &Compiled, t: TargetKey, route: u64, depth_by: Option<u64>) -> LayerClock {
    let id = layer_id(RouteKey(route), depth_by.map(SourceKey));
    c.programs[&t]
        .layers()
        .iter()
        .find(|l| l.id == id)
        .expect("the layer")
        .clock
}

fn anchor_of(clock: LayerClock) -> Option<RateAnchor> {
    match clock {
        LayerClock::Seconds { anchor, .. } => anchor,
        LayerClock::Beats { .. } => None,
    }
}

/// One LFO is phase-coherent across its targets: a route
/// added to an LFO after its rate edit reads the anchor the LFO's other
/// layers read, so at any frame both targets are at one phase.
#[test]
fn a_route_added_after_a_rate_edit_reads_the_lfos_anchor() {
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(3, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(4.0));
    let c1 = rig.compile(Some(&c0), now(3, 480_000));
    rig.routes.push((RouteKey(200), route(10, to(2), 0.5)));
    let c2 = rig.compile(Some(&c1), now(3, 600_000));
    let one = layer_clock(&c2, target(1), 100, None);
    let two = layer_clock(&c2, target(2), 200, None);
    assert!(anchor_of(one).is_some(), "the edit's anchor is carried");
    assert_eq!(one, two, "both layers read the LFO's one clock");
    assert_eq!(c2.clocks[&SourceKey(10)], one);
    let t = time(0.0, 1e6, 3);
    assert_eq!(one.cycles(&t), two.cycles(&t));
}

/// A route moved to another LFO takes that LFO's clock: it is not a rate
/// edit of the route's layer, and does not continue the first LFO's phase
/// at the second's rate.
#[test]
fn a_route_moved_to_another_lfo_takes_that_lfos_clock() {
    let mut rig = Rig::new();
    rig.time
        .push((SourceKey(20), lfo(LfoShape::Sine, seconds(3.0))));
    rig.routes.push((RouteKey(200), route(20, to(2), 0.5)));
    let c0 = rig.compile(None, now(3, 0));
    rig.route_mut(100).source = SourceKey(20);
    let c1 = rig.compile(Some(&c0), now(3, 480_000));
    let moved = layer_clock(&c1, target(1), 100, None);
    assert_eq!(moved, seconds(3.0), "B's own clock, no anchor");
    assert_eq!(moved, layer_clock(&c1, target(2), 200, None));
}

/// Adding or removing a `depth_by` (which changes the layer's id) keeps the
/// LFO's live anchor: the layer is the same LFO's, at the same phase.
#[test]
fn a_depth_by_change_keeps_the_lfos_anchor() {
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(3, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(4.0));
    let c1 = rig.compile(Some(&c0), now(3, 480_000));
    let a1 = anchor_of(layer_clock(&c1, target(1), 100, None)).expect("anchored");
    rig.route_mut(100).scale = scaled(route(10, to(1), 0.0), 15, 1.0).scale;
    let c2 = rig.compile(Some(&c1), now(3, 600_000));
    let scaled = layer_clock(&c2, target(1), 100, Some(15));
    assert_eq!(anchor_of(scaled), Some(a1), "adding a depth_by keeps it");
    rig.route_mut(100).scale = None;
    let c3 = rig.compile(Some(&c2), now(3, 700_000));
    let plain = layer_clock(&c3, target(1), 100, None);
    assert_eq!(anchor_of(plain), Some(a1), "removing it keeps it");
    let t = time(0.0, 1e6, 3);
    assert_eq!(
        plain.cycles(&t),
        layer_clock(&c1, target(1), 100, None).cycles(&t)
    );
}

/// A second rate edit while the first's anchor is pending but nearer than
/// `ANCHOR_REUSE_MIN` re-anchors at `now + RATE_ANCHOR_MARGIN` rather than
/// reuse a frame the publish may miss. The new clock agrees with the old
/// one from the old anchor on; before it, the step a lane switching at `s`
/// takes is exactly the documented `(R1 − R0) · (a − s)`. (The far branch,
/// reusing the anchor's frame, is `an_anchor_is_carried_until_a_play_start`.)
#[test]
fn a_second_rate_edit_near_its_anchor_reanchors() {
    let mut rig = Rig::new();
    let c0 = rig.compile(None, now(7, 0));
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(5.0));
    let c1 = rig.compile(Some(&c0), now(7, 10_000));
    let old = layer_clock(&c1, target(1), 100, None);
    let a1 = anchor_of(old).expect("anchored");
    *rig.source_mut(10) = lfo(LfoShape::Sine, seconds(3.0));
    let edit = now(7, 20_000);
    assert!(a1.at.since(edit.play).expect("ahead") < ANCHOR_REUSE_MIN);
    let c2 = rig.compile(Some(&c1), edit);
    let new = layer_clock(&c2, target(1), 100, None);
    let a2 = anchor_of(new).expect("anchored");
    assert_eq!(
        (a2.at, a2.prev_rate),
        (edit.after(RATE_ANCHOR_MARGIN).play, Hz::new(5.0))
    );
    let cyc = |c: LayerClock, p: f64| c.cycles(&time(0.0, p, 7)).expect("a phase");
    let a = a1.at.get() as f64;
    let b = a2.at.get() as f64;
    for p in [a, a + 1000.0, b - 1.0, b] {
        assert!((cyc(new, p) - cyc(old, p)).abs() < 1e-9, "agrees at {p}");
    }
    for p in [20_000.0, 21_000.0, a - 1.0] {
        let step = (5.0 - 2.0) * (a - p) / SR.get();
        assert!(
            ((cyc(old, p) - cyc(new, p)) - step).abs() < 1e-9,
            "the documented step at {p}"
        );
    }
}

/// A value that is not finite is refused where it would reach the lane: an
/// infinite constant wave (a route's depth cannot be one: `Bipolar` is
/// range-checked), and a clock's infinite rate or phase (a beat span cannot
/// be one: `BeatsPerCycle` is range-checked),
/// on an LFO no route reads (so the source check refuses it, not a
/// program). An infinite phase after a rate edit would otherwise carry the
/// live anchor with a jump of `inf − inf`, a NaN in `Compiled::clocks`
/// that breaks `Compiled: Eq`.
#[test]
fn a_value_that_is_not_finite_is_refused() {
    let t1 = [(target(1), ranged(0.0, 1.0))];
    for v in [f64::INFINITY, f64::NAN] {
        let constant = [(
            SourceKey(1),
            TimeSource::Lfo {
                wave: Waveform::Const(v),
                clock: beats(1.0),
            },
        )];
        assert_eq!(
            run(&constant, &[(RouteKey(1), route(1, to(1), 0.5))], &t1),
            Err(ProgramCompileError::BadSource(SourceKey(1))),
            "constant {v}"
        );
    }
    let clock = |rate: f64, phase: f64| LayerClock::Seconds {
        rate: Hz::new(rate),
        phase: PhaseIncrement::new(phase),
        anchor: None,
    };
    let beat = |phase: f64| LayerClock::Beats {
        per_cycle: BeatsPerCycle::of(1.0),
        phase: PhaseIncrement::new(phase),
    };
    for c in [
        clock(f64::INFINITY, 0.0),
        clock(1.0, f64::INFINITY),
        clock(1.0, f64::NEG_INFINITY),
        beat(f64::INFINITY),
    ] {
        let unrouted = [(SourceKey(1), lfo(LfoShape::Sine, c))];
        assert_eq!(
            run(&unrouted, &[], &t1),
            Err(ProgramCompileError::BadSource(SourceKey(1))),
            "{c:?}"
        );
    }
    // A rate edit (a live anchor), then an
    // infinite phase on the unrouted LFO.
    let at = |rate, phase, prev: Option<&Compiled>, play| {
        compile(
            &Declarations {
                time: &[(SourceKey(10), lfo(LfoShape::Sine, clock(rate, phase)))],
                signals: &[],
                routes: &[],
                targets: &t1,
            },
            prev,
            now(3, play),
            SR,
        )
    };
    let c0 = at(1.0, 0.0, None, 0).expect("compiles");
    let c1 = at(2.0, 0.0, Some(&c0), 1000).expect("compiles");
    assert!(anchor_of(c1.clocks[&SourceKey(10)]).is_some());
    assert_eq!(
        at(2.0, f64::INFINITY, Some(&c1), 2000),
        Err(ProgramCompileError::BadSource(SourceKey(10)))
    );
}

/// A finite phase past `MAX_CLOCK_PHASE` is refused by name, in either clock: two such
/// phases of opposite sign (`+1e308`, `-1e308`) would make a carried anchor's jump `inf`
/// and the next sum `inf - inf`. The bound itself compiles.
#[test]
fn a_clock_phase_past_the_bound_is_refused() {
    let t1 = [(target(1), ranged(0.0, 1.0))];
    let seconds = |phase: f64| LayerClock::Seconds {
        rate: Hz::new(1.0),
        phase: PhaseIncrement::new(phase),
        anchor: None,
    };
    let beat = |phase: f64| LayerClock::Beats {
        per_cycle: BeatsPerCycle::of(1.0),
        phase: PhaseIncrement::new(phase),
    };
    for c in [
        seconds(1e308),
        seconds(-1e308),
        beat(1e308),
        beat(-MAX_CLOCK_PHASE * 2.0),
    ] {
        let unrouted = [(SourceKey(1), lfo(LfoShape::Sine, c))];
        assert_eq!(
            run(&unrouted, &[], &t1),
            Err(ProgramCompileError::PhaseOutOfRange(SourceKey(1))),
            "{c:?}"
        );
    }
    for c in [seconds(MAX_CLOCK_PHASE), beat(-MAX_CLOCK_PHASE)] {
        let unrouted = [(SourceKey(1), lfo(LfoShape::Sine, c))];
        assert!(run(&unrouted, &[], &t1).is_ok(), "{c:?}");
    }
}

/// A depth route's scaler reads its own LFO's dated clock, not the scaled
/// LFO's: with a seconds-LFO scaler at another rate than the scaled one,
/// the layer's `DepthBy` clock is `clocks[scaler]`, before and after a
/// rate edit of the scaler (which anchors it).
#[test]
fn a_depth_by_scaler_reads_its_own_lfos_clock() {
    let mut rig = Rig::new();
    rig.time
        .push((SourceKey(20), lfo(LfoShape::Sine, seconds(3.0))));
    rig.route_mut(100).scale = scaled(route(10, to(1), 0.0), 20, 1.0).scale;
    let depth_clock = |c: &Compiled| {
        let id = layer_id(RouteKey(100), Some(SourceKey(20)));
        c.programs[&target(1)]
            .layers()
            .iter()
            .find(|l| l.id == id)
            .expect("the layer")
            .depth_by
            .as_ref()
            .expect("scaled")
            .clock
    };
    let c0 = rig.compile(None, now(7, 0));
    assert_eq!(depth_clock(&c0), c0.clocks[&SourceKey(20)]);
    assert_eq!(depth_clock(&c0), seconds(3.0));
    *rig.source_mut(20) = lfo(LfoShape::Sine, seconds(7.0));
    let c1 = rig.compile(Some(&c0), now(7, 10_000));
    assert!(anchor_of(c1.clocks[&SourceKey(20)]).is_some(), "anchored");
    assert_eq!(depth_clock(&c1), c1.clocks[&SourceKey(20)]);
    assert_eq!(
        layer_clock(&c1, target(1), 100, Some(20)),
        c1.clocks[&SourceKey(10)]
    );
}

/// An edge's gain is a target like a node's param: a route to it compiles
/// to its own program, keyed apart from a param target whose node bits are
/// the same, and a range declared for it is its own.
#[test]
fn an_edge_is_a_target_apart_from_a_param_of_the_same_bits() {
    let edge = TargetKey::edge(1);
    assert_ne!(edge, target(1));
    assert_eq!(edge.node(), None);
    assert_eq!(target(1).node(), Some(1));
    let sources = [(SourceKey(1), lfo(LfoShape::Sine, seconds(1.0)))];
    let routes = [
        (RouteKey(1), route(1, edge, 0.5)),
        (RouteKey(2), route(1, to(1), 0.25)),
    ];
    let targets = [(edge, ranged(-1.0, 1.0)), (target(1), ranged(20.0, 200.0))];
    let c = run(&sources, &routes, &targets).expect("compiles");
    assert!(c.programs.contains_key(&edge));
    assert!(c.programs.contains_key(&target(1)));
    assert_eq!(c.programs.len(), 2);
}

/// A node's mix is a target like a param: keyed apart from an edge's gain and
/// from a param of the node whose bits are the same, on the node it names, and
/// a route to it compiles to its own program over its own range.
#[test]
fn a_mix_is_a_target_apart_from_an_edge_and_a_param_of_the_same_bits() {
    let mix = TargetKey::mix(1);
    assert_ne!(mix, TargetKey::edge(1));
    assert_ne!(mix, target(1));
    assert_eq!(mix.node(), Some(1));
    let sources = [(SourceKey(1), lfo(LfoShape::Sine, seconds(1.0)))];
    let routes = [
        (RouteKey(1), route(1, mix, 0.5)),
        (RouteKey(2), route(1, to(1), 0.25)),
    ];
    let targets = [(mix, ranged(0.0, 1.0)), (target(1), ranged(20.0, 200.0))];
    let c = run(&sources, &routes, &targets).expect("compiles");
    assert!(c.programs.contains_key(&mix));
    assert!(c.programs.contains_key(&target(1)));
    assert_eq!(c.programs.len(), 2);
}

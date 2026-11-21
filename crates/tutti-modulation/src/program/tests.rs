//! `ControlProgram`'s evaluation, corners and refusals, against analytic
//! figures.

use super::*;

use tutti_curve::{Breakpoint, Trajectory};
use tutti_curve::{Curvature, Exponent};
use tutti_rt::{Param, SplitMix64};
use tutti_units::{Azimuth, Hz, ParamId, UnitParam};

// The 1e-12 bounds: each figure is at most 50 f64 operations on values of at most 10
// (the phase argument of 3 cycles times 2 pi, the span 3), each rounding at most 2^-53 (1.1e-16)
// relative, so under 5e-14 absolute; 1e-12 leaves a factor 20 for libm's last-place error.

const SR: f64 = 48_000.0;

fn at_beat(beat: f64) -> ProgramTime {
    ProgramTime {
        beat: Beat::new(beat),
        play: SamplePosition::new(0.0),
        play_epoch: 1,
        rate: SampleRate::of(SR),
    }
}

fn at_play(play: f64, epoch: u32) -> ProgramTime {
    ProgramTime {
        beat: Beat::new(0.0),
        play: SamplePosition::new(play),
        play_epoch: epoch,
        rate: SampleRate::of(SR),
    }
}

fn amount(depth: f64) -> Amount {
    Amount {
        depth: Depth::of(depth),
        polarity: Polarity::Bipolar,
        curve: Law::Linear,
    }
}

fn layer(id: u64, clock: LayerClock, shape: LfoShape, amount: Amount) -> Layer {
    Layer {
        id: LayerId(id),
        clock,
        wave: Waveform::Shape { shape, seed: 0 },
        amount,
        depth_by: None,
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

/// `program` with `depths` bound.
fn bound(
    mut program: ControlProgram,
    depths: impl IntoIterator<Item = (LayerId, Param<Depth>)>,
) -> ControlProgram {
    program.bind_depths(depths).expect("the cells bind");
    program
}

fn program(layers: Vec<Layer>) -> ControlProgram {
    ControlProgramBuilder::new()
        .layers(layers)
        .build()
        .expect("a valid program")
}

/// A 500 at beat 0 → 0 at beat 4 trajectory.
fn ramp() -> Arc<dyn Curve> {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(500.0)));
    e.add_breakpoint(Breakpoint::new(Beat::new(4.0), Hz::new(0.0)));
    Arc::new(e)
}

/// Automation replaces the knob where it has a value, to the bit (no
/// offsets: the sum starts at `-0.0`, so the base passes through); the knob
/// holds where it has none; neither: no value.
#[test]
fn automation_replaces_the_knob() {
    let p = ControlProgram::from_curve(ramp());
    assert_eq!(
        p.value_at(None, Some(1000.0), &at_beat(0.0), 100.0, &[]),
        Some(500.0)
    );
    assert_eq!(
        p.value_at(None, Some(1000.0), &at_beat(2.0), 100.0, &[]),
        Some(250.0)
    );
    assert_eq!(
        p.value_at(None, None, &at_beat(2.0), 100.0, &[]),
        Some(250.0)
    );
    let empty: Arc<dyn Curve> = Arc::new(Trajectory::<Hz>::new());
    let p = ControlProgram::from_curve(empty);
    assert_eq!(
        p.value_at(None, Some(1000.0), &at_beat(2.0), 100.0, &[]),
        Some(1000.0)
    );
    assert_eq!(p.value_at(None, None, &at_beat(2.0), 100.0, &[]), None);
    assert!(p.is_pure_automation());
    assert!(!ControlProgram::default().is_pure_automation());
}

/// A layer's offset is `shape(raw) × span`: a ±1 square at depth 0.25 on a
/// span of 800 is ±200, on its edges' sides; a span that is not finite
/// leaves the base alone.
#[test]
fn a_layer_offsets_by_depth_times_span() {
    let p = program(vec![layer(1, beats(4.0), LfoShape::Square, amount(0.25))]);
    assert_eq!(
        p.value_at(None, Some(1000.0), &at_beat(1.0), 800.0, &[]),
        Some(1200.0)
    );
    assert_eq!(
        p.value_at(None, Some(1000.0), &at_beat(3.0), 800.0, &[]),
        Some(800.0)
    );
    assert_eq!(
        p.value_at(None, Some(1000.0), &at_beat(3.0), f64::INFINITY, &[]),
        Some(1000.0)
    );
}

/// A layer whose depth is in a cell reads the cell at every evaluation: the
/// square's ±1 at the cell's depth on a span of 800 is `±800 × depth`, a
/// depth stored after the program was built is the next evaluation's, and a
/// fork's detached program holds the depth it had and is moved by nothing
/// after. A program without the cell uses the depth it was built with, and an
/// id no layer has is ignored.
#[test]
fn a_layer_reads_its_depth_from_a_cell() {
    let cell = Param::new(Depth::of(0.25));
    let p = bound(
        program(vec![layer(1, beats(4.0), LfoShape::Square, amount(0.5))]),
        [(LayerId(1), cell.handle()), (LayerId(9), cell.handle())],
    );
    let value = |p: &ControlProgram, beat: f64| {
        p.value_at(None, Some(1000.0), &at_beat(beat), 800.0, &[])
            .unwrap()
    };
    assert_eq!(value(&p, 1.0), 1200.0, "the cell's 0.25, not the built 0.5");
    assert_eq!(value(&p, 3.0), 800.0);

    let fork = p.detached();
    cell.store(Depth::of(-0.5));
    assert_eq!(value(&p, 1.0), 600.0, "the next evaluation reads it");
    assert_eq!(value(&p, 3.0), 1400.0);
    assert_eq!(value(&fork, 1.0), 1200.0, "the fork's cell is its own");
    assert_ne!(p, fork, "a detached program has cells of its own");

    let plain = program(vec![layer(1, beats(4.0), LfoShape::Square, amount(0.5))]);
    assert_eq!(value(&plain, 1.0), 1400.0);
    assert_eq!(
        bound(plain.clone(), [(LayerId(9), cell)]),
        plain,
        "an id no layer has is ignored"
    );
}

/// **A block evaluates at one depth, whatever is stored while it runs.** A
/// snapshot is each cell loaded once; evaluating at two times from it gives
/// both at the depth the snapshot holds, and a store after it reaches only the
/// next snapshot. A square at depth 0.25 on a span of 800 is `1000 +- 200`;
/// stored to 0.5 between the two evaluations, the first would be 1200 and the
/// second 600 if each loaded the cell again.
#[test]
fn an_evaluation_from_a_snapshot_never_splits_its_depth() {
    let cell = Param::new(Depth::of(0.25));
    let p = bound(
        program(vec![layer(1, beats(4.0), LfoShape::Square, amount(0.0))]),
        [(LayerId(1), cell.handle())],
    );
    let snapshot = p.depth_snapshot();
    let at = |beat| {
        p.value_at_in(None, Some(1000.0), &at_beat(beat), 800.0, &[], &snapshot)
            .unwrap()
    };
    assert_eq!(at(1.0), 1200.0);
    cell.store(Depth::of(0.5));
    assert_eq!(at(3.0), 800.0, "the same depth after the store");
    assert_eq!(at(1.0), 1200.0);
    let next = p.depth_snapshot();
    assert_ne!(snapshot, next, "the next snapshot has it");
    assert_eq!(
        p.value_at_in(None, Some(1000.0), &at_beat(1.0), 800.0, &[], &next),
        Some(1400.0)
    );
}

/// **Snapshots differ when any depth does, bit for bit**, however the bits
/// combine: two cells that each flip one mantissa bit, chosen so that a fold
/// of their bits would cancel, still differ.
#[test]
fn snapshots_compare_every_depth() {
    let (a, b) = (Param::new(Depth::of(0.125)), Param::new(Depth::of(0.125)));
    let p = bound(
        program(vec![
            layer(1, beats(4.0), LfoShape::Square, amount(0.0)),
            layer(2, beats(4.0), LfoShape::Square, amount(0.0)),
        ]),
        [(LayerId(1), a.handle()), (LayerId(2), b.handle())],
    );
    let before = p.depth_snapshot();
    assert_eq!(before, p.depth_snapshot());
    a.store(Depth::of(0.125 + 2.0f64.powi(-11)));
    b.store(Depth::of(0.1875));
    assert_ne!(before, p.depth_snapshot());
    assert_eq!(DepthSnapshot::EMPTY, program(vec![]).depth_snapshot());
}

/// **A program holds at most `MAX_LANE_DEPTHS` depth cells, one per id**: more
/// are refused with the count, and two for one id with that id; nothing is
/// built.
#[test]
fn depth_cells_are_bounded_and_one_per_layer() {
    let mut p = program(vec![layer(1, beats(4.0), LfoShape::Square, amount(0.5))]);
    let cell = Param::new(Depth::of(0.1));
    let many = (0..=MAX_LANE_DEPTHS).map(|_| (LayerId(1), cell.handle()));
    assert_eq!(
        p.bind_depths(many).unwrap_err(),
        ProgramError::TooManyDepths {
            count: MAX_LANE_DEPTHS + 1
        }
    );
    let twice = [(LayerId(1), cell.handle()), (LayerId(1), cell.handle())];
    assert_eq!(
        p.bind_depths(twice).unwrap_err(),
        ProgramError::DuplicateLayer(LayerId(1))
    );
}

/// An input's offset is exactly `depth × span × x` for IEEE-exact figures;
/// an input that has not sent adds nothing, even unipolar (whose raw 0
/// would be half its depth).
#[test]
fn an_inputs_offset_is_depth_times_span_times_x() {
    let inputs = vec![
        InputLayer {
            id: LayerId(1),
            source: SourceId::new(ParamId::new(40)),
            amount: amount(0.5),
        },
        InputLayer {
            id: LayerId(2),
            source: SourceId::new(ParamId::new(41)),
            amount: Amount {
                depth: Depth::of(0.25),
                polarity: Polarity::Unipolar,
                curve: Law::Linear,
            },
        },
    ];
    let p = ControlProgramBuilder::new()
        .inputs(inputs)
        .build()
        .expect("valid");
    // 0.5 × 64 × 0.75 = 24.
    assert_eq!(
        p.value_at(None, Some(100.0), &at_beat(0.0), 64.0, &[Some(0.75), None]),
        Some(124.0)
    );
    // Unipolar x = 1: 0.25 × 64 × 1 = 16.
    assert_eq!(
        p.value_at(
            None,
            Some(100.0),
            &at_beat(0.0),
            64.0,
            &[Some(-0.25), Some(1.0)]
        ),
        Some(100.0 - 8.0 + 16.0)
    );
}

/// Layers sum in `f64` and the value is the sum, unrounded: the offsets sum to
/// 11 000 000 and the knob is 0.5, so the value is 11 000 000.5, which an
/// `f32` cannot hold. Insertion order does not matter: `new` sorts by id.
#[test]
fn layers_sum_in_id_order_in_f64() {
    let l = |id, d| layer(id, beats(4.0), LfoShape::Square, amount(d));
    let a = program(vec![l(3, 0.3), l(1, 0.1), l(2, 0.7)]);
    let b = program(vec![l(1, 0.1), l(2, 0.7), l(3, 0.3)]);
    assert_eq!(a, b);
    let span = 1.0e7;
    // Each layer's offset is its depth of the span, in `f64`.
    let want = 0.5f64 + 0.1 * span + 0.7 * span + 0.3 * span;
    // An `f32` cannot hold the sum: it rounds to 11 000 000.5's neighbour.
    assert_ne!(f64::from(want as f32), want);
    assert!((want - 11_000_000.5).abs() < 1e-8);
    assert_eq!(
        a.value_at(None, Some(0.5), &at_beat(1.0), span, &[]),
        Some(want)
    );
}

/// Phase is a function of time, not of the program: a depth edit (a new
/// program) evaluates the same wave at the same phase, scaled. For a sine
/// at depth 0.3 then 0.6, the offset doubles at every sampled time.
///
/// A program has no age to offset phase by, so the property is the closed
/// form itself; the lane test `an_lfos_phase_survives_a_depth_edit` pins it
/// across a republish.
#[test]
fn phase_survives_a_depth_edit() {
    let clock = LayerClock::Seconds {
        rate: Hz::new(3.0),
        phase: PhaseIncrement::new(0.125),
        anchor: None,
    };
    let a = program(vec![layer(1, clock, LfoShape::Sine, amount(0.3))]);
    let b = program(vec![layer(1, clock, LfoShape::Sine, amount(0.6))]);
    for k in 0..50 {
        let t = at_play(f64::from(k) * 977.0, 5);
        let va = a.value_at(None, Some(0.0), &t, 10.0, &[]).unwrap();
        let vb = b.value_at(None, Some(0.0), &t, 10.0, &[]).unwrap();
        let phase = 3.0 * f64::from(k) * 977.0 / SR + 0.125;
        let want = (phase * std::f64::consts::TAU).sin() * 3.0;
        assert!((va - want).abs() < 1e-12, "{k}: {va} vs {want}");
        assert!((vb - 2.0 * va).abs() < 1e-12, "{k}: {vb} vs 2 × {va}");
    }
}

/// A rate anchor in the current epoch continues the phase through a rate
/// change: at the anchor the phase is the anchor's, before it the old rate
/// runs and after it the new one; the anchor's frame is a corner. An
/// anchor from another epoch is ignored.
#[test]
fn a_rate_anchor_continues_the_phase() {
    let anchor = RateAnchor {
        epoch: 9,
        at: Frame::new(48_000),
        phase: 10.25,
        prev_rate: Hz::new(2.0),
    };
    let clock = LayerClock::Seconds {
        rate: Hz::new(4.0),
        phase: PhaseIncrement::new(0.0),
        anchor: Some(anchor),
    };
    assert_eq!(clock.cycles(&at_play(48_000.0, 9)), Some(10.25));
    assert_eq!(clock.cycles(&at_play(24_000.0, 9)), Some(9.25));
    assert_eq!(clock.cycles(&at_play(60_000.0, 9)), Some(11.25));
    assert_eq!(clock.cycles(&at_play(24_000.0, 8)), Some(2.0));
    // At play 40 800 the layer is at cycle 9.95 (rate 2 back from the
    // anchor): a square's next edge, cycle 10.0, is at play 42 000, before
    // the anchor; from there the next (10.5) would be past it, so the
    // anchor itself is the corner.
    let p = program(vec![layer(1, clock, LfoShape::Square, amount(1.0))]);
    assert_eq!(
        p.next_corner(&at_play(40_800.0, 9)).play,
        Some(SamplePosition::new(42_000.0))
    );
    assert_eq!(
        p.next_corner(&at_play(42_000.0, 9)).play,
        Some(SamplePosition::new(48_000.0))
    );
    // From the anchor: edge 10.5 at rate 4 is 3000 frames on.
    assert_eq!(
        p.next_corner(&at_play(48_000.0, 9)).play,
        Some(SamplePosition::new(51_000.0))
    );
}

/// Each wave's corners, on a beat clock of 4 beats a cycle: a square's two
/// edges, a triangle's peaks, a saw's wrap, a random's steps; a sine has
/// none unless a curved bipolar amount kinks it at its zero crossings. The
/// next corner is strictly after the time, even when the time is one.
#[test]
fn each_wave_has_its_corners() {
    let corner = |shape, amount, beat| {
        program(vec![layer(1, beats(4.0), shape, amount)])
            .next_corner(&at_beat(beat))
            .beat
    };
    let lin = amount(1.0);
    let curved = Amount {
        curve: Law::PowerIn(Exponent::of(2.0)),
        ..lin
    };
    assert_eq!(corner(LfoShape::Square, lin, 0.5), Some(Beat::new(2.0)));
    assert_eq!(corner(LfoShape::Square, lin, 2.0), Some(Beat::new(4.0)));
    assert_eq!(corner(LfoShape::Triangle, lin, 0.0), Some(Beat::new(1.0)));
    assert_eq!(corner(LfoShape::Triangle, lin, 1.0), Some(Beat::new(3.0)));
    assert_eq!(
        corner(LfoShape::Triangle, curved, 1.0),
        Some(Beat::new(2.0))
    );
    assert_eq!(corner(LfoShape::Sawtooth, lin, 1.0), Some(Beat::new(4.0)));
    assert_eq!(
        corner(LfoShape::Sawtooth, curved, 1.0),
        Some(Beat::new(2.0))
    );
    assert_eq!(corner(LfoShape::Random, lin, 5.0), Some(Beat::new(8.0)));
    assert_eq!(corner(LfoShape::Sine, lin, 1.0), None);
    assert_eq!(corner(LfoShape::Sine, curved, 1.0), Some(Beat::new(2.0)));
    let uni = Amount {
        polarity: Polarity::Unipolar,
        ..curved
    };
    assert_eq!(corner(LfoShape::Sine, uni, 1.0), None, "unipolar: no kink");
}

/// Every curve but `Linear` bends a bipolar sine at its zero crossings,
/// each kind of law included; `Linear` does not.
#[test]
fn every_non_linear_curve_kinks_a_bipolar_sine() {
    let corner = |curve| {
        let a = Amount {
            curve,
            ..amount(1.0)
        };
        program(vec![layer(1, beats(4.0), LfoShape::Sine, a)])
            .next_corner(&at_beat(1.0))
            .beat
    };
    assert_eq!(corner(Law::Linear), None);
    for c in [
        Law::Hold,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerOut(Exponent::of(3.0)),
        Law::PowerInOut(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
        Law::Exp(Curvature::of(3.0)),
    ] {
        assert_eq!(corner(c), Some(Beat::new(2.0)), "{c:?}");
    }
}

/// The automation's next breakpoint is a corner, and a seconds layer's
/// corner is on the play clock: `Corners` keeps the two apart.
#[test]
fn automation_and_seconds_corners_are_on_their_clocks() {
    let p = ControlProgramBuilder::new()
        .automation(ramp())
        .layers(vec![layer(1, seconds(2.0), LfoShape::Square, amount(0.1))])
        .build()
        .expect("valid");
    let c = p.next_corner(&at_play(1_000.0, 1));
    assert_eq!(c.beat, Some(Beat::new(4.0)));
    // Edge 0.5 at 2 Hz is 12 000 frames.
    assert_eq!(c.play, Some(SamplePosition::new(12_000.0)));
}

/// The left limit differs from the value only at a jump: on a square's edge
/// it is the level before, a frame later they agree.
///
/// The automation's left limit is the curve's just before the time: on a
/// stepped breakpoint, the step's level.
#[test]
fn the_left_limit_is_the_level_before_a_jump() {
    let p = program(vec![layer(1, beats(4.0), LfoShape::Square, amount(1.0))]);
    assert_eq!(
        p.value_at(None, Some(0.0), &at_beat(2.0), 1.0, &[]),
        Some(-1.0)
    );
    assert_eq!(
        p.value_before(None, Some(0.0), &at_beat(2.0), 1.0, &[]),
        Some(1.0)
    );
    assert_eq!(
        p.value_before(None, Some(0.0), &at_beat(2.5), 1.0, &[]),
        Some(-1.0)
    );
    let r = program(vec![layer(1, beats(1.0), LfoShape::Random, amount(1.0))]);
    let before = r.value_before(None, Some(0.0), &at_beat(3.0), 1.0, &[]);
    assert_eq!(before, r.value_at(None, Some(0.0), &at_beat(2.5), 1.0, &[]));
    assert_ne!(before, r.value_at(None, Some(0.0), &at_beat(3.0), 1.0, &[]));
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::with_law(
        Beat::new(0.0),
        Hz::new(100.0),
        Law::Hold,
    ));
    e.add_breakpoint(Breakpoint::new(Beat::new(2.0), Hz::new(300.0)));
    let a = ControlProgram::from_curve(Arc::new(e));
    assert_eq!(a.value_at(None, None, &at_beat(2.0), 1.0, &[]), Some(300.0));
    assert_eq!(
        a.value_before(None, None, &at_beat(2.0), 1.0, &[]),
        Some(100.0)
    );
}

/// `depth_by` scales a layer's offset by a second wave mapped to `[0, 1]`,
/// and its corners are the layer's too: a ±1 square on 4 beats scaled by a
/// square on 8 is +1, then −1, then 0 while the depth wave is low; from
/// beat 0.5 the next corner is the depth wave's (a square on 3: beat 1.5),
/// before the layer's own (beat 2).
#[test]
fn depth_by_scales_the_offset_and_adds_its_corners() {
    let scaled = |depth_clock| Layer {
        depth_by: Some(DepthBy {
            clock: depth_clock,
            wave: Waveform::Shape {
                shape: LfoShape::Square,
                seed: 0,
            },
            amount: DepthBy::UNIT,
        }),
        ..layer(1, beats(4.0), LfoShape::Square, amount(1.0))
    };
    let p = program(vec![scaled(beats(8.0))]);
    assert_eq!(
        p.value_at(None, Some(0.0), &at_beat(1.0), 1.0, &[]),
        Some(1.0)
    );
    assert_eq!(
        p.value_at(None, Some(0.0), &at_beat(3.0), 1.0, &[]),
        Some(-1.0)
    );
    assert_eq!(
        p.value_at(None, Some(0.0), &at_beat(5.0), 1.0, &[]),
        Some(0.0)
    );
    let p = program(vec![scaled(beats(3.0))]);
    assert_eq!(p.next_corner(&at_beat(0.5)).beat, Some(Beat::new(1.5)));
}

/// A curve that reads one value and freezes to another: what a program
/// holds is the frozen copy.
struct Thawed;
impl Curve for Thawed {
    fn value_at(&self, _: Beat) -> Option<f64> {
        Some(1.0)
    }
    fn frozen(&self) -> Option<Arc<dyn Curve>> {
        let mut e = Trajectory::new();
        e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(2.0)));
        Some(Arc::new(e))
    }
}

/// Both constructors take the curve's frozen copy, so a lane never reads a
/// live cell through it.
#[test]
fn a_program_holds_the_frozen_curve() {
    let p = ControlProgramBuilder::new()
        .automation(Arc::new(Thawed))
        .build()
        .expect("valid");
    assert_eq!(p.value_at(None, None, &at_beat(1.0), 1.0, &[]), Some(2.0));
    let p = ControlProgram::from_curve(Arc::new(Thawed));
    assert_eq!(p.value_at(None, None, &at_beat(1.0), 1.0, &[]), Some(2.0));
}

/// Random steps replay: the same cycle gives the same value whatever the
/// program's history, a seed moves the sequence, and the smooth shape
/// meets each step at the cycle's start.
#[test]
fn random_waves_replay_by_cycle() {
    let w = Waveform::Shape {
        shape: LfoShape::Random,
        seed: 0,
    };
    let s = Waveform::Shape {
        shape: LfoShape::Random,
        seed: 7,
    };
    let smooth = Waveform::Shape {
        shape: LfoShape::RandomSmooth,
        seed: 0,
    };
    for n in 0..20 {
        let c = f64::from(n);
        assert_eq!(w.raw_at(c + 0.3, false), hash_bipolar(n.into()));
        assert_eq!(w.raw_at(c + 0.3, false), w.raw_at(c + 0.9, false));
        // The smooth wave at a cycle's start is the step before it, and
        // runs to the cycle's own step.
        assert_eq!(smooth.raw_at(c, false), hash_bipolar(i64::from(n) - 1));
        // The left limit reads `tol` (1e-9 at these cycles) early, on a slope of at most 2.
        assert!((smooth.raw_at(c + 1.0, true) - hash_bipolar(n.into())).abs() < 1e-8);
    }
    assert!((0..20).any(|n| s.raw_at(f64::from(n), false) != w.raw_at(f64::from(n), false)));
}

/// The random steps cover `[-1, 1]` evenly: over 4 096 cycles, under two
/// seeds and on both sides of cycle zero, every value is in range, both ends
/// are reached to within 1%, and each eighth of the range holds at least
/// three quarters of its fair share (512).
#[test]
fn random_steps_cover_the_range() {
    for seed in [0, 7] {
        let w = Waveform::Shape {
            shape: LfoShape::Random,
            seed,
        };
        let mut buckets = [0u32; 8];
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for n in -2048..2048 {
            let v = w.raw_at(f64::from(n) + 0.5, false);
            assert!((-1.0..=1.0).contains(&v), "seed {seed} cycle {n}: {v}");
            lo = lo.min(v);
            hi = hi.max(v);
            buckets[(((v + 1.0) * 4.0) as usize).min(7)] += 1;
        }
        assert!(lo < -0.99 && hi > 0.99, "seed {seed}: [{lo}, {hi}]");
        assert!(
            buckets.iter().all(|&b| b >= 384),
            "seed {seed}: {buckets:?}"
        );
    }
}

/// A drawn period: its points' values on their positions, its pieces along
/// their curves, the last piece wrapping to the next period's first point,
/// and a curved bipolar layer's zero crossings as corners.
#[test]
fn a_drawn_wave_follows_its_points() {
    let points: Arc<[PeriodPoint]> = Arc::from(vec![
        PeriodPoint {
            at: Phase::new(0.1),
            value: 1.0,
            curve: Law::Linear,
        },
        PeriodPoint {
            at: Phase::new(0.5),
            value: -1.0,
            curve: Law::PowerIn(Exponent::of(2.0)),
        },
        PeriodPoint {
            at: Phase::new(0.9),
            value: 0.5,
            curve: Law::Linear,
        },
    ]);
    let w = Waveform::Drawn(Arc::clone(&points));
    let near = |c: f64, want: f64| {
        let v = w.raw_at(c, false);
        assert!((v - want).abs() < 1e-12, "at {c}: {v} vs {want}");
    };
    near(0.1, 1.0);
    near(0.3, 0.0);
    near(1.5, -1.0);
    // The wrap piece: 0.5 at 0.9 to 1.0 at 1.1.
    near(0.95, 0.625);
    near(0.05, 0.875);
    // The exponential piece from -1 to 0.5: -1 + 1.5 t² crosses at
    // t = √(2/3).
    let want = 0.5 + 0.4 * (2.0f64 / 3.0).sqrt();
    let kinked = w.next_corner(0.6, true, true).expect("a crossing");
    // 30 bisection halvings of the piece (0.4 cycles wide) bound the error by 0.4 * 2^-30.
    assert!((kinked - want).abs() < 1e-9, "{kinked} vs {want}");
    assert_eq!(w.next_corner(0.6, true, false), Some(0.9));
}

/// `new` refuses past the lane's fixed state, duplicate ids and a bad
/// drawing, by name.
#[test]
fn new_refuses_what_a_lane_cannot_hold() {
    let l = |id| layer(id, beats(1.0), LfoShape::Sine, amount(0.1));
    assert_eq!(
        ControlProgramBuilder::new()
            .layers((0..17).map(l))
            .build()
            .unwrap_err(),
        ProgramError::TooManyLayers { count: 17 }
    );
    let input = |id| InputLayer {
        id: LayerId(id),
        source: SourceId::new(ParamId::new(1)),
        amount: amount(0.1),
    };
    assert_eq!(
        ControlProgramBuilder::new()
            .inputs((0..9).map(input))
            .build()
            .unwrap_err(),
        ProgramError::TooManyInputs { count: 9 }
    );
    assert_eq!(
        ControlProgramBuilder::new()
            .layers(vec![l(4), l(4)])
            .build()
            .unwrap_err(),
        ProgramError::DuplicateLayer(LayerId(4))
    );
    let bad = Layer {
        wave: Waveform::Drawn(Arc::from(vec![
            PeriodPoint {
                at: Phase::new(0.5),
                value: 0.0,
                curve: Law::Linear,
            },
            PeriodPoint {
                at: Phase::new(0.2),
                value: 0.0,
                curve: Law::Linear,
            },
        ])),
        ..l(5)
    };
    assert_eq!(
        ControlProgramBuilder::new()
            .layers(vec![bad])
            .build()
            .unwrap_err(),
        ProgramError::BadDrawnWave(LayerId(5))
    );
    assert!(
        ControlProgramBuilder::new()
            .layers((0..16).map(l))
            .build()
            .is_ok()
    );
}

/// A depth-by amount other than the unit one shapes the scale, and a curved
/// bipolar one kinks it at the scaling wave's zero crossings, which are
/// corners as a layer's own are: a constant +1 layer scaled by a sine on 4
/// beats through an exponential bipolar amount has a corner at beat 2 (the
/// sine's crossing), and none with the unit amount or a curved unipolar
/// one; its value at beat 1 (sine +1) is `shape(1, 0.5, Bipolar,
/// Exponential)` = 0.5, at beat 3 (sine −1) −0.5.
#[test]
fn a_curved_bipolar_depth_by_kinks_at_its_zero_crossings() {
    let scaled = |by| Layer {
        wave: Waveform::Const(1.0),
        depth_by: Some(DepthBy {
            clock: beats(4.0),
            wave: Waveform::Shape {
                shape: LfoShape::Sine,
                seed: 0,
            },
            amount: by,
        }),
        ..layer(1, beats(4.0), LfoShape::Sine, amount(1.0))
    };
    let curved = Amount {
        depth: Depth::of(0.5),
        polarity: Polarity::Bipolar,
        curve: Law::PowerIn(Exponent::of(2.0)),
    };
    let p = program(vec![scaled(curved)]);
    assert_eq!(p.next_corner(&at_beat(1.0)).beat, Some(Beat::new(2.0)));
    assert_eq!(
        p.value_at(None, Some(0.0), &at_beat(1.0), 1.0, &[]),
        Some(0.5)
    );
    assert_eq!(
        p.value_at(None, Some(0.0), &at_beat(3.0), 1.0, &[]),
        Some(-0.5)
    );
    let unit = program(vec![scaled(DepthBy::UNIT)]);
    assert_eq!(unit.next_corner(&at_beat(1.0)).beat, None);
    let uni = Amount {
        polarity: Polarity::Unipolar,
        ..curved
    };
    assert_eq!(
        program(vec![scaled(uni)]).next_corner(&at_beat(1.0)).beat,
        None,
        "unipolar: no kink"
    );
}

/// The unit depth-by amount is `(raw + 1) / 2` in `f64`, to the bit: the scale itself, and the
/// scaled offset through a drawn depth wave holding each raw value.
#[test]
fn the_unit_depth_by_is_half_raw_plus_one_in_f64() {
    for raw in [-1.0f64, -0.7, 0.0, 1e-7, 0.3, 0.9, 1.0] {
        let held = Waveform::Drawn(Arc::from(vec![PeriodPoint {
            at: Phase::new(0.0),
            value: raw,
            curve: Law::Linear,
        }]));
        let l = Layer {
            wave: Waveform::Const(1.0),
            depth_by: Some(DepthBy {
                clock: beats(4.0),
                wave: held,
                amount: DepthBy::UNIT,
            }),
            ..layer(1, beats(4.0), LfoShape::Sine, amount(0.25))
        };
        let span = 1000.0;
        let off = Amount::offset(&amount(0.25), 1.0, span);
        let want = 100.0f64 + (-0.0 + off * ((raw + 1.0) * 0.5));
        let d = l.depth_by.clone().expect("scaled");
        assert_eq!(
            d.scale(raw).to_bits(),
            ((raw + 1.0) * 0.5).to_bits(),
            "raw {raw}"
        );
        let got = program(vec![l]).value_at(None, Some(100.0), &at_beat(1.0), span, &[]);
        assert_eq!(got, Some(want), "raw {raw}");
    }
}

/// A constant wave is its value at every time on any clock, even one that
/// gives no position (a rate that is not finite, which silences a moving wave), and
/// has no corner; `is_constant` holds only while its depth-by is constant
/// too.
#[test]
fn a_constant_wave_needs_no_clock() {
    let dead = LayerClock::Seconds {
        rate: Hz::new(f64::INFINITY),
        phase: PhaseIncrement::new(0.0),
        anchor: None,
    };
    let c = Layer {
        wave: Waveform::Const(-1.0),
        ..layer(1, dead, LfoShape::Sine, amount(0.5))
    };
    assert!(c.is_constant());
    let p = program(vec![c.clone()]);
    for b in [0.0, 1.5, 7.0] {
        assert_eq!(
            p.value_at(None, Some(10.0), &at_beat(b), 8.0, &[]),
            Some(6.0)
        );
    }
    assert_eq!(p.next_corner(&at_beat(0.0)), Corners::default());
    let scaled = Layer {
        depth_by: Some(DepthBy {
            clock: beats(1.0),
            wave: Waveform::Shape {
                shape: LfoShape::Sine,
                seed: 0,
            },
            amount: DepthBy::UNIT,
        }),
        ..c
    };
    assert!(!scaled.is_constant());
}

/// `new` refuses a NaN anywhere in a layer, by name, so a program is `Eq`
/// (it equals itself).
#[test]
fn new_refuses_a_nan() {
    let l = || layer(3, beats(1.0), LfoShape::Sine, amount(0.1));
    let nan_rate = Layer {
        clock: seconds(f64::NAN),
        ..l()
    };
    let nan_const = Layer {
        wave: Waveform::Const(f64::NAN),
        ..l()
    };
    let nan_depth_by = Layer {
        depth_by: Some(DepthBy {
            clock: LayerClock::Seconds {
                rate: Hz::new(1.0),
                phase: PhaseIncrement::new(f64::NAN),
                anchor: None,
            },
            wave: Waveform::Const(0.0),
            amount: DepthBy::UNIT,
        }),
        ..l()
    };
    for bad in [nan_rate, nan_const, nan_depth_by] {
        assert_eq!(
            ControlProgramBuilder::new()
                .layers(vec![bad])
                .build()
                .unwrap_err(),
            ProgramError::NotANumber(LayerId(3))
        );
    }
    let p = program(vec![l()]);
    assert_eq!(p, p.clone());
}

/// A bearing's value: automation from 170 to -170 over four beats turns
/// through 180 (`wrap(170 + 5 b)`), where a line's goes through 0; the sum
/// with an offset is wrapped, never clamped; an offset scales by the span
/// given (the arc's length).
#[test]
fn a_circular_params_value_takes_the_short_arc_and_wraps_its_sum() {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Azimuth::new(170.0)));
    e.add_breakpoint(Breakpoint::new(Beat::new(4.0), Azimuth::new(-170.0)));
    let p = ControlProgram::from_curve(Arc::new(e));
    let az = UnitParam::Azimuth;
    for b in [0.5f64, 1.0, 1.9, 2.0, 2.1, 3.0, 3.5] {
        let want = Azimuth::new(170.0 + 5.0 * b).wrap().get();
        let got = p
            .value_at(Some(az), None, &at_beat(b), 360.0, &[])
            .expect("a value");
        assert!((got - want).abs() < 1e-3, "beat {b}: {got} against {want}");
        assert!(got.abs() > 170.0 - 1e-3, "beat {b}: {got} left the seam");
        // The left limit agrees inside a piece.
        let before = p
            .value_before(Some(az), None, &at_beat(b), 360.0, &[])
            .expect("a value");
        // On the circle: at the seam itself the left limit is 180, and 180 is
        // -180.
        assert!(
            Azimuth::new(before - want).wrap().get().abs() < 1e-2,
            "beat {b}: {before}"
        );
        // A height over the same curve is the straight line.
        let line = p
            .value_at(Some(UnitParam::Elevation), None, &at_beat(b), 360.0, &[])
            .expect("a value");
        assert!((line - (170.0 - 85.0 * b)).abs() < 1e-3, "beat {b}: {line}");
    }
    // 170 and a constant layer of 0.1 of a 360 arc (36) is 206: -154.
    let l = layer(1, beats(4.0), LfoShape::Square, amount(0.1));
    let l = Layer {
        wave: Waveform::Const(1.0),
        ..l
    };
    let q = program(vec![l]);
    assert_eq!(
        q.value_at(Some(az), Some(170.0), &at_beat(1.0), 360.0, &[]),
        Some(Azimuth::new(170.0 + 0.1 * 360.0).wrap().get())
    );
    assert_eq!(
        q.value_at(
            Some(UnitParam::Elevation),
            Some(170.0),
            &at_beat(1.0),
            360.0,
            &[]
        ),
        Some(170.0 + 0.1 * 360.0),
        "a line's sum is not wrapped"
    );
}

/// A periodic wave reads the `floor`-based fraction of its position as the
/// phase, so whole cycles before it change nothing: a sine at `n + 0.25` is 1,
/// a square at `n + 0.1` is 1 and at `n + 0.9` is -1, a sawtooth (`2p - 1`) at
/// `n + 0.5` is 0.
#[test]
fn a_periodic_wave_reads_the_fraction_of_its_position() {
    for (shape, phase, expected) in [
        (LfoShape::Sine, 0.25, 1.0f64),
        (LfoShape::Square, 0.1, 1.0),
        (LfoShape::Square, 0.9, -1.0),
        (LfoShape::Sawtooth, 0.5, 0.0),
    ] {
        let wave = Waveform::Shape { shape, seed: 0 };
        for cycle in [0.0, 3.0, 1_000.0] {
            let got = wave.raw_at(cycle + phase, false);
            assert!(
                (got - expected).abs() < 1e-12,
                "{shape:?} at {}: expected {expected}, got {got}",
                cycle + phase
            );
        }
    }
}

/// A negative position reads the end of the cycle: a sawtooth (`2p - 1`) at
/// `-0.25` and at `-3.25` is the sawtooth at phase 0.75, which is 0.5. A
/// `fract` wrap would keep the sign and read -1.5.
#[test]
fn a_negative_position_reads_the_end_of_the_cycle() {
    let saw = Waveform::Shape {
        shape: LfoShape::Sawtooth,
        seed: 0,
    };
    for cycle in [0.0, -3.0] {
        let got = saw.raw_at(cycle - 0.25, false);
        assert!((got - 0.5).abs() < 1e-12, "{got} at {}", cycle - 0.25);
    }
}

/// The random LFO's value for cycle 0 is not a constant -1: the mix of index 0
/// is not 0.
#[test]
fn the_hash_of_cycle_zero_is_inside_the_range_and_not_the_floor() {
    let h = hash_bipolar(0);
    assert!(h > -1.0 && h < 1.0, "{h}");
}

/// A wave's raw value is `f64` end to end: a constant of 1/3 gives an offset of 1/3 of the
/// span to 1e-15 (an `f32` raw is 1/3 to only 1e-8), and a random step is the splitmix64 word's
/// top 53 bits over 2^53, mapped to `[-1, 1]`, where 24 bits would quantise it to 2^-23.
#[test]
fn a_raw_value_keeps_f64_precision() {
    let third = 1.0f64 / 3.0;
    assert!(f64::from(third as f32) != third);
    let l = Layer {
        wave: Waveform::Const(third),
        ..layer(1, beats(4.0), LfoShape::Sine, amount(1.0))
    };
    let got = program(vec![l])
        .value_at(None, Some(0.0), &at_beat(1.0), 1.0, &[])
        .expect("a value");
    assert!((got - third).abs() < 1e-15, "{got}");
    for n in [0i64, 1, 7, -3, 1_000_003] {
        let want = ((SplitMix64::mix(n as u64) >> 11) as f64 / 9_007_199_254_740_992.0) * 2.0 - 1.0;
        assert_eq!(hash_bipolar(n), want, "index {n}");
    }
    assert!(
        (0..64).any(|n| {
            let v = hash_bipolar(n);
            (v * 8_388_608.0).fract() != 0.0
        }),
        "a step has bits below 2^-23"
    );
}

//! The ladder and the crossover allocate nothing at any corner of their controls: every
//! cell moving between its ends every block, every driven port fed a ramp or a hostile control signal
//! (NaN, an infinity, a value far out of range, a square between the ends), blocks of 1, 7, 33
//! and 64 frames, widths that fall into every lane group, and `reset`, `prepare` and a fork run
//! inside the window. The output is finite and under a bound derived below.
//!
//! A control signal is fed outside the window (`Direct::feed` copies it); the cells move inside it.
//!
//! **The ladder's bound.** The saturator holds the loop input to `|u| <= 1` whatever the
//! controls. A trapezoidal stage `s' = 2 G in + (1 - 2 G) s`, `lp = G in + (1 - G) s` with
//! `G = g / (1 + g) <= 1/2` (a cutoff of at most a quarter of the rate: `g <= 1`) is a convex
//! combination of its input and its state at every frame, however `G` moves, so every stage
//! stays within the largest input: `|lp_n| <= 1`, and the taps `LP12`, `LP24`, `HP12` and
//! `HP24` (`u - 2 lp1 + lp2`, `u - 4 lp1 + 6 lp2 - 4 lp3 + lp4`) within 1, 1, 4 and 16. The cutoff
//! of a moving sweep stays at or under 11.9 kHz at 48 kHz for that. A cutoff held anywhere (an
//! initial 1e9, clamped under Nyquist) is a time-invariant stage of l1 norm `2 G < 2`, and the
//! taps stay within 4, 16, 9 and 81 (`2^n` per stage, the binomial weights of the taps); the
//! resonance and the drive move freely there, since they act only through `u`. The state is
//! `f32` in one of the two, so a rounding factor of 1.001.
//!
//! **The crossover's bound.** A section is a trapezoidal state-variable filter, and for a
//! gain that moves anywhere in `[0, G]`, `G = tan(0.499 pi)`, at every sample, its taps are within
//! `T` times its input's bound (`tutti_fixtures::tpt::tpt_section_bound`, derived in the docs
//! of `tutti-dsp`'s crossover state). A band of `n` splits is at most `2 n` sections in series,
//! so each band stays within `T^(2 n)` of the input's peak, 0.5 here (with the `f32` factor
//! 1.001). The sweep covers the whole range: a split at 1 Hz, at the cap, a square between
//! them on every grid point, and values the node holds or clamps.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, MaxBlock};
use tutti_graph::contract::Direct;
use tutti_graph::{Node, ParamNode, Prepare};
use tutti_nodes::{
    CrossoverConfig, CrossoverControls, CrossoverNode, LadderFilterConfig, LadderFilterNode,
    LadderType,
};
use tutti_units::{Hz, Resonance, SampleRate, Samples, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate::SR_48K;
const FRAMES: usize = 64;
const BLOCKS: [usize; 4] = [1, 7, 33, 64];
const NAN: f32 = f32::NAN;
const INF: f32 = f32::INFINITY;

/// A control signal's frames: `lo` to `hi` over the block, then each hostile shape.
fn signal<F: Float>(shape: usize, lo: f32, hi: f32, bad: f32) -> Vec<F> {
    (0..FRAMES)
        .map(|i| match shape {
            0 => lo + (hi - lo) * i as f32 / (FRAMES - 1) as f32,
            1 => NAN,
            2 => {
                if i % 2 == 1 {
                    NAN
                } else {
                    hi
                }
            }
            3 => INF,
            4 => f32::NEG_INFINITY,
            5 => bad,
            _ => {
                if (i / 3) % 2 == 0 {
                    lo
                } else {
                    hi
                }
            }
        })
        .map(F::from_f32)
        .collect()
}
const SHAPES: usize = 7;

fn fill<F: Float>(rig: &mut Direct<impl Node<F>, F>) {
    for (n, c) in rig.inputs_mut().iter_mut().enumerate() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = match (i + n) % 5 {
                0 | 1 => F::lit(0.5),
                2 => F::ZERO,
                _ => F::lit(-0.5),
            };
        }
    }
}

/// Runs `blocks` blocks of the lengths in [`BLOCKS`], `between(i)` first (the cells move),
/// resetting at one block in 53 and preparing at one in 59, and returns the output's
/// peak, or `None` if a sample was not finite.
fn walk<F: Float, N: Node<F>>(
    rig: &mut Direct<N, F>,
    rate: SampleRate,
    blocks: usize,
    outputs: usize,
    mut between: impl FnMut(usize),
) -> Option<f64> {
    let (mut peak, mut finite) = (0.0f64, true);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..blocks {
            between(i);
            if i % 53 == 52 {
                Node::<F>::reset(&mut *rig.node);
            }
            if i % 59 == 58 {
                Node::<F>::prepare(
                    &mut *rig.node,
                    &Prepare::new(rate, MaxBlock::of(Samples::new(FRAMES))),
                );
            }
            rig.set_block_len(BLOCKS[i % BLOCKS.len()]);
            rig.block();
            for c in 0..outputs {
                for &s in rig.output().plane(c).samples() {
                    finite &= s.is_finite();
                    peak = peak.max(s.to_f64().abs());
                }
            }
        }
    });
    finite.then_some(peak)
}

const TYPES: [LadderType; 4] = [
    LadderType::LP12,
    LadderType::LP24,
    LadderType::HP12,
    LadderType::HP24,
];

/// The ladder, state `f32` and `f64`, every type, widths 1, 2 and 9, every control signal shape on every
/// port, the cells moving across their ends each block. The cutoff stays at or under
/// 11.9 kHz (or is not a number, which the node holds at its last good value); the bound is
/// the one derived above for a stage gain of at most 1/2.
#[test]
fn the_ladder_corners_are_allocation_free_with_every_control_moving() {
    const CUTOFFS: [f32; 7] = [1.0, 20.0, 400.0, 11_900.0, NAN, INF, -5.0];
    const RESONANCES: [f32; 5] = [0.0, 1.0, 5.0, -1.0, NAN];
    const DRIVES: [f32; 9] = [0.0, 1e-30, 1.0, 100.0, 1e30, f32::MAX, NAN, INF, -3.0];
    macro_rules! sweep {
        ($f:ty) => {
            for ty in TYPES {
                let bound = match ty {
                    LadderType::LP12 | LadderType::LP24 => 1.0,
                    LadderType::HP12 => 4.0,
                    _ => 16.0,
                } * 1.001;
                for width in [1u16, 2, 9] {
                    for shape in 0..SHAPES {
                        let node = LadderFilterNode::<$f>::from(
                            LadderFilterConfig::new(ChannelLayout::from(width), ty)
                                .freq(Hz::new(1_000.0))
                                .res(Resonance::new(0.5)),
                        );
                        let set = node.param_set();
                        let mut rig = Direct::<_, $f>::with_format(node, SR, FRAMES);
                        fill(&mut rig);
                        rig.feed(0, Some(&signal(shape, 1.0, 11_900.0, 5_000.0)));
                        rig.feed(1, Some(&signal(shape, 0.0, 1.0, 9.0)));
                        rig.feed(2, Some(&signal(shape, 0.0, 1e30, 1e30)));
                        let peak = walk(&mut rig, SR, 240, usize::from(width), |i| {
                            set.set(UnitParam::Cutoff, f64::from(CUTOFFS[i % CUTOFFS.len()]));
                            set.set(UnitParam::Q, f64::from(RESONANCES[i % RESONANCES.len()]));
                            set.set(UnitParam::Drive, f64::from(DRIVES[i % DRIVES.len()]));
                        });
                        let peak = peak.unwrap_or_else(|| {
                            panic!("{ty:?} width {width} signals {shape}: not finite")
                        });
                        assert!(
                            peak <= bound,
                            "{ty:?} width {width} signals {shape}: {peak} over {bound}"
                        );
                    }
                }
            }
        };
    }
    sweep!(f32);
    sweep!(f64);
}

/// A cutoff held at the top of the range (1e9, held under 0.998 of Nyquist) with the resonance
/// and the drive moving across all their ends and every control signal shape on them: the bound is the
/// time-invariant stage's, 4, 16, 9 and 81.
#[test]
fn the_ladder_at_a_held_extreme_cutoff_is_allocation_free_and_bounded() {
    for ty in TYPES {
        let bound = match ty {
            LadderType::LP12 => 4.0,
            LadderType::LP24 => 16.0,
            LadderType::HP12 => 9.0,
            _ => 81.0,
        } * 1.001;
        for cutoff in [1.0e9f64, 23_900.0, 1.0] {
            for shape in 0..SHAPES {
                let node = LadderFilterNode::<f32>::from(
                    LadderFilterConfig::new(ChannelLayout::STEREO, ty)
                        .freq(Hz::new(cutoff))
                        .res(Resonance::new(1.0)),
                );
                let set = node.param_set();
                let mut rig = Direct::new(node, SR, FRAMES);
                fill(&mut rig);
                rig.feed(1, Some(&signal(shape, 0.0, 1.0, 9.0)));
                rig.feed(2, Some(&signal(shape, 0.0, 1e30, 1e30)));
                let peak = walk(&mut rig, SR, 240, 2, |i| {
                    set.set(UnitParam::Q, if i % 2 == 0 { 0.0 } else { 1.0 });
                    set.set(UnitParam::Drive, if i % 3 == 0 { 1e30 } else { 0.01 });
                })
                .unwrap_or_else(|| panic!("{ty:?} at {cutoff} Hz signals {shape}: not finite"));
                assert!(
                    peak <= bound,
                    "{ty:?} at {cutoff} Hz signals {shape}: {peak}"
                );
            }
        }
    }
}

/// The ladder with the cutoff moving anywhere from 1 Hz to past Nyquist (cell and control signal, out
/// of range values included: 1e9, which the node holds under 0.998 of Nyquist, and the
/// non-finite ones it holds at the last good value), at 48 and 96 kHz, with the resonance and
/// the drive moving too. The output is finite and under a bound derived for a stage gain
/// that moves anywhere in `(0, G_max]`, `G = g / (1 + g)`, `g = tan(pi fc / fs)` and
/// `fc <= 0.998 fs / 2`: a stage is `s' = 2 G in + (1 - 2 G) s`, `lp = G in + (1 - G) s`. With
/// `|in| <= M` and `B = G_max M / (1 - G_max)`, `|s'| <= 2 G M + (2 G - 1) B <= B` for `G >= 1/2`
/// and `|s'| <= max(M, B) = B` below it (a convex combination), so `|s| <= B` at every frame,
/// and `|lp| <= G M + (1 - G) B <= M + B = K M` with `K = 1 / (1 - G_max) = 1 + g_max`.
/// The loop input has `|u| <= 1`, so stage `n` is within `K^n`, the taps `LP12` and `LP24`
/// within `K^2` and `K^4`, and `HP12` and `HP24` (binomial weights) within `(1 + K)^2` and
/// `(1 + K)^4`. The figure is far above what the filter reaches (`K^4` is 1e10) and no
/// tighter bound was derived; it holds against a state that grows without bound.
#[test]
fn the_ladder_with_the_cutoff_anywhere_is_finite_and_within_the_stage_bound() {
    const FRACTIONS: [f32; 9] = [1e-6, 0.002, 0.1, 0.25, 0.4, 0.4985, 1e9, NAN, INF];
    for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
        let k = 1.0 + (core::f64::consts::PI * 0.998 / 2.0).tan();
        for ty in TYPES {
            let bound = match ty {
                LadderType::LP12 => k.powi(2),
                LadderType::LP24 => k.powi(4),
                LadderType::HP12 => (1.0 + k).powi(2),
                _ => (1.0 + k).powi(4),
            } as f32
                * 1.001;
            let hz = |f: f32| {
                if f.is_finite() && f < 1e8 {
                    f * rate.get() as f32
                } else {
                    f
                }
            };
            for shape in 0..SHAPES {
                let node = LadderFilterNode::<f32>::from(
                    LadderFilterConfig::new(ChannelLayout::STEREO, ty)
                        .freq(Hz::new(1_000.0))
                        .res(Resonance::new(0.5)),
                );
                let set = node.param_set();
                let mut rig = Direct::new(node, rate, FRAMES);
                fill(&mut rig);
                let top = 0.4985 * rate.get() as f32;
                rig.feed(0, Some(&signal(shape, 1.0, top, 1e9)));
                rig.feed(1, Some(&signal(shape, 0.0, 1.0, 9.0)));
                rig.feed(2, Some(&signal(shape, 0.0, 1e30, 1e30)));
                let peak = walk(&mut rig, rate, 240, 2, |i| {
                    set.set(
                        UnitParam::Cutoff,
                        f64::from(hz(FRACTIONS[i % FRACTIONS.len()])),
                    );
                    set.set(UnitParam::Q, if i % 2 == 0 { 1.0 } else { 0.0 });
                });
                let peak = peak
                    .unwrap_or_else(|| panic!("{ty:?} at {rate:?} signals {shape}: not finite"));
                assert!(
                    peak <= f64::from(bound),
                    "{ty:?} at {rate:?} signals {shape}: {peak} over {bound}"
                );
            }
        }
    }
}

/// A fork runs inside the window as the node does: built outside (a fork allocates its
/// cells), then walked with its own cells moving.
#[test]
fn a_forked_ladder_and_crossover_are_allocation_free() {
    let ladder = LadderFilterNode::<f32>::from(
        LadderFilterConfig::new(ChannelLayout::MONO, LadderType::HP24)
            .freq(Hz::new(800.0))
            .res(Resonance::new(0.9)),
    );
    let fork = ladder.fork_fresh();
    let set = fork.param_set();
    let mut rig = Direct::new(fork, SR, FRAMES);
    fill(&mut rig);
    let peak = walk(&mut rig, SR, 200, 1, |i| {
        set.set(
            UnitParam::Cutoff,
            f64::from(100.0 + 50.0 * (i % 200) as f32),
        );
    });
    assert!(peak.is_some_and(|p| p <= 16.0 * 1.001), "{peak:?}");
    let crossover = CrossoverNode::<f32>::try_from(CrossoverConfig::new(
        BusTopology::STEREO,
        CrossoverControls::try_from(&[Hz::new(200.0), Hz::new(2_000.0)][..]).unwrap(),
    ))
    .unwrap();
    let fork = crossover.fork_fresh();
    let set = fork.param_set();
    let mut rig = Direct::new(fork, SR, FRAMES);
    fill(&mut rig);
    let peak = walk(&mut rig, SR, 200, 6, |i| {
        set.set(
            UnitAddr::new(UnitParam::Cutoff, 0),
            f64::from(100.0 + 5.0 * (i % 200) as f32),
        );
    });
    assert!(peak.is_some(), "finite");
}

/// The splits a moving crossover visits: from the 1 Hz floor to the cap, and values the node
/// holds at the last good one (not a number, an infinity) or clamps (zero, negative, far over
/// Nyquist).
const SPLITS: [f32; 12] = [
    1.0, 50.0, 400.0, 1_000.0, 12_000.0, 23_999.0, 1.0e9, 0.0, -5.0, NAN, INF, 24_000.0,
];

/// The frames a design takes to arrive, and so the grid a square control signal switches on.
const GRID: usize = 16;

/// The crossover's control signals: the shapes of `signal`, and a square that switches between the floor
/// and the cap at every grid point (`SHAPES`).
fn crossover_signal<F: Float>(shape: usize) -> Vec<F> {
    if shape == SHAPES {
        (0..FRAMES)
            .map(|i| {
                F::from_f32(if (i / GRID).is_multiple_of(2) {
                    1.0
                } else {
                    23_952.0
                })
            })
            .collect()
    } else {
        signal(shape, 1.0, 23_952.0, 1.0e9)
    }
}

/// The crossover at every band count and widths 1, 2 and 5 (30 of the 32 channels a rig
/// drives at 6 bands), every split cell moving across 1 Hz to the cap (and the hostile values)
/// out of order each block, every control signal shape on every split, a square between the ends on every
/// grid point among them, with `reset`, `prepare` and blocks of every odd length. The bands are
/// finite and within the bound derived above.
#[test]
fn the_crossover_corners_are_allocation_free_with_every_split_moving() {
    let section =
        tutti_fixtures::tpt::tpt_section_bound((core::f64::consts::PI * 0.499).tan() * 1.001);
    for bands in 2..=6usize {
        let bound = 0.5 * 1.001 * section.tap.powi(2 * (bands as i32 - 1));
        for width in [1u16, 2, 5] {
            if usize::from(width) * bands > tutti_graph::contract::RIG_MAX_CHANNELS {
                continue;
            }
            for shape in 0..=SHAPES {
                let initial: Vec<Hz> = (0..bands - 1)
                    .map(|i| Hz::new(150.0 * 3f64.powi(i as i32)))
                    .collect();
                let node = CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                    BusTopology::assumed(ChannelLayout::from(width)).unwrap(),
                    CrossoverControls::try_from(&initial[..]).unwrap(),
                ))
                .unwrap();
                let set = node.param_set();
                let mut rig = Direct::new(node, SR, FRAMES);
                fill(&mut rig);
                for k in 0..bands - 1 {
                    rig.feed(k, Some(&crossover_signal(shape)));
                }
                let outs = usize::from(width) * bands;
                let peak = walk(&mut rig, SR, 240, outs, |i| {
                    for k in 0..bands - 1 {
                        let hz = SPLITS[(i + 3 * k) % SPLITS.len()];
                        set.set(UnitAddr::new(UnitParam::Cutoff, k as u8), f64::from(hz));
                    }
                })
                .unwrap_or_else(|| {
                    panic!("{bands} bands width {width} signals {shape}: not finite")
                });
                assert!(
                    peak <= bound,
                    "{bands} bands width {width} signals {shape}: {peak} over {bound}"
                );
            }
        }
    }
}

/// Splits held at the corners of the range: 1 Hz (the floor), 23 999 Hz and 1e9 (both held
/// under 0.499 of the rate), with hostile control signals that hold the last good value (not a number,
/// an infinity) and cells written with out-of-range values (out of order, below the floor).
/// The design is the same at every block, so the bound is the held one, derived: a band is a
/// linear time-invariant cascade of sections, so its output is within the model's l1 norm of the
/// input's peak, 0.5 here (`Lti::l1_norm`, the largest band's product of its sections' l1
/// norms), with a factor 1.001 for the `f32` rounding.
#[test]
fn the_crossover_at_held_extreme_splits_stays_within_the_models_l1_bound() {
    use tutti_buffers::BusTopology;
    use tutti_dsp::{CrossoverBands, CrossoverControls, CrossoverModel, Lti};
    for set in [
        [1.0f64, 2.0],
        [23_999.0, 24_000.0],
        [1.0e9, 2.0e9],
        [1.0, 1.0e9],
    ] {
        for shape in [1usize, 3, 4] {
            let hz = [Hz::new(set[0]), Hz::new(set[1])];
            let node = CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                BusTopology::STEREO,
                CrossoverControls::try_from(&hz[..]).unwrap(),
            ))
            .unwrap();
            let cells = node.param_set();
            let mut rig = Direct::new(node, SR, FRAMES);
            fill(&mut rig);
            rig.feed(0, Some(&signal(shape, 1.0, 2.0, 1.0)));
            rig.feed(1, Some(&signal(shape, 1.0, 2.0, 1.0)));
            // The model the kernel retargets: `design_into` over the cell values, which
            // clamps under 0.499 of the rate and orders the splits; the node applies no other fit.
            let controls = CrossoverControls::try_from(&hz[..]).unwrap();
            let model = CrossoverModel::designed(CrossoverBands::of(3), &controls, SR);
            let bound = 0.5 * model.l1_norm() as f32 * 1.001;
            let peak = walk(&mut rig, SR, 240, 6, |i| {
                // A write the node holds at the last good value, or one that lands on the
                // same clamp: the design does not move.
                cells.set(
                    UnitAddr::new(UnitParam::Cutoff, 0),
                    // A param word is an `f32`.
                    if i % 2 == 0 {
                        f64::from(NAN)
                    } else {
                        f64::from(set[0] as f32)
                    },
                );
            })
            .unwrap_or_else(|| panic!("{set:?} signals {shape}: not finite"));
            assert!(
                peak <= f64::from(bound),
                "{set:?} signals {shape}: {peak} over {bound}"
            );
        }
    }
}

//! The SVF, the EQ band, the ladder and the crossover on the stream grid: the render is a
//! function of the stream, not of how it is cut into blocks, and a control reaches
//! the render within one 16-frame segment of the grid point that sampled it.
//!
//! Both are analytic: the first renders one signal at several block lengths and
//! asks for the same bits; the second reads the filter's own held responses (an
//! impulse into a filter with no state answers with its first coefficient set, so
//! the output at the impulse is the coefficient set in force there).

// These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.

use tutti_buffers::BusTopology;
use tutti_buffers::ChannelLayout;
use tutti_buffers::Planes;
use tutti_dsp::{SvfCoefficients, SvfModel};
use tutti_graph::contract::{drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, TransportChanges, TransportPosition};
use tutti_nodes::{
    CrossoverConfig, CrossoverControls, CrossoverNode, EqBandConfig, EqBandNode,
    LadderFilterConfig, LadderFilterNode, LadderType, SvfControls, SvfFilterConfig, SvfFilterNode,
    SvfType,
};
use tutti_units::{Db, Frame, Hz, Q, Resonance, SampleRate, Samples, UnitAddr, UnitParam};

const RATE: SampleRate = SampleRate::SR_48K;
const FRAMES: usize = 2_048;
const BLOCKS: [usize; 4] = [1, 17, 64, 512];

fn noise(seed: u32) -> Vec<f32> {
    tutti_fixtures::noise(seed, FRAMES)
        .into_iter()
        .map(|x| 0.8 * x)
        .collect()
}

/// `node`, prepared for 512-frame blocks, fed `inputs` in blocks of `block` with
/// `signals` (each a function of the absolute frame, in port order) cut to match.
fn render<N: Node + ParamNode + 'static>(
    node: N,
    block: usize,
    inputs: &[Vec<f32>],
    signals: &[Option<Vec<f32>>],
    act: impl Fn(&tutti_graph::ParamSet),
) -> Vec<Vec<f32>> {
    let mut node = prepared(node, RATE, 512);
    act(&node.param_set());
    let len = inputs[0].len();
    let mut out: Vec<Vec<f32>> = Vec::new();
    for start in (0..len).step_by(block) {
        let end = (start + block).min(len);
        let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[start..end]).collect();
        let params: Vec<Option<&[f32]>> = signals
            .iter()
            .map(|l| l.as_ref().map(|l| &l[start..end]))
            .collect();
        let env = Env {
            frame: Frame::new(start as u64),
            sample_rate: RATE,
            block_len: Samples::new(end - start),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let driven = drive_input(
            &mut node as &mut dyn Node,
            &env,
            Planes::new(&ins).unwrap(),
            &params,
            &[],
        )
        .audio;
        out.resize(driven.len(), Vec::new());
        for (all, part) in out.iter_mut().zip(driven) {
            all.extend(part);
        }
    }
    out
}

/// An exponential sweep of `lo` to `hi` over the render, a function of the
/// absolute frame.
fn sweep(lo: f32, hi: f32) -> Vec<f32> {
    (0..FRAMES)
        .map(|i| lo * (hi / lo).powf(i as f32 / FRAMES as f32))
        .collect()
}

fn same_at_every_block(what: &str, make: impl Fn() -> Box<dyn Fn(usize) -> Vec<Vec<f32>>>) {
    let run = make();
    let whole = run(512);
    assert!(whole[0].iter().any(|s| s.abs() > 1e-3), "{what}: silent");
    for block in BLOCKS {
        assert!(
            run(block) == whole,
            "{what}: blocks of {block} differ from blocks of 512"
        );
    }
}

/// An SVF bell and an EQ band with a control signal on every control it has, and the
/// other cells moved before the first block, render the same bits whether the
/// stream arrives in blocks of 1, 17, 64 or 512 frames.
#[test]
fn blocks_of_any_length_render_alike_with_signals_and_moved_cells() {
    let inputs = [noise(1), noise(2)];
    same_at_every_block("svf bell, three signals", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            let signals = [
                Some(sweep(300.0, 9_000.0)),
                Some(sweep(0.6, 7.0)),
                Some(
                    (0..FRAMES)
                        .map(|i| 9.0 * (i as f32 * 0.01).sin())
                        .collect::<Vec<_>>(),
                ),
            ];
            render(
                SvfFilterNode::<f32>::from(
                    SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::Bell)
                        .freq(Hz::new(1_000.0))
                        .q(Q::new(1.0))
                        .gain(Db::new(3.0)),
                ),
                block,
                &inputs,
                &signals,
                |_| {},
            )
        })
    });
    same_at_every_block("eq band, a cutoff signal, gain and Q cells moved", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            render(
                EqBandNode::<f32>::from(
                    EqBandConfig::new(SvfType::HighShelf)
                        .freq(Hz::new(4_000.0))
                        .q(Q::new(0.8))
                        .gain(Db::new(8.0)),
                ),
                block,
                &inputs[..1],
                &[Some(sweep(1_000.0, 8_000.0)), None, None],
                |set| {
                    // Before the first block: nothing to ramp from, so the cells
                    // are the controls of the first solve at every block length.
                    assert!(set.set(UnitParam::GainDb, -5.0));
                    assert!(set.set(UnitParam::Q, 2.5));
                },
            )
        })
    });
}

/// A stereo ladder with a control signal on its cutoff, its resonance and its drive, and one
/// with the drive cell moved before the first block under a cutoff control signal, render the
/// same bits at blocks of 1, 17, 64 or 512 frames: the drive is on the grid like the
/// coefficients (a control signal sampled per segment, a cell held).
#[test]
fn the_ladder_renders_alike_at_every_block_length_with_signals() {
    let inputs = [noise(4), noise(5)];
    same_at_every_block("ladder, cutoff, resonance and drive signals", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            let signals = [
                Some(sweep(200.0, 8_000.0)),
                Some(
                    (0..FRAMES)
                        .map(|i| 0.5 + 0.4 * (i as f32 * 0.013).sin())
                        .collect::<Vec<_>>(),
                ),
                Some(
                    (0..FRAMES)
                        .map(|i| 3.0 + 2.5 * (i as f32 * 0.007).sin())
                        .collect::<Vec<_>>(),
                ),
            ];
            render(
                LadderFilterNode::<f32>::from(
                    LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                        .freq(Hz::new(1_000.0))
                        .res(Resonance::new(0.3)),
                ),
                block,
                &inputs,
                &signals,
                |_| {},
            )
        })
    });
    same_at_every_block("ladder, a cutoff signal and the drive cell moved", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            render(
                LadderFilterNode::<f32>::from(
                    LadderFilterConfig::new(ChannelLayout::MONO, LadderType::HP12)
                        .freq(Hz::new(2_000.0))
                        .res(Resonance::new(0.4)),
                ),
                block,
                &inputs[..1],
                &[Some(sweep(500.0, 6_000.0)), None, None],
                |set| {
                    assert!(set.set(UnitParam::Drive, 4.0));
                },
            )
        })
    });
}

/// A step in a drive control signal at frame 40 reaches the render within one segment of the
/// grid point that samples it, as a cutoff's does: the drive is on the grid.
///
/// The point at 32 reads the old drive and its segment ends on it at frame 47; the
/// point at 48 reads the new one and its segment ends on it at frame 63. An impulse
/// into a ladder with no state answers with the drive in force at that frame, so the
/// output there is the held filter's with that drive, bit for bit. (Drive was once
/// read per sample, outside the solve: a step at 40 would be in the render at 40.)
#[test]
fn a_drive_signal_step_is_in_the_render_within_one_segment() {
    const STEP: usize = 40;
    let (old, new) = (1.0f32, 6.0f32);
    let step: Vec<f32> = (0..64).map(|i| if i < STEP { old } else { new }).collect();
    let response = |drive: Option<f32>, at: usize| -> f64 {
        let mut x = vec![0.0f32; 64];
        x[at] = 0.5;
        let signal = drive.map_or_else(|| step.clone(), |d| vec![d; 64]);
        let node = LadderFilterNode::<f32>::from(
            LadderFilterConfig::new(ChannelLayout::MONO, LadderType::HP24)
                .freq(Hz::new(1_500.0))
                .res(Resonance::new(0.2)),
        );
        let out = render(node, 64, &[x], &[None, None, Some(signal)], |_| {});
        f64::from(out[0][at])
    };
    let at_old = response(Some(old), 47);
    let at_new = response(Some(new), 63);
    assert_ne!(at_old, at_new, "the two drives must be told apart");
    assert_eq!(
        response(None, 47),
        at_old,
        "frame 47 is still on the old drive"
    );
    assert_eq!(response(None, 63), at_new, "frame 63 is on the new drive");
}

/// A step in a cutoff control signal at frame 40 reaches the SVF on the stream grid: the point
/// at 32 reads the old cutoff, the point at 48 the new one, and the coefficients ramp
/// from 48 to land at frame 63. With no state a low-pass answers an impulse with the
/// `a3` of the set in force at that frame, so the output at frame 47 is the old set's
/// `a3` and at frame 63 the new set's, bit for bit.
#[test]
fn an_svf_cutoff_step_lands_on_the_grid_point_after_it() {
    const STEP: usize = 40;
    let (old, new) = (500.0f64, 5_000.0f64);
    // The control signal carries `f32` words.
    let step: Vec<f32> = (0..64)
        .map(|i| (if i < STEP { old } else { new }) as f32)
        .collect();
    let response = |at: usize| -> f32 {
        let mut x = vec![0.0f32; 64];
        x[at] = 1.0;
        let node = SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(old))
                .q(Q::new(0.707)),
        );
        let out = render(node, 64, &[x], &[Some(step.clone()), None, None], |_| {});
        out[0][at]
    };
    let a3 = |freq: f64| {
        let controls = SvfControls {
            ty: SvfType::LowPass,
            freq: Hz::new(freq),
            q: Q::new(0.707),
            gain: Db::new(0.0),
        };
        SvfCoefficients::<f64>::from(&SvfModel::new(controls, RATE)).a3 as f32
    };
    assert_ne!(a3(old), a3(new), "the two sets must be told apart");
    assert_eq!(
        response(47).to_bits(),
        a3(old).to_bits(),
        "frame 47 is on the old set"
    );
    assert_eq!(
        response(63).to_bits(),
        a3(new).to_bits(),
        "frame 63 is on the new set"
    );
    let mid = response(55);
    assert!(
        a3(old) < mid && mid < a3(new),
        "frame 55 is under way: {mid}, between {} and {}",
        a3(old),
        a3(new)
    );
}

/// A cell moved before the first block (no control signal) is held: every block length
/// renders the same bits.
///
/// Not sensitive to the grid (a held set has no ramp to cut);
/// it pins the path the control signals' case does not take, one retarget at the first
/// grid point and none after.
#[test]
fn held_cells_render_alike_at_every_block_length() {
    let inputs = [noise(3)];
    same_at_every_block("svf low-pass, cells", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            render(
                SvfFilterNode::<f32>::from(
                    SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                        .freq(Hz::new(800.0))
                        .q(Q::new(1.2)),
                ),
                block,
                &inputs,
                &[None, None, None],
                |set| {
                    assert!(set.set(UnitParam::Cutoff, 3_000.0));
                },
            )
        })
    });
}

/// A step in a cutoff control signal at frame 40 reaches the render within one segment of
/// the grid point that samples it.
///
/// The grid points are 0, 16, 32, 48. The point at 32 reads the old cutoff and
/// its segment ends on that set at frame 47: an impulse there (into a filter with
/// no state) is answered as the old held filter answers it. The point at 48 is
/// the first to read the new cutoff and its segment ends on that set at frame 63,
/// so an impulse there is answered as the new held filter answers it: 16 frames
/// after the last point that did not see the step, 24 after the step itself.
#[test]
fn a_signal_step_is_in_the_render_within_one_segment() {
    const STEP: usize = 40;
    let (old, new) = (400.0f64, 4_000.0f64);
    // The control signal carries `f32` words.
    let signal: Vec<f32> = (0..64)
        .map(|i| (if i < STEP { old } else { new }) as f32)
        .collect();
    let response = |cutoff: Option<f64>, at: usize| -> f64 {
        let mut x = vec![0.0f32; 64];
        x[at] = 1.0;
        let held = cutoff.map(|c| vec![c as f32; 64]);
        let signals = [Some(held.unwrap_or_else(|| signal.clone())), None, None];
        let node = SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(old))
                .q(Q::new(0.707)),
        );
        let out = render(node, 64, &[x], &signals, |_| {});
        f64::from(out[0][at])
    };
    let at_old = response(Some(old), 47);
    let at_new = response(Some(new), 63);
    assert_ne!(at_old, at_new, "the two cutoffs must be told apart");
    assert_eq!(
        response(None, 47),
        at_old,
        "frame 47 is still on the old set"
    );
    assert_eq!(response(None, 63), at_new, "frame 63 is on the new set");
}

/// A stereo three-band crossover with a control signal on each split, and one with a control signal on
/// the upper split and the lower cell moved before the first block, render the same
/// bits (all six outputs) at blocks of 1, 17, 64 or 512 frames.
#[test]
fn the_crossover_renders_alike_at_every_block_length_with_signals() {
    let inputs = [noise(6), noise(7)];
    same_at_every_block("crossover, a signal on each split", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            let signals = [Some(sweep(200.0, 1_500.0)), Some(sweep(2_500.0, 9_000.0))];
            render(
                CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                    BusTopology::STEREO,
                    CrossoverControls::try_from(&[Hz::new(300.0), Hz::new(3_000.0)][..]).unwrap(),
                ))
                .unwrap(),
                block,
                &inputs,
                &signals,
                |_| {},
            )
        })
    });
    same_at_every_block("crossover, a signal and a moved cell", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            render(
                CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                    BusTopology::STEREO,
                    CrossoverControls::try_from(&[Hz::new(300.0), Hz::new(3_000.0)][..]).unwrap(),
                ))
                .unwrap(),
                block,
                &inputs,
                &[None, Some(sweep(2_500.0, 9_000.0))],
                |set| {
                    // Before the first block: nothing to ramp from.
                    assert!(set.set(UnitAddr::new(UnitParam::Cutoff, 0), 900.0));
                },
            )
        })
    });
}

/// A step in a crossover's control signal at frame 40 is held to the next grid point, then ramps: the
/// point at 32 reads the old frequency and its segment runs on that set to frame 47; the
/// point at 48 reads the new one and the integrator gain `g = tan(pi f / fs)` moves linearly
/// to it over the 16 frames, frame 48 + m at step m + 1 of 16, reached exactly at frame 63. An
/// impulse into a crossover with no state answers with the set in force at that frame, so the
/// low band there is `a3^2`, `a3 = g^2 / (1 + g (g + sqrt2))`, at that frame's gain.
#[test]
fn a_crossover_signal_step_is_held_to_the_next_grid_point_then_ramps() {
    const STEP: usize = 40;
    let (old, new) = (400.0f64, 4_000.0f64);
    // The control signal carries `f32` words.
    let signal: Vec<f32> = (0..64)
        .map(|i| (if i < STEP { old } else { new }) as f32)
        .collect();
    let response = |cutoff: Option<f64>, at: usize| -> f64 {
        let mut x = vec![0.0f32; 64];
        x[at] = 1.0;
        let held = cutoff.map(|c| vec![c as f32; 64]);
        let signals = [Some(held.unwrap_or_else(|| signal.clone()))];
        let node = CrossoverNode::<f32>::try_from(CrossoverConfig::new(
            BusTopology::MONO,
            CrossoverControls::try_from(&[Hz::new(old)][..]).unwrap(),
        ))
        .unwrap();
        let out = render(node, 64, &[x], &signals, |_| {});
        f64::from(out[0][at])
    };
    let at_old = response(Some(old), 47);
    let at_new = response(Some(new), 63);
    assert_ne!(at_old, at_new, "the two frequencies must be told apart");
    assert_eq!(
        response(None, 47),
        at_old,
        "frame 47 is still on the old set"
    );
    assert_eq!(
        response(None, 63),
        at_new,
        "frame 63 is on the new set, exactly"
    );
    let g = |hz: f64| (core::f64::consts::PI * hz / RATE.get()).tan();
    for at in [48usize, 49, 55, 62] {
        let step = (at - 47) as f64 / 16.0;
        let g = g(old) + (g(new) - g(old)) * step;
        let a3 = g * g / (1.0 + g * (g + core::f64::consts::SQRT_2));
        let got = response(None, at);
        assert!(
            (got - a3 * a3).abs() < 1e-6 * a3 * a3,
            "frame {at}: {got} against {}",
            a3 * a3
        );
    }
}

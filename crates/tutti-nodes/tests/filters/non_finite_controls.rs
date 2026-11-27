//! A non-finite control value never reaches a recursive node's state.
//!
//! Every control cell here is a raw `Arc<AtomicF64>` a host can write
//! anything into. A NaN or ±∞ that reached a filter integrator, a delay line,
//! an LFO phase or an envelope follower would stay there for good — every later
//! sample is computed from it — so the nodes read a non-finite value as
//! "unchanged" and keep the last good one.
//!
//! The hard case is the one tested: the bad value lands in the **same block as
//! another control moving**, which is what sends it into the coefficient solve
//! (a held filter never re-solves, so a NaN alone would be ignored by
//! accident). After it, and after a finite value is written back, the output
//! must be finite.

// These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.

use std::sync::atomic::Ordering;
use tutti_buffers::{ChannelLayout, ChannelTopology, MaxBlock, Mono, Planes};

use tutti_fixtures::Noise;
use tutti_graph::contract::drive;
use tutti_graph::{Node, ParamNode, Prepare};
use tutti_nodes::{
    ChannelTimes, CompressorConfig, CompressorControls, CompressorNode, DelayConfig, DelayControls,
    DelayLayout, DelayNode, GateConfig, GateControls, GateNode, KeySource, LadderFilterConfig,
    LadderFilterNode, LadderType, MaxDelay, ModDelayConfig, ModDelayNode, PhaserConfig, PhaserNode,
    Stages, Stagger, SvfFilterConfig, SvfFilterNode, SvfType,
};
use tutti_rt::ParamCell;
use tutti_units::{
    CompressionRatio, Db, Drive, Feedback, Hz, Q, Resonance, SampleRate, Samples, Seconds,
    UnitParam,
};

const BAD: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

/// 64 frames of noise per channel, planar.
fn noise_block(channels: usize, seed: u32) -> Vec<Vec<f32>> {
    let mut stream = Noise::seeded(seed);
    (0..channels)
        .map(|_| stream.by_ref().take(64).collect())
        .collect()
}

/// A control cell, and a finite value it can be moved to.
type Cell = (&'static str, ParamCell, f32);

/// [`render_finite`] for a graph node: `blocks` 64-frame blocks through
/// `tutti_graph::contract::drive` at `rate`.
fn render_node_finite(node: &mut dyn Node, rate: SampleRate, blocks: usize, seed: u32) -> bool {
    let width = usize::from(node.shape().audio_in().count());
    let mut all = true;
    for b in 0..blocks {
        let chans = noise_block(width, seed + b as u32);
        let refs: Vec<&[f32]> = chans.iter().map(|c| &c[..]).collect();
        let out = drive(node, rate, Planes::new(&refs).unwrap(), &[]);
        all &= out.iter().flatten().all(|s| s.is_finite());
    }
    all
}

/// [`survives`] for a graph node: the same cases, the rate change a
/// re-`prepare`.
fn survives_node<N: Node>(name: &str, make: impl Fn() -> N, cells: impl Fn(&N) -> Vec<Cell>) {
    let (a, b) = (SampleRate::SR_48K, SampleRate::SR_44K1);
    for bad in BAD {
        let count = cells(&make()).len();
        for k in 0..count {
            let mut node = make();
            node.prepare(&Prepare::new(a, MaxBlock::of(Samples::new(64))));
            assert!(
                render_node_finite(&mut node, a, 4, 1),
                "{name}: finite before"
            );
            let cs = cells(&node);
            let (cell, atomic, alt) = &cs[k];
            let before = atomic.load(Ordering::Acquire);
            atomic.store(f64::from(bad), Ordering::Release);
            for (j, (_, other, other_alt)) in cs.iter().enumerate() {
                if j != k {
                    other.store(f64::from(*other_alt), Ordering::Release);
                }
            }
            assert!(
                render_node_finite(&mut node, a, 8, 10),
                "{name}: {cell} = {bad} (with every other control moving) reached the output"
            );
            node.prepare(&Prepare::new(b, MaxBlock::of(Samples::new(64))));
            assert!(
                render_node_finite(&mut node, b, 8, 30),
                "{name}: {cell} = {bad} reached the output through a rate change"
            );
            atomic.store(
                if before.is_finite() {
                    f64::from(*alt)
                } else {
                    before
                },
                Ordering::Release,
            );
            assert!(
                render_node_finite(&mut node, b, 8, 20),
                "{name}: {cell} = {bad} left the state non-finite after a finite write"
            );
        }
    }
}

/// A NaN or infinite control never reaches the output of the SVF's cutoff, the ladder's
/// resonance, the delay's delay time, the LFO rate, the mod-mix depth, the compressor's
/// attack, the gate's attack and range, or the shared mod feedback: each read goes
/// through `LastGood`. The gate's hold is sanitised too but cannot go non-finite in the
/// output (it becomes an integer frame count, where NaN saturates to 0), so it is not
/// observable here.
#[test]
fn svf_ladder_delay_moddelay_phaser_compressor_gate_hold_off_non_finite_controls() {
    survives_node(
        "svf",
        || {
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::Bell)
                    .freq(Hz::new(900.0))
                    .q(Q::new(1.0)),
            )
        },
        |n| {
            vec![
                (
                    "cutoff",
                    n.param_set().cell(UnitParam::Cutoff).unwrap(),
                    2_000.0,
                ),
                ("q", n.param_set().cell(UnitParam::Q).unwrap(), 3.0),
                ("gain", n.param_set().cell(UnitParam::GainDb).unwrap(), 6.0),
            ]
        },
    );
    survives_node(
        "ladder",
        || {
            LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                    .freq(Hz::new(900.0))
                    .res(Resonance::new(0.5)),
            )
        },
        |n| {
            vec![
                (
                    "cutoff",
                    n.param_set().cell(UnitParam::Cutoff).unwrap(),
                    3_000.0,
                ),
                ("resonance", n.param_set().cell(UnitParam::Q).unwrap(), 0.8),
                ("drive", n.param_set().cell(UnitParam::Drive).unwrap(), 3.0),
            ]
        },
    );
    survives_node(
        "delay",
        || {
            let layout =
                DelayLayout::builder(ChannelTopology::STEREO, MaxDelay::of(Seconds::new(0.1)))
                    .times(ChannelTimes::try_from(&[Seconds::new(0.013)][..]).unwrap())
                    .build()
                    .unwrap();
            DelayNode::<f32>::from(DelayConfig::new(layout).controls(DelayControls {
                sync: tutti_nodes::DelaySync::Free,
                time: Seconds::new(0.01),
                feedback: Feedback::new(0.5),
                cross_feedback: Feedback::new(0.2),
            }))
        },
        |n| {
            vec![
                (
                    "delay time",
                    n.param_set().cell(UnitParam::DelayTime).unwrap(),
                    0.02,
                ),
                (
                    "feedback",
                    n.param_set().cell(UnitParam::Feedback).unwrap(),
                    0.7,
                ),
                (
                    "cross feedback",
                    n.param_set().cell(UnitParam::CrossFeedback).unwrap(),
                    0.1,
                ),
            ]
        },
    );
    survives_node(
        "chorus",
        || ModDelayNode::<f32>::from(ModDelayConfig::chorus(ChannelTopology::STEREO)),
        |n| {
            vec![
                ("rate", n.param_set().cell(UnitParam::Rate).unwrap(), 3.0),
                (
                    "depth",
                    n.param_set().cell(UnitParam::Depth).unwrap(),
                    0.008,
                ),
                (
                    "feedback",
                    n.param_set().cell(UnitParam::Feedback).unwrap(),
                    0.6,
                ),
            ]
        },
    );
    survives_node(
        "phaser",
        || {
            PhaserNode::<f32>::from(
                PhaserConfig::new(ChannelTopology::STEREO, Stages::of(6), Stagger::NONE).unwrap(),
            )
        },
        |n| {
            vec![
                ("rate", n.param_set().cell(UnitParam::Rate).unwrap(), 2.0),
                ("depth", n.param_set().cell(UnitParam::Depth).unwrap(), 0.9),
                (
                    "feedback",
                    n.param_set().cell(UnitParam::Feedback).unwrap(),
                    0.8,
                ),
            ]
        },
    );
    survives_node(
        "compressor",
        || {
            CompressorNode::<f32>::from(
                CompressorConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(CompressorControls {
                        threshold: Db::new(-20.0),
                        knee: Db::new(6.0),
                        ratio: CompressionRatio::new(4.0),
                        attack: Seconds::new(0.001),
                        release: Seconds::new(0.05),
                        ..CompressorControls::DEFAULT
                    }),
            )
        },
        |n| {
            vec![
                (
                    "threshold",
                    n.param_set().cell(UnitParam::Threshold).unwrap(),
                    -30.0,
                ),
                ("ratio", n.param_set().cell(UnitParam::Ratio).unwrap(), 8.0),
                (
                    "attack",
                    n.param_set().cell(UnitParam::Attack).unwrap(),
                    0.01,
                ),
                (
                    "release",
                    n.param_set().cell(UnitParam::Release).unwrap(),
                    0.2,
                ),
                (
                    "makeup",
                    n.param_set().cell(UnitParam::GainDb).unwrap(),
                    3.0,
                ),
                ("knee", n.param_set().cell(UnitParam::Knee).unwrap(), 2.0),
            ]
        },
    );
    survives_node(
        "gate",
        || {
            GateNode::<f32>::from(
                GateConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(GateControls {
                        threshold: Db::new(-30.0),
                        attack: Seconds::new(0.001),
                        hold: Seconds::new(0.01),
                        release: Seconds::new(0.05),
                        range: Db::new(-80.0),
                        ..GateControls::DEFAULT
                    }),
            )
        },
        |n| {
            vec![
                (
                    "threshold",
                    n.param_set().cell(UnitParam::Threshold).unwrap(),
                    -20.0,
                ),
                (
                    "attack",
                    n.param_set().cell(UnitParam::Attack).unwrap(),
                    0.005,
                ),
                ("hold", n.param_set().cell(UnitParam::Hold).unwrap(), 0.02),
                (
                    "release",
                    n.param_set().cell(UnitParam::Release).unwrap(),
                    0.1,
                ),
                (
                    "range",
                    n.param_set().cell(UnitParam::Range).unwrap(),
                    -20.0,
                ),
            ]
        },
    );
}

/// The convolver's gain is its only cell (the mix is the graph's, which refuses
/// a non-finite one: `tutti-session/tests/nodes/node_mix.rs`); it does not feed
/// recursive state, but the block's end starts the next block's ramp.
#[test]
fn the_convolver_holds_off_non_finite_controls() {
    use tutti_fixtures::signal::decay_ir;

    let ir = decay_ir(128, Seconds::new(0.1), SampleRate::SR_48K);
    survives_node(
        "convolver",
        || shared_ir_node(ChannelLayout::STEREO, Mono::new(&ir), Samples::new(64)).unwrap(),
        |n| vec![("gain", n.gain().cell(), 2.0)],
    );
}

/// The waveshaper's drive cell holds a non-finite write off: an infinite drive
/// built an infinitely hard shaper and NaN output (0 * inf) before.
#[test]
fn the_distortion_holds_off_non_finite_controls() {
    use tutti_nodes::{DistortionConfig, DistortionNode, ShapeKind, Waveshape};
    for kind in [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::TriangleFold,
        ShapeKind::Crush,
        ShapeKind::SoftCrush,
    ] {
        survives_node(
            "distortion",
            || {
                DistortionNode::<f32>::from(
                    DistortionConfig::new(ChannelLayout::STEREO, Waveshape::new(kind))
                        .drive(Drive::new(2.0)),
                )
            },
            |n| vec![("drive", n.param_set().cell(UnitParam::Drive).unwrap(), 5.0)],
        );
    }
}

/// A waveshaper built with a drive that is NaN or infinite renders finite.
///
/// The first block's `LastGood` read rebuilds the
/// shaper from 0 whatever the constructor built, so this pins the behaviour.
#[test]
fn a_distortion_built_with_a_non_finite_drive_renders_finite() {
    use tutti_nodes::{DistortionConfig, DistortionNode, ShapeKind, Waveshape};
    for bad in BAD {
        let mut node = DistortionNode::<f32>::from(
            DistortionConfig::new(ChannelLayout::STEREO, Waveshape::new(ShapeKind::Tanh))
                .drive(Drive::new(f64::from(bad))),
        );
        node.prepare(&Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        assert!(
            render_node_finite(&mut node, SampleRate::SR_48K, 4, 1),
            "drive {bad}"
        );
    }
}

fn unassigned_bus(
    width: impl Into<tutti_buffers::ChannelLayout>,
) -> tutti_buffers::ChannelTopology {
    tutti_buffers::ChannelTopology::unassigned(width.into()).expect("a test width fits a bus")
}

/// `width` unassigned channels sharing the response `ir`.
fn shared_ir_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    width: impl Into<tutti_buffers::ChannelLayout>,
    ir: tutti_buffers::Mono<'_, S>,
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(unassigned_bus(width), &[ir]).block_size(block_size),
    )
}

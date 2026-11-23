//! The table: one row per node, each a node at its corners with the controls it moves.
//! Every row runs through [`Gate`](crate::harness::Gate), which warms the node up and then steps
//! it under `assert_no_alloc`.

use crate::harness::{Gate, alternate, every_third, feeds};
use std::sync::Arc;
use tutti_buffers::{BusTopology, ChannelLayout, Interleaved, Mono, Wave};
use tutti_dsp::{CombDelay, MaxGrains, SvfType, Wavetable};
use tutti_nodes::{
    BitCrusherConfig, BitCrusherNode, ButterworthOrder, CombFilterConfig, CombFilterNode, CombForm,
    CutAlignment, CutFilterConfig, CutFilterNode, CutResponse, DcBlockerConfig, DcBlockerNode,
    DiodeClipperConfig, DiodeClipperNode, DistanceConfig, DistanceNode, DynamicEqConfig,
    DynamicEqNode, DynamicMode, EllipticTolerances, EnvelopeFilterConfig, EnvelopeFilterNode,
    FrequencyShifterConfig, FrequencyShifterNode, GranularConfig, GranularNode, HysteresisConfig,
    HysteresisNode, KeySource, LevellerConfig, LevellerNode, ModalBankConfig, ModalBankNode, Mode,
    Modes, OversamplingFactor, PassbandRipple, PitchShifterConfig, PitchShifterNode, ShimmerConfig,
    ShimmerNode, StopbandAttenuation, TransientShaperConfig, TransientShaperNode, TubeStageConfig,
    TubeStageNode, WavetableConfig, WavetableNode,
};
use tutti_units::{
    CompressionRatio, Db, Gain, Hz, Metres, Normalized, Q, SampleRate, Seconds, Semitones,
    UnitAddr, UnitParam,
};

/// Every oversampled saturator is stepped at each of these.
const FACTORS: [OversamplingFactor; 3] = [
    OversamplingFactor::X2,
    OversamplingFactor::X4,
    OversamplingFactor::X8,
];

/// The rows: a node, its input, the controls it moves.
pub const ROWS: &[fn()] = &[
    bit_crusher,
    comb_filter,
    frequency_shifter,
    cut_filter,
    dc_blocker,
    diode_clipper,
    distance,
    dynamic_eq,
    envelope_filter,
    granular,
    hysteresis,
    leveller,
    modal_bank,
    pitch_shifter,
    shimmer,
    transient_shaper,
    tube_stage,
    wavetable,
];

fn bit_crusher() {
    let node = BitCrusherNode::<f32>::from(BitCrusherConfig::new(ChannelLayout::STEREO));
    Gate::new(feeds::sine).run("bit crusher", node, |c, i| {
        c.set(UnitParam::BitDepth, alternate(i, 3.0, 11.5));
        c.set(UnitParam::HoldFactor, f64::from(1.0 + (i % 5) as f32 * 1.3));
    });
}

fn comb_filter() {
    for form in [CombForm::Feedforward, CombForm::Feedback] {
        let node = CombFilterNode::from(CombFilterConfig::new(
            ChannelLayout::STEREO,
            form,
            CombDelay::of(Seconds::new(0.003)),
        ));
        Gate::new(feeds::offset_square).run(&format!("comb {form:?}"), node, |c, i| {
            c.set(UnitParam::Feedback, alternate(i, 0.2, 0.9));
        });
    }
}

fn frequency_shifter() {
    let node = FrequencyShifterNode::from(FrequencyShifterConfig::new(ChannelLayout::STEREO));
    Gate::new(feeds::offset_square).run("frequency shifter", node, |c, i| {
        c.set(UnitParam::Frequency, alternate(i, -220.0, 1_700.0));
    });
}

/// Every cutoff move designs into the held scratch and ramps the coefficients; the elliptic
/// alignment's Jacobi functions run on the stack.
fn cut_filter() {
    let elliptic = CutAlignment::Elliptic {
        order: ButterworthOrder::of(8),
        tolerances: EllipticTolerances::new(
            PassbandRipple::new(Db::new(0.5)).unwrap(),
            StopbandAttenuation::new(Db::new(60.0)).unwrap(),
        )
        .unwrap(),
    };
    let alignments = [
        (
            "linkwitz-riley",
            CutAlignment::LinkwitzRiley(ButterworthOrder::of(8)),
        ),
        ("elliptic", elliptic),
    ];
    for (name, alignment) in alignments {
        let config = CutFilterConfig::new(ChannelLayout::STEREO, CutResponse::LowPass, alignment);
        Gate::new(feeds::offset_square).run(
            &format!("cut filter {name}"),
            CutFilterNode::from(config),
            |c, i| {
                c.set(UnitParam::Cutoff, alternate(i, 80.0, 6_000.0));
            },
        );
    }
}

/// The corner moves the pole ramp.
fn dc_blocker() {
    let node = DcBlockerNode::<f32>::from(DcBlockerConfig::new(ChannelLayout::STEREO));
    Gate::new(feeds::offset_square).run("dc blocker", node, |c, i| {
        c.set(UnitParam::Cutoff, alternate(i, 5.0, 90.0));
    });
}

fn diode_clipper() {
    for factor in FACTORS {
        let node = DiodeClipperNode::<f32>::from(
            DiodeClipperConfig::new(ChannelLayout::STEREO).oversampling(factor),
        );
        Gate::new(feeds::ramp).blocks(1_000).every_channel().run(
            &format!("diode clipper {factor:?}"),
            node,
            |c, i| {
                c.set(UnitParam::Cutoff, alternate(i, 600.0, 9_000.0));
                c.set(UnitParam::GainDb, every_third(i, -6.0, 24.0));
            },
        );
    }
}

/// With air absorption on, the distance moves across the line.
fn distance() {
    let node = DistanceNode::<f32>::from(
        DistanceConfig::new(ChannelLayout::STEREO, Metres::of(60.0))
            .air_absorption(Hz::new(6_000.0)),
    );
    Gate::new(feeds::sine)
        .frames(480)
        .run("distance", node, |c, i| {
            c.set(UnitParam::Distance, [0.5, 12.0, 55.0, 3.0][i % 4]);
        });
}

/// A bell design every grid period and the ramp of both filters.
fn dynamic_eq() {
    let nodes = [
        (
            "de-esser",
            DynamicEqNode::<f32>::from(DynamicEqConfig::de_esser(
                ChannelLayout::STEREO,
                Hz::new(6_000.0),
            )),
        ),
        (
            "upward side chain",
            DynamicEqNode::<f32>::from(
                DynamicEqConfig::new(ChannelLayout::MONO, Hz::new(6_000.0), Q::new(1.5))
                    .mode(DynamicMode::Upward)
                    .ratio(CompressionRatio::new(3.0))
                    .range(Db::new(9.0))
                    .key(KeySource::Sidechain),
            ),
        ),
    ];
    for (name, node) in nodes {
        Gate::new(feeds::bright_square).run(&format!("dynamic eq {name}"), node, |c, i| {
            c.set(UnitParam::Cutoff, alternate(i, 3_000.0, 6_000.0));
            c.set(UnitParam::Threshold, f64::from(-30.0 + (i % 7) as f32));
        });
    }
}

fn envelope_filter() {
    let config =
        EnvelopeFilterConfig::new(ChannelLayout::STEREO, Hz::new(400.0), SvfType::BandPass)
            .depth(Semitones::new(36.0));
    Gate::new(feeds::square).blocks(256).run(
        "envelope filter",
        EnvelopeFilterNode::<f32>::from(config),
        |c, i| {
            c.set(UnitParam::Cutoff, alternate(i, 300.0, 3_000.0));
            c.set(UnitParam::Semitones, every_third(i, -12.0, 36.0));
        },
    );
}

/// Eight slots under up to 400 grains a second of 50 ms: slots run out.
fn granular() {
    let mut source = Wave::<f32>::new(2, SampleRate::SR_48K);
    let frames: Vec<f32> = (0..2 * 9_600).map(|i| (i as f32 * 0.05).sin()).collect();
    source
        .append(Interleaved::new(&frames, ChannelLayout::STEREO).unwrap())
        .unwrap();
    let node = GranularNode::<f32>::try_from(
        GranularConfig::new(ChannelLayout::STEREO, Arc::new(source))
            .max_grains(MaxGrains::of(8))
            .seed(11)
            .jitter([Normalized::of(0.3), Normalized::of(0.5)]),
    )
    .unwrap();
    Gate::new(feeds::silence)
        .frames(480)
        .run("granular", node, |c, i| {
            let k = (i % 4) as f64;
            c.set(UnitParam::Rate, 20.0 + 130.0 * k);
            c.set(UnitParam::Pitch, -700.0 + 500.0 * k);
            c.set(UnitParam::GrainLength, 0.005 + 0.015 * k);
            c.set(UnitAddr::new(UnitParam::Jitter, 0), 0.1 * k);
            c.set(UnitAddr::new(UnitParam::Jitter, 1), 0.2 * k);
            c.set(UnitParam::Position, 0.2 * k);
        });
}

fn hysteresis() {
    for factor in FACTORS {
        let node = HysteresisNode::<f32>::from(
            HysteresisConfig::new(ChannelLayout::STEREO).oversampling(factor),
        );
        Gate::new(feeds::ramp).blocks(1_000).every_channel().run(
            &format!("hysteresis {factor:?}"),
            node,
            |c, i| {
                c.set(UnitParam::GainDb, every_third(i, -6.0, 18.0));
            },
        );
    }
}

/// 256 blocks of 64 frames cross the window's hop; both targets move.
fn leveller() {
    let node = LevellerNode::<f32>::try_from(LevellerConfig::new(BusTopology::STEREO)).unwrap();
    let target = |i: usize| alternate(i, -30.0, -16.0);
    Gate::new(feeds::quiet_square)
        .blocks(256)
        .run("leveller", node, |c, i| {
            c.set(UnitAddr::new(UnitParam::Threshold, 0), target(i));
            c.set(UnitAddr::new(UnitParam::Threshold, 1), target(i + 1) - 30.0);
        });
}

fn modal_bank() {
    let modes = [(440.0, 0.4), (1_130.0, 0.2), (3_070.0, 0.1)].map(|(f, t)| Mode {
        frequency: Hz::new(f),
        decay: Seconds::new(t),
        gain: Gain::new(1.0),
    });
    let node = ModalBankNode::from(ModalBankConfig::new(
        ChannelLayout::STEREO,
        Modes::try_from(&modes[..]).unwrap(),
    ));
    Gate::new(feeds::pulse).run("modal bank", node, |c, i| {
        c.set(UnitParam::Frequency, alternate(i, 220.0, 880.0));
        c.set(UnitParam::Decay, every_third(i, 0.2, 1.5));
    });
}

/// A block of 480 frames among hops of 512, the pitch across the whole range.
fn pitch_shifter() {
    let node = PitchShifterNode::<f32>::from(PitchShifterConfig::new(ChannelLayout::STEREO));
    Gate::new(feeds::sine)
        .frames(480)
        .run("pitch shifter", node, |c, i| {
            c.set(UnitParam::Pitch, [-2_400.0, 700.0, 2_400.0, 0.0][i % 4]);
        });
}

/// A block of 480 frames among chunks of 32 and hops of 512, shift and send across their ranges.
fn shimmer() {
    let node = ShimmerNode::<f32>::from(ShimmerConfig::new());
    Gate::new(feeds::shared_sine)
        .frames(480)
        .run("shimmer", node, |c, i| {
            c.set(UnitParam::Pitch, [-2_400.0, 700.0, 2_400.0, 0.0][i % 4]);
            c.set(UnitParam::Feedback, [0.0, 0.5, 5.0, 0.99][i % 4]);
        });
}

fn transient_shaper() {
    let config = TransientShaperConfig::new(ChannelLayout::STEREO).attack(Db::new(6.0));
    Gate::new(feeds::square).blocks(256).run(
        "transient shaper",
        TransientShaperNode::<f32>::from(config),
        |c, i| {
            c.set(
                UnitAddr::new(UnitParam::GainDb, 0),
                alternate(i, -12.0, 12.0),
            );
            c.set(
                UnitAddr::new(UnitParam::GainDb, 1),
                alternate(i + 1, -12.0, 12.0),
            );
        },
    );
}

fn tube_stage() {
    for factor in FACTORS {
        let node = TubeStageNode::<f32>::from(
            TubeStageConfig::new(ChannelLayout::STEREO).oversampling(factor),
        );
        Gate::new(feeds::ramp).blocks(1_000).every_channel().run(
            &format!("tube stage {factor:?}"),
            node,
            |c, i| {
                c.set(UnitParam::GainDb, every_third(i, -6.0, 24.0));
            },
        );
    }
}

/// The frequency sweeps the mip levels while the frame position moves.
fn wavetable() {
    let saw: Vec<f32> = (0..1024).map(|i| i as f32 / 512.0 - 1.0).collect();
    let square: Vec<f32> = (0..1024)
        .map(|i| if i < 512 { 1.0 } else { -1.0 })
        .collect();
    let table = Wavetable::new(&[Mono::new(&saw), Mono::new(&square)]).expect("two frames");
    let node = WavetableNode::<f32>::from(WavetableConfig::new(ChannelLayout::STEREO, table));
    Gate::new(feeds::silence).run("wavetable", node, |c, i| {
        c.set(
            UnitParam::Frequency,
            30.0 * 1.0037f64.powi((i % 1_900) as i32),
        );
        c.set(UnitParam::Position, (i % 7) as f64 / 6.0);
    });
}

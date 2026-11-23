//! The table: one row per node, each a node at its corners with the controls it moves.
//! Every row runs through [`Gate`](crate::harness::Gate), which warms the node up and then steps
//! it under `assert_no_alloc`.

use crate::harness::{Gate, alternate, every_third, feeds};
use std::sync::Arc;
use tutti_buffers::{ChannelIndex, ChannelLayout, ChannelTopology, Interleaved, Mono, Wave};
use tutti_dsp::{
    Algorithm, CombDelay, FmPatch, Inline, MaxGrains, OperatorCount, OperatorFrequency,
    OperatorSpec, Partial, Partials, SvfType, Tap, TapSet, Wavetable,
};
use tutti_graph::IntoNode;
use tutti_nodes::{
    AdditiveConfig, AdditiveNode, AutoPanConfig, AutoPanNode, BellFilterConfig, BellFilterNode,
    BitCrusherConfig, BitCrusherNode, ButterworthOrder, CombFilterConfig, CombFilterNode, CombForm,
    CutAlignment, CutFilterConfig, CutFilterNode, CutResponse, DcBlockerConfig, DcBlockerNode,
    DecorrelatorConfig, DecorrelatorNode, DiodeClipperConfig, DiodeClipperNode, DistanceConfig,
    DistanceNode, DynamicEqConfig, DynamicEqNode, DynamicMode, EarlyReflectionsConfig,
    EarlyReflectionsNode, EllipticTolerances, EnvelopeFilterConfig, EnvelopeFilterNode, FmConfig,
    FmNode, FrequencyShifterConfig, FrequencyShifterNode, GranularConfig, GranularNode,
    GraphicBands, GraphicEqConfig, GraphicEqNode, HysteresisConfig, HysteresisNode,
    KarplusStrongConfig, KarplusStrongNode, KeySource, LevellerConfig, LevellerNode, MidSideConfig,
    MidSideNode, ModalBankConfig, ModalBankNode, Mode, Modes, NoiseColour, NoiseConfig, NoiseNode,
    OversamplingFactor, PassbandRipple, PitchShifterConfig, PitchShifterNode, PolarityConfig,
    PolarityNode, RingModConfig, RingModNode, ShimmerConfig, ShimmerNode, StereoWidthConfig,
    StereoWidthNode, StopbandAttenuation, TiltFilterConfig, TiltFilterNode, TransientShaperConfig,
    TransientShaperNode, TremoloConfig, TremoloNode, TubeStageConfig, TubeStageNode,
    WaveguideStringConfig, WaveguideStringNode, WavetableConfig, WavetableNode,
};
use tutti_units::{
    Amplitude, CompressionRatio, Db, FrequencyRatio, Gain, Hz, Metres, ModulationIndex, Normalized,
    Q, SampleRate, SampleSpan, Seconds, Semitones, UnitAddr, UnitParam,
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
    tremolo_auto_pan_ring_mod,
    physical_strings,
    early_reflections_decorrelator,
    fm_additive,
    bell_tilt_graphic_eq,
    utility,
];

fn bit_crusher() {
    let node = BitCrusherNode::<f32>::from(BitCrusherConfig::new(ChannelLayout::STEREO));
    Gate::new(feeds::sine)
        .warm_moves()
        .run("bit crusher", node, |c, i| {
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
        Gate::new(feeds::offset_square).warm_moves().run(
            &format!("comb {form:?}"),
            node,
            |c, i| {
                c.set(UnitParam::Feedback, alternate(i, 0.2, 0.9));
            },
        );
    }
}

fn frequency_shifter() {
    let node = FrequencyShifterNode::from(FrequencyShifterConfig::new(ChannelLayout::STEREO));
    Gate::new(feeds::offset_square)
        .warm_moves()
        .run("frequency shifter", node, |c, i| {
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
        Gate::new(feeds::offset_square).warm_moves().run(
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
    Gate::new(feeds::offset_square)
        .warm_moves()
        .run("dc blocker", node, |c, i| {
            c.set(UnitParam::Cutoff, alternate(i, 5.0, 90.0));
        });
}

fn diode_clipper() {
    for factor in FACTORS {
        let node = DiodeClipperNode::<f32>::from(
            DiodeClipperConfig::new(ChannelLayout::STEREO).oversampling(factor),
        );
        Gate::new(feeds::ramp)
            .warm_moves()
            .blocks(1_000)
            .every_channel()
            .run(&format!("diode clipper {factor:?}"), node, |c, i| {
                c.set(UnitParam::Cutoff, alternate(i, 600.0, 9_000.0));
                c.set(UnitParam::GainDb, every_third(i, -6.0, 24.0));
            });
    }
}

/// With air absorption on, the distance moves across the line.
fn distance() {
    let node = DistanceNode::<f32>::from(
        DistanceConfig::new(ChannelLayout::STEREO, Metres::of(60.0))
            .air_absorption(Hz::new(6_000.0)),
    );
    Gate::new(feeds::sine)
        .warm_moves()
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
        Gate::new(feeds::bright_square).warm_moves().run(
            &format!("dynamic eq {name}"),
            node,
            |c, i| {
                c.set(UnitParam::Cutoff, alternate(i, 3_000.0, 6_000.0));
                c.set(UnitParam::Threshold, f64::from(-30.0 + (i % 7) as f32));
            },
        );
    }
}

fn envelope_filter() {
    let config =
        EnvelopeFilterConfig::new(ChannelLayout::STEREO, Hz::new(400.0), SvfType::BandPass)
            .depth(Semitones::new(36.0));
    let base = |c: &<EnvelopeFilterNode<f32> as IntoNode>::Controls, i| {
        c.set(UnitParam::Cutoff, alternate(i, 300.0, 3_000.0));
    };
    Gate::new(feeds::square).blocks(256).run_phased(
        "envelope filter",
        EnvelopeFilterNode::<f32>::from(config),
        base,
        |c, i| {
            base(c, i);
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
        .warm_moves()
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
        Gate::new(feeds::ramp)
            .warm_moves()
            .blocks(1_000)
            .every_channel()
            .run(&format!("hysteresis {factor:?}"), node, |c, i| {
                c.set(UnitParam::GainDb, every_third(i, -6.0, 18.0));
            });
    }
}

/// 256 blocks of 64 frames cross the window's hop; both targets move.
fn leveller() {
    let node = LevellerNode::<f32>::try_from(LevellerConfig::new(ChannelTopology::STEREO)).unwrap();
    let target = |i: usize| alternate(i, -30.0, -16.0);
    let first = |c: &<LevellerNode<f32> as IntoNode>::Controls, i| {
        c.set(UnitAddr::new(UnitParam::Threshold, 0), target(i));
    };
    Gate::new(feeds::quiet_square)
        .blocks(256)
        .run_phased("leveller", node, first, |c, i| {
            first(c, i);
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
    Gate::new(feeds::pulse)
        .warm_moves()
        .run("modal bank", node, |c, i| {
            c.set(UnitParam::Frequency, alternate(i, 220.0, 880.0));
            c.set(UnitParam::Decay, every_third(i, 0.2, 1.5));
        });
}

/// A block of 480 frames among hops of 512, the pitch across the whole range.
fn pitch_shifter() {
    let node = PitchShifterNode::<f32>::from(PitchShifterConfig::new(ChannelLayout::STEREO));
    Gate::new(feeds::sine)
        .warm_moves()
        .frames(480)
        .run("pitch shifter", node, |c, i| {
            c.set(UnitParam::Pitch, [-2_400.0, 700.0, 2_400.0, 0.0][i % 4]);
        });
}

/// A block of 480 frames among chunks of 32 and hops of 512, shift and send across their ranges.
fn shimmer() {
    let node = ShimmerNode::<f32>::from(ShimmerConfig::new());
    Gate::new(feeds::shared_sine)
        .warm_moves()
        .frames(480)
        .run("shimmer", node, |c, i| {
            c.set(UnitParam::Pitch, [-2_400.0, 700.0, 2_400.0, 0.0][i % 4]);
            c.set(UnitParam::Feedback, [0.0, 0.5, 5.0, 0.99][i % 4]);
        });
}

fn transient_shaper() {
    let config = TransientShaperConfig::new(ChannelLayout::STEREO).attack(Db::new(6.0));
    let first = |c: &<TransientShaperNode<f32> as IntoNode>::Controls, i| {
        c.set(
            UnitAddr::new(UnitParam::GainDb, 0),
            alternate(i, -12.0, 12.0),
        );
    };
    Gate::new(feeds::square).blocks(256).run_phased(
        "transient shaper",
        TransientShaperNode::<f32>::from(config),
        first,
        |c, i| {
            first(c, i);
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
        Gate::new(feeds::ramp)
            .warm_moves()
            .blocks(1_000)
            .every_channel()
            .run(&format!("tube stage {factor:?}"), node, |c, i| {
                c.set(UnitParam::GainDb, every_third(i, -6.0, 24.0));
            });
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
    Gate::new(feeds::silence)
        .warm_moves()
        .run("wavetable", node, |c, i| {
            c.set(
                UnitParam::Frequency,
                30.0 * 1.0037f64.powi((i % 1_900) as i32),
            );
            c.set(UnitParam::Position, (i % 7) as f64 / 6.0);
        });
}

fn tremolo_auto_pan_ring_mod() {
    let gate = Gate::new(feeds::offset_square).warm_moves();
    let tremolo = TremoloNode::from(TremoloConfig::new(ChannelLayout::STEREO));
    gate.run("tremolo", tremolo, |c, i| {
        c.set(UnitParam::Rate, alternate(i, 3.0, 40.0));
        c.set(UnitParam::Depth, every_third(i, 0.2, 1.0));
    });
    gate.run(
        "auto pan",
        AutoPanNode::from(AutoPanConfig::new()),
        |c, i| {
            c.set(UnitParam::Rate, alternate(i, 3.0, 40.0));
            c.set(UnitParam::Depth, every_third(i, 0.2, 1.0));
        },
    );
    let ring_mod = RingModNode::from(RingModConfig::new(ChannelLayout::STEREO));
    gate.run("ring mod", ring_mod, |c, i| {
        c.set(UnitParam::Frequency, alternate(i, 220.0, 1_700.0));
    });
}

/// A pitch move retunes the loop on the audio thread.
fn physical_strings() {
    let gate = Gate::new(feeds::pulse).warm_moves();
    let karplus = KarplusStrongNode::from(KarplusStrongConfig::new(ChannelLayout::STEREO));
    gate.run("karplus-strong", karplus, |c, i| {
        c.set(UnitParam::Frequency, alternate(i, 220.0, 880.0));
        c.set(UnitParam::Decay, every_third(i, 0.2, 1.5));
    });
    let waveguide = WaveguideStringNode::from(WaveguideStringConfig::new(ChannelLayout::STEREO));
    gate.run("waveguide string", waveguide, |c, i| {
        c.set(UnitParam::Frequency, alternate(i, 220.0, 880.0));
        c.set(UnitParam::Decay, every_third(i, 0.2, 1.5));
    });
}

fn early_reflections_decorrelator() {
    let gate = Gate::new(feeds::offset_square).warm_moves();
    let tap = |delay: f64, gain: f64, channel: u8| Tap {
        delay: SampleSpan::new(delay).unwrap(),
        gain: Gain::new(gain),
        channel,
    };
    let taps = TapSet {
        taps: Inline::from_slice(&[
            tap(17.0, 0.6, 0),
            tap(101.5, -0.4, 1),
            tap(333.25, 0.3, 0),
            tap(1_000.0, -0.2, 1),
        ])
        .unwrap(),
    };
    let reflections = EarlyReflectionsConfig::new(ChannelLayout::STEREO, taps);
    let node = EarlyReflectionsNode::<f32>::try_from(reflections).unwrap();
    gate.run("early reflections", node, |_, _| {});
    let node = DecorrelatorNode::<f32>::from(DecorrelatorConfig::new(ChannelLayout::STEREO));
    gate.run("decorrelator", node, |_, _| {});
}

fn fm_additive() {
    let gate = Gate::new(feeds::silence)
        .warm_moves()
        .frames(480)
        .floor(1e-3);
    let ratio = |x: f64| OperatorFrequency::Ratio(FrequencyRatio::new(x).unwrap());
    let patch = FmPatch::new(Algorithm::chain(OperatorCount::of(3))).operators(&[
        OperatorSpec::new(ratio(1.0)),
        OperatorSpec::new(ratio(2.0)).level(Amplitude::new(0.5)),
        OperatorSpec::new(ratio(1.0)),
    ]);
    let fm = FmNode::<f32>::from(FmConfig::new(patch).index(ModulationIndex::of(2.0)));
    gate.run("fm", fm, |c, i| {
        c.set(UnitParam::Frequency, 110.0 * (1 + i % 5) as f64);
        c.set(UnitAddr::new(UnitParam::Index, 0), (i % 7) as f64);
    });
    let partial = |ratio: f64, amplitude: f64| {
        Partial::new(
            FrequencyRatio::new(ratio).unwrap(),
            Amplitude::new(amplitude),
        )
    };
    let partials =
        Partials::try_from(&[partial(1.0, 0.5), partial(2.0, 0.25), partial(40.0, 0.1)][..])
            .unwrap();
    let additive = AdditiveNode::<f32>::from(AdditiveConfig::new(partials));
    gate.run("additive", additive, |c, i| {
        c.set(UnitParam::Frequency, 220.0 * (1 + i % 4) as f64);
    });
}

/// Every control moves: a design into the held scratch and the coefficient ramp.
fn bell_tilt_graphic_eq() {
    let gate = Gate::new(feeds::offset_square).warm_moves();
    let bell = BellFilterConfig::new(
        ChannelLayout::STEREO,
        Hz::new(1_000.0),
        Q::new(1.0),
        Db::new(0.0),
    );
    gate.run("bell filter", BellFilterNode::from(bell), |c, i| {
        c.set(UnitParam::Cutoff, alternate(i, 80.0, 15_000.0));
        c.set(UnitParam::Q, alternate(i, 0.5, 12.0));
        c.set(UnitParam::GainDb, alternate(i, 15.0, -15.0));
    });
    let tilt = TiltFilterConfig::new(ChannelLayout::STEREO, Hz::new(1_000.0), Db::new(0.0));
    gate.run("tilt filter", TiltFilterNode::from(tilt), |c, i| {
        c.set(UnitParam::Cutoff, alternate(i, 80.0, 15_000.0));
        c.set(UnitParam::GainDb, alternate(i, 15.0, -15.0));
    });
    for bands in [GraphicBands::Octave, GraphicBands::ThirdOctave] {
        let config = GraphicEqConfig::new(ChannelTopology::STEREO, bands);
        let node = GraphicEqNode::from(config);
        gate.run(&format!("graphic eq {bands:?}"), node, |c, i| {
            for band in 0..bands.count() {
                let addr = UnitAddr::new(UnitParam::GainDb, u8::try_from(band).unwrap());
                c.set(
                    addr,
                    if (i + band).is_multiple_of(2) {
                        15.0
                    } else {
                        -15.0
                    },
                );
            }
        });
    }
}

fn utility() {
    let gate = Gate::new(feeds::offset_square).warm_moves();
    let polarity = PolarityConfig::new(ChannelLayout::STEREO).only([ChannelIndex::of(1)]);
    gate.run(
        "polarity",
        PolarityNode::try_from(polarity).unwrap(),
        |_, _| {},
    );
    gate.run(
        "mid/side encode",
        MidSideNode::from(MidSideConfig::encode()),
        |_, _| {},
    );
    gate.run(
        "mid/side decode",
        MidSideNode::from(MidSideConfig::decode()),
        |_, _| {},
    );
    let width = StereoWidthNode::<f32>::from(StereoWidthConfig::new());
    gate.run("stereo width", width, |c, i| {
        c.set(UnitParam::StereoWidth, alternate(i, 0.2, 1.8));
    });
    for colour in [NoiseColour::White, NoiseColour::Pink, NoiseColour::Brown] {
        let node = NoiseNode::<f32>::from(NoiseConfig::new(colour).seed(1));
        gate.run(&format!("noise {colour:?}"), node, |c, i| {
            c.set(UnitParam::Volume, alternate(i, 0.2, 0.9));
        });
    }
}

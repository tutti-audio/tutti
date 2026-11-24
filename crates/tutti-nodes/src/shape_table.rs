//! The shape each node declares: widths, cost, latency, tail ceiling, the params in order and
//! the base each starts at. One row per node; every figure is the node's documented one.

use crate::test_support::prepared;
use crate::*;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_dsp::{
    Algorithm, CombDelay, FmPatch, Operator, OperatorCount, OperatorFrequency, OperatorSpec,
    Partial, Partials, Route,
};
use tutti_fixtures::for_each_row;
use tutti_graph::contract::prepared as prepared_at_rate;
use tutti_graph::{Cost, Node};
use tutti_units::{
    BeatsPerCycle, Bipolar, FrequencyRatio, Gain, Latency, ModulationIndex, SampleRate, Samples,
    Seconds, Tail, UnitAddr, UnitParam,
};

/// What a row pins; a figure left unset is not checked.
#[derive(Default)]
struct Facts {
    inputs: Option<u16>,
    outputs: Option<u16>,
    cost: Option<Cost>,
    latency: Option<Samples>,
    tail: Option<Tail>,
    finite_tail: bool,
    params: Option<Vec<UnitAddr>>,
    bases: Vec<Option<f64>>,
}

impl Facts {
    fn io(mut self, inputs: u16, outputs: u16) -> Self {
        self.inputs = Some(inputs);
        self.outputs = Some(outputs);
        self
    }
    fn inputs(mut self, inputs: u16) -> Self {
        self.inputs = Some(inputs);
        self
    }
    fn cost(mut self, cost: Cost) -> Self {
        self.cost = Some(cost);
        self
    }
    fn no_latency(mut self) -> Self {
        self.latency = Some(Samples::ZERO);
        self
    }
    fn tail(mut self, tail: Tail) -> Self {
        self.tail = Some(tail);
        self
    }
    fn finite_tail(mut self) -> Self {
        self.finite_tail = true;
        self
    }
    fn params(mut self, params: &[UnitParam]) -> Self {
        self.params = Some(params.iter().map(|&p| UnitAddr::from(p)).collect());
        self
    }
    fn addrs(mut self, params: Vec<UnitAddr>) -> Self {
        self.params = Some(params);
        self
    }
    /// The bases of the first `bases.len()` declared params.
    fn bases(mut self, bases: &[Option<f64>]) -> Self {
        self.bases = bases.to_vec();
        self
    }
}

fn some(bases: &[f64]) -> Vec<Option<f64>> {
    bases.iter().map(|&b| Some(b)).collect()
}

/// `node` prepared at `rate` for blocks of up to 64 frames.
fn prepared_at<N: Node>(node: N, rate: SampleRate) -> N {
    prepared_at_rate(node, rate, 64)
}

/// `node`'s shape against `facts`.
fn check<N: Node>(node: N, facts: Facts) {
    let shape = node.shape();
    if let Some(n) = facts.inputs {
        assert_eq!(shape.audio_in().count(), n, "audio inputs");
    }
    if let Some(n) = facts.outputs {
        assert_eq!(shape.audio_out().count(), n, "audio outputs");
    }
    if let Some(cost) = facts.cost {
        assert_eq!(shape.cost, cost, "cost");
    }
    if let Some(latency) = facts.latency {
        assert_eq!(shape.latency, Latency::new(latency), "latency");
    }
    if let Some(tail) = facts.tail {
        assert_eq!(shape.tail, tail, "tail");
    }
    if facts.finite_tail {
        assert!(
            matches!(shape.tail, Tail::Finite(_)),
            "a finite tail, got {:?}",
            shape.tail
        );
    }
    if let Some(params) = &facts.params {
        assert_eq!(shape.params.as_slice(), &params[..], "declared params");
    }
    for (k, want) in facts.bases.iter().enumerate() {
        assert_eq!(node.param_base(k), *want, "base of param {k}");
    }
}

#[test]
fn each_node_declares_its_shape() {
    let rows: Vec<(&str, fn())> = vec![
        ("auto_pan", || {
            check(
                prepared(AutoPanNode::from(AutoPanConfig::new())),
                Facts::default()
                    .io(2, 2)
                    .cost(Cost::Moderate)
                    .tail(Tail::None)
                    .params(&[UnitParam::Rate, UnitParam::Depth])
                    .bases(&some(&[1.0, 1.0])),
            )
        }),
        ("bit_crusher (configured)", || {
            check(
                prepared(BitCrusherNode::<f32>::from(
                    BitCrusherConfig::new(3u16)
                        .bits(BitDepth::new(8.0))
                        .hold(HoldFactor::new(1.0)),
                )),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Light)
                    .tail(Tail::None)
                    .no_latency()
                    .params(&[UnitParam::BitDepth, UnitParam::HoldFactor]),
            )
        }),
        ("bit_crusher (default: 8 bits, hold 1)", || {
            check(
                BitCrusherNode::<f32>::from(BitCrusherConfig::new(1u16)),
                Facts::default().bases(&some(&[8.0, 1.0])),
            )
        }),
        ("dc_blocker (configured)", || {
            check(
                prepared(DcBlockerNode::<f32>::from(
                    DcBlockerConfig::new(3u16).corner(Hz::new(10.0)),
                )),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Light)
                    .params(&[UnitParam::Cutoff])
                    .finite_tail(),
            )
        }),
        ("dc_blocker (default: 10 Hz)", || {
            check(
                DcBlockerNode::<f32>::from(DcBlockerConfig::new(1u16)),
                Facts::default().bases(&some(&[10.0])),
            )
        }),
        ("tremolo (configured)", || {
            check(
                prepared(TremoloNode::from(
                    TremoloConfig::new(3u16)
                        .rate(Hz::new(5.0))
                        .depth(Bipolar::of(0.5)),
                )),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Moderate)
                    .tail(Tail::None)
                    .params(&[UnitParam::Rate, UnitParam::Depth]),
            )
        }),
        ("tremolo (default: 5 Hz, half depth)", || {
            check(
                TremoloNode::<f32>::from(TremoloConfig::new(1u16)),
                Facts::default().bases(&some(&[5.0, 0.5])),
            )
        }),
        ("spring_reverb (default: 2 s, 4 kHz, 1)", || {
            check(
                prepared(SpringReverbNode::<f32>::from(SpringReverbConfig::new(3u16))),
                Facts::default()
                    .io(3, 3)
                    .params(&[UnitParam::Decay, UnitParam::Cutoff, UnitParam::Diffusion])
                    .bases(&some(&[2.0, 4_000.0, 1.0])),
            )
        }),
        ("ring_mod (default: 440 Hz)", || {
            check(
                prepared(RingModNode::from(RingModConfig::new(3u16))),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Moderate)
                    .tail(Tail::None)
                    .params(&[UnitParam::Frequency])
                    .bases(&some(&[440.0])),
            )
        }),
        ("frequency_shifter", || {
            check(
                prepared(FrequencyShifterNode::<f32>::from(
                    FrequencyShifterConfig::new(3u16).shift(Hz::new(-12.5)),
                )),
                Facts::default()
                    .io(3, 3)
                    .no_latency()
                    .finite_tail()
                    .params(&[UnitParam::Frequency])
                    .bases(&some(&[-12.5])),
            )
        }),
        ("frequency_shifter (empty layout)", || {
            check(
                FrequencyShifterNode::<f32>::from(FrequencyShifterConfig::new(
                    ChannelLayout::EMPTY,
                )),
                Facts::default().inputs(0),
            )
        }),
        ("polarity", || {
            check(
                prepared(PolarityNode::try_from(PolarityConfig::new(3u16)).unwrap()),
                Facts::default().io(3, 3).cost(Cost::Light).tail(Tail::None),
            )
        }),
        ("mid_side", || {
            check(
                MidSideNode::from(MidSideConfig::encode()),
                Facts::default().io(2, 2).cost(Cost::Light),
            )
        }),
        ("oscillator", || {
            check(
                OscillatorNode::from(
                    OscillatorConfig::new(OscillatorWave::Sine)
                        .frequency(Hz::new(440.0))
                        .level(Amplitude::new(0.25)),
                ),
                Facts::default()
                    .io(0, 1)
                    .params(&[UnitParam::Frequency, UnitParam::Volume])
                    .bases(&some(&[440.0, 0.25])),
            )
        }),
        ("noise", || {
            check(
                NoiseNode::<f32>::from(
                    NoiseConfig::new(NoiseColour::White).level(Amplitude::new(0.25)),
                ),
                Facts::default()
                    .io(0, 1)
                    .params(&[UnitParam::Volume])
                    .bases(&some(&[0.25])),
            )
        }),
        ("additive", || {
            let partial = |ratio: f64, amplitude: f64| {
                Partial::new(
                    FrequencyRatio::new(ratio).unwrap(),
                    Amplitude::new(amplitude),
                )
            };
            let partials = Partials::try_from(
                &[partial(1.0, 0.5), partial(3.0, 0.25), partial(60.0, 0.9)][..],
            )
            .unwrap();
            check(
                prepared(AdditiveNode::<f32>::from(
                    AdditiveConfig::new(partials)
                        .frequency(Hz::new(330.0))
                        .level(Amplitude::new(0.25)),
                )),
                Facts::default()
                    .io(0, 1)
                    .tail(Tail::None)
                    .params(&[UnitParam::Frequency, UnitParam::Volume])
                    .bases(&some(&[330.0, 0.25])),
            )
        }),
        ("fm", || {
            let ratio = |x: f64| OperatorFrequency::Ratio(FrequencyRatio::new(x).unwrap());
            let route = Route {
                modulator: Operator::of(0),
                target: Operator::of(0),
            };
            let algorithm =
                Algorithm::try_from((OperatorCount::of(1), &[route][..], &[Operator::of(0)][..]))
                    .unwrap();
            let patch = FmPatch::new(algorithm).operators(&[OperatorSpec::new(ratio(1.0))]);
            check(
                prepared(FmNode::<f32>::from(
                    FmConfig::new(patch)
                        .frequency(Hz::new(220.0))
                        .level(Amplitude::new(0.5))
                        .index(ModulationIndex::of(2.0))
                        .feedback(ModulationIndex::of(0.75)),
                )),
                Facts::default()
                    .io(0, 1)
                    .tail(Tail::None)
                    .addrs(vec![
                        UnitAddr::from(UnitParam::Frequency),
                        UnitAddr::from(UnitParam::Volume),
                        UnitAddr::new(UnitParam::Index, 0),
                        UnitAddr::new(UnitParam::Index, 1),
                    ])
                    .bases(&some(&[220.0, 0.5, 2.0, 0.75])),
            )
        }),
        ("karplus_strong", || {
            check(
                prepared(KarplusStrongNode::<f32>::from(
                    KarplusStrongConfig::new(2u16)
                        .frequency(Hz::new(440.0))
                        .decay(Seconds::new(1.5)),
                )),
                Facts::default()
                    .io(2, 2)
                    .no_latency()
                    .tail(Tail::Unbounded)
                    .params(&[UnitParam::Frequency, UnitParam::Decay])
                    .bases(&some(&[440.0, 1.5])),
            )
        }),
        ("waveguide_string", || {
            check(
                prepared(WaveguideStringNode::<f32>::from(
                    WaveguideStringConfig::new(2u16)
                        .frequency(Hz::new(440.0))
                        .decay(Seconds::new(1.5)),
                )),
                Facts::default()
                    .io(2, 2)
                    .no_latency()
                    .tail(Tail::Unbounded)
                    .params(&[UnitParam::Frequency, UnitParam::Decay])
                    .bases(&some(&[440.0, 1.5])),
            )
        }),
        ("modal_bank (first mode)", || {
            let modes: Vec<Mode> = [(440.0, 0.2, 1.0), (1_130.0, 0.1, -0.5)]
                .iter()
                .map(|&(f, t, g)| Mode {
                    frequency: Hz::new(f),
                    decay: Seconds::new(t),
                    gain: Gain::new(g),
                })
                .collect();
            check(
                prepared(ModalBankNode::<f32>::from(ModalBankConfig::new(
                    2u16,
                    Modes::try_from(&modes[..]).unwrap(),
                ))),
                Facts::default()
                    .io(2, 2)
                    .no_latency()
                    .params(&[UnitParam::Frequency, UnitParam::Decay])
                    .bases(&some(&[440.0, 0.2])),
            )
        }),
        ("comb (feedforward)", || {
            let delay = CombDelay::of(Seconds::new(0.004));
            check(
                prepared_at(
                    CombFilterNode::<f32>::from(
                        CombFilterConfig::new(1u16, CombForm::Feedforward, delay)
                            .gain(Feedback::new(0.5)),
                    ),
                    SampleRate::of(1_000.0),
                ),
                Facts::default()
                    .params(&[UnitParam::Feedback])
                    .no_latency()
                    .tail(Tail::Finite(Samples::new(4)))
                    .bases(&some(&[0.5])),
            )
        }),
        ("comb (empty layout)", || {
            check(
                CombFilterNode::<f32>::from(CombFilterConfig::new(
                    ChannelLayout::EMPTY,
                    CombForm::Feedback,
                    CombDelay::of(Seconds::new(0.004)),
                )),
                Facts::default().inputs(0),
            )
        }),
        ("hysteresis (default: 12 dB)", || {
            check(
                prepared(HysteresisNode::<f32>::from(HysteresisConfig::new(3u16))),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Heavy)
                    .tail(Tail::Unbounded)
                    .params(&[UnitParam::GainDb])
                    .bases(&some(&[12.0])),
            )
        }),
        ("hysteresis (empty layout)", || {
            check(
                HysteresisNode::<f32>::from(HysteresisConfig::new(0u16)),
                Facts::default().inputs(0),
            )
        }),
        ("diode_clipper (default: 5 kHz, 12 dB)", || {
            check(
                DiodeClipperNode::<f32>::from(DiodeClipperConfig::new(3u16)),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Heavy)
                    .params(&[UnitParam::Cutoff, UnitParam::GainDb])
                    .bases(&some(&[5_000.0, 12.0])),
            )
        }),
        ("diode_clipper (empty layout)", || {
            check(
                DiodeClipperNode::<f32>::from(DiodeClipperConfig::new(0u16)),
                Facts::default().inputs(0),
            )
        }),
        ("tube_stage (default: 6 dB)", || {
            check(
                TubeStageNode::<f32>::from(TubeStageConfig::new(3u16)),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Heavy)
                    .params(&[UnitParam::GainDb])
                    .bases(&some(&[6.0])),
            )
        }),
        ("tube_stage (empty layout)", || {
            check(
                TubeStageNode::<f32>::from(TubeStageConfig::new(0u16)),
                Facts::default().inputs(0),
            )
        }),
        ("distortion", || {
            check(
                DistortionNode::<f32>::from(
                    DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh)
                        .drive(Drive::new(1.5)),
                ),
                Facts::default()
                    .io(2, 2)
                    .params(&[UnitParam::Drive])
                    .bases(&some(&[1.5])),
            )
        }),
        ("stereo_width", || {
            check(
                StereoWidthNode::<f32>::from(StereoWidthConfig::new().width(StereoWidth::new(1.5))),
                Facts::default()
                    .io(2, 2)
                    .cost(Cost::Light)
                    .params(&[UnitParam::StereoWidth])
                    .bases(&some(&[1.5])),
            )
        }),
        ("brickwall_limiter", || {
            check(
                prepared(BrickwallLimiterNode::<f32>::from(
                    BrickwallLimiterConfig::new(ChannelTopology::STEREO, Db::new(-1.0)),
                )),
                Facts::default()
                    .io(2, 2)
                    .no_latency()
                    .tail(Tail::None)
                    .params(&[UnitParam::Ceiling])
                    .bases(&some(&[-1.0])),
            )
        }),
        ("cut_filter", || {
            check(
                prepared(CutFilterNode::<f32>::from(
                    CutFilterConfig::new(
                        3u16,
                        CutResponse::HighPass,
                        CutAlignment::Butterworth(ButterworthOrder::of(8)),
                    )
                    .cutoff(Hz::new(120.0)),
                )),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Moderate)
                    .params(&[UnitParam::Cutoff])
                    .finite_tail()
                    .bases(&some(&[120.0])),
            )
        }),
        ("tilt_filter", || {
            check(
                prepared(TiltFilterNode::<f32>::from(TiltFilterConfig::new(
                    3u16,
                    Hz::new(120.0),
                    Db::new(9.0),
                ))),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Moderate)
                    .params(&[UnitParam::Cutoff, UnitParam::GainDb])
                    .finite_tail()
                    .bases(&some(&[120.0, 9.0])),
            )
        }),
        ("bell_filter", || {
            check(
                prepared(BellFilterNode::<f32>::from(BellFilterConfig::new(
                    3u16,
                    Hz::new(120.0),
                    Q::new(4.0),
                    Db::new(9.0),
                ))),
                Facts::default()
                    .io(3, 3)
                    .cost(Cost::Moderate)
                    .params(&[UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb])
                    .tail(Tail::Unbounded)
                    .bases(&some(&[120.0, 4.0, 9.0])),
            )
        }),
        ("ladder_filter (a resonant ladder rings on)", || {
            check(
                LadderFilterNode::<f32>::from(
                    LadderFilterConfig::new(6u16, LadderType::LP24)
                        .freq(Hz::new(1000.0))
                        .res(Resonance::new(0.3)),
                ),
                Facts::default()
                    .io(6, 6)
                    .params(&[UnitParam::Cutoff, UnitParam::Q, UnitParam::Drive])
                    .bases(&some(&[1000.0, 0.3, 1.0]))
                    .tail(Tail::Unknown),
            )
        }),
        ("svf_filter (a resonant filter rings on)", || {
            check(
                SvfFilterNode::<f32>::from(
                    SvfFilterConfig::new(6u16, SvfType::LowPass)
                        .freq(Hz::new(1000.0))
                        .q(Q::new(0.7)),
                ),
                Facts::default()
                    .io(6, 6)
                    .params(&[UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb])
                    .bases(&some(&[1000.0, 0.7, 0.0]))
                    .tail(Tail::Unknown),
            )
        }),
        ("lfo (free-running)", || {
            check(
                LfoNode::from(
                    LfoConfig::new(LfoShape::Sine)
                        .rate(Hz::new(2.0))
                        .depth(Bipolar::of(0.25)),
                ),
                Facts::default()
                    .params(&[UnitParam::Depth, UnitParam::Rate])
                    .bases(&some(&[0.25, 2.0])),
            )
        }),
        ("lfo (beat-synced: the rate is kept)", || {
            check(
                LfoNode::from(
                    LfoConfig::new(LfoShape::Sine)
                        .beat_sync(BeatsPerCycle::of(4.0))
                        .depth(Bipolar::of(0.5)),
                ),
                Facts::default()
                    .params(&[UnitParam::Depth, UnitParam::Rate])
                    .bases(&some(&[0.5, 1.0])),
            )
        }),
    ];
    for_each_row(rows, |row| row());
}

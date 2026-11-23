//! A fork of each param node starts from the values last set and shares no cell with the
//! live node: [`assert_param_fork`] over a node of every kind.

use crate::test_support::for_each_row;
use crate::*;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_dsp::CombDelay;
use tutti_dsp::{
    Algorithm, FmPatch, OperatorCount, OperatorFrequency, OperatorSpec, Partial, Partials,
};
use tutti_graph::ParamNode;
use tutti_graph::contract::assert_param_fork;
use tutti_units::{FrequencyRatio, Gain, Metres, Seconds, UnitAddr, UnitParam};

/// The nodes whose fork has nothing beyond the shared property to pin.
#[test]
fn a_fork_starts_from_the_values_set_and_shares_no_cell() {
    let rows: Vec<(&str, fn())> = vec![
        ("auto_pan", || {
            assert_param_fork(AutoPanNode::from(AutoPanConfig::new()))
        }),
        ("bit_crusher", || {
            assert_param_fork(BitCrusherNode::<f32>::from(BitCrusherConfig::new(2u16)))
        }),
        ("dc_blocker", || {
            assert_param_fork(DcBlockerNode::<f32>::from(DcBlockerConfig::new(2u16)))
        }),
        ("ring_mod", || {
            assert_param_fork(RingModNode::from(RingModConfig::new(2u16)))
        }),
        ("tremolo", || {
            assert_param_fork(TremoloNode::from(TremoloConfig::new(2u16)))
        }),
        ("spring_reverb", || {
            assert_param_fork(SpringReverbNode::<f32>::from(SpringReverbConfig::new(2u16)))
        }),
        ("comb", || {
            assert_param_fork(CombFilterNode::<f32>::from(
                CombFilterConfig::new(1u16, CombForm::Feedback, CombDelay::of(Seconds::new(0.004)))
                    .gain(Feedback::new(0.5)),
            ))
        }),
        ("frequency_shifter", || {
            assert_param_fork(FrequencyShifterNode::from(
                FrequencyShifterConfig::new(2u16).shift(Hz::new(-100.0)),
            ))
        }),
        ("oscillator", || {
            assert_param_fork(OscillatorNode::from(
                OscillatorConfig::new(OscillatorWave::Saw).frequency(Hz::new(220.0)),
            ))
        }),
        ("stereo_width", || {
            assert_param_fork(StereoWidthNode::<f32>::from(
                StereoWidthConfig::new().width(StereoWidth::new(0.5)),
            ))
        }),
        ("hysteresis", || {
            assert_param_fork(HysteresisNode::<f32>::from(HysteresisConfig::new(
                ChannelLayout::STEREO,
            )))
        }),
        ("diode_clipper", || {
            assert_param_fork(DiodeClipperNode::<f32>::from(DiodeClipperConfig::new(
                ChannelLayout::STEREO,
            )))
        }),
        ("tube_stage", || {
            assert_param_fork(TubeStageNode::<f32>::from(TubeStageConfig::new(
                ChannelLayout::STEREO,
            )))
        }),
        ("distortion", || {
            assert_param_fork(DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh)
                    .drive(Drive::new(2.0)),
            ))
        }),
        ("karplus_strong", || {
            assert_param_fork(KarplusStrongNode::<f32>::from(
                KarplusStrongConfig::new(2u16)
                    .frequency(Hz::new(440.0))
                    .decay(Seconds::new(1.0)),
            ))
        }),
        ("waveguide_string", || {
            assert_param_fork(WaveguideStringNode::<f32>::from(
                WaveguideStringConfig::new(2u16)
                    .frequency(Hz::new(440.0))
                    .decay(Seconds::new(1.0)),
            ))
        }),
        ("strip", || {
            assert_param_fork(BusStripNode::from(BusStripConfig::new(
                ChannelTopology::STEREO,
            )))
        }),
        ("gate", || {
            assert_param_fork(GateNode::<f32>::from(
                GateConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(GateControls {
                        threshold: Db::new(-30.0),
                        attack: Seconds::new(0.001),
                        hold: Seconds::new(0.01),
                        release: Seconds::new(0.1),
                        range: Db::new(-20.0),
                        ..GateControls::DEFAULT
                    }),
            ))
        }),
        ("compressor", || {
            assert_param_fork(CompressorNode::<f32>::from(
                CompressorConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(CompressorControls {
                        threshold: Db::new(-20.0),
                        ratio: CompressionRatio::new(4.0),
                        attack: Seconds::new(0.001),
                        release: Seconds::new(0.1),
                        knee: Db::new(6.0),
                        makeup: Db::new(3.0),
                        ..CompressorControls::DEFAULT
                    }),
            ))
        }),
        ("multiband_compressor", || {
            assert_param_fork(MultibandCompressorNode::<f32>::from(
                MultibandCompressorConfig::new(ChannelTopology::STEREO, MultibandBands::of(4)),
            ))
        }),
        ("pitch_shifter", || {
            assert_param_fork(PitchShifterNode::from(
                PitchShifterConfig::new(2u16).pitch(Cents::new(300.0)),
            ))
        }),
        ("distance", || {
            assert_param_fork(DistanceNode::from(
                DistanceConfig::new(2u16, Metres::of(50.0)).distance(Metres::of(8.0)),
            ))
        }),
        ("eq_band", || {
            assert_param_fork(EqBandNode::<f32>::from(
                EqBandConfig::new(SvfType::Bell)
                    .freq(Hz::new(1000.0))
                    .q(Q::new(1.0))
                    .gain(Db::new(6.0)),
            ))
        }),
        ("ladder", || {
            assert_param_fork(LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                    .freq(Hz::new(800.0))
                    .res(Resonance::new(0.3)),
            ))
        }),
        ("cut", || {
            assert_param_fork(CutFilterNode::from(CutFilterConfig::new(
                2u16,
                CutResponse::LowPass,
                CutAlignment::Butterworth(ButterworthOrder::of(4)),
            )))
        }),
        ("bell", || {
            assert_param_fork(BellFilterNode::from(BellFilterConfig::new(
                2u16,
                Hz::new(300.0),
                Q::new(2.0),
                Db::new(-6.0),
            )))
        }),
        ("tilt", || {
            assert_param_fork(TiltFilterNode::from(TiltFilterConfig::new(
                2u16,
                Hz::new(300.0),
                Db::new(-6.0),
            )))
        }),
        ("crossover", || {
            assert_param_fork(
                CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                    ChannelTopology::MONO,
                    CrossoverControls::try_from(
                        &[Hz::new(500.0), Hz::new(900.0), Hz::new(3_000.0)][..],
                    )
                    .unwrap(),
                ))
                .unwrap(),
            )
        }),
        ("modal_bank", || {
            let modes: Vec<Mode> = [(440.0, 0.2, 1.0), (1_130.0, 0.1, -0.5)]
                .iter()
                .map(|&(f, t, g)| Mode {
                    frequency: Hz::new(f),
                    decay: Seconds::new(t),
                    gain: Gain::new(g),
                })
                .collect();
            assert_param_fork(ModalBankNode::<f32>::from(ModalBankConfig::new(
                2u16,
                Modes::try_from(&modes[..]).unwrap(),
            )))
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
            assert_param_fork(AdditiveNode::<f32>::from(AdditiveConfig::new(partials)))
        }),
        ("fm", || {
            let ratio = |x: f64| OperatorFrequency::Ratio(FrequencyRatio::new(x).unwrap());
            let patch = FmPatch::new(Algorithm::chain(OperatorCount::of(2))).operators(&[
                OperatorSpec::new(ratio(1.0)),
                OperatorSpec::new(ratio(10.0)),
            ]);
            assert_param_fork(FmNode::<f32>::from(FmConfig::new(patch)))
        }),
        ("graphic_eq", || {
            let node = GraphicEqNode::<f32>::from(GraphicEqConfig::new(
                ChannelTopology::default_for(ChannelLayout::MONO).expect("mono is a bus"),
                GraphicBands::Octave,
            ));
            assert!(
                node.param_set()
                    .set(UnitAddr::new(UnitParam::GainDb, 2), 6.0)
            );
            assert_param_fork(node)
        }),
        ("noise", || {
            assert_param_fork(NoiseNode::<f32>::from(NoiseConfig::new(NoiseColour::Pink)))
        }),
    ];
    for_each_row(rows, |check| check());
}

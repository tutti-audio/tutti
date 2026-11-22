//! A node of a graph of `f64` runs in `f64` and narrows nowhere: on a signal of 29 bits, which
//! an `f32` cannot carry, a node that is the identity, a delay or a scaling by a known factor
//! returns that figure to `2^-45`, and one that is linear satisfies superposition,
//! `h(a x + b y) = a h(x) + b h(y)`, to `2^-40` of its peak, which an `f32` anywhere in the
//! stream or the state breaks at about `2^-24`.

use crate::test_support::{F64_TOLERANCE, assert_linear, render_f64};
use crate::{
    AutoPanConfig, AutoPanNode, BellFilterConfig, BellFilterNode, BitCrusherConfig, BitCrusherNode,
    BrickwallLimiterConfig, BrickwallLimiterNode, ButterworthOrder, CompressorConfig,
    CompressorControls, CompressorNode, CrossoverConfig, CrossoverControls, CrossoverNode,
    CutAlignment, CutFilterConfig, CutFilterNode, CutResponse, DcBlockerConfig, DcBlockerNode,
    DecorrelatorConfig, DecorrelatorNode, DelayConfig, DelayNode, DistortionConfig, DistortionNode,
    DynamicEqConfig, DynamicEqNode, EarlyReflectionsConfig, EarlyReflectionsNode,
    EnvelopeFilterConfig, EnvelopeFilterNode, FdnConfig, FdnNode, FreeverbConfig, FreeverbNode,
    GateConfig, GateControls, GateNode, LadderFilterConfig, LadderFilterNode, LimiterConfig,
    LimiterControls, LimiterNode, MaxDelay, ModDelayConfig, ModDelayNode, PhaserConfig,
    PhaserControls, PhaserNode, PlateReverbConfig, PlateReverbNode, RingModConfig, RingModNode,
    SpringReverbConfig, SpringReverbNode, StereoWidthConfig, StereoWidthNode, SvfFilterConfig,
    SvfFilterNode, TiltFilterConfig, TiltFilterNode, TransientShaperConfig, TransientShaperNode,
    TremoloConfig, TremoloNode,
};
use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_dsp::{
    DelayControls, DelayLayout, FdnLayout, FreeverbControls, FreezeMode, LadderType, ShapeKind,
    Stages, SvfType,
};
use tutti_dsp::{Inline, Tap, TapSet};
use tutti_fixtures::signal::{assert_close_f64, wide_signal};
use tutti_graph::IntoNode;
use tutti_graph::contract::Direct;
use tutti_units::{
    BitDepth, CompressionRatio, Db, Depth, Drive, Feedback, Hz, Mix, Q, Resonance, SampleRate,
    Seconds, Semitones,
};
use tutti_units::{Gain, SampleSpan};

const RATE: SampleRate = SampleRate::SR_48K;

/// `node` alone in an `f64` graph at `rate`: the whole of `inputs` (one `Vec` per input port,
/// all one length) as one block, and what each output port holds.
fn render<N: IntoNode<f64>>(node: N, rate: SampleRate, inputs: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let refs: Vec<&[f64]> = inputs.iter().map(Vec::as_slice).collect();
    render_f64(node, rate, &refs, inputs[0].len())
}

/// `channels` copies of `x`.
fn on(channels: usize, x: &[f64]) -> Vec<Vec<f64>> {
    vec![x.to_vec(); channels]
}

/// A 16-frame delay of a 29-bit signal is that signal 16 frames on. A 1.024 kHz graph makes
/// 16 frames `2^-6` s, a time an `f32` holds exactly.
#[test]
fn the_delay_returns_a_wide_signal_sixteen_frames_on() {
    const RATE: SampleRate = SampleRate::of(1_024.0);
    let x = wide_signal(256);
    let node = DelayNode::<f64>::from(
        DelayConfig::new(DelayLayout::new(
            BusTopology::MONO,
            MaxDelay::of(Seconds::new(0.5)),
        ))
        .controls(DelayControls {
            time: Seconds::new(16.0 / RATE.get()),
            ..DelayControls::DEFAULT
        }),
    );
    let want: Vec<f64> = (0..256usize)
        .map(|n| n.checked_sub(16).map_or(0.0, |k| x[k]))
        .collect();
    assert_close_f64(&render(node, RATE, &on(1, &x))[0], &want, F64_TOLERANCE);
}

/// A limiter under its threshold is its input delayed by the lookahead, its gain being
/// `10^0 = 1`.
#[test]
fn the_limiter_under_its_threshold_returns_a_wide_signal_a_lookahead_late() {
    let x = wide_signal(4_800);
    let node = LimiterNode::<f64>::from(LimiterConfig::new(BusTopology::MONO).controls(
        LimiterControls {
            threshold: Db::new(0.0),
            ceiling: Db::new(0.0),
            ..LimiterControls::DEFAULT
        },
    ));
    let mut d = Direct::<_, f64>::with_format(node, RATE, x.len());
    let latency = tutti_graph::Node::<f64>::shape(&*d.node)
        .latency
        .samples()
        .get();
    d.inputs_mut()
        .plane_mut(0)
        .samples_mut()
        .copy_from_slice(&x);
    d.block();
    let want: Vec<f64> = (0..x.len())
        .map(|n| n.checked_sub(latency).map_or(0.0, |k| x[k]))
        .collect();
    assert_close_f64(d.output().plane(0).samples(), &want, F64_TOLERANCE);
}

/// A compressor with its knee wholly above the level, an open gate, a brickwall ceiling above
/// the peak, a transient shaper at zero amounts and a dynamic EQ band with no range return a
/// 29-bit signal as it is.
#[test]
fn the_dynamics_that_do_nothing_return_a_wide_signal_as_it_is() {
    let x = wide_signal(4_800);
    let input = on(1, &x);
    let compressor = CompressorNode::<f64>::from(
        CompressorConfig::new(BusTopology::MONO).controls(CompressorControls {
            threshold: Db::new(0.0),
            knee: Db::new(6.0),
            ratio: CompressionRatio::new(4.0),
            attack: Seconds::new(0.002),
            release: Seconds::new(0.05),
            makeup: Db::new(0.0),
            ..CompressorControls::DEFAULT
        }),
    );
    assert_close_f64(&render(compressor, RATE, &input)[0], &x, F64_TOLERANCE);
    let gate = GateNode::<f64>::from(GateConfig::new(BusTopology::MONO).controls(GateControls {
        threshold: Db::new(-40.0),
        attack: Seconds::new(0.0),
        hold: Seconds::new(0.0),
        release: Seconds::new(0.0),
        range: Db::new(-80.0),
        ..GateControls::DEFAULT
    }));
    assert_close_f64(&render(gate, RATE, &input)[0], &x, F64_TOLERANCE);
    let brickwall = BrickwallLimiterNode::<f64>::from(BrickwallLimiterConfig::new(
        BusTopology::STEREO,
        Db::new(0.0),
    ));
    assert_close_f64(&render(brickwall, RATE, &input)[0], &x, F64_TOLERANCE);
    let shaper = TransientShaperNode::<f64>::from(TransientShaperConfig::new(ChannelLayout::MONO));
    assert_close_f64(&render(shaper, RATE, &input)[0], &x, F64_TOLERANCE);
    let band = DynamicEqNode::<f64>::from(
        DynamicEqConfig::new(ChannelLayout::MONO, Hz::new(1_000.0), Q::new(1.0))
            .range(Db::new(0.0)),
    );
    assert_close_f64(&render(band, RATE, &input)[0], &x, F64_TOLERANCE);
}

/// A crusher at 32 bits and no hold passes a 29-bit signal exactly.
#[test]
fn the_bit_crusher_at_thirty_two_bits_passes_a_wide_signal_exactly() {
    let x = wide_signal(300);
    let node = BitCrusherNode::<f64>::from(
        BitCrusherConfig::new(ChannelLayout::MONO).bits(BitDepth::new(32.0)),
    );
    assert_close_f64(&render(node, RATE, &on(1, &x))[0], &x, F64_TOLERANCE);
}

/// The soft-sign curve is `h x / (1 + |h x|)` at the drive `h`.
#[test]
fn the_distortion_is_the_softsign_curve_of_a_wide_signal() {
    let x = wide_signal(300);
    let h = 1.7;
    let want: Vec<f64> = x.iter().map(|&x| h * x / (1.0 + (h * x).abs())).collect();
    let node = DistortionNode::<f64>::from(
        DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Softsign).drive(Drive::new(1.7)),
    );
    assert_close_f64(&render(node, RATE, &on(1, &x))[0], &want, F64_TOLERANCE);
}

/// A tremolo at depth 0 has the gain 1 at every frame, so it returns its input.
#[test]
fn the_tremolo_at_depth_zero_returns_a_wide_signal() {
    let x = wide_signal(300);
    let node =
        TremoloNode::<f64>::from(TremoloConfig::new(ChannelLayout::MONO).depth(Depth::of(0.0)));
    assert_close_f64(&render(node, RATE, &on(1, &x))[0], &x, F64_TOLERANCE);
}

/// A ring modulator at a quarter of the rate multiplies by the carrier `0, 1, 0, -1, ...`.
#[test]
fn the_ring_modulator_at_a_quarter_of_the_rate_multiplies_by_zero_one_zero_minus_one() {
    let x = wide_signal(64);
    let node = RingModNode::<f64>::from(
        RingModConfig::new(ChannelLayout::MONO).frequency(Hz::new(RATE.get() / 4.0)),
    );
    let out = render(node, RATE, &on(1, &x)).remove(0);
    let want: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(n, &x)| x * [0.0, 1.0, 0.0, -1.0][n % 4])
        .collect();
    assert_close_f64(&out, &want, F64_TOLERANCE);
}

/// A auto-pan at a quarter of the rate and full depth sits at the centre, hard right, the
/// centre and hard left in turn: at the hard positions one side carries the input and the
/// other nothing.
#[test]
fn the_auto_pan_at_a_quarter_of_the_rate_is_hard_right_then_hard_left() {
    let x = wide_signal(64);
    let node = AutoPanNode::<f64>::from(AutoPanConfig::new().rate(Hz::new(RATE.get() / 4.0)));
    let out = render(node, RATE, &[x.clone(), x.clone()]);
    for n in (1..64).step_by(2) {
        let (kept, dropped) = if n % 4 == 1 {
            (&out[1], &out[0])
        } else {
            (&out[0], &out[1])
        };
        assert!((kept[n] - x[n]).abs() <= F64_TOLERANCE, "frame {n} kept");
        assert!(dropped[n].abs() <= F64_TOLERANCE, "frame {n} dropped");
    }
}

/// A unity stereo width is the input pair.
#[test]
fn the_natural_stereo_width_returns_a_wide_pair() {
    let x = wide_signal(300);
    let y: Vec<f64> = x.iter().rev().map(|v| v - 0.3).collect();
    let node = StereoWidthNode::<f64>::from(StereoWidthConfig::new());
    let out = render(node, RATE, &[x.clone(), y.clone()]);
    assert_close_f64(&out[0], &x, F64_TOLERANCE);
    assert_close_f64(&out[1], &y, F64_TOLERANCE);
}

/// A constant of 29 bits through an all-pass or a low-pass of unity DC gain, once settled,
/// is that constant: the figure holds to `2^-45` only if the whole recursion is `f64`.
#[test]
fn the_filters_pass_a_wide_constant_at_unity_gain() {
    const DC: f64 = 0x2000_0001 as f64 / 2_147_483_648.0;
    const FRAMES: usize = 16_384;
    let held = vec![DC; FRAMES];
    let input = on(1, &held);
    let settled = |out: Vec<Vec<f64>>| out[0][FRAMES - 1];
    let svf = SvfFilterNode::<f64>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.707)),
    );
    assert!(
        (settled(render(svf, RATE, &input)) - DC).abs() <= F64_TOLERANCE,
        "svf"
    );
    let cut = CutFilterNode::<f64>::from(CutFilterConfig::new(
        ChannelLayout::MONO,
        CutResponse::LowPass,
        CutAlignment::Butterworth(ButterworthOrder::of(2)),
    ));
    assert!(
        (settled(render(cut, RATE, &input)) - DC).abs() <= F64_TOLERANCE,
        "cut"
    );
    let bell = BellFilterNode::<f64>::from(BellFilterConfig::new(
        ChannelLayout::MONO,
        Hz::new(2_000.0),
        Q::new(2.0),
        Db::new(6.0),
    ));
    assert!(
        (settled(render(bell, RATE, &input)) - DC).abs() <= F64_TOLERANCE,
        "bell"
    );
    let tilt = TiltFilterNode::<f64>::from(TiltFilterConfig::new(
        ChannelLayout::MONO,
        Hz::new(1_000.0),
        Db::new(0.0),
    ));
    assert!(
        (settled(render(tilt, RATE, &input)) - DC).abs() <= F64_TOLERANCE,
        "tilt"
    );
    let envelope = EnvelopeFilterNode::<f64>::from(
        EnvelopeFilterConfig::new(ChannelLayout::MONO, Hz::new(1_000.0), SvfType::LowPass)
            .depth(Semitones::new(0.0)),
    );
    assert!(
        (settled(render(envelope, RATE, &input)) - DC).abs() <= F64_TOLERANCE,
        "envelope"
    );
    let phaser = PhaserNode::<f64>::try_from(
        PhaserConfig::new(BusTopology::MONO, Stages::of(4)).controls(PhaserControls {
            feedback: Feedback::new(0.0),
            ..PhaserControls::DEFAULT
        }),
    )
    .unwrap();
    assert!(
        (settled(render(phaser, RATE, &input)) - DC).abs() <= F64_TOLERANCE,
        "phaser"
    );
}

/// The two bands of a split sum to a 29-bit constant once settled.
#[test]
fn the_crossover_bands_sum_to_a_wide_constant() {
    const DC: f64 = 0x2000_0001 as f64 / 2_147_483_648.0;
    const FRAMES: usize = 16_384;
    let node = CrossoverNode::<f64>::try_from(CrossoverConfig::new(
        BusTopology::MONO,
        CrossoverControls::try_from(&[Hz::new(1_000.0)][..]).unwrap(),
    ))
    .unwrap();
    let out = render(node, RATE, &on(1, &vec![DC; FRAMES]));
    let sum = out[0][FRAMES - 1] + out[1][FRAMES - 1];
    assert!((sum - DC).abs() <= F64_TOLERANCE, "{sum} against {DC}");
}

/// A ladder with no resonance is four unity-gain stages, each behind a `tanh`: a constant `c`
/// settles to `tanh` applied four times at unit drive.
#[test]
fn the_ladder_settles_a_wide_constant_to_its_saturation() {
    const DC: f64 = 0x2000_0001 as f64 / 2_147_483_648.0;
    const FRAMES: usize = 4_096;
    let node = LadderFilterNode::<f64>::from(
        LadderFilterConfig::new(ChannelLayout::MONO, LadderType::LP24)
            .freq(Hz::new(5_000.0))
            .res(Resonance::new(0.0)),
    );
    let out = render(node, RATE, &on(1, &vec![DC; FRAMES]));
    let settled = DC.tanh().tanh().tanh().tanh();
    assert!(
        (out[0][FRAMES - 1] - settled).abs() <= F64_TOLERANCE,
        "{} against {settled}",
        out[0][FRAMES - 1]
    );
}

/// The linear nodes: lines, modulated lines, the reverbs, the filters and the modulators.
#[test]
fn the_linear_nodes_satisfy_superposition() {
    assert_linear(
        || {
            DelayNode::<f64>::from(
                DelayConfig::new(DelayLayout::new(
                    BusTopology::MONO,
                    MaxDelay::of(Seconds::new(0.05)),
                ))
                .controls(DelayControls {
                    time: Seconds::new(0.01),
                    feedback: Feedback::new(0.6),
                    ..DelayControls::DEFAULT
                }),
            )
        },
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || ModDelayNode::<f64>::from(ModDelayConfig::chorus(tutti_buffers::BusTopology::MONO)),
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || {
            PhaserNode::<f64>::try_from(PhaserConfig::new(BusTopology::MONO, Stages::of(4)))
                .unwrap()
        },
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || {
            FreeverbNode::<f64>::from(FreeverbConfig::new().controls(FreeverbControls {
                room: Mix::new(0.7),
                damp: Mix::new(0.3),
            }))
        },
        SampleRate::SR_44K1,
        1,
        6_000,
    );
    assert_linear(
        || PlateReverbNode::<f64>::from(PlateReverbConfig::new()),
        SampleRate::SR_44K1,
        1,
        6_000,
    );
    assert_linear(
        || SpringReverbNode::<f64>::from(SpringReverbConfig::new(1u16)),
        SampleRate::SR_44K1,
        1,
        6_000,
    );
    assert_linear(
        || {
            FdnNode::<f64>::from(FdnConfig::new().layout(
                FdnLayout::new(8, tutti_dsp::MaxDecay::DEFAULT, FreezeMode::Decaying).unwrap(),
            ))
        },
        SampleRate::SR_44K1,
        1,
        6_000,
    );
    assert_linear(
        || {
            EarlyReflectionsNode::<f64>::try_from(EarlyReflectionsConfig::new(
                ChannelLayout::MONO,
                TapSet {
                    taps: Inline::from_slice(&[
                        Tap {
                            delay: SampleSpan::new(37.5).unwrap(),
                            gain: Gain::new(0.7),
                            channel: 0,
                        },
                        Tap {
                            delay: SampleSpan::new(410.0).unwrap(),
                            gain: Gain::new(-0.3),
                            channel: 1,
                        },
                    ])
                    .unwrap(),
                },
            ))
            .unwrap()
        },
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || DecorrelatorNode::<f64>::from(DecorrelatorConfig::new(ChannelLayout::STEREO)),
        RATE,
        2,
        2_400,
    );
    assert_linear(
        || DcBlockerNode::<f64>::from(DcBlockerConfig::new(ChannelLayout::MONO)),
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || {
            SvfFilterNode::<f64>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::BandPass)
                    .freq(Hz::new(1_000.0))
                    .q(Q::new(2.0)),
            )
        },
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || {
            CrossoverNode::<f64>::try_from(CrossoverConfig::new(
                BusTopology::STEREO,
                CrossoverControls::try_from(&[Hz::new(1_000.0)][..]).unwrap(),
            ))
            .unwrap()
        },
        RATE,
        2,
        2_400,
    );
}

/// The modulators of the level, which are linear in the input.
#[test]
fn the_modulators_satisfy_superposition() {
    assert_linear(
        || TremoloNode::<f64>::from(TremoloConfig::new(ChannelLayout::MONO)),
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || RingModNode::<f64>::from(RingModConfig::new(ChannelLayout::MONO)),
        RATE,
        1,
        2_400,
    );
    assert_linear(
        || AutoPanNode::<f64>::from(AutoPanConfig::new()),
        RATE,
        2,
        2_400,
    );
}

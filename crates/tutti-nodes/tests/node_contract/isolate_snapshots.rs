//! Every forkable node in this crate forks to a **snapshot** of its controls:
//! no live control move reaches the fork. One row per node, one control per
//! cell the node reads; `tutti_graph::contract::IsolateRow` runs each control
//! through fork → render → move live → render again (must be unchanged) → fork
//! again → render (must differ, so the control is audible and the row can
//! fail). A node's fork is `fork_fresh` (`tutti_graph::param_parts`), so every
//! cell a control writes is checked — addressed by its `ParamSet` or not (a
//! gate's hold) — with the fork taken as the graph takes
//! it.
//!
//! Units with no live cell are not rows — there is nothing to move:
//! `AutomationLaneNode` (its `Arc<dyn Curve>`
//! is read-only, or frozen by its fork when it is not — its own unit test
//! `a_fork_freezes_a_live_curve`) and the `testing` stimulus nodes.

use tutti_buffers::Mono;
use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_graph::ParamNode;
use tutti_graph::contract::IsolateRow;
use tutti_nodes::{
    AutoPanConfig, AutoPanNode, BrickwallLimiterNode, BusStripNode, ChannelTimes,
    CompressorControls, CompressorNode, DelayControls, DelayLayout, DelayNode, DistortionNode,
    DynamicEqConfig, DynamicEqNode, EqBandNode, GateControls, GateNode, InterpolationMode,
    KeySource, LadderFilterNode, LadderType, LfoConfig, LfoNode, LfoShape, LimiterControls,
    LimiterNode, MaxDelay, ModDelayNode, PhaserControls, PhaserNode, RingModConfig, RingModNode,
    ShapeKind, Stages, SvfControls, SvfFilterNode, SvfType, TremoloConfig, TremoloNode,
};
use tutti_nodes::{
    BellFilterConfig, BellFilterNode, BitCrusherConfig, BitCrusherNode, ButterworthOrder,
    CutAlignment, CutFilterConfig, CutFilterNode, CutResponse, DcBlockerConfig, DcBlockerNode,
    EnvelopeFilterConfig, EnvelopeFilterNode, LevellerConfig, LevellerNode, TransientShaperConfig,
    TransientShaperNode,
};
use tutti_nodes::{
    PlateReverbConfig, PlateReverbNode, StereoWidthConfig, StereoWidthNode, TiltFilterConfig,
    TiltFilterNode,
};
use tutti_units::{
    Amplitude, CompressionRatio, Db, Depth, Drive, Feedback, Hz, Q, Resonance, Samples, Seconds,
    Semitones, StereoWidth, UnitAddr, UnitParam,
};

#[test]
fn compressor() {
    IsolateRow::new("CompressorNode (stereo)", || {
        CompressorNode::<f32>::new(
            BusTopology::STEREO,
            KeySource::Sidechain,
            CompressorControls {
                threshold: Db::new(-20.0),
                knee: Db::new(3.0),
                ratio: CompressionRatio::new(4.0),
                attack: Seconds::new(0.005),
                release: Seconds::new(0.05),
                makeup: Db::new(2.0),
                ..CompressorControls::DEFAULT
            },
        )
    })
    .control("threshold", |c| {
        assert!(c.param_set().set(UnitParam::Threshold, -30.0));
    })
    .control("ratio", |c| {
        assert!(c.param_set().set(UnitParam::Ratio, 10.0));
    })
    .control("attack", |c| {
        assert!(c.param_set().set(UnitParam::Attack, 0.02));
    })
    .control("release", |c| {
        assert!(c.param_set().set(UnitParam::Release, 0.2));
    })
    .control("makeup", |c| {
        assert!(c.param_set().set(UnitParam::GainDb, 6.0));
    })
    .control("knee", |c| {
        assert!(c.param_set().set(UnitParam::Knee, 9.0));
    })
    .check();
}

#[test]
fn gate() {
    IsolateRow::new("GateNode (stereo)", || {
        GateNode::<f32>::new(
            BusTopology::STEREO,
            KeySource::Sidechain,
            GateControls {
                threshold: Db::new(-15.0),
                attack: Seconds::new(0.001),
                hold: Seconds::new(0.005),
                release: Seconds::new(0.02),
                range: Db::new(-40.0),
                ..GateControls::DEFAULT
            },
        )
    })
    .control("threshold", |g| {
        assert!(g.param_set().set(UnitParam::Threshold, -35.0));
    })
    .control("attack", |g| {
        assert!(g.param_set().set(UnitParam::Attack, 0.01));
    })
    .control("hold", |g| {
        assert!(g.param_set().set(UnitParam::Hold, 0.05));
    })
    .control("release", |g| {
        assert!(g.param_set().set(UnitParam::Release, 0.1));
    })
    .control("range", |g| {
        assert!(g.param_set().set(UnitParam::Range, -10.0));
    })
    .check();
}

#[test]
fn limiter() {
    IsolateRow::new("LimiterNode (stereo)", || {
        LimiterNode::<f32>::stereo(LimiterControls {
            threshold: Db::new(-6.0),
            ceiling: Db::new(-3.0),
            ..LimiterControls::DEFAULT
        })
    })
    .control("threshold", |l| {
        assert!(l.param_set().set(UnitParam::Threshold, -12.0));
    })
    .control("ceiling", |l| {
        assert!(l.param_set().set(UnitParam::Ceiling, -9.0));
    })
    .control("release", |l| {
        assert!(l.param_set().set(UnitParam::Release, 0.3));
    })
    .check();
}

#[test]
fn brickwall_limiter() {
    IsolateRow::new("BrickwallLimiterNode (stereo)", || {
        BrickwallLimiterNode::<f32>::new(Db::new(-3.0))
    })
    .control("ceiling", |l| {
        assert!(l.param_set().set(UnitParam::Ceiling, -9.0));
    })
    .check();
}

#[test]
fn delay_line() {
    IsolateRow::new("DelayNode (stereo, ping-pong)", || {
        let layout = DelayLayout::new(
            BusTopology::STEREO,
            MaxDelay::of(Seconds::new(1.0)),
            InterpolationMode::Linear,
        )
        .with_times(ChannelTimes::each(&[Seconds::new(0.07)]).unwrap())
        .unwrap();
        DelayNode::<f32>::from_layout(
            layout,
            DelayControls {
                time: Seconds::new(0.05),
                feedback: Feedback::new(0.5_f64),
                cross_feedback: Feedback::new(0.3_f64),
            },
        )
    })
    .control("delay time", |d| {
        assert!(d.param_set().set(UnitParam::DelayTime, 0.08));
    })
    .control("feedback", |d| {
        assert!(d.param_set().set(UnitParam::Feedback, 0.1));
    })
    .control("cross feedback", |d| {
        assert!(d.param_set().set(UnitParam::CrossFeedback, 0.0));
    })
    .check();
}

#[test]
fn distortion() {
    IsolateRow::new("DistortionNode (stereo)", || {
        DistortionNode::<f32>::new(ShapeKind::Tanh, Drive::new(2.0))
    })
    .control("drive", |d| {
        assert!(d.param_set().set(UnitParam::Drive, 8.0))
    })
    .check();
}

#[test]
fn bit_crusher() {
    IsolateRow::new("BitCrusherNode (stereo)", || {
        BitCrusherNode::<f32>::from(BitCrusherConfig::new(ChannelLayout::STEREO))
    })
    .control("bits", |b| {
        assert!(b.param_set().set(UnitParam::BitDepth, 3.0))
    })
    .control("hold", |b| {
        assert!(b.param_set().set(UnitParam::HoldFactor, 5.0))
    })
    .check();
}

#[test]
fn stereo_width() {
    IsolateRow::new("StereoWidthNode", || {
        StereoWidthNode::<f32>::from(StereoWidthConfig::new().width(StereoWidth::new(1.5)))
    })
    .control("width", |w| {
        assert!(w.param_set().set(UnitParam::StereoWidth, 0.25))
    })
    .check();
}

#[test]
fn tremolo() {
    IsolateRow::new("TremoloNode (stereo)", || {
        TremoloNode::from(TremoloConfig::new(ChannelLayout::STEREO))
    })
    .control("rate", |t| {
        assert!(t.param_set().set(UnitParam::Rate, 40.0))
    })
    .control("depth", |t| {
        assert!(t.param_set().set(UnitParam::Depth, 1.0))
    })
    .check();
}

#[test]
fn dc_blocker() {
    IsolateRow::new("DcBlockerNode (stereo)", || {
        DcBlockerNode::<f32>::from(DcBlockerConfig::new(ChannelLayout::STEREO))
    })
    .control("corner", |d| {
        assert!(d.param_set().set(UnitParam::Cutoff, 80.0))
    })
    .check();
}

#[test]
fn bell_filter() {
    IsolateRow::new("BellFilterNode (stereo)", || {
        BellFilterNode::from(BellFilterConfig::new(
            ChannelLayout::STEREO,
            Hz::new(2_000.0),
            Q::new(2.0),
            Db::new(6.0),
        ))
    })
    .control("gain", |d| {
        assert!(d.param_set().set(UnitParam::GainDb, -9.0))
    })
    .check();
}

#[test]
fn tilt_filter() {
    IsolateRow::new("TiltFilterNode (stereo)", || {
        TiltFilterNode::from(TiltFilterConfig::new(
            ChannelLayout::STEREO,
            Hz::new(1_000.0),
            Db::new(6.0),
        ))
    })
    .control("gain", |d| {
        assert!(d.param_set().set(UnitParam::GainDb, -9.0))
    })
    .check();
}

#[test]
fn dynamic_eq() {
    IsolateRow::new("DynamicEqNode<f32> (stereo, side-chain)", || {
        DynamicEqNode::<f32>::from(
            DynamicEqConfig::de_esser(ChannelLayout::STEREO, Hz::new(6_000.0))
                .key(KeySource::Sidechain),
        )
    })
    .control("threshold", |d| {
        assert!(d.param_set().set(UnitParam::Threshold, -40.0))
    })
    .control("centre", |d| {
        assert!(d.param_set().set(UnitParam::Cutoff, 3_000.0))
    })
    .check();
}

#[test]
fn cut_filter() {
    IsolateRow::new("CutFilterNode (stereo)", || {
        CutFilterNode::from(CutFilterConfig::new(
            ChannelLayout::STEREO,
            CutResponse::HighPass,
            CutAlignment::Butterworth(ButterworthOrder::of(6)),
        ))
    })
    .control("cutoff", |d| {
        assert!(d.param_set().set(UnitParam::Cutoff, 400.0))
    })
    .check();
}

#[test]
fn envelope_filter() {
    IsolateRow::new("EnvelopeFilterNode<f32> (stereo)", || {
        EnvelopeFilterNode::<f32>::from(
            EnvelopeFilterConfig::new(ChannelLayout::STEREO, Hz::new(500.0), SvfType::LowPass)
                .depth(Semitones::new(24.0)),
        )
    })
    .control("base", |d| {
        assert!(d.param_set().set(UnitParam::Cutoff, 900.0))
    })
    .control("q", |d| assert!(d.param_set().set(UnitParam::Q, 3.0)))
    .control("depth", |d| {
        assert!(d.param_set().set(UnitParam::Semitones, 12.0))
    })
    .check();
}

#[test]
fn transient_shaper() {
    IsolateRow::new("TransientShaperNode<f32> (stereo)", || {
        TransientShaperNode::<f32>::from(
            TransientShaperConfig::new(ChannelLayout::STEREO)
                .attack(Db::new(6.0))
                .sustain(Db::new(-3.0)),
        )
    })
    .control("attack", |d| {
        assert!(d.param_set().set(UnitAddr::new(UnitParam::GainDb, 0), 12.0))
    })
    .control("sustain", |d| {
        assert!(d.param_set().set(UnitAddr::new(UnitParam::GainDb, 1), 9.0))
    })
    .check();
}

/// A leveller at `target` with a 40 dB cut and a 9 dB boost, 50 ms times and no gate. The window
/// reads from its first 100 ms hop, so each row renders long enough for the gain to move.
fn leveller_at(target: f64) -> LevellerNode<f32> {
    LevellerNode::<f32>::try_from(
        LevellerConfig::new(BusTopology::STEREO)
            .target(Db::new(target))
            .max_boost(Db::new(9.0))
            .max_cut(Db::new(40.0))
            .gate(Db::new(-100.0))
            .attack(Seconds::new(0.05))
            .release(Seconds::new(0.05)),
    )
    .unwrap()
}

/// The stimulus is loud, so a target of -20 LUFS cuts: the target, the cut and both times show.
#[test]
fn leveller_cutting() {
    IsolateRow::new("LevellerNode<f32> (stereo, cutting)", || leveller_at(-20.0))
        .frames(65_536)
        .control("target", |d| {
            assert!(
                d.param_set()
                    .set(UnitAddr::new(UnitParam::Threshold, 0), -30.0)
            )
        })
        .control("max cut", |d| {
            assert!(d.param_set().set(UnitAddr::new(UnitParam::Range, 1), 3.0))
        })
        .control("attack", |d| {
            assert!(d.param_set().set(UnitParam::Attack, 0.01))
        })
        .control("release", |d| {
            assert!(d.param_set().set(UnitParam::Release, 0.01))
        })
        .check();
}

/// A target of 0 LUFS is above the stimulus, so the gain rises: the boost and the gate show
/// (a gate at 0 LUFS holds the gain).
#[test]
fn leveller_boosting() {
    IsolateRow::new("LevellerNode<f32> (stereo, boosting)", || leveller_at(0.0))
        .frames(65_536)
        .control("max boost", |d| {
            assert!(d.param_set().set(UnitAddr::new(UnitParam::Range, 0), 3.0))
        })
        .control("gate", |d| {
            assert!(
                d.param_set()
                    .set(UnitAddr::new(UnitParam::Threshold, 1), 0.0)
            )
        })
        .check();
}

#[test]
fn auto_pan() {
    IsolateRow::new("AutoPanNode", || {
        AutoPanNode::from(AutoPanConfig::new().depth(Depth::of(0.2)))
    })
    .control("rate", |p| {
        assert!(p.param_set().set(UnitParam::Rate, 40.0))
    })
    .control("depth", |p| {
        assert!(p.param_set().set(UnitParam::Depth, 1.0))
    })
    .check();
}

#[test]
fn ring_mod() {
    IsolateRow::new("RingModNode (stereo)", || {
        RingModNode::from(RingModConfig::new(ChannelLayout::STEREO))
    })
    .control("frequency", |r| {
        assert!(r.param_set().set(UnitParam::Frequency, 900.0))
    })
    .check();
}

#[test]
fn lfo() {
    IsolateRow::new("LfoNode (sine, 3 Hz)", || {
        LfoNode::from(
            LfoConfig::new(LfoShape::Sine)
                .rate(Hz::new(3.0))
                .depth(Depth::clamped(0.8)),
        )
    })
    .control("rate", |l| {
        assert!(ParamNode::<f32>::param_set(l).set(UnitParam::Rate, 5.0))
    })
    .control("depth", |l| {
        assert!(ParamNode::<f32>::param_set(l).set(UnitParam::Depth, 0.3))
    })
    .check();
}

#[test]
fn chorus() {
    IsolateRow::new("ModDelayNode (stereo chorus)", || {
        ModDelayNode::<f32>::chorus(BusTopology::STEREO)
    })
    .control("rate", |c| assert!(c.param_set().set(UnitParam::Rate, 3.0)))
    .control("depth", |c| {
        assert!(c.param_set().set(UnitParam::Depth, 0.001))
    })
    .control("feedback", |c| {
        assert!(c.param_set().set(UnitParam::Feedback, 0.6));
    })
    .check();
}

#[test]
fn phaser() {
    IsolateRow::new("PhaserNode (stereo, 4 stages)", || {
        PhaserNode::<f32>::stereo(
            Stages::of(4),
            PhaserControls {
                feedback: Feedback::new(0.3_f64),
                ..PhaserControls::DEFAULT
            },
        )
    })
    .control("rate", |p| assert!(p.param_set().set(UnitParam::Rate, 3.0)))
    .control("depth", |p| {
        assert!(p.param_set().set(UnitParam::Depth, 0.2))
    })
    .control("feedback", |p| {
        assert!(p.param_set().set(UnitParam::Feedback, 0.7));
    })
    .check();
}

#[test]
fn plate_reverb() {
    IsolateRow::new("PlateReverbNode", || {
        PlateReverbNode::<f32>::from(PlateReverbConfig::new().decay(Seconds::new(1.0)))
    })
    .control("decay", |p| {
        assert!(p.param_set().set(UnitParam::Decay, 6.0))
    })
    .control("damping", |p| {
        assert!(p.param_set().set(UnitParam::Damping, 0.8))
    })
    .control("predelay", |p| {
        assert!(p.param_set().set(UnitParam::DelayTime, 0.01))
    })
    .control("bandwidth", |p| {
        assert!(p.param_set().set(UnitParam::Cutoff, 500.0))
    })
    .control("modulation", |p| {
        assert!(p.param_set().set(UnitParam::Depth, 1.0))
    })
    .control("diffusion", |p| {
        assert!(p.param_set().set(UnitParam::Diffusion, 0.2))
    })
    .check();
}

#[test]
fn ladder() {
    IsolateRow::new("LadderFilterNode (LP12)", || {
        LadderFilterNode::<f32>::new(
            LadderType::LP12,
            Hz::new(1_000.0),
            Resonance::new_clamped(0.3),
        )
    })
    .control("cutoff", |l| {
        assert!(l.param_set().set(UnitParam::Cutoff, 3_000.0));
    })
    .control("resonance", |l| {
        assert!(l.param_set().set(UnitParam::Q, 0.8));
    })
    .control("drive", |l| {
        assert!(l.param_set().set(UnitParam::Drive, 4.0));
    })
    .check();
}

/// The SVF and the EQ band are graph nodes: their fork is
/// `tutti_graph::param_parts`', from the values set through their
/// `ParamSet`, and `assert_param_fork` is its snapshot check (their own
/// unit tests run it).
#[test]
fn svf() {
    tutti_graph::contract::assert_param_fork(SvfFilterNode::<f32>::with_controls(
        ChannelLayout::MONO,
        SvfControls {
            ty: SvfType::Bell,
            freq: Hz::new(1_000.0),
            q: Q::new(0.7),
            gain: Db::new(6.0),
        },
    ));
}

#[test]
fn eq_band() {
    tutti_graph::contract::assert_param_fork(EqBandNode::<f32>::new(
        SvfType::Bell,
        Hz::new(1_000.0),
        Q::new(0.7),
        Db::new(6.0),
    ));
}

#[test]
fn bus_strip() {
    IsolateRow::new("BusStripNode (stereo)", BusStripNode::stereo)
        .control("volume", |s| s.set_volume(Amplitude::new(0.5)))
        .control("balance", |s| {
            assert!(ParamNode::<f32>::param_set(s).set(UnitParam::Balance, -0.7))
        })
        .check();
}
#[test]
fn fir_convolver() {
    use tutti_dsp::{LinearPhaseDesign, Tilt};
    use tutti_nodes::FirConvolverConfig;
    use tutti_units::{Db, Hz, SampleRate};

    let tilt = Tilt::new(Db::new(6.0))
        .design(Hz::new(2_000.0), SampleRate::SR_48K)
        .unwrap();
    let fir = LinearPhaseDesign::new(Hz::new(200.0))
        .design(SampleRate::SR_48K, &[tilt])
        .unwrap();
    IsolateRow::new("ConvolverNode (FirConvolverConfig)", move || {
        FirConvolverConfig::new(tutti_buffers::ChannelLayout::MONO, &fir)
            .block_size(Samples::new(64))
            .build::<f32>()
            .unwrap()
    })
    .control("gain", |c| c.set_gain(Amplitude::new(0.3)))
    .check();
}
#[test]
fn convolver() {
    use tutti_nodes::ConvolverNode;
    // A short decaying tail: a fixed LCG, so the IR is the same every run.
    let ir: Vec<f32> = (0..512u32)
        .map(|n| {
            let r = n.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) >> 8;
            let white = r as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            white * (-(n as f32) / 128.0).exp()
        })
        .collect();
    IsolateRow::new("ConvolverNode (mono)", move || {
        ConvolverNode::new(Mono::new(&ir), Samples::new(64))
    })
    .control("gain", |c| c.set_gain(Amplitude::new(0.3)))
    .check();
}

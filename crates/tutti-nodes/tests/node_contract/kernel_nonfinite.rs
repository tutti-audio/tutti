//! A kernel never latches a non-finite value: one NaN or infinity in its input is heard in
//! at most one block of output, and every block after that block is finite, for every
//! effect `KernelNode` runs and the multiband compressor that runs kernels.

use tutti_buffers::{AmbisonicFormat, AmbisonicOrder, Bed, BusTopology, ChannelLayout, MaxBlock};
use tutti_buffers::{Float, Planes};
use tutti_dsp::{FdnLayout, FreeverbControls, FreezeMode, LadderType, ShapeKind, Stages, SvfType};
use tutti_dsp::{Inline, Tap, TapSet};
use tutti_graph::contract::drive_input;
use tutti_graph::{Env, Node, PlayClock, Prepare, TransportChanges, TransportPosition};
use tutti_nodes::{
    AmbisonicDecoderConfig, AmbisonicDecoderNode, AmbisonicEncoderConfig, AmbisonicEncoderNode,
    AmbisonicRotatorConfig, AmbisonicRotatorNode, Antialiasing, AutoPanConfig, AutoPanNode,
    BrickwallLimiterConfig, BrickwallLimiterNode, CompressorConfig, CompressorControls,
    CompressorNode, CrossoverConfig, CrossoverControls, CrossoverNode, DelayConfig, DelayControls,
    DelayLayout, DelayNode, DiodeClipperConfig, DiodeClipperNode, DistanceConfig, DistanceNode,
    DistortionConfig, DistortionNode, DynamicEqConfig, DynamicEqNode, FdnConfig, FdnNode,
    FreeverbConfig, FreeverbNode, GateConfig, GateControls, GateNode, InterpolationMode,
    LadderFilterConfig, LadderFilterNode, LimiterConfig, LimiterControls, LimiterNode, MaxDelay,
    ModDelayConfig, ModDelayNode, MultibandBands, MultibandCompressorConfig,
    MultibandCompressorNode, PeakDetection, PhaserConfig, PhaserNode, PitchShifterConfig,
    PitchShifterNode, PlateReverbConfig, PlateReverbNode, RingModConfig, RingModNode,
    ShimmerConfig, ShimmerNode, SpringReverbConfig, SpringReverbNode, SvfFilterConfig,
    SvfFilterNode, TremoloConfig, TremoloNode, TubeStageConfig, TubeStageNode,
};
use tutti_nodes::{
    BellFilterConfig, BellFilterNode, BitCrusherConfig, BitCrusherNode, ButterworthOrder,
    CutAlignment, CutFilterConfig, CutFilterNode, CutResponse, DcBlockerConfig, DcBlockerNode,
    EnvelopeFilterConfig, EnvelopeFilterNode, LevellerConfig, LevellerNode, TiltFilterConfig,
    TiltFilterNode, TransientShaperConfig, TransientShaperNode,
};
use tutti_nodes::{
    CombFilterConfig, CombFilterNode, CombForm, FrequencyShifterConfig, FrequencyShifterNode,
};
use tutti_nodes::{
    DecorrelatorConfig, DecorrelatorNode, EarlyReflectionsConfig, EarlyReflectionsNode,
};
use tutti_nodes::{GraphicBands, GraphicEqConfig, GraphicEqNode};
use tutti_nodes::{
    KarplusStrongConfig, KarplusStrongNode, WaveguideStringConfig, WaveguideStringNode,
};
use tutti_nodes::{ModalBankConfig, ModalBankNode, Mode, Modes};
use tutti_rt::SplitMix64;
use tutti_units::{
    BitDepth, Cents, Db, Drive, Feedback, Frame, HoldFactor, Hz, Metres, Mix, Q, Resonance,
    SampleRate, Samples, Seconds, Semitones,
};
use tutti_units::{Gain, SampleSpan};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;
/// The block whose input carries the one non-finite sample.
const POISONED: u64 = 4;
const BLOCKS: u64 = 160;

/// Every kernel node, built fresh. The filters, the lines, the modulation and the reverbs
/// latch a non-finite sample without the node's reset; the distortion and the dynamics
/// recover from one on their own, and their rows hold that.
fn rows() -> Vec<(&'static str, Box<dyn Node>)> {
    vec![
        (
            "svf",
            Box::new(SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                    .freq(Hz::new(1_000.0))
                    .q(Q::new(0.707)),
            )),
        ),
        (
            "ladder",
            Box::new(LadderFilterNode::<f32>::from(
                LadderFilterConfig::new(ChannelLayout::MONO, LadderType::LP24)
                    .freq(Hz::new(1_000.0))
                    .res(Resonance::new(0.5)),
            )),
        ),
        (
            "crossover",
            Box::new(
                CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                    BusTopology::STEREO,
                    CrossoverControls::try_from(&[Hz::new(1_000.0)][..]).unwrap(),
                ))
                .unwrap(),
            ),
        ),
        (
            // A feedback line of a whole number of frames: one sample circulating in it falls
            // on a block's last frame only when the period lines up.
            "delay",
            Box::new(DelayNode::<f32>::from(
                DelayConfig::new(DelayLayout::new(
                    BusTopology::MONO,
                    MaxDelay::of(Seconds::new(0.05)),
                    InterpolationMode::Linear,
                ))
                .controls(DelayControls {
                    time: Seconds::new(0.01),
                    feedback: Feedback::new(0.9),
                    ..DelayControls::DEFAULT
                }),
            )),
        ),
        (
            "diode clipper",
            Box::new(DiodeClipperNode::<f32>::from(DiodeClipperConfig::new(
                ChannelLayout::STEREO,
            ))),
        ),
        (
            "tube stage",
            Box::new(TubeStageNode::<f32>::from(TubeStageConfig::new(
                ChannelLayout::STEREO,
            ))),
        ),
        (
            "distortion",
            Box::new(DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh)
                    .drive(Drive::new(4.0)),
            )),
        ),
        // The antialiasing's history is the shaper's own state: a non-finite sample enters it
        // as 0, so it is heard at its own frame.
        (
            "distortion first order",
            Box::new(DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh)
                    .drive(Drive::new(4.0))
                    .antialiasing(Antialiasing::FirstOrder)
                    .expect("an order the kind has"),
            )),
        ),
        (
            "distortion second order",
            Box::new(DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Softsign)
                    .drive(Drive::new(4.0))
                    .antialiasing(Antialiasing::SecondOrder)
                    .expect("an order the kind has"),
            )),
        ),
        (
            "compressor",
            Box::new(CompressorNode::<f32>::from(
                CompressorConfig::new(BusTopology::MONO).controls(CompressorControls::DEFAULT),
            )),
        ),
        (
            // A hold of 91 frames captures the poisoned frame (273 is 3 x 91) and holds it
            // past the block's end: the node's reset is what ends it. The reset matters for NaN only;
            // an infinity quantizes to +-1, which is finite, and the next capture replaces it.
            "bit crusher",
            Box::new(BitCrusherNode::<f32>::from(
                BitCrusherConfig::new(ChannelLayout::STEREO)
                    .bits(BitDepth::new(12.0))
                    .hold(HoldFactor::new(91.0)),
            )),
        ),
        (
            "dc blocker",
            Box::new(DcBlockerNode::<f32>::from(DcBlockerConfig::new(
                ChannelLayout::STEREO,
            ))),
        ),
        (
            "cut filter",
            Box::new(CutFilterNode::from(CutFilterConfig::new(
                ChannelLayout::STEREO,
                CutResponse::LowPass,
                CutAlignment::LinkwitzRiley(ButterworthOrder::of(4)),
            ))),
        ),
        (
            "bell filter",
            Box::new(BellFilterNode::from(BellFilterConfig::new(
                ChannelLayout::STEREO,
                Hz::new(2_000.0),
                Q::new(2.0),
                Db::new(6.0),
            ))),
        ),
        (
            "graphic eq",
            Box::new(GraphicEqNode::from(GraphicEqConfig::new(
                BusTopology::STEREO,
                GraphicBands::ThirdOctave,
            ))),
        ),
        (
            "tilt filter",
            Box::new(TiltFilterNode::from(TiltFilterConfig::new(
                ChannelLayout::STEREO,
                Hz::new(1_000.0),
                Db::new(6.0),
            ))),
        ),
        (
            "dynamic eq",
            Box::new(DynamicEqNode::<f32>::from(
                DynamicEqConfig::de_esser(ChannelLayout::STEREO, Hz::new(6_000.0))
                    .threshold(Db::new(-60.0)),
            )),
        ),
        (
            "envelope filter",
            Box::new(EnvelopeFilterNode::<f32>::from(
                EnvelopeFilterConfig::new(ChannelLayout::STEREO, Hz::new(500.0), SvfType::BandPass)
                    .depth(Semitones::new(24.0)),
            )),
        ),
        (
            "transient shaper",
            Box::new(TransientShaperNode::<f32>::from(
                TransientShaperConfig::new(ChannelLayout::STEREO)
                    .attack(Db::new(12.0))
                    .sustain(Db::new(-6.0)),
            )),
        ),
        (
            "leveller",
            Box::new(
                LevellerNode::<f32>::try_from(LevellerConfig::new(BusTopology::STEREO)).unwrap(),
            ),
        ),
        (
            "gate",
            Box::new(GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO).controls(GateControls::DEFAULT),
            )),
        ),
        (
            "limiter",
            Box::new(LimiterNode::<f32>::from(
                LimiterConfig::new(BusTopology::MONO).controls(LimiterControls::DEFAULT),
            )),
        ),
        (
            "true-peak limiter",
            Box::new(LimiterNode::<f32>::from(
                LimiterConfig::new(BusTopology::MONO)
                    .detection(PeakDetection::TruePeak)
                    .controls(LimiterControls::DEFAULT),
            )),
        ),
        (
            "brickwall",
            Box::new(BrickwallLimiterNode::<f32>::from(
                BrickwallLimiterConfig::new(BusTopology::STEREO, Db::new(-1.0)),
            )),
        ),
        (
            "chorus",
            Box::new(ModDelayNode::<f32>::from(ModDelayConfig::chorus(
                BusTopology::MONO,
            ))),
        ),
        (
            "flanger",
            Box::new(ModDelayNode::<f32>::from(ModDelayConfig::flanger(
                BusTopology::MONO,
            ))),
        ),
        (
            "phaser",
            Box::new(
                PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::MONO, Stages::of(4)))
                    .unwrap(),
            ),
        ),
        (
            "fdn",
            Box::new(FdnNode::<f32>::from(FdnConfig::new().layout(
                FdnLayout::new(8, tutti_dsp::MaxDecay::DEFAULT, FreezeMode::Decaying).unwrap(),
            ))),
        ),
        (
            "shimmer",
            Box::new(ShimmerNode::<f32>::from(
                ShimmerConfig::new()
                    .pitch(Cents::new(1200.0))
                    .send(Feedback::new_clamped(0.9)),
            )),
        ),
        (
            "freeverb",
            Box::new(FreeverbNode::<f32>::from(FreeverbConfig::new().controls(
                FreeverbControls {
                    room: Mix::new(0.9),
                    damp: Mix::new(0.2),
                },
            ))),
        ),
        (
            "plate",
            Box::new(PlateReverbNode::<f32>::from(PlateReverbConfig::new())),
        ),
        (
            "spring",
            Box::new(SpringReverbNode::<f32>::from(SpringReverbConfig::new(
                ChannelLayout::STEREO,
            ))),
        ),
        // The LFO and the carrier are state of the clock, not of the input: a NaN sample is
        // heard at its own frame and nowhere else.
        (
            "tremolo",
            Box::new(TremoloNode::from(TremoloConfig::new(ChannelLayout::STEREO))),
        ),
        (
            "auto-pan",
            Box::new(AutoPanNode::from(AutoPanConfig::new())),
        ),
        // A gain per channel and frame and no state: nothing remembers the sample.
        (
            "ambisonic encoder",
            Box::new(
                AmbisonicEncoderNode::<f32>::try_from(AmbisonicEncoderConfig::new(ambix(
                    AmbisonicOrder::MAX,
                )))
                .unwrap(),
            ),
        ),
        (
            "ambisonic rotator",
            Box::new(
                AmbisonicRotatorNode::<f32>::try_from(AmbisonicRotatorConfig::new(ambix(
                    AmbisonicOrder::MAX,
                )))
                .unwrap(),
            ),
        ),
        (
            "ambisonic decoder",
            Box::new(
                AmbisonicDecoderNode::<f32>::try_from(
                    AmbisonicDecoderConfig::new(Bed::Surround51).input(ambix(AmbisonicOrder::MAX)),
                )
                .unwrap(),
            ),
        ),
        (
            "ring mod",
            Box::new(RingModNode::from(RingModConfig::new(ChannelLayout::STEREO))),
        ),
        (
            "comb (feedback)",
            Box::new(CombFilterNode::<f32>::from(CombFilterConfig::new(
                ChannelLayout::STEREO,
                CombForm::Feedback,
                Seconds::new(0.002),
            ))),
        ),
        (
            "karplus-strong",
            Box::new(KarplusStrongNode::<f32>::from(KarplusStrongConfig::new(
                ChannelLayout::STEREO,
            ))),
        ),
        (
            "waveguide string",
            Box::new(WaveguideStringNode::<f32>::from(
                WaveguideStringConfig::new(ChannelLayout::STEREO),
            )),
        ),
        (
            "modal bank",
            Box::new(ModalBankNode::<f32>::from(ModalBankConfig::new(
                ChannelLayout::STEREO,
                Modes::try_from(
                    &[Mode {
                        frequency: Hz::new(440.0),
                        decay: Seconds::new(0.3),
                        gain: Gain::new(1.0),
                    }][..],
                )
                .unwrap(),
            ))),
        ),
        (
            "early reflections",
            Box::new(
                EarlyReflectionsNode::<f32>::try_from(EarlyReflectionsConfig::new(
                    ChannelLayout::STEREO,
                    TapSet {
                        taps: Inline::from_slice(&[Tap {
                            delay: SampleSpan::new(300.0).unwrap(),
                            gain: Gain::new(0.5),
                            channel: 1,
                        }])
                        .unwrap(),
                    },
                ))
                .unwrap(),
            ),
        ),
        (
            "decorrelator",
            Box::new(DecorrelatorNode::<f32>::from(DecorrelatorConfig::new(
                ChannelLayout::STEREO,
            ))),
        ),
        (
            "frequency shifter",
            Box::new(FrequencyShifterNode::<f32>::from(
                FrequencyShifterConfig::new(ChannelLayout::STEREO).shift(Hz::new(300.0)),
            )),
        ),
        (
            "distance",
            Box::new(DistanceNode::<f32>::from(
                DistanceConfig::new(ChannelLayout::STEREO, Metres::of(50.0))
                    .air_absorption(Hz::new(4000.0))
                    .distance(Metres::of(3.0)),
            )),
        ),
        (
            "pitch shifter",
            Box::new(PitchShifterNode::<f32>::from(
                PitchShifterConfig::new(ChannelLayout::STEREO).pitch(Cents::new(700.0)),
            )),
        ),
        (
            "multiband",
            Box::new(MultibandCompressorNode::<f32>::from(
                MultibandCompressorConfig::new(BusTopology::STEREO, MultibandBands::of(3)),
            )),
        ),
    ]
}

/// Deterministic noise on `-0.5..0.5`.
fn noise(rng: &mut SplitMix64) -> f32 {
    f32::from_fraction_bits(rng.next_u64()) - 0.5
}

/// Drives `node` with noise, `bad` at frame 17 of block [`POISONED`] on every input channel,
/// and returns which blocks' outputs hold a non-finite sample.
fn poisoned_blocks(node: Box<dyn Node>, bad: f32) -> Vec<u64> {
    let mut node = node;
    node.prepare(&Prepare::new(SR, MaxBlock::of(Samples::new(BLOCK))));
    let width = usize::from(node.shape().audio_in().count());
    let mut rng = SplitMix64::new(1);
    let mut out = Vec::new();
    for block in 0..BLOCKS {
        let mut planes = vec![vec![0.0f32; BLOCK]; width];
        for plane in &mut planes {
            for s in plane.iter_mut() {
                *s = noise(&mut rng);
            }
            if block == POISONED {
                plane[17] = bad;
            }
        }
        let inputs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
        let env = Env {
            frame: Frame::new(block * BLOCK as u64),
            sample_rate: SR,
            block_len: Samples::new(BLOCK),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let d = drive_input(&mut *node, &env, Planes::new(&inputs).unwrap(), &[], &[]);
        if d.audio.iter().flatten().any(|s| !s.is_finite()) {
            out.push(block);
        }
    }
    out
}

/// Every row hears `bad` in at most one block, at or after [`POISONED`].
fn assert_heard_once(bad: f32) {
    let latched: Vec<_> = rows()
        .into_iter()
        .map(|(name, node)| (name, poisoned_blocks(node, bad)))
        .filter(|(_, blocks)| blocks.len() > 1 || blocks.iter().any(|&b| b < POISONED))
        .collect();
    assert!(
        latched.is_empty(),
        "non-finite output past one block: {latched:?}"
    );
}

/// One NaN in the input is heard in at most one block, at or after the block that carried
/// it, and never again.
#[test]
fn one_nan_input_sample_is_heard_in_at_most_one_block() {
    assert_heard_once(f32::NAN);
}

/// One infinite input sample is heard in at most one block, at or after the block that
/// carried it, and never again.
#[test]
fn one_infinite_input_sample_is_heard_in_at_most_one_block() {
    assert_heard_once(f32::INFINITY);
}

mod logged {
    //! A kernel that resets after a non-finite block says so on the realtime log: one warning
    //! per reset, at the earliest non-finite frame, naming its channel (the lowest on a tie).

    use super::{BLOCK, POISONED, SR};
    use tutti_buffers::{BusTopology, ChannelLayout, MaxBlock};
    use tutti_graph::{
        Cx, Edge, Editor, Executor, InPort, IntoNode, Io, LogFilter, Node, NodeKey, OutPort,
        PortSource, Prepare, RtLevel, RtLogReaders, RtRecord, RtValue, Shape, Status,
        TransportPosition, Unforkable,
    };
    use tutti_nodes::{
        CompressorConfig, CompressorControls, CompressorNode, DcBlockerConfig, DcBlockerNode,
        MultibandBands, MultibandCompressorConfig, MultibandCompressorNode,
    };
    use tutti_units::{Frame, Samples, Tail};

    const MESSAGE: &str = "non-finite output; the node's state resets";
    /// The frame of the poisoned block that carries the non-finite sample.
    const AT: usize = 17;
    const ALL: LogFilter = LogFilter::new(Some(RtLevel::Trace), Some(RtLevel::Trace));

    /// A stereo source of a constant that writes `bad` at frame [`AT`] of its call number
    /// [`POISONED`] on channel `channel` only, when it has one.
    struct Poison {
        bad: f32,
        channel: Option<usize>,
        calls: u64,
    }

    impl Node for Poison {
        fn shape(&self) -> Shape {
            Shape::source_unassigned(ChannelLayout::STEREO).with_tail(Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            for c in 0..2 {
                io.output(c).fill(0.25);
            }
            if let (Some(c), true) = (self.channel, self.calls == POISONED) {
                io.output(c)[AT] = self.bad;
            }
            self.calls += 1;
            Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// `node` fed by a [`Poison`] source, both channels of `node` the graph's outputs; renders
    /// `blocks` blocks and returns every record the node logged.
    fn records(
        node: impl IntoNode,
        bad: f32,
        poisoned_channel: Option<usize>,
        blocks: u64,
    ) -> Vec<RtRecord> {
        let (mut ed, mut exec): (Editor, Executor) =
            Editor::new(Prepare::new(SR, MaxBlock::of(Samples::new(BLOCK))));
        let [src, fx] = [1, 2].map(NodeKey);
        ed.insert(
            src,
            "poison",
            Unforkable(Poison {
                bad,
                channel: poisoned_channel,
                calls: 0,
            }),
        );
        ed.insert(fx, "fx", node);
        let topology = &mut ed.spec_mut().topology;
        for port in 0..2u16 {
            topology.edges.insert(
                InPort { node: fx, port },
                vec![Edge::Direct(PortSource::Node(OutPort { node: src, port }))],
            );
        }
        topology.set_outputs((0..2).map(|port| PortSource::Node(OutPort { node: fx, port })));
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        let readers = RtLogReaders::new(&mut exec);
        // The gate opens from the block after a drain stores the filter.
        readers.drain(ALL, |_| {});
        let mut records = Vec::new();
        for _ in 0..blocks {
            let mut bufs = vec![vec![0.0f32; BLOCK]; 2];
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
            exec.process(BLOCK, &TransportPosition::default(), &[], &mut refs);
            readers.drain(ALL, |r| records.push(*r));
        }
        records
    }

    /// The one warning a NaN on `channel` leaves, the frame and the channel it names.
    fn assert_one_warning(records: &[RtRecord], channel: usize, frame: u64) {
        assert_eq!(records.len(), 1, "one reset, one record: {records:?}");
        let r = &records[0];
        assert_eq!(r.level(), RtLevel::Warn);
        assert_eq!(r.message(), MESSAGE);
        assert_eq!(r.fields(), &[("channel", RtValue::U64(channel as u64))]);
        assert_eq!(r.frame(), Frame::new(frame));
        assert_eq!(r.repeats(), 0, "no repeat of the one reset");
    }

    fn dc_blocker() -> DcBlockerNode<f32> {
        DcBlockerNode::<f32>::from(DcBlockerConfig::new(ChannelLayout::STEREO))
    }

    #[test]
    fn a_nan_in_the_second_channel_logs_one_warning_at_its_frame_naming_channel_one() {
        let records = records(dc_blocker(), f32::NAN, Some(1), 8);
        assert_one_warning(&records, 1, POISONED * BLOCK as u64 + AT as u64);
    }

    #[test]
    fn an_infinity_in_the_first_channel_logs_one_warning_naming_channel_zero() {
        let records = records(dc_blocker(), f32::INFINITY, Some(0), 8);
        assert_one_warning(&records, 0, POISONED * BLOCK as u64 + AT as u64);
    }

    #[test]
    fn a_finite_input_logs_nothing() {
        assert!(records(dc_blocker(), 0.0, None, 8).is_empty());
        let compressor = CompressorNode::<f32>::from(
            CompressorConfig::new(BusTopology::STEREO).controls(CompressorControls::DEFAULT),
        );
        assert!(records(compressor, 0.0, None, 8).is_empty());
    }

    #[test]
    fn the_multiband_compressor_logs_its_reset_once() {
        let node = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::STEREO,
            MultibandBands::of(3),
        ));
        let records = records(node, f32::NAN, Some(1), 8);
        assert_one_warning(&records, 1, POISONED * BLOCK as u64 + AT as u64);
    }
}

/// The AmbiX bus of `order`: ACN, SN3D.
fn ambix(order: AmbisonicOrder) -> BusTopology {
    BusTopology::ambisonic(AmbisonicFormat::ambix(order))
}

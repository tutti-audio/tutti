//! A kernel never latches a non-finite value: one NaN or infinity in its input is heard in
//! at most one block of output, and every block after that block is finite, for every
//! effect `KernelNode` runs and the multiband compressor that runs kernels.

use tutti_buffers::Planes;
use tutti_dsp::{FdnControls, FdnLayout, FreeverbControls, LadderType, ShapeKind, Stages, SvfType};
use tutti_graph::contract::drive_input;
use tutti_graph::{Env, Node, PlayClock, Prepare, TransportChanges, TransportPosition};
use tutti_nodes::{
    Antialiasing, AutoPanConfig, AutoPanNode, BrickwallLimiterNode, CompressorControls,
    CompressorNode, CrossoverNode, DelayControls, DelayNode, DistortionNode, DynamicEqConfig,
    DynamicEqNode, FdnNode, FreeverbNode, GateControls, GateNode, LadderFilterNode,
    LimiterControls, LimiterNode, MaxDelay, ModDelayNode, MultibandCompressorNode, PeakDetection,
    PhaserControls, PhaserNode, RingModConfig, RingModNode, SvfFilterNode, TremoloConfig,
    TremoloNode,
};
use tutti_nodes::{
    BellFilterConfig, BellFilterNode, BitCrusherConfig, BitCrusherNode, ButterworthOrder,
    CutAlignment, CutFilterConfig, CutFilterNode, CutResponse, DcBlockerConfig, DcBlockerNode,
    TiltFilterConfig, TiltFilterNode, TransientShaperConfig, TransientShaperNode,
};
use tutti_types::{
    BitDepth, BusTopology, ChannelLayout, Db, Drive, Feedback, Frame, HoldFactor, Hz, MaxBlock,
    Mix, Q, Resonance, SampleRate, Samples, Seconds,
};

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
            Box::new(SvfFilterNode::<f64>::new(
                SvfType::LowPass,
                Hz(1_000.0),
                Q(0.707),
            )),
        ),
        (
            "ladder",
            Box::new(LadderFilterNode::<f64>::new(
                LadderType::LP24,
                Hz(1_000.0),
                Resonance(0.5),
            )),
        ),
        (
            "crossover",
            Box::new(CrossoverNode::new(ChannelLayout::STEREO, &[Hz(1_000.0)]).unwrap()),
        ),
        (
            // A feedback line of a whole number of frames: one sample circulating in it falls
            // on a block's last frame only when the period lines up.
            "delay",
            Box::new(DelayNode::mono(
                MaxDelay::of(Seconds(0.05)),
                DelayControls {
                    time: Seconds(0.01),
                    feedback: Feedback(0.9),
                    ..DelayControls::DEFAULT
                },
            )),
        ),
        (
            "distortion",
            Box::new(DistortionNode::new(ShapeKind::Tanh, Drive(4.0))),
        ),
        // The antialiasing's history is the shaper's own state: a non-finite sample enters it
        // as 0, so it is heard at its own frame.
        (
            "distortion first order",
            Box::new(
                DistortionNode::new(ShapeKind::Tanh, Drive(4.0))
                    .antialiasing(Antialiasing::FirstOrder),
            ),
        ),
        (
            "distortion second order",
            Box::new(
                DistortionNode::new(ShapeKind::Softsign, Drive(4.0))
                    .antialiasing(Antialiasing::SecondOrder),
            ),
        ),
        (
            "compressor",
            Box::new(CompressorNode::mono(CompressorControls::DEFAULT)),
        ),
        (
            // A hold of 91 frames captures the poisoned frame (273 is 3 x 91) and holds it
            // past the block's end: the node's reset is what ends it. The reset matters for NaN only;
            // an infinity quantizes to +-1, which is finite, and the next capture replaces it.
            "bit crusher",
            Box::new(BitCrusherNode::from(
                BitCrusherConfig::new(ChannelLayout::STEREO)
                    .bits(BitDepth::new(12.0))
                    .hold(HoldFactor::new(91.0)),
            )),
        ),
        (
            "dc blocker",
            Box::new(DcBlockerNode::from(DcBlockerConfig::new(
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
                Hz(2_000.0),
                Q(2.0),
                Db(6.0),
            ))),
        ),
        (
            "tilt filter",
            Box::new(TiltFilterNode::from(TiltFilterConfig::new(
                ChannelLayout::STEREO,
                Hz(1_000.0),
                Db(6.0),
            ))),
        ),
        (
            "dynamic eq",
            Box::new(DynamicEqNode::from(
                DynamicEqConfig::de_esser(ChannelLayout::STEREO, Hz(6_000.0)).threshold(Db(-60.0)),
            )),
        ),
        (
            "transient shaper",
            Box::new(TransientShaperNode::from(
                TransientShaperConfig::new(ChannelLayout::STEREO)
                    .attack(Db(12.0))
                    .sustain(Db(-6.0)),
            )),
        ),
        ("gate", Box::new(GateNode::mono(GateControls::DEFAULT))),
        (
            "limiter",
            Box::new(LimiterNode::mono(LimiterControls::DEFAULT)),
        ),
        (
            "true-peak limiter",
            Box::new(
                LimiterNode::mono(LimiterControls::DEFAULT).detection(PeakDetection::TruePeak),
            ),
        ),
        ("brickwall", Box::new(BrickwallLimiterNode::new(Db(-1.0)))),
        ("chorus", Box::new(ModDelayNode::chorus(BusTopology::MONO))),
        (
            "flanger",
            Box::new(ModDelayNode::flanger(BusTopology::MONO)),
        ),
        (
            "phaser",
            Box::new(PhaserNode::mono(Stages::of(4), PhaserControls::DEFAULT)),
        ),
        (
            "fdn",
            Box::new(FdnNode::live(
                FdnLayout::new(8, tutti_dsp::MaxDecay::DEFAULT, false).unwrap(),
                FdnControls::DEFAULT,
            )),
        ),
        (
            "freeverb",
            Box::new(FreeverbNode::live(FreeverbControls {
                room: Mix(0.9),
                damp: Mix(0.2),
            })),
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
        (
            "ring mod",
            Box::new(RingModNode::from(RingModConfig::new(ChannelLayout::STEREO))),
        ),
        (
            "multiband",
            Box::new(MultibandCompressorNode::new(ChannelLayout::STEREO, 3).unwrap()),
        ),
    ]
}

/// Deterministic noise on `-0.5..0.5`.
fn noise(state: &mut u32) -> f32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state as f32 / u32::MAX as f32) - 0.5
}

/// Drives `node` with noise, `bad` at frame 17 of block [`POISONED`] on every input channel,
/// and returns which blocks' outputs hold a non-finite sample.
fn poisoned_blocks(node: Box<dyn Node>, bad: f32) -> Vec<u64> {
    let mut node = node;
    node.prepare(&Prepare::new(SR, MaxBlock::of(Samples(BLOCK))));
    let width = usize::from(node.shape().audio_in().count());
    let mut state = 0x9E37_79B9u32;
    let mut out = Vec::new();
    for block in 0..BLOCKS {
        let mut planes = vec![vec![0.0f32; BLOCK]; width];
        for plane in &mut planes {
            for s in plane.iter_mut() {
                *s = noise(&mut state);
            }
            if block == POISONED {
                plane[17] = bad;
            }
        }
        let inputs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
        let env = Env {
            frame: Frame::new(block * BLOCK as u64),
            sample_rate: SR,
            block_len: Samples(BLOCK),
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
    use tutti_graph::{
        Cx, Edge, Editor, Executor, InPort, IntoNode, Io, LogFilter, Node, NodeKey, OutPort,
        Prepare, RtLevel, RtLogReaders, RtRecord, RtValue, Shape, Source, Status,
        TransportPosition, Unforkable,
    };
    use tutti_nodes::{
        CompressorControls, CompressorNode, DcBlockerConfig, DcBlockerNode, MultibandCompressorNode,
    };
    use tutti_types::{ChannelLayout, Frame, MaxBlock, Samples, Tail};

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
            Editor::new(Prepare::new(SR, MaxBlock::of(Samples(BLOCK))));
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
                vec![Edge::Direct(Source::Node(OutPort { node: src, port }))],
            );
        }
        topology.set_outputs((0..2).map(|port| Source::Node(OutPort { node: fx, port })));
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

    fn dc_blocker() -> DcBlockerNode {
        DcBlockerNode::from(DcBlockerConfig::new(ChannelLayout::STEREO))
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
        let compressor = CompressorNode::stereo(CompressorControls::DEFAULT);
        assert!(records(compressor, 0.0, None, 8).is_empty());
    }

    #[test]
    fn the_multiband_compressor_logs_its_reset_once() {
        let node = MultibandCompressorNode::new(ChannelLayout::STEREO, 3).unwrap();
        let records = records(node, f32::NAN, Some(1), 8);
        assert_one_warning(&records, 1, POISONED * BLOCK as u64 + AT as u64);
    }
}

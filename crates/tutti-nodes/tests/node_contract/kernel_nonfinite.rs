//! A kernel never latches a non-finite value: one NaN or infinity in its input is heard in
//! at most one block of output, and every block after that block is finite, for every
//! effect `KernelNode` runs and the multiband compressor that runs kernels.

use tutti_dsp::{FdnControls, FdnLayout, FreeverbControls, LadderType, ShapeKind, Stages, SvfType};
use tutti_graph::contract::drive_input;
use tutti_graph::{Env, Node, PlayClock, Prepare, TransportChanges, TransportPosition};
use tutti_nodes::{
    BrickwallLimiterNode, CompressorControls, CompressorNode, CrossoverNode, DelayControls,
    DelayNode, DistortionNode, FdnNode, FreeverbNode, GateControls, GateNode, LadderFilterNode,
    LimiterControls, LimiterNode, MaxDelay, ModDelayNode, MultibandCompressorNode, PhaserControls,
    PhaserNode, SvfFilterNode,
};
use tutti_types::{
    BusTopology, ChannelLayout, Db, Drive, Feedback, Frame, Hz, MaxBlock, Mix, Q, Resonance,
    SampleRate, Samples, Seconds,
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
        (
            "compressor",
            Box::new(CompressorNode::mono(CompressorControls::DEFAULT)),
        ),
        ("gate", Box::new(GateNode::mono(GateControls::DEFAULT))),
        (
            "limiter",
            Box::new(LimiterNode::mono(LimiterControls::DEFAULT)),
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
        let d = drive_input(&mut *node, &env, &inputs, &[], &[]);
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

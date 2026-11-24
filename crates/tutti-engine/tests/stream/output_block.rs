//! The output block, tested without a sound card: the `MAX_FRAMES` ceiling, the
//! zero-fill and the stream's width.
//!
//! The sample-format conversion is the driver's (`tutti-cpal` tests its
//! matrix); the main output meter's tests (the centre of a wide device, the analytic
//! level, enabled against disabled) are in `tests/outputs/main_output_meter.rs`: the
//! meter is an op in the graph's plan, not this callback's.

use crate::fixtures::{SAMPLE_RATE, dc_state, fan, graph_engine, rolling_state};
use std::sync::Arc;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_engine::{
    AudioCallbackState, MAX_FRAMES, ManualStreamDriver, OutputBlock, OutputQuantum, OutputSpec,
    OutputStream, QuantumOutOfRange,
};
use tutti_graph::NodeKey;
use tutti_nodes::testing::Const;
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

/// **An over-sized callback renders its head, silences its tail and counts it.**
///
/// The ceiling exists so the callback never allocates. It is observable here
/// only because the `debug_assert` on a backend's contract lives at the *driver*
/// boundary rather than inside `render_with`: it is a claim about the backend,
/// not about the block, and keeping it out is what lets a debug build reach this.
/// A callback at the ceiling counts nothing; one frame past it counts once.
#[test]
fn an_oversized_callback_silences_its_tail_and_counts_it() {
    let engine = graph_engine(&Transport::new(SampleRate::of(SAMPLE_RATE)), |ed| {
        ed.insert(NodeKey(1), "dc", Const::mono(1.0));
        fan(ed, NodeKey(1), 2);
    });
    let blocks = engine.blocks();
    let mut block = OutputBlock::new(
        Arc::new(AudioCallbackState::new(engine)),
        ChannelLayout::STEREO,
    );

    let mut whole = vec![-1.0f32; MAX_FRAMES * 2];
    block.render(&mut whole);
    assert_eq!(
        blocks.get().truncated,
        0,
        "a callback at the ceiling is whole"
    );

    let frames = MAX_FRAMES + 64;
    let mut out = vec![-1.0f32; frames * 2];
    block.render(&mut out);

    assert_eq!(blocks.get().truncated, 1, "the silenced tail is counted");
    assert_eq!(
        block.callback_frames(),
        Some(Samples::new(MAX_FRAMES)),
        "the block reports what it actually rendered, not what it was asked for"
    );
    assert!(
        out[..MAX_FRAMES * 2].iter().all(|&s| s == 1.0),
        "the head must carry the rendered signal"
    );
    assert!(
        out[MAX_FRAMES * 2..].iter().all(|&s| s == 0.0),
        "the tail past MAX_FRAMES must be silence, not the caller's stale \
         buffer contents"
    );
}

/// The block reports the width it was built for, and renders at it.
#[test]
fn the_block_renders_at_the_width_it_was_built_for() {
    for width in [1usize, 2, 6, 8] {
        let mut block = OutputBlock::new(
            rolling_state(width).1,
            ChannelLayout::try_from(width).expect("a test width is small"),
        );
        assert_eq!(block.channels(), width);
        let mut out = vec![0.0f32; 32 * width];
        block.render(&mut out);
        assert_eq!(block.callback_frames(), Some(Samples::new(32)));
    }
}

/// A zero-length callback is a no-op, not a panic.
///
/// Reachable in practice: some backends hand over an empty buffer on the first
/// callback after a device change.
#[test]
fn an_empty_callback_renders_nothing_and_does_not_panic() {
    let mut block = OutputBlock::new(dc_state(1.0, 2), ChannelLayout::STEREO);
    let mut out: [f32; 0] = [];
    block.render(&mut out);
    assert_eq!(block.callback_frames(), Some(Samples::new(0)));
}

/// **A callback quantum is from one frame to `MAX_FRAMES`.** A quantum the
/// callback could not render whole cannot be written, and a block longer than
/// the ceiling is refused when converted, never shortened.
#[test]
fn a_quantum_holds_from_one_frame_to_the_ceiling() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: OutputQuantum::new,
        get: OutputQuantum::get,
        of: Some(OutputQuantum::of),
        valid: &[1, 512, MAX_FRAMES],
        invalid: &[0, MAX_FRAMES + 1, usize::MAX],
    });
    let long = MaxBlock::of(Samples::new(MAX_FRAMES + 1));
    assert_eq!(
        OutputQuantum::try_from(long),
        Err(QuantumOutOfRange(MAX_FRAMES + 1))
    );
    let ceiling = MaxBlock::of(Samples::new(MAX_FRAMES));
    assert_eq!(
        OutputQuantum::try_from(ceiling).map(OutputQuantum::max_block),
        Ok(ceiling)
    );
}

/// **A stream's callbacks publish the smallest and largest size they were
/// handed since the last take**, whatever size the spec asked for, a callback
/// of no frames is not a size, and a restart forgets what was not taken.
#[test]
fn a_stream_publishes_the_callback_sizes_and_forgets_them_on_a_restart() {
    let state = dc_state(1.0, 2);
    let spec = OutputSpec::new(
        SampleRate::of(SAMPLE_RATE),
        ChannelLayout::STEREO,
        tutti_buffers::SampleFormat::F32,
    )
    .with_quantum(OutputQuantum::of(512));
    let mut stream = OutputStream::new(spec);
    let frames = stream.callback_frames();
    assert_eq!(frames.take(), None, "no callback yet");

    let (driver, handle) = ManualStreamDriver::new();
    stream.start_with(Arc::clone(&state), driver).unwrap();
    for n in [480, 0, 512, 471] {
        handle.render_block(Samples::new(n)).unwrap();
    }
    let span = frames.take().expect("callbacks ran");
    assert_eq!(
        (span.smallest(), span.largest()),
        (OutputQuantum::of(471), OutputQuantum::of(512)),
        "what was handed, not asked; the empty callback is no size"
    );
    assert_eq!(frames.take(), None, "a take empties the cell");
    assert_eq!(
        handle.callback_frames(),
        Some(Samples::new(471)),
        "the block's own last"
    );

    handle.render_block(Samples::new(480)).unwrap();
    stream.stop();
    let (driver, _handle) = ManualStreamDriver::new();
    stream.start_with(state, driver).unwrap();
    assert_eq!(frames.take(), None, "the new stream has run no callback");
}

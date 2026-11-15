//! The output block, tested without a sound card: the `MAX_FRAMES` clamp, the
//! zero-fill and the stream's width.
//!
//! The sample-format conversion is the driver's (`tutti-cpal` tests its
//! matrix); the master meter's tests (the centre of a wide device, the analytic
//! level, enabled against disabled) are in `tests/outputs/master_meter.rs`: the
//! meter is an op in the graph's plan, not this callback's.

use crate::fixtures::{dc_state, rolling_state};
use tutti_engine::{MAX_FRAMES, OutputBlock};
use tutti_types::ChannelLayout;

/// **An over-sized callback renders its head and silences its tail.**
///
/// The clamp exists so the callback never allocates. It is observable here
/// only because the `debug_assert` on a backend's contract lives at the *driver*
/// boundary rather than inside `render_with`: it is a claim about the backend,
/// not about the block, and keeping it out is what lets a debug build reach this.
#[test]
fn an_oversized_callback_silences_its_tail() {
    let mut block = OutputBlock::new(dc_state(1.0, 2), ChannelLayout::STEREO);

    let frames = MAX_FRAMES + 64;
    let mut out = vec![-1.0f32; frames * 2];
    block.render(&mut out);

    assert_eq!(
        block.last_rendered_frames(),
        MAX_FRAMES,
        "the clamp must report what it actually rendered, not what it was asked for"
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
        assert_eq!(block.last_rendered_frames(), 32);
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
    assert_eq!(block.last_rendered_frames(), 0);
}

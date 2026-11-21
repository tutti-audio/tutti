//! [`BlockMismatch`]: a block that is not the shape a processor was built for.

use tutti_buffers::{BlockSide, InputOutput, Planes, Sample};
use tutti_units::Samples;

/// A block the processor does not take: refused before it touches a sample, so no block is
/// ever processed in part.
///
/// A processor built for a width takes blocks of exactly that many channels on the sides it
/// reads and writes. The frame counts of its input and output agree by the block's type
/// ([`InputOutput`]), so a mismatch is in the channels, or in a third view (a key) that is
/// not at the block's frame count.
///
/// ```
/// use tutti_buffers::{BlockSide, ChannelLayout, InputOutput, Planes, PlanesMut};
/// use tutti_dsp::{BlockMismatch, DcBlocker};
/// use tutti_units::{SampleRate, Samples};
///
/// let mut blocker = DcBlocker::<f32>::new(ChannelLayout::STEREO, SampleRate::SR_48K);
/// let input = [&[0.0f32; 4][..]];
/// let mut out = [&mut [0.0f32; 4][..]];
/// let mut block = InputOutput::new(
///     Samples::new(4),
///     Planes::new(&input).unwrap(),
///     PlanesMut::new(&mut out).unwrap(),
/// )
/// .unwrap();
/// assert_eq!(
///     blocker.process(&mut block),
///     Err(BlockMismatch::Width { side: BlockSide::Input, expected: 2, found: 1 }),
/// );
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockMismatch {
    /// A side of the block has other channels than the processor takes; the input is checked
    /// first.
    #[error("the block's {side} has {found} channels; the processor takes {expected}")]
    Width {
        /// The side of the block that differs.
        side: BlockSide,
        /// The channels the processor takes on that side.
        expected: usize,
        /// The channels the block has on it.
        found: usize,
    },
    /// The key view is not at the block's frame count.
    #[error("the key has {found} frames; the block has {block}")]
    KeyFrames {
        /// The block's frame count.
        block: Samples,
        /// The key's.
        found: Samples,
    },
}

/// `block` has `input` channels in and `output` out.
pub(crate) fn expect_widths<S: Sample>(
    block: &mut InputOutput<'_, S>,
    input: usize,
    output: usize,
) -> Result<(), BlockMismatch> {
    let found = block.input().channels();
    if found != input {
        return Err(BlockMismatch::Width {
            side: BlockSide::Input,
            expected: input,
            found,
        });
    }
    let found = block.output().channels();
    if found != output {
        return Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: output,
            found,
        });
    }
    Ok(())
}

/// `block` has `output` channels out, whatever its input: the width of a processor that mixes
/// any input down.
pub(crate) fn expect_output_width<S: Sample>(
    block: &mut InputOutput<'_, S>,
    output: usize,
) -> Result<(), BlockMismatch> {
    let found = block.output().channels();
    if found != output {
        return Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: output,
            found,
        });
    }
    Ok(())
}

/// `block` has as many channels out as in, the width of a processor that takes any: that width.
pub(crate) fn expect_same_width<S: Sample>(
    block: &mut InputOutput<'_, S>,
) -> Result<usize, BlockMismatch> {
    let width = block.input().channels();
    expect_widths(block, width, width)?;
    Ok(width)
}

/// `key` holds the block's frames.
pub(crate) fn expect_key_frames<S: Sample>(
    block: &InputOutput<'_, S>,
    key: &Planes<'_, S>,
) -> Result<(), BlockMismatch> {
    if key.channels() > 0 && key.frames() != block.frames() {
        return Err(BlockMismatch::KeyFrames {
            block: block.frames(),
            found: key.frames(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;

//! [`InputOutput`]: one block's frame count, with input planes and output
//! planes checked to hold it.

use tutti_units::Samples;

use crate::{BufferError, Planes, PlanesMut, Sample};

/// One block: its frame count, and its input and output views at that count.
///
/// A processor reads `input` and writes `output` over the block's frames. The
/// block is built once by [`new`](Self::new), which refuses a view with
/// channels at another frame count, so a processor taking an `InputOutput`
/// never meets one. An empty view ([`Planes::default`],
/// [`PlanesMut::default`], no channels) is the side of a processor with none
/// (an instrument's input, a MIDI effect's output); the block still carries
/// its frame count.
///
/// ```
/// use tutti_buffers::{InputOutput, Planes, PlanesMut};
/// use tutti_units::Samples;
///
/// let left = [0.5f32; 4];
/// let input = [&left[..]];
/// let (mut out_l, mut out_r) = ([0.0f32; 4], [0.0f32; 4]);
/// let mut output = [&mut out_l[..], &mut out_r[..]];
///
/// let mut block = InputOutput::new(
///     Samples(4),
///     Planes::new(&input).unwrap(),
///     PlanesMut::new(&mut output).unwrap(),
/// )
/// .unwrap();
/// assert_eq!(block.frames(), Samples(4));
/// assert_eq!(block.input().channels(), 1);
/// assert_eq!(block.output().channels(), 2);
///
/// // A MIDI effect has no audio on either side; the block keeps its length.
/// let block = InputOutput::<f32>::new(Samples(4), Planes::default(), PlanesMut::default()).unwrap();
/// assert_eq!(block.frames(), Samples(4));
/// ```
#[derive(Debug)]
pub struct InputOutput<'a, S: Sample = f32> {
    frames: Samples,
    input: Planes<'a, S>,
    output: PlanesMut<'a, 'a, S>,
}

/// Which view of a block a [`BufferError::BlockLengthMismatch`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockSide {
    /// The input planes.
    Input,
    /// The output planes.
    Output,
}

impl core::fmt::Display for BlockSide {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Input => "input",
            Self::Output => "output",
        })
    }
}

impl<'a, S: Sample> InputOutput<'a, S> {
    /// The block of `frames` frames over `input` and `output`.
    ///
    /// # Errors
    ///
    /// [`BufferError::BlockLengthMismatch`] when a view with channels holds a
    /// frame count other than `frames`; the input is checked first.
    pub fn new(
        frames: Samples,
        input: Planes<'a, S>,
        output: PlanesMut<'a, 'a, S>,
    ) -> Result<Self, BufferError> {
        let check = |side, channels, found| {
            if channels > 0 && found != frames {
                Err(BufferError::BlockLengthMismatch {
                    side,
                    block: frames,
                    found,
                })
            } else {
                Ok(())
            }
        };
        check(BlockSide::Input, input.channels(), input.frames())?;
        check(BlockSide::Output, output.channels(), output.frames())?;
        Ok(Self {
            frames,
            input,
            output,
        })
    }

    /// The block's frame count.
    #[must_use]
    pub const fn frames(&self) -> Samples {
        self.frames
    }

    /// The input planes; empty for a processor with no input.
    #[must_use]
    pub const fn input(&self) -> Planes<'a, S> {
        self.input
    }

    /// The output planes, reborrowed: their samples are writable, the views
    /// themselves are not replaceable, so the frame counts stay equal.
    #[must_use]
    pub fn output(&mut self) -> PlanesMut<'_, 'a, S> {
        self.output.reborrow()
    }

    /// The input and the output at once, for a processor that reads one while
    /// it writes the other.
    #[must_use]
    pub fn split(&mut self) -> (Planes<'a, S>, PlanesMut<'_, 'a, S>) {
        (self.input, self.output.reborrow())
    }

    /// The frame count, the input and the output, taken apart.
    #[must_use]
    pub fn into_parts(self) -> (Samples, Planes<'a, S>, PlanesMut<'a, 'a, S>) {
        (self.frames, self.input, self.output)
    }
}

#[cfg(test)]
mod tests;

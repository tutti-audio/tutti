//! [`InputOutput`]: one block's input planes and output planes, checked to
//! hold the same frame count.

use tutti_types::Samples;

use crate::{BufferError, Planes, PlanesMut, Sample};

/// One block's input and output, at one frame count.
///
/// A processor reads `input` and writes `output` over the same frames. The
/// pair is built once by [`new`](Self::new), which refuses mismatched frame
/// counts, so a processor taking an `InputOutput` never meets one. An empty
/// input ([`Planes::default`], no channels) is the input of a processor with
/// none, such as an instrument: the block's frames are the output's.
///
/// ```
/// use tutti_buffers::{InputOutput, Planes, PlanesMut};
/// use tutti_types::Samples;
///
/// let left = [0.5f32; 4];
/// let input = [&left[..]];
/// let (mut out_l, mut out_r) = ([0.0f32; 4], [0.0f32; 4]);
/// let mut output = [&mut out_l[..], &mut out_r[..]];
///
/// let mut block = InputOutput::new(
///     Planes::new(&input).unwrap(),
///     PlanesMut::new(&mut output).unwrap(),
/// )
/// .unwrap();
/// assert_eq!(block.frames(), Samples(4));
/// assert_eq!(block.input().channels(), 1);
/// assert_eq!(block.output().channels(), 2);
///
/// // An instrument has no input; the block takes the output's frames.
/// let mut output = [&mut out_l[..]];
/// let block = InputOutput::new(Planes::default(), PlanesMut::new(&mut output).unwrap()).unwrap();
/// assert_eq!(block.frames(), Samples(4));
/// ```
#[derive(Debug)]
pub struct InputOutput<'i, 's, 'a, S: Sample = f32> {
    input: Planes<'i, S>,
    output: PlanesMut<'s, 'a, S>,
}

impl<'i, 's, 'a, S: Sample> InputOutput<'i, 's, 'a, S> {
    /// The pair of `input` and `output`.
    ///
    /// # Errors
    ///
    /// [`BufferError::LengthMismatch`] when `input` has channels and holds a
    /// different frame count from `output`.
    pub fn new(input: Planes<'i, S>, output: PlanesMut<'s, 'a, S>) -> Result<Self, BufferError> {
        if input.channels() > 0 && input.frames() != output.frames() {
            return Err(BufferError::LengthMismatch {
                src: input.frames(),
                dst: output.frames(),
            });
        }
        Ok(Self { input, output })
    }

    /// The block's frame count: the output's, which an input with channels
    /// matches.
    #[must_use]
    pub const fn frames(&self) -> Samples {
        self.output.frames()
    }

    /// The input planes; empty for a processor with no input.
    #[must_use]
    pub const fn input(&self) -> &Planes<'i, S> {
        &self.input
    }

    /// The output planes.
    pub fn output(&mut self) -> &mut PlanesMut<'s, 'a, S> {
        &mut self.output
    }

    /// The input and the output at once, for a processor that reads one while
    /// it writes the other.
    pub fn split(&mut self) -> (&Planes<'i, S>, &mut PlanesMut<'s, 'a, S>) {
        (&self.input, &mut self.output)
    }
}

#[cfg(test)]
mod tests;

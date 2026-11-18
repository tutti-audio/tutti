//! [`PluginAudio`]: one block's audio, in the width the plugin was activated at.

use tutti_buffers::{BufferError, Float, InputOutput, Planes, PlanesMut};
use tutti_units::Samples;

/// One block's input and output, in the sample width the plugin was activated
/// at.
///
/// The width is a value fixed at activation, so [`Processor::process`] takes
/// the block as an enum and each format's implementation matches once and
/// continues in one generic body over its [`InputOutput`]. The block carries
/// no rate and no maximum length: the activation fixes both, and a processor
/// keeps what it needs of them.
///
/// [`Processor::process`]: crate::Processor::process
#[derive(Debug)]
pub enum PluginAudio<'a> {
    /// A 32-bit block, the width every format negotiates.
    F32(InputOutput<'a, f32>),
    /// A 64-bit block, for a plugin that negotiated a 64-bit bus.
    F64(InputOutput<'a, f64>),
}

impl<'a> From<InputOutput<'a, f32>> for PluginAudio<'a> {
    fn from(io: InputOutput<'a, f32>) -> Self {
        Self::F32(io)
    }
}

impl<'a> From<InputOutput<'a, f64>> for PluginAudio<'a> {
    fn from(io: InputOutput<'a, f64>) -> Self {
        Self::F64(io)
    }
}

impl<'a> PluginAudio<'a> {
    /// The block of `frames` over `inputs` and `outputs`, one slice per
    /// channel. No inputs is an empty `inputs`; no outputs an empty `outputs`,
    /// and the block still has its `frames`.
    ///
    /// # Errors
    ///
    /// [`BufferError`] when the channels of a direction differ in length, or
    /// a direction with channels does not hold `frames`.
    pub fn new<S: Float>(
        frames: Samples,
        inputs: &'a [&'a [S]],
        outputs: &'a mut [&'a mut [S]],
    ) -> Result<Self, BufferError>
    where
        Self: From<InputOutput<'a, S>>,
    {
        let input = if inputs.is_empty() {
            Planes::default()
        } else {
            Planes::new(inputs)?
        };
        let output = if outputs.is_empty() {
            PlanesMut::default()
        } else {
            PlanesMut::new(outputs)?
        };
        InputOutput::new(frames, input, output).map(Self::from)
    }
}

impl PluginAudio<'_> {
    /// The block's frame count, whichever width this carries.
    #[must_use]
    pub fn frames(&self) -> Samples {
        match self {
            Self::F32(io) => io.frames(),
            Self::F64(io) => io.frames(),
        }
    }

    /// Zeroes every output channel, whichever width this carries.
    ///
    /// The way to make a silent block actually silent: an output slice's
    /// contents are otherwise whatever the last block left, so a plugin that
    /// declines to render repeats stale audio rather than going quiet.
    /// Allocation-free and RT-safe.
    pub fn clear_outputs(&mut self) {
        match self {
            Self::F32(io) => io.output().silence(),
            Self::F64(io) => io.output().silence(),
        }
    }
}

#[cfg(test)]
mod tests {
    use tutti_buffers::{Planes, PlanesMut};

    use super::*;

    /// Clearing a 64-bit block zeroes every output channel and leaves the input.
    #[test]
    fn clear_outputs_zeroes_every_channel_of_either_width() {
        let input_data = [0.25f64; 4];
        let input = [&input_data[..]];
        let (mut l, mut r) = ([1.0f64; 4], [2.0f64; 4]);
        let mut outputs = [&mut l[..], &mut r[..]];
        let mut audio = PluginAudio::from(
            InputOutput::new(
                Samples(4),
                Planes::new(&input).expect("one plane"),
                PlanesMut::new(&mut outputs).expect("two planes"),
            )
            .expect("equal lengths"),
        );
        assert_eq!(audio.frames(), Samples(4));
        audio.clear_outputs();
        assert_eq!((l, r), ([0.0; 4], [0.0; 4]));
        assert_eq!(input_data, [0.25; 4]);
    }

    /// A block with no output channel still has the frames it was made with: a
    /// plugin with no audio output (MIDI-only) sees the block's length.
    #[test]
    fn a_block_with_no_output_channel_keeps_its_frames() {
        let mut none: [&mut [f32]; 0] = [];
        let audio =
            PluginAudio::new(Samples(96), &[], &mut none).expect("no channels fit any block");
        assert_eq!(audio.frames(), Samples(96));
    }

    /// Channels of unequal length are refused, never trimmed.
    #[test]
    fn channels_of_unequal_length_are_refused() {
        let (long, short) = ([0.0f32; 4], [0.0f32; 3]);
        let inputs = [&long[..], &short[..]];
        let (mut l, mut r) = ([0.0f32; 4], [0.0f32; 4]);
        let mut outputs = [&mut l[..], &mut r[..]];
        assert!(PluginAudio::new(Samples(4), &inputs, &mut outputs).is_err());
    }

    /// A direction that does not hold the block's frames is refused.
    #[test]
    fn a_direction_that_does_not_hold_the_frames_is_refused() {
        let input = [0.0f32; 4];
        let inputs = [&input[..]];
        let mut l = [0.0f32; 4];
        let mut outputs = [&mut l[..]];
        assert!(PluginAudio::new(Samples(8), &inputs, &mut outputs).is_err());
    }
}

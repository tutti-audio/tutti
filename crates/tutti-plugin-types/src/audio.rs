//! [`channel_block`]: one block's audio, as the view a [`Processor`](crate::Processor)
//! is handed.

use tutti_buffers::{BufferError, Float, InputOutput, Planes, PlanesMut};
use tutti_units::Samples;

/// The block of `frames` over `inputs` and `outputs`, one slice per channel,
/// at the width `S` of its samples. No inputs is an empty `inputs`; no outputs
/// an empty `outputs`, and the block still has its `frames`.
///
/// The block carries no rate and no maximum length: the activation fixes both,
/// and a processor keeps what it needs of them.
///
/// # Errors
///
/// [`BufferError`] when the channels of a direction differ in length, or a
/// direction with channels does not hold `frames`.
pub fn channel_block<'a, S: Float>(
    frames: Samples,
    inputs: &'a [&'a [S]],
    outputs: &'a mut [&'a mut [S]],
) -> Result<InputOutput<'a, 'a, 'a, S>, BufferError> {
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
    InputOutput::new(frames, input, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block with no output channel still has the frames it was made with: a
    /// plugin with no audio output (MIDI-only) sees the block's length.
    #[test]
    fn a_block_with_no_output_channel_keeps_its_frames() {
        let mut none: [&mut [f32]; 0] = [];
        let io = channel_block(Samples(96), &[], &mut none).expect("no channels fit any block");
        assert_eq!(io.frames(), Samples(96));
    }

    /// A 64-bit block keeps its samples as they are: the width is the type's.
    #[test]
    fn a_wide_block_holds_its_samples_unrounded() {
        let wide = 1.0f64 + f64::EPSILON;
        let input = [wide; 4];
        let inputs = [&input[..]];
        let mut l = [0.0f64; 4];
        let mut outputs = [&mut l[..]];
        let io = channel_block(Samples(4), &inputs, &mut outputs).expect("equal lengths");
        assert_eq!(io.input().plane(0).samples(), &[wide; 4]);
    }

    /// Channels of unequal length are refused, never trimmed.
    #[test]
    fn channels_of_unequal_length_are_refused() {
        let (long, short) = ([0.0f32; 4], [0.0f32; 3]);
        let inputs = [&long[..], &short[..]];
        let (mut l, mut r) = ([0.0f32; 4], [0.0f32; 4]);
        let mut outputs = [&mut l[..], &mut r[..]];
        assert!(channel_block(Samples(4), &inputs, &mut outputs).is_err());
    }

    /// A direction that does not hold the block's frames is refused.
    #[test]
    fn a_direction_that_does_not_hold_the_frames_is_refused() {
        let input = [0.0f32; 4];
        let inputs = [&input[..]];
        let mut l = [0.0f32; 4];
        let mut outputs = [&mut l[..]];
        assert!(channel_block(Samples(8), &inputs, &mut outputs).is_err());
    }
}

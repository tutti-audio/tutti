use super::*;

/// An input holding fewer frames than the output is refused with both counts.
#[test]
fn an_input_shorter_than_the_output_is_refused_with_both_counts() {
    let short = [0.0f32; 3];
    let input = [&short[..]];
    let mut long = [0.0f32; 4];
    let mut output = [&mut long[..]];
    let refused = InputOutput::new(
        Planes::new(&input).expect("one plane"),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect_err("3 input frames against 4 output frames");
    assert_eq!(
        refused,
        BufferError::BlockLengthMismatch {
            input: Samples(3),
            output: Samples(4)
        }
    );
}

/// An input holding more frames than the output is refused too: the block's
/// frames are never a truncation of the input.
#[test]
fn an_input_longer_than_the_output_is_refused() {
    let long = [0.0f32; 5];
    let input = [&long[..]];
    let mut short = [0.0f32; 4];
    let mut output = [&mut short[..]];
    let refused = InputOutput::new(
        Planes::new(&input).expect("one plane"),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect_err("5 input frames against 4 output frames");
    assert_eq!(
        refused,
        BufferError::BlockLengthMismatch {
            input: Samples(5),
            output: Samples(4)
        }
    );
}

/// An empty input pairs with an output of any length; the block has the
/// output's frames and the input no channel.
#[test]
fn an_empty_input_takes_the_outputs_frame_count() {
    let mut out = [0.0f32; 7];
    let mut output = [&mut out[..]];
    let block = InputOutput::new(
        Planes::default(),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect("no input is any length");
    assert_eq!(block.frames(), Samples(7));
    assert_eq!(block.input().channels(), 0);
}

/// Through `split`, a processor writes the output from the input: a gain of 2
/// lands in every output sample.
#[test]
fn split_writes_the_output_from_the_input() {
    let src = [0.25f32, -0.5, 1.0];
    let input = [&src[..]];
    let mut out = [0.0f32; 3];
    let mut output = [&mut out[..]];
    let mut block = InputOutput::new(
        Planes::new(&input).expect("one plane"),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect("equal lengths");
    let (input, mut output) = block.split();
    for (o, i) in output
        .plane_mut(0)
        .samples_mut()
        .iter_mut()
        .zip(input.plane(0).iter())
    {
        *o = 2.0 * i;
    }
    assert_eq!(out, [0.5, -1.0, 2.0]);
}

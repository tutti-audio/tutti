use super::*;

/// An input holding fewer frames than the block is refused, naming the input,
/// the block's count and the input's.
#[test]
fn an_input_shorter_than_the_block_is_refused_with_both_counts() {
    let short = [0.0f32; 3];
    let input = [&short[..]];
    let mut out = [0.0f32; 4];
    let mut output = [&mut out[..]];
    let refused = InputOutput::new(
        Samples(4),
        Planes::new(&input).expect("one plane"),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect_err("3 input frames in a 4-frame block");
    assert_eq!(
        refused,
        BufferError::BlockLengthMismatch {
            side: BlockSide::Input,
            block: Samples(4),
            found: Samples(3)
        }
    );
}

/// An output holding more frames than the block is refused too: the block's
/// frames are never a truncation of a view.
#[test]
fn an_output_longer_than_the_block_is_refused() {
    let src = [0.0f32; 4];
    let input = [&src[..]];
    let mut long = [0.0f32; 5];
    let mut output = [&mut long[..]];
    let refused = InputOutput::new(
        Samples(4),
        Planes::new(&input).expect("one plane"),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect_err("5 output frames in a 4-frame block");
    assert_eq!(
        refused,
        BufferError::BlockLengthMismatch {
            side: BlockSide::Output,
            block: Samples(4),
            found: Samples(5)
        }
    );
}

/// With no channel on either side the block keeps its own frame count: a
/// processor with no audio still learns the block length.
#[test]
fn a_block_with_no_audio_keeps_its_frame_count() {
    let block = InputOutput::<f32>::new(Samples(512), Planes::default(), PlanesMut::default())
        .expect("empty views fit any block");
    assert_eq!(block.frames(), Samples(512));
    assert_eq!(block.input().channels(), 0);
}

/// An empty input pairs with an output of the block's length.
#[test]
fn an_empty_input_takes_any_block_length() {
    let mut out = [0.0f32; 7];
    let mut output = [&mut out[..]];
    let block = InputOutput::new(
        Samples(7),
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
        Samples(3),
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

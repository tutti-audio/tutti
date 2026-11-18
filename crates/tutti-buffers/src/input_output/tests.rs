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
    let mut block = block;
    assert_eq!(block.output().channels(), 0);
}

/// An empty input does not excuse the output: an output of another length
/// than the block is refused.
#[test]
fn an_empty_input_still_checks_the_output() {
    let mut out = [0.0f32; 6];
    let mut output = [&mut out[..]];
    let refused = InputOutput::new(
        Samples(7),
        Planes::default(),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect_err("6 output frames in a 7-frame block");
    assert_eq!(
        refused,
        BufferError::BlockLengthMismatch {
            side: BlockSide::Output,
            block: Samples(7),
            found: Samples(6)
        }
    );
}

/// With both views off the block's length, the input is the one named, and
/// the error text says which side.
#[test]
fn the_input_is_checked_first_and_named_in_the_text() {
    let src = [0.0f32; 2];
    let input = [&src[..]];
    let mut out = [0.0f32; 3];
    let mut output = [&mut out[..]];
    let refused = InputOutput::new(
        Samples(4),
        Planes::new(&input).expect("one plane"),
        PlanesMut::new(&mut output).expect("one plane"),
    )
    .expect_err("neither view holds 4 frames");
    assert_eq!(
        refused,
        BufferError::BlockLengthMismatch {
            side: BlockSide::Input,
            block: Samples(4),
            found: Samples(2)
        }
    );
    assert_eq!(
        refused.to_string(),
        format!(
            "the block is {} frames and its input holds {}",
            Samples(4),
            Samples(2)
        )
    );
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

/// A node-style function over `&mut InputOutput<'_, S>`: it writes through
/// `output()` twice, each reborrow ending before the next.
fn write_twice<S: Sample>(io: &mut InputOutput<'_, S>, first: S, second: S) {
    for mut plane in io.output().iter_mut() {
        plane.samples_mut().fill(first);
    }
    let mut out = io.output();
    out.slice_mut(Samples(2)..Samples(4))
        .iter_mut()
        .for_each(|mut plane| plane.samples_mut().fill(second));
}

/// A block over a per-block local table takes `&mut InputOutput<'_, S>`, and
/// two successive reborrows of its output both land in the audio.
#[test]
fn a_block_over_a_local_table_is_writable_twice_through_one_lifetime() {
    let src = [0.0f32; 4];
    let input = [&src[..]];
    let (mut l, mut r) = ([0.0f32; 4], [0.0f32; 4]);
    {
        let mut output = [&mut l[..], &mut r[..]];
        let mut io = InputOutput::new(
            Samples(4),
            Planes::new(&input).expect("one plane"),
            PlanesMut::new(&mut output).expect("two planes"),
        )
        .expect("equal frame counts");
        write_twice(&mut io, 1.0, 2.0);
    }
    assert_eq!((l, r), ([1.0, 1.0, 2.0, 2.0], [1.0, 1.0, 2.0, 2.0]));
}

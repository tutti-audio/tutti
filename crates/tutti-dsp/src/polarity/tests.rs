use super::*;
use tutti_buffers::InputOutput;
use tutti_buffers::{ChannelIndex, MAX_CHANNELS};
use tutti_buffers::{Planes, PlanesMut};

fn run(flips: &Polarity, input: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let mut out = vec![vec![9.0f32; input[0].len()]; input.len()];
    flips
        .process(
            &mut InputOutput::paired(
                Planes::from_vecs(input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    out
}

/// A chosen channel is the negation of its input bit for bit (a zero flips its sign), the
/// others are copies: channels 0 and 2 of four.
#[test]
fn the_chosen_channels_are_negated_exactly_and_the_rest_copied() {
    let input: Vec<Vec<f32>> = (0..4)
        .map(|c| vec![0.0, -0.0, 1.0, -0.25, 1e-30, f32::MAX, 0.1 + c as f32])
        .collect();
    let flips = Polarity::new(ChannelLayout::from(4u16), [0, 2, 2].map(ChannelIndex::of)).unwrap();
    let out = run(&flips, &input);
    for c in 0..4 {
        for (y, x) in out[c].iter().zip(&input[c]) {
            let want = if c.is_multiple_of(2) { -x } else { *x };
            assert_eq!(y.to_bits(), want.to_bits(), "channel {c}");
        }
    }
}

/// `all` inverts every channel of the width, and inverting twice is the identity.
#[test]
fn all_inverts_every_channel_and_twice_is_identity() {
    let input = vec![vec![0.5f32, -0.75, 1.0]; 3];
    let flips = Polarity::all(ChannelLayout::from(3u16));
    let once = run(&flips, &input);
    assert!(
        once.iter()
            .zip(&input)
            .all(|(o, i)| { o.iter().zip(i).all(|(y, x)| *y == -x) })
    );
    assert_eq!(run(&flips, &once), input);
}

/// A channel the width lacks is refused by name, whatever its position in the list, and
/// naming none inverts none.
#[test]
fn a_channel_past_the_width_is_refused() {
    for bad in [2, 5, MAX_CHANNELS - 1].map(ChannelIndex::of) {
        assert_eq!(
            Polarity::new(ChannelLayout::STEREO, [ChannelIndex::of(1), bad]),
            Err(ChannelOutOfRange {
                channel: bad,
                width: 2
            })
        );
    }
    let none = Polarity::new(ChannelLayout::STEREO, []).unwrap();
    assert!(
        !none.inverts(ChannelIndex::of(0))
            && !none.inverts(ChannelIndex::of(1))
            && !none.inverts(ChannelIndex::of(5))
    );
    assert_eq!(none.width(), ChannelLayout::STEREO);
}

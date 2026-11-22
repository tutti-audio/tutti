//! A topology placed on a destination of which only the width is known
//! (`fold_to_width`): by speaker where both ends have an order, by index
//! where either has none, and never a guess.

use tutti_buffers::{ChannelLayout, ChannelTopology, Speaker};
use tutti_buffers::{Float, FoldError};
use tutti_graph::{MainOutputError, fold_to_width};

const M: f32 = <f32 as Float>::MINUS_3_DB;

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::default_for(ChannelLayout::from(n))
}

fn width(n: u16) -> ChannelLayout {
    ChannelLayout::from(n)
}

/// A channel no speaker names is refused whatever the destination, even one
/// that has an order for it.
#[test]
fn a_channel_no_speaker_names_is_refused_for_every_destination() {
    use Speaker::*;
    let lfe2 = ChannelTopology::new([FrontLeft, FrontRight, FrontCenter, LowFrequency, Unknown(5)])
        .unwrap();
    for dst in [1u16, 2, 5, 6] {
        assert_eq!(
            fold_to_width::<f32>(&lfe2, width(dst)).map(|_| ()),
            Err(FoldError::Unnamed {
                end: tutti_buffers::FoldEnd::Source,
                channel: 4
            }),
            "into {dst}"
        );
    }
}

/// An all-unassigned source is independent buses: channel `i` stays channel
/// `i`, a wider destination is zero-filled, and a narrower one is refused, even
/// at four, which `ChannelTopology::default_for` would read as quad.
#[test]
fn an_unassigned_source_goes_by_index_and_only_widens() {
    let four = ChannelTopology::unassigned(ChannelLayout::QUAD);
    let mut out = [9.0f32; 6];
    fold_to_width::<f32>(&four, width(6))
        .unwrap()
        .frame(&[1.0, 2.0, 3.0, 4.0], &mut out);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);
    assert_eq!(
        fold_to_width::<f32>(&four, width(2)).map(|_| ()),
        Err(FoldError::Unplaced {
            channel: 2,
            speaker: Speaker::Unassigned
        })
    );
}

/// A bare four read in the engine's order is a quad: what a caller that reads
/// a count that way gets by passing `default_for` first.
#[test]
fn a_bare_count_in_the_engines_order_places_by_speaker() {
    let mut out = [0.0f32; 2];
    fold_to_width::<f32>(
        &ChannelTopology::default_for(ChannelLayout::QUAD),
        ChannelLayout::STEREO,
    )
    .unwrap()
    .frame(&[1.0, 2.0, 3.0, 4.0], &mut out);
    assert_eq!(out, [1.0 + M * 3.0, 2.0 + M * 4.0]);
}

/// A named source widens into a width with no order by index, and a narrowing
/// into one is refused: 5.1 into five channels would lose a surround.
#[test]
fn a_named_source_goes_by_index_into_a_width_with_no_order() {
    let mut five = [0.0f32; 5];
    fold_to_width::<f32>(&smpte(2), width(5))
        .unwrap()
        .frame(&[1.0, 2.0], &mut five);
    assert_eq!(five, [1.0, 2.0, 0.0, 0.0, 0.0]);
    assert!(matches!(
        fold_to_width::<f32>(&smpte(6), width(5)),
        Err(FoldError::Unplaced { channel: 5, .. })
    ));
}

/// Where both ends have an order the fold is `between` the two, so a file that
/// names its channels in another order is permuted: back before side, as WAV
/// writes 7.1.
#[test]
fn a_named_source_with_an_order_is_placed_by_speaker() {
    use Speaker::*;
    let wav71 = ChannelTopology::new([
        FrontLeft,
        FrontRight,
        FrontCenter,
        LowFrequency,
        BackLeft,
        BackRight,
        SideLeft,
        SideRight,
    ])
    .unwrap();
    let mut out = [0.0f32; 8];
    fold_to_width::<f32>(&wav71, width(8))
        .unwrap()
        .frame(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], &mut out);
    assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 6.0, 7.0, 4.0, 5.0]);

    let mut stereo = [0.0f32; 2];
    fold_to_width::<f32>(&smpte(6), ChannelLayout::STEREO)
        .unwrap()
        .frame(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut stereo);
    assert_eq!(stereo, [M, M], "a centre-only 5.1 reaches both sides");
}

/// Zero channels at either end are refused, with the one zero-width error.
#[test]
fn a_zero_width_at_either_end_is_refused() {
    let zero = FoldError::Buffer(tutti_buffers::BufferError::ZeroWidth);
    assert_eq!(
        fold_to_width::<f32>(&ChannelTopology::new([]).unwrap(), width(2)).map(|_| ()),
        Err(zero)
    );
    assert_eq!(
        fold_to_width::<f32>(&smpte(2), ChannelLayout::EMPTY).map(|_| ()),
        Err(zero)
    );
    assert_eq!(
        fold_to_width::<f32>(&ChannelTopology::new([]).unwrap(), width(3)).map(|_| ()),
        Err(zero)
    );
}

#[test]
fn a_main_output_error_says_what_was_refused() {
    let wide = MainOutputError::Width {
        declared: ChannelLayout::QUAD,
        requested: width(5),
    };
    assert!(wide.to_string().contains("declared"));
    let folded = MainOutputError::from(FoldError::Unplaced {
        channel: 2,
        speaker: Speaker::Unassigned,
    });
    assert_eq!(
        folded.to_string(),
        "source channel 2 (Unassigned) has no place in the destination"
    );
}

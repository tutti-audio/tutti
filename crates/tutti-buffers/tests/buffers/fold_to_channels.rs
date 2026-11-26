//! A topology placed on output channels (`Fold::to_channels`): by speaker where both ends have an order, by index
//! where either has none, and never a guess.

use tutti_buffers::{
    AmbisonicFormat, AmbisonicOrder, ChannelLayout, ChannelSet, ChannelTopology, Speaker,
};
use tutti_buffers::{Float, Fold, FoldError, FoldLevels};
use tutti_units::Amplitude;

fn to_width(src: &ChannelTopology, dst: ChannelLayout) -> Result<Fold, FoldError> {
    Fold::to_channels(&ChannelSet::from(*src), &ChannelSet::default_for(dst)).build()
}

const M: f32 = <f32 as Float>::MINUS_3_DB;

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::default_for(ChannelLayout::from(n)).unwrap()
}

fn width(n: u16) -> ChannelLayout {
    ChannelLayout::from(n)
}

/// A destination wider than a topology holds is unassigned channels, so it takes
/// the source by index: the source's channels first and zero after, never a
/// matrix, and a source that names no speaker is still refused.
#[test]
fn a_destination_wider_than_a_topology_takes_channels_by_index() {
    let fold = to_width(&smpte(6), width(20)).unwrap();
    assert_eq!(fold.dst_layout(), width(20));
    let src: Vec<f32> = (1..=6).map(|c| c as f32).collect();
    let mut out = vec![9.0f32; 20];
    fold.frame(&src, &mut out);
    assert_eq!(&out[..6], &src[..]);
    assert!(out[6..].iter().all(|&s| s == 0.0));
    let lfe2 = ChannelTopology::new([Speaker::FrontLeft, Speaker::Unknown(5)]).unwrap();
    assert!(matches!(
        to_width(&lfe2, width(20)),
        Err(FoldError::Unnamed { .. })
    ));
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
            to_width(&lfe2, width(dst)).map(|_| ()),
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
    let four = ChannelTopology::unassigned(ChannelLayout::QUAD).unwrap();
    let mut out = [9.0f32; 6];
    to_width(&four, width(6))
        .unwrap()
        .frame(&[1.0, 2.0, 3.0, 4.0], &mut out);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);
    assert_eq!(
        to_width(&four, width(2)).map(|_| ()),
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
    to_width(&tutti_buffers::Bed::Quad.topology(), ChannelLayout::STEREO)
        .unwrap()
        .frame(&[1.0, 2.0, 3.0, 4.0], &mut out);
    assert_eq!(out, [1.0 + M * 3.0, 2.0 + M * 4.0]);
}

/// A named source widens into a width with no order by index, and a narrowing
/// into one is refused: 5.1 into five channels would lose a surround.
#[test]
fn a_named_source_goes_by_index_into_a_width_with_no_order() {
    let mut five = [0.0f32; 5];
    to_width(&smpte(2), width(5))
        .unwrap()
        .frame(&[1.0, 2.0], &mut five);
    assert_eq!(five, [1.0, 2.0, 0.0, 0.0, 0.0]);
    assert!(matches!(
        to_width(&smpte(6), width(5)),
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
    to_width(&wav71, width(8))
        .unwrap()
        .frame(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], &mut out);
    assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 6.0, 7.0, 4.0, 5.0]);

    let mut stereo = [0.0f32; 2];
    to_width(&smpte(6), ChannelLayout::STEREO)
        .unwrap()
        .frame(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut stereo);
    assert_eq!(stereo, [M, M], "a centre-only 5.1 reaches both sides");
}

/// Zero channels at either end are refused, with the one zero-width error.
#[test]
fn a_zero_width_at_either_end_is_refused() {
    let zero = FoldError::Buffer(tutti_buffers::BufferError::ZeroWidth);
    assert_eq!(
        to_width(&ChannelTopology::new([]).unwrap(), width(2)).map(|_| ()),
        Err(zero)
    );
    assert_eq!(
        to_width(&smpte(2), ChannelLayout::EMPTY).map(|_| ()),
        Err(zero)
    );
    assert_eq!(
        to_width(&ChannelTopology::new([]).unwrap(), width(3)).map(|_| ()),
        Err(zero)
    );
}

/// A level that is not a gain is refused on every path, including the by-index
/// one, which has no levels of its own.
#[test]
fn a_level_that_is_not_a_gain_is_refused_even_by_index() {
    let levels = FoldLevels {
        centre: Amplitude::new(f64::NAN),
        ..FoldLevels::default()
    };
    for dst in [width(2), width(5), width(20)] {
        let set = ChannelSet::default_for(dst);
        assert!(
            matches!(
                Fold::<f32>::to_channels(&ChannelSet::from(smpte(2)), &set)
                    .levels(levels)
                    .build(),
                Err(FoldError::NotAGain { .. })
            ),
            "into {dst:?}"
        );
    }
}

/// An ambisonic source is not speaker feeds, so onto output channels it goes by
/// index (where `Fold::between` would decode or refuse it): first order's four
/// components keep their channels in a quad output, and a stereo output, which
/// is narrower, is refused rather than truncated.
#[test]
fn an_ambisonic_source_goes_by_index() {
    let foa = ChannelTopology::from(AmbisonicFormat::ambix(AmbisonicOrder::of(1)));
    let fold = to_width(&foa, width(4)).unwrap();
    let src = [1.0f32, 2.0, 3.0, 4.0];
    let mut out = [9.0f32; 4];
    fold.frame(&src, &mut out);
    assert_eq!(out, src, "channel i stays channel i");
    assert!(matches!(
        to_width(&foa, width(2)),
        Err(FoldError::Unplaced { .. })
    ));
}

/// `smpte(n)` with its first two speakers swapped: the same speakers in another order.
fn swapped_front(n: u16) -> ChannelTopology {
    let mut speakers = smpte(n).positions().unwrap().to_vec();
    speakers.swap(0, 1);
    ChannelTopology::new(speakers).unwrap()
}

/// Two sets of one width in different orders are folded by speaker, not copied: a 5.1 whose
/// front pair is swapped, into the default 5.1, moves channel 0 to 1 and 1 to 0 and leaves the
/// rest where they are.
#[test]
fn a_same_width_reorder_moves_the_channels() {
    let fold = Fold::<f32>::to_channels(
        &ChannelSet::from(swapped_front(6)),
        &ChannelSet::default_for(width(6)),
    )
    .build()
    .unwrap();
    assert!(!fold.is_identity());
    let src: Vec<f32> = (1..=6).map(|c| c as f32).collect();
    let mut out = vec![0.0f32; 6];
    fold.frame(&src, &mut out);
    assert_eq!(out, [2.0, 1.0, 3.0, 4.0, 5.0, 6.0]);
}

/// A source with no topology (past `MAX_CHANNELS`) is channels by index: into a destination as
/// wide or wider, the channels stay where they are and the rest is zero; into a named
/// destination that is narrower it is `Unplaced`, never a truncation.
#[test]
fn a_source_with_no_topology_goes_by_index() {
    let wide_set = ChannelSet::unassigned(width(24));
    assert!(wide_set.topology().is_none());
    let wider = ChannelSet::unassigned(width(30));
    let fold = Fold::<f32>::to_channels(&wide_set, &wider).build().unwrap();
    let src: Vec<f32> = (1..=24).map(|c| c as f32).collect();
    let mut out = vec![9.0f32; 30];
    fold.frame(&src, &mut out);
    assert_eq!(&out[..24], &src[..]);
    assert!(out[24..].iter().all(|&s| s == 0.0));
    let narrowed = Fold::<f32>::to_channels(&wide_set, &ChannelSet::default_for(width(2))).build();
    assert!(
        matches!(narrowed, Err(FoldError::Unplaced { .. })),
        "{narrowed:?}"
    );
}

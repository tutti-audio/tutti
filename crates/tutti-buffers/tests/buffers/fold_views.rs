//! The fold's appliers over views: each writes the analytic figure of the
//! pair, and each refuses a width or length mismatch before it writes.

use tutti_buffers::Float;
use tutti_buffers::{BufferError, Fold, Interleaved, InterleavedMut, MonoMut, Planes, PlanesMut};
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_units::Samples;

const M: f32 = <f32 as Float>::MINUS_3_DB;

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::default_for(ChannelLayout::from(n)).unwrap()
}

fn five_one_to_stereo() -> Fold {
    Fold::between(&smpte(6), &smpte(2)).unwrap()
}

/// Frame `i` of a 5.1 source: `FL FR C LFE SL SR`, each channel its own figure.
fn frame(i: usize) -> [f32; 6] {
    let b = i as f32 * 0.01;
    [0.1 + b, 0.2 + b, 0.3 + b, 0.4 + b, 0.5 + b, 0.6 + b]
}

/// The ITU stereo figures of [`frame`], in the fold's summing order: the direct
/// channel, then the centre, then the surround, each folded one at −3 dB.
fn stereo_of(f: &[f32; 6]) -> [f32; 2] {
    [f[0] + M * f[2] + M * f[4], f[1] + M * f[2] + M * f[5]]
}

/// 150 frames: past the planar group's 64-frame chunk, so a chunk edge is run.
const FRAMES: usize = 150;

fn interleaved_source() -> Vec<f32> {
    (0..FRAMES).flat_map(frame).collect()
}

fn planar_source() -> Vec<Vec<f32>> {
    (0..6)
        .map(|c| (0..FRAMES).map(|i| frame(i)[c]).collect())
        .collect()
}

#[test]
fn fold_writes_the_itu_figures_over_interleaved_views() {
    let src = interleaved_source();
    let mut dst = vec![f32::NAN; FRAMES * 2];
    five_one_to_stereo()
        .fold(
            Interleaved::new(&src, ChannelLayout::from(6u16)).unwrap(),
            InterleavedMut::new(&mut dst, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
    for (i, got) in dst.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(*got, stereo_of(&frame(i)), "frame {i}");
    }
}

#[test]
fn fold_planar_writes_the_itu_figures_into_planes() {
    let src = planar_source();
    let mut dst = vec![vec![f32::NAN; FRAMES]; 2];
    five_one_to_stereo()
        .fold_planar(
            Planes::from_vecs(&src).unwrap(),
            PlanesMut::from_vecs(&mut dst).unwrap(),
        )
        .unwrap();
    for (i, (&l, &r)) in dst[0].iter().zip(&dst[1]).enumerate() {
        assert_eq!([l, r], stereo_of(&frame(i)), "frame {i}");
    }
}

#[test]
fn fold_planar_to_interleaved_writes_the_itu_figures_across_chunk_edges() {
    let src = planar_source();
    let mut dst = vec![f32::NAN; FRAMES * 2];
    five_one_to_stereo()
        .fold_planar_to_interleaved(
            Planes::from_vecs(&src).unwrap(),
            InterleavedMut::new(&mut dst, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
    for (i, got) in dst.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(*got, stereo_of(&frame(i)), "frame {i}");
    }
}

/// One destination channel is written and no other: the right channel of a
/// stereo fold, into a buffer that held a sentinel.
#[test]
fn fold_planar_channel_writes_one_destination_channel() {
    let src = planar_source();
    let mut right = vec![f32::NAN; FRAMES];
    five_one_to_stereo()
        .fold_planar_channel(
            Planes::from_vecs(&src).unwrap(),
            1,
            MonoMut::new(&mut right),
        )
        .unwrap();
    for (i, got) in right.iter().enumerate() {
        assert_eq!(*got, stereo_of(&frame(i))[1], "frame {i}");
    }
}

#[test]
fn an_applier_over_a_slice_of_views_folds_only_those_frames() {
    let src = interleaved_source();
    let mut dst = vec![0.0f32; 20];
    let from = Interleaved::new(&src, ChannelLayout::from(6u16)).unwrap();
    let mut to = InterleavedMut::new(&mut dst, ChannelLayout::STEREO).unwrap();
    five_one_to_stereo()
        .fold(
            from.slice(Samples::new(5)..Samples::new(15)),
            to.slice_mut(Samples::new(0)..Samples::new(10)),
        )
        .unwrap();
    assert_eq!(&dst[..2], stereo_of(&frame(5)));
    assert_eq!(&dst[18..], stereo_of(&frame(14)));
}

/// The one widening case: a stereo source into 5.1 puts its pair on the front
/// and leaves the rest zero.
#[test]
fn fold_widens_by_zero_filling_what_nothing_feeds() {
    let fold = Fold::between(&smpte(2), &smpte(6)).unwrap();
    let src = [0.25f32, -0.5];
    let mut dst = [9.0f32; 6];
    fold.fold(
        Interleaved::new(&src, ChannelLayout::STEREO).unwrap(),
        InterleavedMut::new(&mut dst, ChannelLayout::from(6u16)).unwrap(),
    )
    .unwrap();
    assert_eq!(dst, [0.25, -0.5, 0.0, 0.0, 0.0, 0.0]);
}

/// The input's width is checked first, then the output's, then the frame
/// counts, and nothing is written for any of them.
#[test]
fn every_applier_refuses_a_width_or_length_mismatch_and_writes_nothing() {
    let fold = five_one_to_stereo();
    let six = ChannelLayout::from(6u16);
    let src = vec![0.5f32; 6 * 4];
    let planes = vec![vec![0.5f32; 4]; 6];
    let narrow_planes = vec![vec![0.5f32; 4]; 5];

    // Input of another width: the view's width, then the fold's.
    let want = BufferError::WidthMismatch {
        src: ChannelLayout::from(5u16),
        dst: six,
    };
    let five = vec![0.5f32; 5 * 4];
    let mut out = vec![7.0f32; 8];
    assert_eq!(
        fold.fold(
            Interleaved::new(&five, ChannelLayout::from(5u16)).unwrap(),
            InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap()
        ),
        Err(want)
    );
    let mut planar_out = vec![vec![7.0f32; 4]; 2];
    assert_eq!(
        fold.fold_planar(
            Planes::from_vecs(&narrow_planes).unwrap(),
            PlanesMut::from_vecs(&mut planar_out).unwrap()
        ),
        Err(want)
    );
    assert_eq!(
        fold.fold_planar_to_interleaved(
            Planes::from_vecs(&narrow_planes).unwrap(),
            InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap()
        ),
        Err(want)
    );
    let mut one = vec![7.0f32; 4];
    assert_eq!(
        fold.fold_planar_channel(
            Planes::from_vecs(&narrow_planes).unwrap(),
            0,
            MonoMut::new(&mut one)
        ),
        Err(want)
    );

    // Output of another width: the fold's width, then the view's.
    let want = BufferError::WidthMismatch {
        src: ChannelLayout::STEREO,
        dst: ChannelLayout::from(3u16),
    };
    let mut three = vec![7.0f32; 12];
    assert_eq!(
        fold.fold(
            Interleaved::new(&src, six).unwrap(),
            InterleavedMut::new(&mut three, ChannelLayout::from(3u16)).unwrap()
        ),
        Err(want)
    );
    assert_eq!(
        fold.fold_planar_to_interleaved(
            Planes::from_vecs(&planes).unwrap(),
            InterleavedMut::new(&mut three, ChannelLayout::from(3u16)).unwrap()
        ),
        Err(want)
    );
    let mut three_planes = vec![vec![7.0f32; 4]; 3];
    assert_eq!(
        fold.fold_planar(
            Planes::from_vecs(&planes).unwrap(),
            PlanesMut::from_vecs(&mut three_planes).unwrap()
        ),
        Err(want)
    );

    // Frame counts that differ.
    let want = BufferError::LengthMismatch {
        src: Samples::new(4),
        dst: Samples::new(3),
    };
    let mut short = vec![7.0f32; 6];
    assert_eq!(
        fold.fold(
            Interleaved::new(&src, six).unwrap(),
            InterleavedMut::new(&mut short, ChannelLayout::STEREO).unwrap()
        ),
        Err(want)
    );
    assert_eq!(
        fold.fold_planar_to_interleaved(
            Planes::from_vecs(&planes).unwrap(),
            InterleavedMut::new(&mut short, ChannelLayout::STEREO).unwrap()
        ),
        Err(want)
    );
    let mut short_planes = vec![vec![7.0f32; 3]; 2];
    assert_eq!(
        fold.fold_planar(
            Planes::from_vecs(&planes).unwrap(),
            PlanesMut::from_vecs(&mut short_planes).unwrap()
        ),
        Err(want)
    );
    let mut short_one = vec![7.0f32; 3];
    assert_eq!(
        fold.fold_planar_channel(
            Planes::from_vecs(&planes).unwrap(),
            0,
            MonoMut::new(&mut short_one)
        ),
        Err(want)
    );

    assert!(
        out.iter()
            .chain(&one)
            .chain(&three)
            .chain(&short)
            .all(|&x| x == 7.0)
    );
    assert!(
        planar_out
            .iter()
            .chain(&three_planes)
            .chain(&short_planes)
            .flatten()
            .all(|&x| x == 7.0)
    );
    assert!(short_one.iter().all(|&x| x == 7.0));
}

#[test]
#[should_panic(expected = "destination channel 2 is out of bounds for 2 channels")]
fn fold_planar_channel_past_the_destination_width_panics() {
    let planes = vec![vec![0.0f32; 4]; 6];
    let mut out = vec![0.0f32; 4];
    let _ = five_one_to_stereo().fold_planar_channel(
        Planes::from_vecs(&planes).unwrap(),
        2,
        MonoMut::new(&mut out),
    );
}

/// The fold is generic over the float it sums in: an `f64` fold lands the
/// centre at `f64`'s −3 dB, which is not `f32`'s widened.
#[test]
fn an_f64_fold_sums_in_f64() {
    let fold: Fold<f64> = Fold::between(&smpte(6), &smpte(2)).unwrap();
    let centre = [0.0f64, 0.0, 1.0, 0.0, 0.0, 0.0];
    let mut out = [0.0f64; 2];
    fold.fold(
        Interleaved::new(&centre, ChannelLayout::from(6u16)).unwrap(),
        InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap(),
    )
    .unwrap();
    assert_eq!(out, [core::f64::consts::FRAC_1_SQRT_2; 2]);
    assert_ne!(out[0], f64::from(M));
    assert_eq!(
        fold.contributions(0),
        [(0, 1.0), (2, f64::MINUS_3_DB), (4, f64::MINUS_3_DB)]
    );
}

/// A mono mix is the one row that adds two sums: the figure is the closed form
/// `(L + R)/2` for a stereo pair, through every applier.
#[test]
fn the_mono_mix_is_the_average_through_every_applier() {
    let fold = Fold::between(&smpte(2), &smpte(1)).unwrap();
    let src = [0.1f32, 0.2, 0.3, 0.5];
    let want = [(0.1f32 + 0.2) * 0.5, (0.3 + 0.5) * 0.5];
    let mut flat = [0.0f32; 2];
    fold.fold(
        Interleaved::new(&src, ChannelLayout::STEREO).unwrap(),
        InterleavedMut::new(&mut flat, ChannelLayout::MONO).unwrap(),
    )
    .unwrap();
    assert_eq!(flat, want);
    let planes = vec![vec![0.1f32, 0.3], vec![0.2, 0.5]];
    let mut planar = vec![vec![0.0f32; 2]];
    fold.fold_planar(
        Planes::from_vecs(&planes).unwrap(),
        PlanesMut::from_vecs(&mut planar).unwrap(),
    )
    .unwrap();
    assert_eq!(planar[0], want);
}

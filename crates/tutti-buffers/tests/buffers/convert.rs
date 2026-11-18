//! `convert`, `interleave` and `deinterleave`: figures and mismatch errors.

use tutti_buffers::{
    BufferError, Interleaved, InterleavedMut, Mono, MonoMut, Planes, PlanesMut, convert,
    convert_counting_clips, convert_mono, convert_planes, deinterleave, interleave,
};
use tutti_types::{ChannelLayout, LayoutError, Samples};

const ST: ChannelLayout = ChannelLayout::STEREO;

#[test]
fn convert_applies_the_sample_rule_to_every_sample() {
    let src = [0.5f32, -1.0, 2.0, f32::NAN, 0.0, -2.0];
    let mut dst = [7i16; 6];
    convert(
        Interleaved::new(&src, ST).unwrap(),
        InterleavedMut::new(&mut dst, ST).unwrap(),
    )
    .unwrap();
    assert_eq!(dst, [16384, -32768, 32767, 0, 0, -32768]);
}

#[test]
fn convert_int_to_float_and_back_restores_the_integers() {
    let pcm = [i16::MIN, -1, 0, 1, 12345, i16::MAX];
    let mut f = [0.0f32; 6];
    let mut back = [0i16; 6];
    let l = ChannelLayout::from(3u16);
    convert(
        Interleaved::new(&pcm, l).unwrap(),
        InterleavedMut::new(&mut f, l).unwrap(),
    )
    .unwrap();
    assert_eq!(f[0], -1.0);
    convert(
        Interleaved::new(&f, l).unwrap(),
        InterleavedMut::new(&mut back, l).unwrap(),
    )
    .unwrap();
    assert_eq!(back, pcm);
}

#[test]
fn convert_refuses_a_width_mismatch_and_writes_nothing() {
    let src = [0.5f32; 4];
    let mut dst = [7i16; 4];
    let err = convert(
        Interleaved::new(&src, ST).unwrap(),
        InterleavedMut::new(&mut dst, ChannelLayout::MONO).unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BufferError::WidthMismatch {
            src: ST,
            dst: ChannelLayout::MONO
        }
    );
    assert_eq!(dst, [7; 4]);
}

#[test]
fn convert_refuses_a_length_mismatch_instead_of_copying_short() {
    let src = [0.5f32; 6];
    let mut dst = [7i16; 4];
    let err = convert(
        Interleaved::new(&src, ST).unwrap(),
        InterleavedMut::new(&mut dst, ST).unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        }
    );
    assert_eq!(dst, [7; 4]);
}

#[test]
fn interleave_places_frame_i_channel_c_at_i_times_stride_plus_c() {
    let (a, b, c) = ([1i16, 2], [3, 4], [5, 6]);
    let mut out = [0i16; 6];
    interleave(
        Planes::new(&[&a, &b, &c]).unwrap(),
        InterleavedMut::new(&mut out, ChannelLayout::from(3u16)).unwrap(),
    )
    .unwrap();
    assert_eq!(out, [1, 3, 5, 2, 4, 6]);
}

#[test]
fn deinterleave_places_frame_i_channel_c_in_plane_c_at_i() {
    let src = [1.0f64, 3.0, 5.0, 2.0, 4.0, 6.0];
    let (mut a, mut b, mut c) = ([0.0; 2], [0.0; 2], [0.0; 2]);
    let mut slices = [&mut a[..], &mut b[..], &mut c[..]];
    deinterleave(
        Interleaved::new(&src, ChannelLayout::from(3u16)).unwrap(),
        PlanesMut::new(&mut slices).unwrap(),
    )
    .unwrap();
    assert_eq!((a, b, c), ([1.0, 2.0], [3.0, 4.0], [5.0, 6.0]));
}

#[test]
fn interleave_of_a_slice_writes_only_that_slice() {
    let (l, r) = ([1i32, 2, 3, 4], [5, 6, 7, 8]);
    let slices = [&l[..], &r[..]];
    let p = Planes::new(&slices).unwrap().slice(Samples(1)..Samples(3));
    let mut out = [0i32; 4];
    interleave(p, InterleavedMut::new(&mut out, ST).unwrap()).unwrap();
    assert_eq!(out, [2, 6, 3, 7]);
}

#[test]
fn interleave_refuses_mismatches_and_writes_nothing() {
    let (l, r) = ([1.0f32; 3], [2.0f32; 3]);
    let mut out = [9.0f32; 4];
    assert_eq!(
        interleave(
            Planes::new(&[&l, &r]).unwrap(),
            InterleavedMut::new(&mut out, ST).unwrap()
        ),
        Err(BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        })
    );
    assert_eq!(
        interleave(
            Planes::new(&[&l]).unwrap(),
            InterleavedMut::new(&mut out, ST).unwrap()
        ),
        Err(BufferError::WidthMismatch {
            src: ChannelLayout::MONO,
            dst: ST
        })
    );
    assert_eq!(out, [9.0; 4]);
}

#[test]
fn deinterleave_refuses_mismatches_and_writes_nothing() {
    let src = [1.0f32; 6];
    let (mut a, mut b) = ([9.0f32; 2], [9.0f32; 2]);
    let mut slices = [&mut a[..], &mut b[..]];
    assert_eq!(
        deinterleave(
            Interleaved::new(&src, ST).unwrap(),
            PlanesMut::new(&mut slices).unwrap()
        ),
        Err(BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        })
    );
    let mut slices = [&mut a[..]];
    assert_eq!(
        deinterleave(
            Interleaved::new(&src[..2], ST).unwrap(),
            PlanesMut::new(&mut slices).unwrap()
        ),
        Err(BufferError::WidthMismatch {
            src: ST,
            dst: ChannelLayout::MONO
        })
    );
    assert_eq!((a, b), ([9.0; 2], [9.0; 2]));
}

#[test]
fn interleave_then_deinterleave_returns_the_planes() {
    let (l, r) = ([0.1f32, 0.2, 0.3], [-0.1, -0.2, -0.3]);
    let mut flat = [0.0f32; 6];
    interleave(
        Planes::new(&[&l, &r]).unwrap(),
        InterleavedMut::new(&mut flat, ST).unwrap(),
    )
    .unwrap();
    let (mut l2, mut r2) = ([0.0f32; 3], [0.0f32; 3]);
    let mut slices = [&mut l2[..], &mut r2[..]];
    deinterleave(
        Interleaved::new(&flat, ST).unwrap(),
        PlanesMut::new(&mut slices).unwrap(),
    )
    .unwrap();
    assert_eq!((l2, r2), (l, r));
}

#[test]
fn buffer_errors_say_what_was_refused() {
    assert!(
        BufferError::ZeroWidth
            .to_string()
            .contains("at least one channel")
    );
    let e = BufferError::PartialFrame { len: 5, width: ST };
    assert!(e.to_string().contains("5 samples") && e.to_string().contains("Stereo"));
    let e = BufferError::from(LayoutError::TooWide(70_000));
    assert!(e.to_string().contains("70000"));
}

/// Planes the caller sized (`from_vecs`) take the strict path: every sample is
/// overwritten, so a reused table keeps nothing from its last use.
#[test]
fn deinterleave_fills_planes_the_caller_sized_and_overwrites_stale_samples() {
    let buf = [1.0f32, -1.0, 2.0, -2.0, 3.0, -3.0];
    let mut planes = vec![vec![99.0f32; 3], vec![99.0f32; 3]];
    deinterleave(
        Interleaved::new(&buf, ST).unwrap(),
        PlanesMut::from_vecs(&mut planes).unwrap(),
    )
    .unwrap();
    assert_eq!(planes[0], [1.0, 2.0, 3.0]);
    assert_eq!(planes[1], [-1.0, -2.0, -3.0]);
}

/// A table of another width than the source is refused and left as it was,
/// where the lenient helper silently emptied the extra planes.
#[test]
fn deinterleave_refuses_a_wider_table_and_leaves_it_untouched() {
    let buf = [1.0f32, 2.0];
    let mut planes = vec![vec![99.0f32], vec![99.0f32], vec![99.0f32]];
    assert_eq!(
        deinterleave(
            Interleaved::new(&buf, ST).unwrap(),
            PlanesMut::from_vecs(&mut planes).unwrap()
        ),
        Err(BufferError::WidthMismatch {
            src: ST,
            dst: ChannelLayout::from(3u16)
        })
    );
    assert_eq!(planes, [[99.0], [99.0], [99.0]]);
}

/// Format and layout change in one pass, by the rule in `Sample`.
#[test]
fn interleave_converts_the_format_in_the_same_pass() {
    let planes = vec![vec![16384i16, 0, -32768], vec![8192, -16384, 0]];
    let mut out = [9.0f32; 6];
    interleave(
        Planes::from_vecs(&planes).unwrap(),
        InterleavedMut::new(&mut out, ST).unwrap(),
    )
    .unwrap();
    assert_eq!(out, [0.5, 0.25, 0.0, -0.5, -1.0, 0.0]);
}

#[test]
fn deinterleave_converts_the_format_in_the_same_pass() {
    let src = [0.5f32, -1.0, 2.0, f32::NAN];
    let mut planes = vec![vec![0i16; 2], vec![0i16; 2]];
    deinterleave(
        Interleaved::new(&src, ST).unwrap(),
        PlanesMut::from_vecs(&mut planes).unwrap(),
    )
    .unwrap();
    assert_eq!(planes, [[16384, 32767], [-32768, 0]]);
}

#[test]
fn convert_planes_converts_plane_by_plane() {
    let src = vec![vec![0.5f32, -0.5], vec![1.0, -1.0]];
    let mut dst = vec![vec![0i16; 2]; 2];
    convert_planes(
        Planes::from_vecs(&src).unwrap(),
        PlanesMut::from_vecs(&mut dst).unwrap(),
    )
    .unwrap();
    assert_eq!(dst, [[16384, -16384], [32767, -32768]]);
}

#[test]
fn convert_planes_refuses_mismatches_and_writes_nothing() {
    let src = vec![vec![0.5f32; 3], vec![0.5f32; 3]];
    let mut short = vec![vec![7i16; 2]; 2];
    assert_eq!(
        convert_planes(
            Planes::from_vecs(&src).unwrap(),
            PlanesMut::from_vecs(&mut short).unwrap()
        ),
        Err(BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        })
    );
    let mut narrow = vec![vec![7i16; 3]];
    assert_eq!(
        convert_planes(
            Planes::from_vecs(&src).unwrap(),
            PlanesMut::from_vecs(&mut narrow).unwrap()
        ),
        Err(BufferError::WidthMismatch {
            src: ST,
            dst: ChannelLayout::MONO
        })
    );
    assert_eq!((short, narrow), (vec![vec![7; 2]; 2], vec![vec![7; 3]]));
}

#[test]
fn interleave_inverts_deinterleave_at_every_width() {
    for width in 1u16..=8 {
        let layout = ChannelLayout::from(width);
        let data: Vec<f32> = (0..usize::from(width) * 37).map(|i| i as f32).collect();
        let mut planes = vec![vec![0.0f32; 37]; usize::from(width)];
        deinterleave(
            Interleaved::new(&data, layout).unwrap(),
            PlanesMut::from_vecs(&mut planes).unwrap(),
        )
        .unwrap();
        let mut back = vec![f32::NAN; data.len()];
        interleave(
            Planes::from_vecs(&planes).unwrap(),
            InterleavedMut::new(&mut back, layout).unwrap(),
        )
        .unwrap();
        assert_eq!(back, data, "width {width}");
    }
}

/// More planes than a layout holds are refused with the layout's own error,
/// carrying the refused count.
#[test]
fn planes_past_a_layout_carry_the_layout_error_and_the_refused_count() {
    use std::error::Error;
    let planes = vec![Vec::<f32>::new(); usize::from(u16::MAX) + 1];
    let refused = Planes::from_vecs(&planes).unwrap_err();
    assert_eq!(
        refused,
        BufferError::Layout(LayoutError::TooWide(u64::from(u16::MAX) + 1))
    );
    // `transparent` forwards `source()` to the layout error's own, which has none.
    assert!(refused.source().is_none());
    assert!(refused.to_string().contains("65536"));
}

#[test]
fn convert_mono_follows_the_sample_rule() {
    let src = [i16::MIN, 0, i16::MAX];
    let mut wide = [9.0f32; 3];
    convert_mono(Mono::new(&src), MonoMut::new(&mut wide)).unwrap();
    assert_eq!(wide, [-1.0, 0.0, 32767.0 / 32768.0]);
    // float to integer is x * 2^15, rounded half to even: 0.5 / 2^15 is a half, rounds to 0.
    let half = [0.5f32 / 32768.0, 1.5 / 32768.0, 2.5 / 32768.0];
    let mut narrow = [7i16; 3];
    convert_mono(Mono::new(&half), MonoMut::new(&mut narrow)).unwrap();
    assert_eq!(narrow, [0, 2, 2]);
}

#[test]
fn convert_mono_refuses_a_length_mismatch_and_writes_nothing() {
    let src = [0.5f32; 3];
    let mut dst = [7i16; 2];
    assert_eq!(
        convert_mono(Mono::new(&src), MonoMut::new(&mut dst)),
        Err(BufferError::LengthMismatch {
            src: Samples(3),
            dst: Samples(2)
        })
    );
    assert_eq!(dst, [7, 7]);
}

fn clips_into_i16(src: &[f32]) -> (usize, Vec<i16>) {
    let mut dst = vec![0i16; src.len()];
    let clipped = convert_counting_clips(
        Interleaved::new(src, ChannelLayout::MONO).unwrap(),
        InterleavedMut::new(&mut dst, ChannelLayout::MONO).unwrap(),
    )
    .unwrap();
    (clipped, dst)
}

#[test]
fn plus_one_and_above_clip_into_i16() {
    let (clipped, dst) = clips_into_i16(&[1.0, 1.5, f32::INFINITY, 1.0e30]);
    assert_eq!(clipped, 4);
    assert_eq!(dst, [i16::MAX; 4]);
}

#[test]
fn minus_one_is_min_and_not_a_clip_but_below_it_is() {
    let (clipped, dst) = clips_into_i16(&[-1.0, -1.0 - 1.0 / 32768.0, f32::NEG_INFINITY]);
    assert_eq!(clipped, 2);
    assert_eq!(dst, [i16::MIN; 3]);
}

#[test]
fn the_top_step_below_one_does_not_clip() {
    let (clipped, dst) = clips_into_i16(&[32767.0 / 32768.0, 0.0, -0.5]);
    assert_eq!(clipped, 0);
    assert_eq!(dst, [32767, 0, -16384]);
}

#[test]
fn nan_reads_as_zero_and_is_not_a_clip() {
    let (clipped, dst) = clips_into_i16(&[f32::NAN]);
    assert_eq!((clipped, dst), (0, vec![0]));
}

#[test]
fn a_narrowing_that_rounds_past_max_is_a_clip_and_a_float_target_never_is() {
    let src = [i16::MAX, 0];
    let mut narrow = [0i8; 2];
    let clipped = convert_counting_clips(
        Interleaved::new(&src, ChannelLayout::MONO).unwrap(),
        InterleavedMut::new(&mut narrow, ChannelLayout::MONO).unwrap(),
    )
    .unwrap();
    assert_eq!((clipped, narrow), (1, [i8::MAX, 0]));

    let loud = [4.0f32, f32::NAN];
    let mut wide = [0.0f64; 2];
    let clipped = convert_counting_clips(
        Interleaved::new(&loud, ChannelLayout::MONO).unwrap(),
        InterleavedMut::new(&mut wide, ChannelLayout::MONO).unwrap(),
    )
    .unwrap();
    assert_eq!(clipped, 0);
    assert_eq!(wide[0], 4.0);
}

#[test]
fn counting_refuses_width_then_frames_and_writes_nothing() {
    let src = [1.0f32; 4];
    let mut dst = [7i16; 4];
    let width = convert_counting_clips(
        Interleaved::new(&src, ChannelLayout::STEREO).unwrap(),
        InterleavedMut::new(&mut dst, ChannelLayout::MONO).unwrap(),
    );
    assert!(matches!(width, Err(BufferError::WidthMismatch { .. })));
    let frames = convert_counting_clips(
        Interleaved::new(&src, ChannelLayout::MONO).unwrap(),
        InterleavedMut::new(&mut dst[..3], ChannelLayout::MONO).unwrap(),
    );
    assert!(matches!(frames, Err(BufferError::LengthMismatch { .. })));
    assert_eq!(dst, [7; 4]);
}

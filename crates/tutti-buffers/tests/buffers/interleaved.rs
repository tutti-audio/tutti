//! The views' construction errors, counts and sub-views.

use tutti_buffers::ChannelLayout;
use tutti_buffers::{BufferError, I24, Interleaved, InterleavedMut, Mono, MonoMut, Sample};
use tutti_units::Samples;

#[test]
fn interleaved_refuses_a_zero_width() {
    let data = [0.0f32; 4];
    assert_eq!(
        Interleaved::new(&data, ChannelLayout::EMPTY).unwrap_err(),
        BufferError::ZeroWidth
    );
    let mut data = [0.0f32; 4];
    assert_eq!(
        InterleavedMut::new(&mut data, ChannelLayout::EMPTY).unwrap_err(),
        BufferError::ZeroWidth
    );
}

#[test]
fn interleaved_refuses_a_length_that_is_not_whole_frames_and_does_not_truncate() {
    let data = [0.0f32; 5];
    assert_eq!(
        Interleaved::new(&data, ChannelLayout::STEREO).unwrap_err(),
        BufferError::PartialFrame {
            len: 5,
            width: ChannelLayout::STEREO
        }
    );
    let mut data = [0i16; 7];
    assert_eq!(
        InterleavedMut::new(&mut data, ChannelLayout::from(3u16)).unwrap_err(),
        BufferError::PartialFrame {
            len: 7,
            width: ChannelLayout::from(3u16)
        }
    );
}

#[test]
fn interleaved_counts_frames_not_samples() {
    let data = [0.0f32; 12];
    let buf = Interleaved::new(&data, ChannelLayout::from(3u16)).unwrap();
    assert_eq!(buf.frames(), Samples::new(4));
    assert_eq!(buf.channels(), 3);
    assert_eq!(buf.samples().len(), 12);
    assert_eq!(buf.layout(), ChannelLayout::from(3u16));
    assert!(!buf.is_empty());
}

#[test]
fn an_empty_slice_is_a_valid_empty_interleaved_view() {
    let buf = Interleaved::<f32>::new(&[], ChannelLayout::STEREO).unwrap();
    assert!(buf.is_empty());
    assert_eq!(buf.frames(), Samples::ZERO);
    assert_eq!(buf.iter().count(), 0);
}

#[test]
fn interleaved_slice_is_denominated_in_frames() {
    let data: Vec<i16> = (0..12).collect();
    let buf = Interleaved::new(&data, ChannelLayout::from(3u16)).unwrap();
    let w = buf.slice(Samples::new(1)..Samples::new(3));
    assert_eq!(w.frames(), Samples::new(2));
    assert_eq!(w.samples(), &[3, 4, 5, 6, 7, 8]);
    assert_eq!(w.layout(), buf.layout());
}

#[test]
fn interleaved_frame_and_iter_yield_stride_long_frames() {
    let data = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
    let buf = Interleaved::new(&data, ChannelLayout::STEREO).unwrap();
    assert_eq!(buf.frame(Samples::new(2)), &[5.0, 6.0]);
    let frames: Vec<&[f32]> = buf.iter().collect();
    assert_eq!(frames, [&[1.0, 2.0][..], &[3.0, 4.0], &[5.0, 6.0]]);
}

#[test]
#[should_panic]
fn interleaved_frame_past_the_end_panics() {
    let data = [0.0f32; 4];
    let _ = Interleaved::new(&data, ChannelLayout::STEREO)
        .unwrap()
        .frame(Samples::new(2));
}

#[test]
fn interleaved_mut_writes_frames_slices_and_reborrows() {
    let mut data = [0i32; 8];
    let mut buf = InterleavedMut::new(&mut data, ChannelLayout::from(4u16)).unwrap();
    buf.frame_mut(Samples::new(1))
        .copy_from_slice(&[1, 2, 3, 4]);
    {
        let mut w = buf.slice_mut(Samples::new(0)..Samples::new(1));
        assert_eq!(w.frames(), Samples::new(1));
        w.frame_mut(Samples::new(0)).fill(9);
    }
    for f in buf.reborrow().iter_mut() {
        f[3] += 100;
    }
    assert_eq!(
        buf.as_interleaved().samples(),
        &[9, 9, 9, 109, 1, 2, 3, 104]
    );
    assert_eq!(buf.samples_mut().len(), 8);
}

#[test]
fn interleaved_views_hold_every_storage_format() {
    let d = [I24::of(1), I24::of(-2)];
    assert_eq!(
        Interleaved::new(&d, ChannelLayout::MONO).unwrap().frames(),
        Samples::new(2)
    );
    let d = [0.5f64; 4];
    assert_eq!(
        Interleaved::new(&d, ChannelLayout::STEREO)
            .unwrap()
            .frames(),
        Samples::new(2)
    );
    assert_eq!(<i8 as Sample>::SILENCE, 0);
}

#[test]
fn mono_reads_as_a_slice_and_slices_and_splits_by_frames() {
    let data = [1i16, 2, 3, 4, 5];
    let m = Mono::new(&data);
    assert_eq!(m.frames(), Samples::new(5));
    assert_eq!(m.as_ref(), &data[..]);
    assert_eq!(
        m.slice(Samples::new(1)..Samples::new(4)).samples(),
        &[2, 3, 4]
    );
    let (a, b) = m.split_at(Samples::new(2));
    assert_eq!((a.samples(), b.samples()), (&[1, 2][..], &[3, 4, 5][..]));
    assert!(Mono::<f32>::new(&[]).is_empty());
}

#[test]
fn mono_view_is_the_size_of_a_slice() {
    assert_eq!(size_of::<Mono<'_, f32>>(), size_of::<&[f32]>());
    assert_eq!(size_of::<MonoMut<'_, i16>>(), size_of::<&mut [i16]>());
}

#[test]
fn mono_mut_split_yields_disjoint_writable_halves() {
    let mut data = [0.0f32; 4];
    let mut m = MonoMut::new(&mut data);
    let (mut a, mut b) = m.split_at_mut(Samples::new(1));
    a.samples_mut().fill(1.0);
    b.samples_mut().fill(2.0);
    assert_eq!(m.frames(), Samples::new(4));
    m.slice_mut(Samples::new(3)..Samples::new(4)).samples_mut()[0] = 3.0;
    assert_eq!(m.as_mono().samples(), &[1.0, 2.0, 2.0, 3.0]);
    assert_eq!(m.reborrow().frames(), Samples::new(4));
}

/// **A frame is one value per channel**: wrapping `n` values makes one frame `n`
/// channels wide, and no values is no width.
#[test]
fn a_frame_view_is_one_frame_as_wide_as_its_values() {
    let data = [0.25f32, -0.5, 1.0];
    let frame = Interleaved::one_frame(&data).unwrap();
    assert_eq!(frame.layout(), ChannelLayout::from(3u16));
    assert_eq!(frame.frames(), Samples::new(1));
    let mut data = [0.0f64; 2];
    let frame = InterleavedMut::one_frame(&mut data).unwrap();
    assert_eq!(frame.layout(), ChannelLayout::STEREO);
    assert_eq!(frame.frames(), Samples::new(1));
    assert_eq!(
        Interleaved::<f32>::one_frame(&[]).unwrap_err(),
        BufferError::ZeroWidth
    );
}

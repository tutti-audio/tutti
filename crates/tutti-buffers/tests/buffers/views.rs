//! The views' one vocabulary: checked access, named iterators, equality over
//! the frames a view covers, splits, `from_vecs`, conversions between views.

use core::iter::FusedIterator;
use tutti_buffers::{
    Frames, FramesMut, Interleaved, InterleavedMut, Mono, MonoMut, PlaneIter, PlaneIterMut, Planes,
    PlanesMut, StereoPlanes,
};
use tutti_types::{ChannelLayout, Samples};

const THREE: ChannelLayout = ChannelLayout::from_count(3);

#[test]
fn a_get_out_of_bounds_is_none_and_one_in_bounds_is_the_element() {
    let data = [1i16, 2, 3, 4, 5, 6];
    let mono = Mono::new(&data);
    assert_eq!(mono.get_frame(Samples(5)), Some(&6));
    assert_eq!(mono.get_frame(Samples(6)), None);
    assert!(mono.get_slice(Samples(2)..Samples(6)).is_some());
    assert!(mono.get_slice(Samples(2)..Samples(7)).is_none());
    assert!(mono.get_slice(Samples(3)..Samples(2)).is_none(), "inverted");

    let buf = Interleaved::new(&data, THREE).unwrap();
    assert_eq!(buf.get_frame(Samples(1)), Some(&[4, 5, 6][..]));
    assert_eq!(buf.get_frame(Samples(2)), None);
    assert_eq!(buf.get_frame(Samples(usize::MAX)), None, "no overflow");
    assert_eq!(
        buf.get_slice(Samples(1)..Samples(2)).unwrap().samples(),
        &[4, 5, 6]
    );
    assert!(buf.get_slice(Samples(0)..Samples(3)).is_none());

    let (l, r) = ([1i16, 2], [3i16, 4]);
    let slices = [&l[..], &r[..]];
    let planes = Planes::new(&slices).unwrap();
    assert_eq!(planes.get_plane(1).unwrap().samples(), &[3, 4]);
    assert!(planes.get_plane(2).is_none());
    assert!(planes.get_slice(Samples(0)..Samples(3)).is_none());
    let pair = StereoPlanes::new(&l, &r).unwrap();
    assert_eq!(pair.get_plane(1).unwrap().samples(), &[3, 4]);
    assert!(pair.get_plane(2).is_none());
    assert!(pair.get_slice(Samples(1)..Samples(3)).is_none());
}

#[test]
fn a_get_out_of_bounds_on_a_write_view_is_none() {
    let mut data = [0i16; 6];
    let mut mono = MonoMut::new(&mut data);
    assert!(mono.get_frame_mut(Samples(5)).is_some());
    assert!(mono.get_frame_mut(Samples(6)).is_none());
    assert_eq!(mono.get_frame(Samples(6)), None);
    assert!(mono.get_slice_mut(Samples(0)..Samples(7)).is_none());
    assert!(mono.get_slice(Samples(0)..Samples(7)).is_none());

    let mut buf = InterleavedMut::new(&mut data, THREE).unwrap();
    assert!(buf.get_frame_mut(Samples(1)).is_some());
    assert!(buf.get_frame_mut(Samples(2)).is_none());
    assert!(buf.get_frame(Samples(2)).is_none());
    assert!(buf.get_slice_mut(Samples(1)..Samples(3)).is_none());
    assert!(buf.get_slice(Samples(1)..Samples(3)).is_none());

    let (mut l, mut r) = ([0i16; 2], [0i16; 2]);
    let mut slices = [&mut l[..], &mut r[..]];
    let mut planes = PlanesMut::new(&mut slices).unwrap();
    assert!(planes.get_plane(1).is_some());
    assert!(planes.get_plane(2).is_none());
    assert!(planes.get_plane_mut(1).is_some());
    assert!(planes.get_plane_mut(2).is_none());
    assert!(planes.get_slice(Samples(0)..Samples(3)).is_none());
    assert!(planes.get_slice_mut(Samples(0)..Samples(3)).is_none());
    assert_eq!(
        planes
            .get_slice_mut(Samples(1)..Samples(2))
            .unwrap()
            .frames(),
        Samples(1)
    );
}

#[test]
#[should_panic(expected = "slice 2..9 is out of bounds for 4 frames")]
fn a_mono_slice_panic_names_the_range_in_frames() {
    let data = [0.0f32; 4];
    let _ = Mono::new(&data).slice(Samples(2)..Samples(9));
}

#[test]
#[should_panic(expected = "slice 2..9 is out of bounds for 4 frames")]
fn an_interleaved_slice_panic_names_the_range_in_frames_not_samples() {
    let data = [0.0f32; 8];
    let _ = Interleaved::new(&data, ChannelLayout::STEREO)
        .unwrap()
        .slice(Samples(2)..Samples(9));
}

#[test]
#[should_panic(expected = "slice 2..9 is out of bounds for 4 frames")]
fn a_planes_slice_panic_names_the_range_in_frames() {
    let (l, r) = ([0.0f32; 4], [0.0f32; 4]);
    let _ = Planes::new(&[&l, &r])
        .unwrap()
        .slice(Samples(2)..Samples(9));
}

#[test]
#[should_panic(expected = "split at 5 is past the 4 frames")]
fn a_split_past_the_end_panics_in_frames() {
    let data = [0.0f32; 8];
    let _ = Interleaved::new(&data, ChannelLayout::STEREO)
        .unwrap()
        .split_at(Samples(5));
}

#[test]
#[should_panic(expected = "frame 2 is out of bounds for 2 frames")]
fn an_interleaved_frame_past_the_end_panics() {
    let data = [0.0f32; 4];
    let _ = Interleaved::new(&data, ChannelLayout::STEREO)
        .unwrap()
        .frame(Samples(2));
}

#[test]
#[should_panic(expected = "channel 2 is out of bounds for 2 channels")]
fn a_plane_past_the_last_panics() {
    let (l, r) = ([0.0f32; 2], [0.0f32; 2]);
    let _ = Planes::new(&[&l, &r]).unwrap().plane(2);
}

#[test]
fn every_view_splits_at_a_frame_into_two_that_cover_it() {
    let data = [1i16, 2, 3, 4, 5, 6];

    let (a, b) = Mono::new(&data).split_at(Samples(2));
    assert_eq!((a.samples(), b.samples()), (&data[..2], &data[2..]));

    let mut owned = data;
    let mut mono = MonoMut::new(&mut owned);
    let (mut a, mut b) = mono.split_at_mut(Samples(4));
    a.samples_mut().fill(0);
    b.samples_mut().fill(9);
    assert_eq!(owned, [0, 0, 0, 0, 9, 9]);

    let buf = Interleaved::new(&data, THREE).unwrap();
    let (a, b) = buf.split_at(Samples(1));
    assert_eq!((a.samples(), b.samples()), (&data[..3], &data[3..]));
    assert_eq!((a.layout(), b.frames()), (THREE, Samples(1)));
    let (all, none) = buf.split_at(Samples(2));
    assert_eq!((all.frames(), none.is_empty()), (Samples(2), true));

    let mut owned = data;
    let mut buf = InterleavedMut::new(&mut owned, THREE).unwrap();
    let (mut a, mut b) = buf.split_at_mut(Samples(1));
    a.samples_mut().fill(0);
    b.frame_mut(Samples(0)).fill(7);
    assert_eq!(owned, [0, 0, 0, 7, 7, 7]);

    let (l, r) = ([1i16, 2, 3], [4i16, 5, 6]);
    let slices = [&l[..], &r[..]];
    let planes = Planes::new(&slices).unwrap();
    let (a, b) = planes.split_at(Samples(1));
    assert_eq!(
        (a.plane(1).samples(), b.plane(1).samples()),
        (&[4][..], &[5, 6][..])
    );
    assert_eq!(
        (a.layout(), b.frames()),
        (ChannelLayout::STEREO, Samples(2))
    );

    let (a, b) = StereoPlanes::new(&l, &r).unwrap().split_at(Samples(2));
    assert_eq!((a.left(), a.right()), (&[1, 2][..], &[4, 5][..]));
    assert_eq!((b.left(), b.right()), (&[3][..], &[6][..]));
}

#[test]
fn a_stereo_pair_slices_both_planes() {
    let (l, r) = ([1i16, 2, 3, 4], [5i16, 6, 7, 8]);
    let pair = StereoPlanes::new(&l, &r).unwrap();
    let mid = pair.slice(Samples(1)..Samples(3));
    assert_eq!((mid.left(), mid.right()), (&[2, 3][..], &[6, 7][..]));
    assert_eq!(pair.channels(), 2);
    assert_eq!(pair.plane(0).samples(), &l);
}

#[test]
fn the_named_iterators_know_their_length_and_run_from_either_end() {
    let data = [1i16, 2, 3, 4, 5, 6, 7, 8];
    let buf = Interleaved::new(&data, ChannelLayout::STEREO).unwrap();
    let mut frames: Frames<'_, i16> = buf.iter();
    assert_eq!(frames.len(), 4);
    assert_eq!(frames.next(), Some(&[1, 2][..]));
    assert_eq!(frames.len(), 3);
    assert_eq!(frames.next_back(), Some(&[7, 8][..]));
    assert_eq!(frames.len(), 2);
    let rest: Vec<_> = frames.clone().rev().collect();
    assert_eq!(rest, [&[5, 6][..], &[3, 4][..]]);
    assert_eq!(frames.count(), 2, "a clone leaves the original unconsumed");

    let mut owned = data;
    let mut buf = InterleavedMut::new(&mut owned, ChannelLayout::STEREO).unwrap();
    let it: FramesMut<'_, i16> = buf.iter_mut();
    assert_eq!(it.len(), 4);
    for (i, frame) in it.rev().enumerate() {
        frame.fill(i as i16);
    }
    assert_eq!(owned, [3, 3, 2, 2, 1, 1, 0, 0]);

    let (l, r, c) = ([1i16, 2], [3i16, 4], [5i16, 6]);
    let slices = [&l[..], &r[..], &c[..]];
    let planes = Planes::new(&slices).unwrap();
    let mut it: PlaneIter<'_, i16> = planes.iter();
    assert_eq!(it.len(), 3);
    assert_eq!(it.next_back().unwrap().samples(), &[5, 6]);
    let firsts: Vec<i16> = it.clone().rev().map(|p| p.samples()[0]).collect();
    assert_eq!(firsts, [3, 1]);
    assert_eq!(it.len(), 2);
    assert_eq!(planes.iter().nth(1).unwrap().samples(), &[3, 4]);
    assert_eq!(
        planes
            .slice(Samples(1)..Samples(2))
            .iter()
            .next_back()
            .unwrap()
            .samples(),
        &[6],
        "an iterator over a slice reads the slice"
    );

    let (mut l, mut r, mut c) = ([0i16; 2], [0i16; 2], [0i16; 2]);
    let mut slices = [&mut l[..], &mut r[..], &mut c[..]];
    let mut planes = PlanesMut::new(&mut slices).unwrap();
    let it: PlaneIterMut<'_, '_, i16> = planes.iter_mut();
    assert_eq!(it.len(), 3);
    for (i, mut plane) in it.rev().enumerate() {
        plane.samples_mut().fill(i as i16 + 1);
    }
    assert_eq!((l, r, c), ([3, 3], [2, 2], [1, 1]));
}

#[test]
fn the_named_iterators_are_fused() {
    fn fused<I: FusedIterator>(mut it: I) {
        while it.next().is_some() {}
        assert!(it.next().is_none());
        assert!(it.next().is_none());
    }
    let data = [0.0f32; 4];
    fused(
        Interleaved::new(&data, ChannelLayout::STEREO)
            .unwrap()
            .iter(),
    );
    let (l, r) = ([0.0f32; 2], [0.0f32; 2]);
    fused(Planes::new(&[&l, &r]).unwrap().iter());
    let mut owned = [0.0f32; 4];
    fused(
        InterleavedMut::new(&mut owned, ChannelLayout::STEREO)
            .unwrap()
            .iter_mut(),
    );
    let (mut a, mut b) = ([0.0f32; 2], [0.0f32; 2]);
    let mut slices = [&mut a[..], &mut b[..]];
    fused(PlanesMut::new(&mut slices).unwrap().iter_mut());
}

#[test]
fn the_read_views_iterate_by_value() {
    let data = [1.0f32, 2.0, 3.0, 4.0];
    assert_eq!(Mono::new(&data).into_iter().sum::<f32>(), 10.0);
    let frames: Vec<_> = Interleaved::new(&data, ChannelLayout::STEREO)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(frames, [&[1.0, 2.0][..], &[3.0, 4.0]]);
    let (l, r) = ([1.0f32], [2.0f32]);
    let slices = [&l[..], &r[..]];
    let firsts: Vec<f32> = Planes::new(&slices)
        .unwrap()
        .into_iter()
        .map(|p| p.samples()[0])
        .collect();
    assert_eq!(firsts, [1.0, 2.0]);
}

/// Two views are equal when they cover the same samples at the same width,
/// whatever slices back them and wherever in them the view starts.
#[test]
fn views_compare_the_frames_they_cover() {
    let long = [0.0f32, 1.0, 2.0, 3.0, 4.0];
    let short = [1.0f32, 2.0];
    assert_eq!(
        Mono::new(&long).slice(Samples(1)..Samples(3)),
        Mono::new(&short)
    );
    assert_ne!(Mono::new(&long), Mono::new(&short));

    let (la, ra) = ([0.0f32, 1.0, 2.0, 3.0], [10.0f32, 11.0, 12.0, 13.0]);
    let (lb, rb) = ([1.0f32, 2.0], [11.0f32, 12.0]);
    let a = [&la[..], &ra[..]];
    let b = [&lb[..], &rb[..]];
    let (a, b) = (Planes::new(&a).unwrap(), Planes::new(&b).unwrap());
    assert_eq!(a.slice(Samples(1)..Samples(3)), b);
    assert_ne!(a, b, "the whole of `a` is not `b`");
    assert_ne!(a.slice(Samples(0)..Samples(2)), b, "another window of `a`");

    let vecs = vec![lb.to_vec(), rb.to_vec()];
    assert_eq!(
        Planes::from_vecs(&vecs).unwrap(),
        b,
        "a Vec table is a table"
    );

    let wide = [&lb[..], &rb[..], &lb[..]];
    assert_ne!(Planes::new(&wide).unwrap(), b, "another width");

    let st = ChannelLayout::STEREO;
    let ia = [0.0f32, 1.0, 2.0, 3.0, 4.0, 5.0];
    let ib = [2.0f32, 3.0];
    assert_eq!(
        Interleaved::new(&ia, st)
            .unwrap()
            .slice(Samples(1)..Samples(2)),
        Interleaved::new(&ib, st).unwrap()
    );
    assert_ne!(
        Interleaved::new(&ia[..4], st).unwrap(),
        Interleaved::new(&ia[..4], ChannelLayout::QUAD).unwrap(),
        "the same samples at another width"
    );
    assert_eq!(
        StereoPlanes::new(&la, &ra)
            .unwrap()
            .slice(Samples(1)..Samples(3)),
        StereoPlanes::new(&la[1..3], &ra[1..3]).unwrap()
    );
}

#[test]
fn planes_from_vecs_reads_the_tables_a_caller_holds() {
    let planes = vec![vec![1i16, 2, 3], vec![4, 5, 6]];
    let view = Planes::from_vecs(&planes).unwrap();
    assert_eq!(view.layout(), ChannelLayout::STEREO);
    assert_eq!(view.frames(), Samples(3));
    assert_eq!(view.plane(1).samples(), &[4, 5, 6]);
    assert_eq!(
        view.slice(Samples(1)..Samples(3)).plane(0).samples(),
        &[2, 3]
    );

    let none: Vec<Vec<i16>> = Vec::new();
    assert_eq!(
        Planes::from_vecs(&none).unwrap_err(),
        tutti_buffers::BufferError::ZeroWidth
    );
    let ragged = vec![vec![0i16; 3], vec![0i16; 2]];
    assert_eq!(
        Planes::from_vecs(&ragged).unwrap_err(),
        tutti_buffers::BufferError::RaggedPlane {
            plane: 1,
            expected: Samples(3),
            found: Samples(2)
        }
    );
}

#[test]
fn planes_mut_from_vecs_writes_the_vecs_in_place_without_growing_them() {
    let mut planes = vec![vec![0i16; 4], vec![0i16; 4]];
    let capacity = planes[0].capacity();
    {
        let mut out = PlanesMut::from_vecs(&mut planes).unwrap();
        assert_eq!(out.frames(), Samples(4));
        for (c, mut plane) in out.slice_mut(Samples(1)..Samples(3)).iter_mut().enumerate() {
            plane.samples_mut().fill(c as i16 + 1);
        }
        out.plane_mut(1).samples_mut()[3] = 9;
        assert_eq!(out.as_planes().plane(1).samples(), &[0, 2, 2, 9]);
        let read = Planes::from(out);
        assert_eq!(read.plane(0).samples(), &[0, 1, 1, 0]);
    }
    assert_eq!(planes, [vec![0, 1, 1, 0], vec![0, 2, 2, 9]]);
    assert_eq!(planes[0].capacity(), capacity);

    let mut ragged = vec![vec![0i16; 3], vec![0i16; 4]];
    assert!(PlanesMut::from_vecs(&mut ragged).is_err());
    let mut none: Vec<Vec<i16>> = Vec::new();
    assert!(PlanesMut::from_vecs(&mut none).is_err());
}

/// The defaults are the views that need no width: no samples, no frames.
#[test]
fn the_default_views_are_empty() {
    let mono = Mono::<f32>::default();
    assert!(mono.is_empty());
    assert_eq!(mono.layout(), ChannelLayout::MONO);
    let planes = Planes::<f32>::default();
    assert!(planes.is_empty());
    assert_eq!((planes.channels(), planes.iter().len()), (0, 0));
    assert!(planes.get_plane(0).is_none());
    assert_eq!(planes, Planes::default());
    let pair = StereoPlanes::<f32>::default();
    assert!(pair.is_empty());
    assert_eq!(pair.channels(), 2);
}

#[test]
fn debug_names_the_frames_and_the_layout_not_the_samples() {
    let data = [123.456f32, 789.0, 1.0, 2.0];
    let text = format!(
        "{:?}",
        Interleaved::new(&data, ChannelLayout::STEREO).unwrap()
    );
    assert!(text.contains("frames") && text.contains("Stereo"), "{text}");
    assert!(!text.contains("123"), "{text}");
    let text = format!("{:?}", Mono::new(&data));
    assert!(text.contains("frames") && !text.contains("789"), "{text}");
    let (l, r) = ([5.5f32], [6.5f32]);
    let text = format!("{:?}", StereoPlanes::new(&l, &r).unwrap());
    assert!(text.contains("frames") && !text.contains("5.5"), "{text}");
    let slices = [&l[..], &r[..]];
    let text = format!("{:?}", Planes::new(&slices).unwrap());
    assert!(text.contains("frames") && !text.contains("6.5"), "{text}");
}

#[test]
fn a_write_view_converts_into_its_read_view_and_lends_its_samples() {
    let mut data = [1i16, 2, 3, 4];
    let mono: Mono<'_, i16> = MonoMut::new(&mut data).into();
    assert_eq!(mono.samples(), &[1, 2, 3, 4]);
    let buf: Interleaved<'_, i16> = InterleavedMut::new(&mut data, ChannelLayout::STEREO)
        .unwrap()
        .into();
    assert_eq!(buf.frames(), Samples(2));
    assert_eq!(Mono::from(&data[..2]).frames(), Samples(2));
    assert_eq!(MonoMut::from(&mut data[..3]).frames(), Samples(3));

    let (mut l, mut r) = ([1i16, 2], [3i16, 4]);
    let mut slices = [&mut l[..], &mut r[..]];
    let planes: Planes<'_, i16> = PlanesMut::new(&mut slices).unwrap().into();
    assert_eq!(planes.plane(1).samples(), &[3, 4]);

    let mut data = [0i16; 4];
    let flat: &mut [i16] = InterleavedMut::new(&mut data, ChannelLayout::STEREO)
        .unwrap()
        .into_samples_mut();
    flat[3] = 7;
    let mono: &mut [i16] = MonoMut::new(&mut data[..2]).into_samples_mut();
    mono[0] = 5;
    assert_eq!(data, [5, 0, 0, 7]);
}

#[test]
fn the_views_read_and_write_as_slices_through_as_ref_and_as_mut() {
    let mut data = [1i16, 2, 3, 4];
    let buf = Interleaved::new(&data, ChannelLayout::STEREO).unwrap();
    assert_eq!(buf.as_ref(), &[1, 2, 3, 4]);
    let mut buf = InterleavedMut::new(&mut data, ChannelLayout::STEREO).unwrap();
    AsMut::<[i16]>::as_mut(&mut buf)[0] = 9;
    assert_eq!(AsRef::<[i16]>::as_ref(&buf), &[9, 2, 3, 4]);
    let mut mono = MonoMut::new(&mut data);
    AsMut::<[i16]>::as_mut(&mut mono)[1] = 8;
    assert_eq!(AsRef::<[i16]>::as_ref(&mono), &[9, 8, 3, 4]);
}

#[test]
fn a_read_view_is_copy_and_a_write_view_reborrows() {
    let data = [1.0f32, 2.0];
    let mono = Mono::new(&data);
    let again = mono;
    assert_eq!((mono.frames(), again.frames()), (Samples(2), Samples(2)));
    let mut owned = [0.0f32; 4];
    let mut buf = InterleavedMut::new(&mut owned, ChannelLayout::STEREO).unwrap();
    buf.reborrow().frame_mut(Samples(0)).fill(1.0);
    buf.reborrow().frame_mut(Samples(1)).fill(2.0);
    assert_eq!(buf.as_interleaved().samples(), &[1.0, 1.0, 2.0, 2.0]);
}

/// The accessors are `const`: a count and a width are known at compile time
/// for a view over a constant.
#[test]
fn the_accessors_are_const() {
    const DATA: [f32; 6] = [0.0; 6];
    const MONO: Samples = Mono::new(&DATA).frames();
    const LAYOUT: ChannelLayout = Mono::new(&DATA).layout();
    const PAIR: Option<StereoPlanes<'static>> = match StereoPlanes::new(&DATA, &DATA) {
        Ok(pair) => Some(pair),
        Err(_) => None,
    };
    assert_eq!(MONO, Samples(6));
    assert_eq!(LAYOUT, ChannelLayout::MONO);
    assert_eq!(PAIR.map(|p| p.frames()), Some(Samples(6)));
}

fn assert_send_sync<T: Send + Sync>() {}

/// Every view and iterator crosses threads: a compile-time assertion.
#[test]
fn the_views_and_their_iterators_are_send_and_sync() {
    assert_send_sync::<Mono<'_>>();
    assert_send_sync::<MonoMut<'_>>();
    assert_send_sync::<Interleaved<'_>>();
    assert_send_sync::<InterleavedMut<'_>>();
    assert_send_sync::<Planes<'_>>();
    assert_send_sync::<PlanesMut<'_, '_>>();
    assert_send_sync::<StereoPlanes<'_>>();
    assert_send_sync::<Frames<'_>>();
    assert_send_sync::<FramesMut<'_>>();
    assert_send_sync::<PlaneIter<'_>>();
    assert_send_sync::<PlaneIterMut<'_, '_>>();
    assert_send_sync::<Interleaved<'_, tutti_buffers::I24>>();
}

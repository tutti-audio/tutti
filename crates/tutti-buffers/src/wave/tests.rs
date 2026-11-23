use super::*;
use crate::{Bed, Speaker};
use crate::{I24, interleave};

const STEREO: ChannelLayout = ChannelLayout::STEREO;

/// `append` of an interleaved block puts frame `i`, channel `c` at
/// `channel(c)[i]` and advances the length by the block's frames.
#[test]
fn append_deinterleaves_frame_major_samples() {
    let mut w = Wave::<f32>::new(2, SampleRate::SR_48K);
    w.append(Interleaved::new(&[1.0, 2.0, 3.0, 4.0], STEREO).unwrap())
        .unwrap();
    assert_eq!(w.len(), Samples::new(2));
    assert_eq!(w.channel(0).samples(), &[1.0, 3.0]);
    assert_eq!(w.channel(1).samples(), &[2.0, 4.0]);
    assert_eq!((w.at(0, 1), w.at(1, 0)), (3.0, 2.0));
}

/// Interleaving a wave's planes and appending the result to a new wave is the
/// identity, for a format that is not the default.
#[test]
fn interleave_then_append_is_identity() {
    let samples: Vec<i16> = (0..30).map(|i| (i * 977 - 12_000) as i16).collect();
    let wave = Wave::from_interleaved(
        SampleRate::SR_48K,
        Interleaved::new(&samples, ChannelLayout::from(3u16)).unwrap(),
    );
    assert_eq!(wave.channels(), 3);
    let mut flat = vec![0i16; samples.len()];
    interleave(
        wave.planes(),
        crate::InterleavedMut::new(&mut flat, wave.layout()).unwrap(),
    )
    .unwrap();
    assert_eq!(flat, samples);
}

/// A block of another width than the wave's is refused and appends nothing.
#[test]
fn append_refuses_a_block_of_the_wrong_width() {
    let mut w = Wave::<f32>::new(2, SampleRate::SR_48K);
    let mono = Interleaved::new(&[0.5, 0.25], ChannelLayout::MONO).unwrap();
    assert_eq!(
        w.append(mono),
        Err(BufferError::WidthMismatch {
            src: ChannelLayout::MONO,
            dst: STEREO,
        })
    );
    assert!(w.is_empty());
    assert!(w.channel(0).is_empty());
}

/// `append_planes` of a different width is refused too.
#[test]
fn append_planes_refuses_the_wrong_width() {
    let mut w = Wave::<f32>::new(2, SampleRate::SR_48K);
    let one = [0.5f32];
    let table = [&one[..]];
    assert!(w.append_planes(Planes::new(&table).unwrap()).is_err());
    assert!(w.is_empty());
}

#[test]
fn zero_rounds_the_duration_to_the_nearest_frame() {
    let w = Wave::<f32>::zero(1, SampleRate::of(2.0), Seconds::new(0.75));
    assert_eq!(w.len(), Samples::new(2));
    assert!(w.channel(0).iter().all(|&s| s == 0.0));
}

/// A wave of an integer format holds that format's silence and values as they
/// are: nothing converts on the way in.
#[test]
fn a_wave_holds_its_format_unconverted() {
    let silent = Wave::<I24>::zero(2, SampleRate::SR_48K, Seconds::new(0.0));
    assert!(silent.is_empty());
    let mut w = Wave::<I24>::new(1, SampleRate::SR_48K);
    w.append(
        Interleaved::new(
            &[I24::of(-8_388_608), I24::of(8_388_607)],
            ChannelLayout::MONO,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(w.channel(0).samples(), &[I24::MIN, I24::MAX]);
}

/// `channel_mut` writes through to the wave and `planes` reads it back.
#[test]
fn channel_mut_writes_through() {
    let mut w = Wave::<f32>::from_mono(SampleRate::SR_48K, Mono::new(&[0.0, 0.0, 0.0]));
    w.channel_mut(0).samples_mut()[1] = 0.75;
    w.set(0, 2, -0.5);
    assert_eq!(w.planes().plane(0).samples(), &[0.0, 0.75, -0.5]);
}

/// A wave built from a count takes `ChannelTopology::default_for` and says it
/// guessed, and one built from a topology keeps it, one channel per speaker,
/// as stated.
#[test]
fn a_count_takes_the_default_layout_and_a_topology_is_kept_as_stated() {
    for n in [1u16, 2, 4, 6, 8, 12] {
        let w = Wave::<f32>::new(usize::from(n), SampleRate::SR_48K);
        assert_eq!(
            w.topology(),
            ChannelTopology::default_for(ChannelLayout::from(n)).as_ref(),
            "{n}"
        );
        assert_eq!(w.placement(), Placement::Default, "{n}");
    }
    for n in [3usize, 5, 7, 16] {
        assert_eq!(
            Wave::<f32>::new(n, SampleRate::SR_48K)
                .topology()
                .unwrap()
                .positions()
                .unwrap(),
            vec![Speaker::Unassigned; n],
            "{n}"
        );
    }
    assert!(
        Wave::<f32>::new(0, SampleRate::SR_48K)
            .topology()
            .unwrap()
            .positions()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        Wave::from_mono(SampleRate::SR_48K, Mono::new(&[0.5f32]))
            .topology()
            .unwrap()
            .positions()
            .unwrap(),
        [Speaker::FrontCenter]
    );
    let quad = Wave::<f32>::with_topology(crate::Bed::Quad.topology(), SampleRate::SR_48K);
    assert_eq!(quad.channels(), 4);
    assert_eq!(quad.topology(), Some(&crate::Bed::Quad.topology()));
    assert_eq!(quad.placement(), Placement::Stated);
}

/// A wave wider than a topology holds keeps its width and has no topology, from
/// a count and from an explicit placement.
#[test]
fn a_wide_wave_keeps_its_width_and_has_no_topology() {
    let w = Wave::<f32>::new(20, SampleRate::SR_48K);
    assert_eq!((w.channels(), w.layout().count()), (20, 20));
    assert_eq!((w.topology(), w.placement()), (None, Placement::Default));
    let stated = Wave::<f32>::unplaced(
        ChannelLayout::from(40u16),
        Placement::Stated,
        SampleRate::SR_48K,
    );
    assert_eq!((stated.channels(), stated.topology()), (40, None));
    assert_eq!(stated.placement(), Placement::Stated);
    assert_ne!(w, Wave::<f32>::new(21, SampleRate::SR_48K));
}

/// A wave wider than a layout holds is refused, not given the wrapped width.
#[test]
#[should_panic(expected = "at most u16::MAX channels")]
fn a_wave_wider_than_a_layout_is_refused() {
    let _ = Wave::<f32>::new(usize::from(u16::MAX) + 1, SampleRate::SR_48K);
}

/// A wave with no channels has no planes and refuses every block.
#[test]
fn a_wave_without_channels_has_empty_planes() {
    let mut w = Wave::<f32>::new(0, SampleRate::SR_48K);
    assert_eq!(w.planes().frames(), Samples::ZERO);
    assert!(w.planes_mut().is_empty());
    assert!(
        w.append(Interleaved::new(&[0.0], ChannelLayout::MONO).unwrap())
            .is_err()
    );
}

#[test]
fn duration_is_frames_over_rate() {
    let w = Wave::<f32>::zero(1, SampleRate::of(4.0), Seconds::new(1.5));
    assert_eq!(w.duration(), Seconds::new(1.5));
}

/// Widening an `f32` wave is exact: every sample is `x as f64`, with the same
/// rate, topology and placement.
#[test]
fn widening_an_f32_wave_is_every_sample_as_f64() {
    let mut w32 = Wave::with_topology(crate::Bed::Quad.topology(), SampleRate::SR_44K1);
    w32.append(
        Interleaved::new(
            &[
                0.1f32,
                -0.2,
                0.3,
                f32::MIN_POSITIVE,
                1.0e-3,
                2.0,
                -3.5,
                1.0 / 3.0,
            ],
            ChannelLayout::from(4u16),
        )
        .unwrap(),
    )
    .unwrap();
    let w64 = Wave::<f64>::from(&w32);
    assert_eq!(w64.sample_rate(), w32.sample_rate());
    assert_eq!(w64.topology(), w32.topology());
    assert_eq!(w64.placement(), w32.placement());
    assert_eq!(w64.len(), w32.len());
    for c in 0..4 {
        for i in 0..2 {
            assert_eq!(w64.at(c, i), f64::from(w32.at(c, i)));
        }
    }
}

/// Mapping a wave applies `f` to every sample and keeps the rate, topology,
/// placement and length; a cast narrows exactly as the cast does.
#[test]
fn mapping_an_f64_wave_with_a_cast_is_every_sample_cast() {
    let mut w64 = Wave::<f64>::with_topology(crate::Bed::Quad.topology(), SampleRate::SR_48K);
    w64.append(
        Interleaved::new(
            &[0.1f64, -0.2, 1.0 / 3.0, 1.0e-30, 5.5, 7.25, -1.0e10, 0.0],
            ChannelLayout::from(4u16),
        )
        .unwrap(),
    )
    .unwrap();
    let w32 = w64.map(|x| x as f32);
    assert_eq!(w32.sample_rate(), w64.sample_rate());
    assert_eq!(w32.topology(), w64.topology());
    assert_eq!(w32.placement(), w64.placement());
    assert_eq!(w32.len(), w64.len());
    for c in 0..4 {
        for i in 0..2 {
            assert_eq!(w32.at(c, i), w64.at(c, i) as f32);
        }
    }
}

/// Equality is samples, rate and speakers: the same wave with its topology
/// stated or the default for its width is equal, and a different sample, rate or speaker is not.
#[test]
fn equality_ignores_where_the_topology_came_from() {
    let stated = || Wave::with_topology(Bed::Mono.topology(), SampleRate::SR_48K);
    let default = || Wave::<f32>::new(1, SampleRate::SR_48K);
    let fill = |mut w: Wave<f32>, v: f32| {
        w.append(Interleaved::new(&[v, 0.5], ChannelLayout::MONO).unwrap())
            .unwrap();
        w
    };
    let (a, b) = (fill(stated(), 0.25), fill(default(), 0.25));
    assert_ne!(a.placement(), b.placement());
    assert_eq!(a, b);
    assert_ne!(a, fill(default(), 0.75), "a sample differs");
    let mut other_rate = Wave::<f32>::new(1, SampleRate::SR_44K1);
    other_rate
        .append(Interleaved::new(&[0.25, 0.5], ChannelLayout::MONO).unwrap())
        .unwrap();
    assert_ne!(a, other_rate, "the rate differs");
}

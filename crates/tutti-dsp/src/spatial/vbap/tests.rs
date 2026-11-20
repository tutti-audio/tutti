use super::*;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Mono, MonoMut};

/// `VbapGains::process_channel` over plain slices.
fn render<F: Float>(
    ramp: &Ramp<VbapGains>,
    first: u32,
    speaker: Option<usize>,
    fold: &Fold<F>,
    left: &[F],
    right: &[F],
    out: &mut [F],
) {
    VbapGains::process_channel(
        ramp,
        first,
        speaker,
        fold,
        Mono::new(left),
        Mono::new(right),
        MonoMut::new(out),
    );
}

use crate::Ramp;

fn gains(mono: bool, a0: f64, b0: f64) -> VbapGains {
    let mut g = VbapGains {
        mono,
        a: [Gain::SILENT; MAX_CHANNELS],
        b: [Gain::SILENT; MAX_CHANNELS],
    };
    g.a[0] = Gain::new(a0);
    g.b[0] = Gain::new(b0);
    g
}

fn stereo_to_mono<F: Float>() -> Fold<F> {
    Fold::between(
        &ChannelTopology::default_for(ChannelLayout::STEREO),
        &ChannelTopology::default_for(ChannelLayout::MONO),
    )
    .unwrap()
}

/// A held gain is exactly `mono * g` (the mono the pair's average, the
/// stereo-to-mono fold) or `l * a + r * b`.
#[test]
fn a_held_gain_is_the_fold_times_g_or_the_pair_weighed() {
    let fold = stereo_to_mono();
    let (l, r) = ([0.5f32, -0.25], [0.125f32, 1.0]);
    let mut out = [0.0f32; 2];
    let mono = [(l[0] + r[0]) * 0.5, (l[1] + r[1]) * 0.5];
    let held = Ramp::new(gains(true, 0.3, 0.0)).steps(16);
    render(&held, 1, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], mono[i] * 0.3);
    }
    let held = Ramp::new(gains(false, 0.3, 0.7)).steps(16);
    render(&held, 1, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], l[i] * 0.3 + r[i] * 0.7);
    }
}

/// In `f64` a held gain is `l * a + r * b` and a ramp reads `k / 16` at step `k`.
#[test]
fn an_f64_render_weighs_the_pair_by_the_widened_gains() {
    let fold = stereo_to_mono::<f64>();
    let (l, r) = ([0.5f64, -0.25], [0.125f64, 1.0]);
    let mut out = [0.0f64; 2];
    let held = Ramp::new(gains(false, 0.3, 0.7)).steps(16);
    render(&held, 1, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], l[i] * 0.3 + r[i] * 0.7);
    }
    let mut ramp = Ramp::new(gains(true, 0.0, 0.0)).steps(16);
    ramp.retarget(gains(true, 1.0, 0.0));
    let (ones, mut rendered) = ([1.0f64; 16], [0.0f64; 16]);
    render(&ramp, 1, Some(0), &fold, &ones, &ones, &mut rendered);
    for (i, &g) in rendered.iter().enumerate() {
        assert_eq!(g, (i + 1) as f64 / 16.0, "frame {i}");
    }
}

/// A ramp from gain 0 to gain 1 over the grid reads `k / 16` at step `k`
/// (exact: a power of two), arrives on the target on step 16, and renders the
/// same bits cut into any calls.
#[test]
fn a_gain_ramp_reads_its_step_and_any_cut_gives_the_same_bits() {
    let fold = stereo_to_mono();
    let mut ramp = Ramp::new(gains(true, 0.0, 0.0)).steps(16);
    ramp.retarget(gains(true, 1.0, 0.0));
    let ones = [1.0f32; 16];
    let mut whole = [0.0f32; 16];
    render(&ramp, 1, Some(0), &fold, &ones, &ones, &mut whole);
    for (i, &g) in whole.iter().enumerate() {
        assert_eq!(g, (i + 1) as f32 / 16.0, "frame {i}");
    }
    let mut cut = [0.0f32; 16];
    let mut first = 1;
    for len in [3usize, 1, 7, 5] {
        let end = first as usize - 1 + len;
        let at = first as usize - 1;
        render(
            &ramp,
            first,
            Some(0),
            &fold,
            &ones[at..end],
            &ones[at..end],
            &mut cut[at..end],
        );
        first += len as u32;
    }
    assert_eq!(whole.map(f32::to_bits), cut.map(f32::to_bits));
    let mut none = [1.0f32; 4];
    render(&ramp, 1, None, &fold, &ones[..4], &ones[..4], &mut none);
    assert_eq!(none, [0.0; 4], "no speaker is silence");
}

/// A ramp from a mono set to a stereo one crosses between the two weighings:
/// the mono set is half its gain on each side, so the step 8 of 16 is the mean
/// of the ends' pairs.
#[test]
fn a_ramp_between_the_branches_blends_their_pairs() {
    let fold = stereo_to_mono();
    let mut ramp = Ramp::new(gains(true, 1.0, 0.0)).steps(16);
    ramp.retarget(gains(false, 0.0, 1.0));
    let (l, r) = ([1.0f32; 16], [0.0f32; 16]);
    let mut out = [0.0f32; 16];
    render(&ramp, 1, Some(0), &fold, &l, &r, &mut out);
    // The left weight goes from 0.5 (half of 1.0) to 0.0.
    assert_eq!(out[7], 0.25);
    assert_eq!(out[15], 0.0);
}

#[test]
fn the_stereo_spread_wraps_instead_of_leaving_the_canonical_range() {
    // Near the rear seam at full width, one source crosses 180 and wraps to
    // the other side: vbap never sees 185 degrees, outside the -180..180
    // range every other path maintains.
    let (a, b) = virtual_sources(Azimuth::new(170.0), StereoWidth::new(1.0));
    assert_eq!(a, Azimuth::new(-175.0));
    assert_eq!(b, Azimuth::new(155.0));

    // The mirrored case, crossing the other way.
    let (a, b) = virtual_sources(Azimuth::new(-170.0), StereoWidth::new(1.0));
    assert_eq!(a, Azimuth::new(-155.0));
    assert_eq!(b, Azimuth::new(175.0));

    // Both stay in range for every bearing and width.
    for deg in -180..=180 {
        for w in [0.0, 0.25, 0.5, 1.0] {
            let (a, b) = virtual_sources(Azimuth::new(f64::from(deg)), StereoWidth::new(w));
            assert!(a.get() >= -180.0 && a.get() <= 180.0, "a={a:?}");
            assert!(b.get() >= -180.0 && b.get() <= 180.0, "b={b:?}");
        }
    }
}

#[test]
fn the_spread_is_symmetric_about_the_bearing_away_from_the_seam() {
    // Front and centre: no wrapping involved, so the two sources should
    // straddle the bearing by exactly the offset.
    let (a, b) = virtual_sources(Azimuth::FRONT, StereoWidth::new(1.0));
    assert_eq!(a, Azimuth::new(15.0));
    assert_eq!(b, Azimuth::new(-15.0));

    // Width scales the offset; zero width collapses both onto the bearing.
    let (a, b) = virtual_sources(Azimuth::new(30.0), StereoWidth::new(0.0));
    assert_eq!(a, Azimuth::new(30.0));
    assert_eq!(b, Azimuth::new(30.0));
}

/// A bearing ramp from 170 to -170 degrees passes through 180, never through
/// the front: every step is within 10 degrees of the seam. A height ramp is a
/// line.
#[test]
fn a_bearing_ramp_takes_the_short_arc_and_a_height_ramp_a_line() {
    let (from, to) = (Azimuth::new(170.0), Azimuth::new(-170.0));
    for k in 0..=10 {
        let at = from.lerp(&to, k, 10);
        assert!(at.get().abs() >= 170.0, "step {k}: {at:?}");
        let height = Lerp::lerp(&Elevation::new(0.0), &Elevation::new(20.0), k, 10);
        assert_eq!(height.get(), 2.0 * f64::from(k), "step {k}");
    }
    assert_eq!(from.lerp(&to, 5, 10).get().abs(), 180.0);
}

/// A width past the one that puts the virtual sources half a turn either side
/// of the bearing saturates: any width up to `f32::MAX` solves to the gains of
/// the saturated one, finite and unit power, where an unclamped offset
/// overflowed to NaN and found no speaker tuple.
#[test]
fn a_huge_width_saturates_instead_of_overflowing() {
    let layout = SpeakerLayout::try_from(tutti_buffers::Bed::Surround714).unwrap();
    let solve = |width: f64| {
        VbapKernel::new(layout.clone()).gains(
            Azimuth::new(30.0),
            Elevation::new(10.0),
            StereoWidth::new(width),
            Spread::POINT,
        )
    };
    let saturated = solve(MAX_WIDTH.get());
    for width in [13.0, 1e6, 3e37, f64::MAX] {
        let got = solve(width);
        for s in 0..11 {
            let (a, b) = got.pair(s);
            assert!(a.get().is_finite() && b.get().is_finite(), "width {width}");
        }
        let power: f64 = (0..11).map(|s| got.pair(s).0.get().powi(2)).sum();
        assert!((power - 1.0).abs() < 1e-12, "width {width}: power {power}");
        assert_eq!(got, saturated, "width {width}");
    }
}

/// A gain that changes only in the sign of its zero is a ramp, not a held
/// gain: the frames before the last are `+0.0` and the last lands on the target
/// `-0.0`, bit for bit.
#[test]
fn a_signed_zero_gain_change_is_a_ramp_that_lands_on_the_target_bits() {
    let fold = stereo_to_mono();
    let mut ramp = Ramp::new(gains(true, 0.0, 0.0)).steps(16);
    ramp.retarget(gains(true, -0.0, 0.0));
    let ones = [1.0f32; 16];
    let mut out = [1.0f32; 16];
    render(&ramp, 1, Some(0), &fold, &ones, &ones, &mut out);
    assert!(out[..15].iter().all(|o| o.to_bits() == 0.0f32.to_bits()));
    assert_eq!(out[15].to_bits(), (-0.0f32).to_bits());
}

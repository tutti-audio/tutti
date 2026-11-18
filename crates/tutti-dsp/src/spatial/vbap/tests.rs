use super::*;
use tutti_buffers::{Mono, MonoMut};
use tutti_types::{ChannelLayout, ChannelTopology};

/// `VbapGains::render_channel` over plain slices.
fn render<F: Float>(
    glide: &Glide<VbapGains>,
    first: u32,
    speaker: Option<usize>,
    fold: &Fold<F>,
    left: &[F],
    right: &[F],
    out: &mut [F],
) {
    VbapGains::render_channel(
        glide,
        first,
        speaker,
        fold,
        Mono::new(left),
        Mono::new(right),
        MonoMut::new(out),
    );
}

use crate::Glide;

fn gains(mono: bool, a0: f32, b0: f32) -> VbapGains {
    let mut g = VbapGains {
        mono,
        a: [0.0; MAX_CHANNELS],
        b: [0.0; MAX_CHANNELS],
    };
    g.a[0] = a0;
    g.b[0] = b0;
    g
}

fn stereo_to_mono<F: Float>() -> Fold<F> {
    Fold::between(
        &ChannelTopology::assumed(ChannelLayout::STEREO),
        &ChannelTopology::assumed(ChannelLayout::MONO),
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
    let held = Glide::landed(gains(true, 0.3, 0.0)).by(16);
    render(&held, 1, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], mono[i] * 0.3);
    }
    let held = Glide::landed(gains(false, 0.3, 0.7)).by(16);
    render(&held, 1, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], l[i] * 0.3 + r[i] * 0.7);
    }
}

/// In `f64` a held gain is `l * a + r * b` and a glide reads `k / 16` at step `k`, with the
/// `f32` gains widened exactly.
#[test]
fn an_f64_render_weighs_the_pair_by_the_widened_gains() {
    let fold = stereo_to_mono::<f64>();
    let (l, r) = ([0.5f64, -0.25], [0.125f64, 1.0]);
    let mut out = [0.0f64; 2];
    let held = Glide::landed(gains(false, 0.3, 0.7)).by(16);
    render(&held, 1, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], l[i] * f64::from(0.3f32) + r[i] * f64::from(0.7f32));
    }
    let mut glide = Glide::landed(gains(true, 0.0, 0.0)).by(16);
    glide.aim(gains(true, 1.0, 0.0));
    let (ones, mut ramp) = ([1.0f64; 16], [0.0f64; 16]);
    render(&glide, 1, Some(0), &fold, &ones, &ones, &mut ramp);
    for (i, &g) in ramp.iter().enumerate() {
        assert_eq!(g, (i + 1) as f64 / 16.0, "frame {i}");
    }
}

/// A glide from gain 0 to gain 1 over the grid reads `k / 16` at step `k`
/// (exact: a power of two), arrives on the target on step 16, and renders the
/// same bits cut into any calls.
#[test]
fn a_gain_glide_reads_its_step_and_any_cut_gives_the_same_bits() {
    let fold = stereo_to_mono();
    let mut glide = Glide::landed(gains(true, 0.0, 0.0)).by(16);
    glide.aim(gains(true, 1.0, 0.0));
    let ones = [1.0f32; 16];
    let mut whole = [0.0f32; 16];
    render(&glide, 1, Some(0), &fold, &ones, &ones, &mut whole);
    for (i, &g) in whole.iter().enumerate() {
        assert_eq!(g, (i + 1) as f32 / 16.0, "frame {i}");
    }
    let mut cut = [0.0f32; 16];
    let mut first = 1;
    for len in [3usize, 1, 7, 5] {
        let end = first as usize - 1 + len;
        let at = first as usize - 1;
        render(
            &glide,
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
    render(&glide, 1, None, &fold, &ones[..4], &ones[..4], &mut none);
    assert_eq!(none, [0.0; 4], "no speaker is silence");
}

/// A glide from a mono set to a stereo one crosses between the two weighings:
/// the mono set is half its gain on each side, so the step 8 of 16 is the mean
/// of the ends' pairs.
#[test]
fn a_glide_between_the_branches_blends_their_pairs() {
    let fold = stereo_to_mono();
    let mut glide = Glide::landed(gains(true, 1.0, 0.0)).by(16);
    glide.aim(gains(false, 0.0, 1.0));
    let (l, r) = ([1.0f32; 16], [0.0f32; 16]);
    let mut out = [0.0f32; 16];
    render(&glide, 1, Some(0), &fold, &l, &r, &mut out);
    // The left weight goes from 0.5 (half of 1.0) to 0.0.
    assert_eq!(out[7], 0.25);
    assert_eq!(out[15], 0.0);
}

#[test]
fn the_stereo_spread_wraps_instead_of_leaving_the_canonical_range() {
    // Near the rear seam at full width, one source crosses 180. A raw
    // `f32` sum would hand vbap 185 degrees, outside the -180..180 range
    // every other path maintains.
    let (a, b) = virtual_sources(Azimuth(170.0), 1.0);
    assert_eq!(a, Azimuth(-175.0));
    assert_eq!(b, Azimuth(155.0));

    // The mirrored case, crossing the other way.
    let (a, b) = virtual_sources(Azimuth(-170.0), 1.0);
    assert_eq!(a, Azimuth(-155.0));
    assert_eq!(b, Azimuth(175.0));

    // Both stay in range for every bearing and width, which is the
    // property the raw-f32 form could not offer.
    for deg in -180..=180 {
        for w in [0.0, 0.25, 0.5, 1.0] {
            let (a, b) = virtual_sources(Azimuth(f64::from(deg)), w);
            assert!(a.get() >= -180.0 && a.get() <= 180.0, "a={a:?}");
            assert!(b.get() >= -180.0 && b.get() <= 180.0, "b={b:?}");
        }
    }
}

#[test]
fn the_spread_is_symmetric_about_the_bearing_away_from_the_seam() {
    // Front and centre: no wrapping involved, so the two sources should
    // straddle the bearing by exactly the offset.
    let (a, b) = virtual_sources(Azimuth::FRONT, 1.0);
    assert_eq!(a, Azimuth(15.0));
    assert_eq!(b, Azimuth(-15.0));

    // Width scales the offset; zero width collapses both onto the bearing.
    let (a, b) = virtual_sources(Azimuth(30.0), 0.0);
    assert_eq!(a, Azimuth(30.0));
    assert_eq!(b, Azimuth(30.0));
}

/// A bearing glide from 170 to -170 degrees passes through 180, never through
/// the front: every step is within 10 degrees of the seam. A height glide is a
/// line.
#[test]
fn a_bearing_glide_takes_the_short_arc_and_a_height_glide_a_line() {
    let (from, to) = (Azimuth(170.0), Azimuth(-170.0));
    for k in 0..=10 {
        let at = from.lerp(&to, k, 10);
        assert!(at.get().abs() >= 170.0, "step {k}: {at:?}");
        let height = Lerp::lerp(&Elevation(0.0), &Elevation(20.0), k, 10);
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
    let layout = SpeakerLayout::from_bed(tutti_types::Bed::Surround714).unwrap();
    let solve = |width: f32| {
        VbapKernel::new(layout.clone()).gains(
            Azimuth(30.0),
            Elevation(10.0),
            StereoWidth(width),
            Spread::POINT,
        )
    };
    let saturated = solve(MAX_WIDTH);
    for width in [13.0, 1e6, 3e37, f32::MAX] {
        let got = solve(width);
        for s in 0..11 {
            let (a, b) = got.pair(s);
            assert!(a.is_finite() && b.is_finite(), "width {width}");
        }
        let power: f32 = (0..11).map(|s| got.pair(s).0.powi(2)).sum();
        assert!((power - 1.0).abs() < 1e-4, "width {width}: power {power}");
        assert_eq!(got, saturated, "width {width}");
    }
}

/// A gain that changes only in the sign of its zero is a glide, not a held
/// gain: the frames before the last are `+0.0` and the last lands on the target
/// `-0.0`, bit for bit.
#[test]
fn a_signed_zero_gain_change_is_a_glide_that_lands_on_the_target_bits() {
    let fold = stereo_to_mono();
    let mut glide = Glide::landed(gains(true, 0.0, 0.0)).by(16);
    glide.aim(gains(true, -0.0, 0.0));
    let ones = [1.0f32; 16];
    let mut out = [1.0f32; 16];
    render(&glide, 1, Some(0), &fold, &ones, &ones, &mut out);
    assert!(out[..15].iter().all(|o| o.to_bits() == 0.0f32.to_bits()));
    assert_eq!(out[15].to_bits(), (-0.0f32).to_bits());
}

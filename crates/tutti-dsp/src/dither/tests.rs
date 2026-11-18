use super::*;
use tutti_types::ChannelLayout;

/// `apply` over a mono run of samples.
fn apply<F: Float>(n: &mut DitherNoise, shape: DitherShape, lsb: Amplitude, s: &mut [F]) {
    n.apply(
        shape,
        lsb,
        InterleavedMut::new(s, ChannelLayout::MONO).unwrap(),
    );
}

const LSB: Amplitude = Amplitude(1.0);

/// The first rectangular draw is the first xorshift32 word from the seed
/// 0x12345678, 2_274_908_837, scaled to `[-0.5, 0.5]`: `2274908837 / (2^32 - 1) - 0.5`.
#[test]
fn the_first_draw_is_the_seeded_word_scaled() {
    let mut s = [0.0f32; 1];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Rectangular,
        LSB,
        &mut s,
    );
    let want = 2_274_908_837.0_f64 / 4_294_967_295.0 - 0.5;
    assert!((f64::from(s[0]) - want).abs() < 1e-7, "{} vs {want}", s[0]);
}

/// The first triangular sample is `(r1 - r2) * lsb` with `r1`, `r2` the
/// first two seeded words, 2274908837 and 358294691, over `2^32 - 1`.
#[test]
fn the_first_triangular_draw_is_the_difference_of_two_seeded_words() {
    let mut s = [0.0f32; 1];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Triangular,
        Amplitude(0.25),
        &mut s,
    );
    let want = (2_274_908_837.0_f64 - 358_294_691.0) / 4_294_967_295.0 * 0.25;
    assert!((f64::from(s[0]) - want).abs() < 1e-7, "{} vs {want}", s[0]);
}

#[test]
fn rectangular_stays_within_half_an_lsb() {
    let mut n = DitherNoise::new();
    let mut s = [0.0f32; 4096];
    apply(&mut n, DitherShape::Rectangular, LSB, &mut s);
    assert!(s.iter().all(|x| x.abs() <= 0.5));
    assert!(s.iter().any(|x| x.abs() > 0.4), "the range is used");
}

#[test]
fn triangular_stays_within_one_lsb() {
    let mut n = DitherNoise::new();
    let mut s = [0.0f32; 4096];
    apply(&mut n, DitherShape::Triangular, LSB, &mut s);
    assert!(s.iter().all(|x| x.abs() <= 1.0));
    assert!(s.iter().any(|x| x.abs() > 0.8), "the range is used");
}

/// One block of N equals two of N/2.
#[test]
fn the_sequence_continues_across_calls() {
    let mut whole = [0.0f32; 16];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Triangular,
        LSB,
        &mut whole,
    );
    let mut split = [0.0f32; 16];
    let mut n = DitherNoise::new();
    let (a, b) = split.split_at_mut(6);
    apply(&mut n, DitherShape::Triangular, LSB, a);
    apply(&mut n, DitherShape::Triangular, LSB, b);
    assert_eq!(whole, split);
}

/// In `f64` the first rectangular draw is the seeded word over `2^32 - 1` scaled to
/// `[-0.5, 0.5]`, and it is added to the signal at full `f64` precision: a signal of 0.1
/// (not an `f32`) leaves exactly an `f32` draw behind once 0.1 is taken off.
#[test]
fn an_f64_block_keeps_its_precision_and_gets_the_seeded_draw() {
    let mut s = [0.1f64; 1];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Rectangular,
        LSB,
        &mut s,
    );
    let want = 2_274_908_837.0_f64 / 4_294_967_295.0 - 0.5;
    let added = s[0] - 0.1;
    assert!((added - want).abs() < 1e-7, "{added} vs {want}");
    assert!(
        (f64::from(added as f32) - added).abs() < 1e-15,
        "the signal was rounded through f32: {added}"
    );
}

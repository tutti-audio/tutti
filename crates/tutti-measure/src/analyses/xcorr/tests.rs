use super::*;
use crate::test_rng::uniform;
use tutti_rt::SplitMix64;
fn cross_correlate(a: &[f64], b: &[f64]) -> CrossCorrelation {
    super::cross_correlate(Mono::new(a), Mono::new(b))
}

/// Any sample format correlates at its nominal level: an `i16` copy of a
/// buffer peaks at the same lag as the `f64` original.
#[test]
fn an_integer_buffer_correlates_at_its_nominal_level() {
    let base = noise(300, 21);
    let mut late = vec![0.0f64; 40];
    late.extend_from_slice(&base);
    let ints = |x: &[f64]| -> Vec<i16> {
        x.iter()
            .map(|&v| (v * f64::from(i16::MAX)) as i16)
            .collect()
    };
    let r = super::cross_correlate(Mono::new(&ints(&late)), Mono::new(&ints(&base)));
    assert_eq!(r.peak().unwrap().lag(), 40);
}

fn noise(len: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    (0..len).map(|_| uniform(&mut rng, -1.0, 1.0)).collect()
}

/// A hand-worked case: `a = [1, 2, 3]`, `b = [0, 1, 0.5]`, with
/// `r[lag] = sum a[n + lag] b[n]` summed out lag by lag below.
#[test]
fn a_small_case_matches_the_hand_worked_sums() {
    let r = cross_correlate(&[1.0, 2.0, 3.0], &[0.0, 1.0, 0.5]);
    // lag -2: a[0]b[2]                 = 0.5
    // lag -1: a[0]b[1] + a[1]b[2]      = 1 + 1   = 2
    // lag  0: a[0]b[0] + a[1]b[1] + a[2]b[2] = 0 + 2 + 1.5 = 3.5
    // lag  1: a[1]b[0] + a[2]b[1]      = 3
    // lag  2: a[2]b[0]                 = 0
    let expected = [0.5, 2.0, 3.5, 3.0, 0.0];
    assert_eq!((r.min_lag(), r.max_lag()), (-2, 2));
    for (lag, want) in (-2..=2).zip(expected) {
        let got = r.at(lag).unwrap();
        assert!((got - want).abs() < 1e-5, "lag {lag}: {got} vs {want}");
    }
    assert_eq!(r.at(3), None);
    assert_eq!(r.at(-3), None);
}

/// A delayed copy peaks at exactly its delay, positive when `a` lags `b`
/// and negative the other way, and unequal lengths work.
#[test]
fn a_delayed_copy_peaks_at_its_delay_in_either_direction() {
    let base = noise(700, 7);
    let delay = 123usize;

    let mut late = vec![0.0f64; delay];
    late.extend_from_slice(&base);
    let r = cross_correlate(&late, &base);
    assert_eq!(r.peak().unwrap().lag(), 123);

    let r = cross_correlate(&base, &late);
    assert_eq!(r.peak().unwrap().lag(), -123);

    // The longest lag in range still lands where it should.
    let tail: Vec<f64> = base[600..].to_vec();
    let r = cross_correlate(&base, &tail);
    assert_eq!(r.peak().unwrap().lag(), 600);
}

/// A buffer against itself peaks at lag zero with its energy, and the
/// result is symmetric about it.
#[test]
fn autocorrelation_peaks_at_zero_with_the_energy_and_is_symmetric() {
    let x = noise(513, 3);
    let r = cross_correlate(&x, &x);
    let energy: f64 = x.iter().map(|v| v * v).sum();
    let peak = r.peak().unwrap();
    assert_eq!(peak.lag(), 0);
    assert!((peak.value() - energy).abs() < energy * 1e-4);
    for lag in 1..=512 {
        let (p, n) = (r.at(lag).unwrap(), r.at(-lag).unwrap());
        assert!((p - n).abs() < energy * 1e-4, "lag {lag}: {p} vs {n}");
    }
}

/// `peak_within` ignores a stronger peak beyond the bound, clamps an
/// oversized bound, and returns `None` for empty input.
#[test]
fn peak_within_respects_the_bound() {
    let base = noise(300, 11);
    let mut late = vec![0.0f64; 100];
    late.extend_from_slice(&base);
    let r = cross_correlate(&late, &base);
    assert_eq!(r.peak_within(Samples::new(100)).unwrap().lag(), 100);
    assert_ne!(r.peak_within(Samples::new(99)).unwrap().lag(), 100);
    assert_eq!(r.peak_within(Samples::new(usize::MAX)), r.peak());
    assert_eq!(cross_correlate(&[], &base).peak(), None);
    assert_eq!(cross_correlate(&base, &[]).values().len(), 0);
}

/// The shaping hook sees the cross-spectrum before the inverse: zeroing it
/// zeroes the result.
#[test]
fn the_shape_hook_reaches_the_cross_spectrum() {
    let x = noise(64, 5);
    let mut fft = FftScratch::new();
    let r = cross_correlate_shaped(&x, &x, &mut fft, |bins| bins.fill(Complex64::default()));
    assert!(r.values().iter().all(|&v| v == 0.0));
}

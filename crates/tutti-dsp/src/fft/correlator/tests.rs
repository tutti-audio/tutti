use super::*;
use tutti_units::Real;

fn correlate(a: &[f32], b: &[f32], size: usize) -> Vec<f32> {
    let mut out = vec![0.0; size];
    Correlator::new().correlate(
        Mono::new(a),
        Mono::new(b),
        &mut FftScratch::new(),
        |_| {},
        MonoMut::new(&mut out),
    );
    out
}

/// `a = [1, 2, 3]` against `b = [0, 1, 0.5]` on 8 points: with
/// `r[lag] = sum a[n + lag] b[n]`, lags -2..=2 are 0.5, 2, 3.5, 3 and 0, the
/// negative ones wrapped to the end.
#[test]
fn a_small_case_is_the_hand_worked_sums() {
    let r = correlate(&[1.0, 2.0, 3.0], &[0.0, 1.0, 0.5], 8);
    let want = [3.5, 3.0, 0.0, 0.0, 0.0, 0.0, 0.5, 2.0];
    for (k, (got, want)) in r.iter().zip(want).enumerate() {
        assert!((got - want).abs() < 1e-5, "index {k}: {got} vs {want}");
    }
}

/// A buffer against itself has its energy at lag zero and is symmetric about it.
#[test]
fn autocorrelation_is_the_energy_at_zero_and_symmetric() {
    let x: Vec<f32> = (0..20).map(|i| ((i * 7 % 11) as f32) - 5.0).collect();
    let r = correlate(&x, &x, 64);
    let energy: f32 = x.iter().map(|v| v * v).sum();
    assert!((r[0] - energy).abs() < 1e-3);
    for k in 1..20 {
        assert!((r[k] - r[64 - k]).abs() < 1e-3, "lag {k}");
    }
}

/// A smaller transform than the linear result aliases only the far lags: on 7
/// points a 6-long signal against a 3-long one keeps lags 0..=4 exact, where
/// `size - b.len()` = 4 is the last unaliased one.
#[test]
fn a_short_transform_keeps_the_short_lags_exact() {
    let a = [1.0, -2.0, 3.0, 1.0, 0.5, 2.0];
    let b = [2.0, 1.0, -1.0];
    let r = correlate(&a, &b, 7);
    for lag in 0..=4usize {
        let direct: f32 = (0..3)
            .filter(|&n| n + lag < 6)
            .map(|n| a[n + lag] * b[n])
            .sum();
        assert!(
            (r[lag] - direct).abs() < 1e-5,
            "lag {lag}: {} vs {direct}",
            r[lag]
        );
    }
}

/// The shape sees `size / 2 + 1` bins and a flat cross-spectrum of ones makes
/// the correlation a unit impulse at lag zero.
#[test]
fn a_shaped_spectrum_is_what_is_inverted() {
    let mut out = vec![0.0; 16];
    let mut seen = 0;
    Correlator::with_capacity(16).correlate(
        Mono::new(&[1.0, 2.0]),
        Mono::new(&[3.0]),
        &mut FftScratch::new(),
        |bins| {
            seen = bins.len();
            bins.fill(Complex::new(1.0, 0.0));
        },
        MonoMut::new(&mut out),
    );
    assert_eq!(seen, 9);
    assert!((out[0] - 1.0).abs() < 1e-6);
    assert!(out[1..].iter().all(|v| v.abs() < 1e-6));
}
